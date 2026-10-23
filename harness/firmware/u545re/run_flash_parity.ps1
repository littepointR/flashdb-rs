<#
.SYNOPSIS
    On-silicon real internal-flash image parity: Rust crate vs host model.

.DESCRIPTION
    Builds the `flash-parity` Rust firmware, runs the host model from
    `crates/flashdb-bench/src/bin/parity_model.rs`, and (full run) flashes the
    firmware on the NUCLEO-U545RE-Q, reads the RESULT/BOARD lines back over SWD
    and compares every deterministic field against the host model.

    The firmware (`harness/firmware/rust/flash_parity.rs`) runs the real
    `flashdb` crate on STM32U5 internal flash (WRITE_GRAN=128, quad-word
    programming):

      * KV partition: 4 x 8 KiB internal-flash pages at 0x0803_0000
        (bank-1 pages 24..27), sec_size 8192, max_size 32768;
      * TS partition: 4 x 8 KiB pages at 0x0803_8000 (pages 28..31);
      * deterministic workload (shared source file, see parity_workload.rs):
        40 KV keys x 32 B + 20 updates + 10 deletes + 10 re-adds + 180 churn
        sets (forces GC), then 120 TS appends x 32 B (rollover disabled,
        timestamps 1000..1119);
      * format/clean, workload, remount, verify every value/payload and iterate
        both databases, then FNV-1a-32 over both raw 32 KiB partition images.

    The host model runs the identical workload on `flashdb-sim` with the same
    geometry and prints the expected `MODEL` line. A full run prints a
    PASS/FAIL table comparing expected vs board values; it exits 0 only if the
    board reports `rc=0 errors=0 verify_ok=1` and all six deterministic fields
    match the host model.

    Expected values (host model, must stay in sync; the script checks the model
    against them on every run):
        kv_entries=41        kv_checksum=0xe271b9e1
        ts_entries=120       ts_checksum=0x43bc2d05
        kv_image_fnv=0x199f5d76  ts_image_fnv=0x481067fb

    Hardware runs are serialised and owned by the Lead. Nobody else flashes the
    board; use -BuildOnly for a hardware-free build check.

.PARAMETER BuildOnly
    Build the firmware and print the host-model expected values, no hardware.
.PARAMETER NoBuild
    Reuse the existing ELF and go straight to flash + run.
.PARAMETER SkipModel
    Do not run the host model; compare the board against the documented
    expected values embedded in this script.
.PARAMETER Chip
    probe-rs chip name (default STM32U545RE).
.PARAMETER TimeoutSec
    Seconds to wait for the firmware's DONE flag (default 180; the workload
    itself takes well under 5 s, the rest is flash + probe latency).
.PARAMETER OutDir
    Artifact directory (default harness/firmware/build/u545re).

.EXAMPLE
    pwsh -NoProfile -File harness/firmware/u545re/run_flash_parity.ps1 -BuildOnly

.EXAMPLE
    pwsh -NoProfile -File harness/firmware/u545re/run_flash_parity.ps1
#>
[CmdletBinding()]
param(
    [switch]$BuildOnly,
    [switch]$NoBuild,
    [switch]$SkipModel,
    [string]$Chip = 'STM32U545RE',
    [int]$TimeoutSec = 180,
    [string]$OutDir = ''
)

$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\board_common.ps1"
$repoRoot = $script:RepoRoot
if (-not $OutDir) { $OutDir = $script:RustOut }

# Deterministic fields compared between the host model and the board RESULT.
$fields = @('kv_entries', 'kv_checksum', 'ts_entries', 'ts_checksum', 'kv_image_fnv', 'ts_image_fnv')

# Expected values of the shared deterministic workload (host model,
# 2026-10-23). This script re-runs the model and fails if it drifts, so a board
# run is always compared against freshly computed expectations.
$expected = [ordered]@{
    kv_entries   = '41'
    kv_checksum  = '0xe271b9e1'
    ts_entries   = '120'
    ts_checksum  = '0x43bc2d05'
    kv_image_fnv = '0x199f5d76'
    ts_image_fnv = '0x481067fb'
}

function ConvertTo-Number([string]$Value) {
    $v = $Value.Trim()
    if ($v -match '^0[xX][0-9a-fA-F]+$') { return [Convert]::ToUInt64($v.Substring(2), 16) }
    if ($v -match '^[0-9]+$') { return [UInt64]$v }
    throw "cannot parse numeric field value '$Value'"
}

function Test-SameField([string]$Name, [string]$A, [string]$B) {
    return (ConvertTo-Number $A) -eq (ConvertTo-Number $B)
}

# ---------------------------------------------------------------------------
# Host model: compute expected values (parity_model itself runs the workload
# twice, so a MODEL line also proves the workload is deterministic).
# ---------------------------------------------------------------------------
function Get-HostModel {
    if ($SkipModel) {
        Write-Host '== host model: skipped (-SkipModel), using documented expected values =='
        return $expected
    }
    Write-Host '== host model: cargo run -p flashdb-bench --bin parity_model =='
    Push-Location $repoRoot
    try {
        $out = & cargo run --quiet --release -p flashdb-bench --bin parity_model 2>&1
        $rc = $LASTEXITCODE
    } finally {
        Pop-Location
    }
    if ($rc -ne 0) {
        Write-Host ($out | Out-String)
        throw "host model failed (rc=$rc)"
    }
    $line = $out | Where-Object { $_ -match '^MODEL ' } | Select-Object -First 1
    if (-not $line) { throw "host model printed no MODEL line: $($out | Out-String)" }
    $model = @{}
    foreach ($tok in ($line -split '\s+')) {
        if ($tok -match '^([A-Za-z0-9_]+)=(.*)$') { $model[$Matches[1]] = $Matches[2] }
    }
    Write-Host "   $line"
    foreach ($f in $fields) {
        if (-not $model.ContainsKey($f)) { throw "host model MODEL line is missing '$f'" }
        if (-not (Test-SameField $f $model[$f] $expected[$f])) {
            throw "host model $f=$($model[$f]) != documented expected $($expected[$f]) -- the shared workload changed; update the expected values (and the report) before running the board"
        }
    }
    Write-Host '   host model matches the documented expected values'
    return $model
}

# ---------------------------------------------------------------------------
# Build firmware
# ---------------------------------------------------------------------------
$elf = Join-Path $OutDir (Get-RustElfName -Profile size -Features @('flash-parity'))
if ($NoBuild) {
    if (-not (Test-Path $elf)) { throw "missing ELF: $elf (run without -NoBuild)" }
} else {
    Write-Host '== building Rust flash-parity firmware (size profile, board-u545, WRITE_GRAN=128) =='
    $elf = Build-RustFirmware -Profile size -Features @('flash-parity') -OutDir $OutDir
}

$model = Get-HostModel

if ($BuildOnly) {
    $size = Find-SizeTool
    if ($size) { Write-Host ''; & $size -A $elf | Write-Host }
    Write-Host ''
    Write-Host 'expected (host model):'
    foreach ($f in $fields) { Write-Host ("  {0,-14} = {1}" -f $f, $model[$f]) }
    Write-Host 'build only: skipping flash/run'
    exit 0
}

# ---------------------------------------------------------------------------
# Full run: flash, read RESULT/BOARD back and compare against the host model
# ---------------------------------------------------------------------------
Write-Host ''
Write-Host '== running real-flash parity on NUCLEO-U545RE-Q (internal flash pages 24..31) =='
$runDir = Join-Path $OutDir 'flash-parity'
$run = Invoke-BoardRun -Elf $elf -Chip $Chip -TimeoutSec $TimeoutSec -OutDir $runDir
Format-BoardRun $run -Title 'Rust flash-parity firmware'

$failures = 0
Write-Host ''
Write-Host '-- parity vs host model --'
foreach ($f in $fields) {
    $got = if ($run.Fields.ContainsKey($f)) { [string]$run.Fields[$f] } else { '<missing>' }
    $ok = $false
    if ($got -ne '<missing>') {
        try { $ok = Test-SameField $f $got $model[$f] } catch { $ok = $false }
    }
    $verdict = if ($ok) { 'PASS' } else { 'FAIL' }
    if (-not $ok) { $failures++ }
    Write-Host ("  {0,-14} model={1,-12} board={2,-12} {3}" -f $f, $model[$f], $got, $verdict)
}

$rc = [string]$run.Fields['rc']
$errors = [string]$run.Fields['errors']
$verifyOk = [string]$run.Fields['verify_ok']
if ($rc -ne '0') { Write-Host "  rc=$rc (expected 0) FAIL"; $failures++ }
if ($errors -ne '0') { Write-Host "  errors=$errors (expected 0) FAIL"; $failures++ }
if ($verifyOk -ne '1') { Write-Host "  verify_ok=$verifyOk (expected 1) FAIL"; $failures++ }

$stack = if ($run.Fields.ContainsKey('stack')) { [int64]$run.Fields['stack'] } else { -1 }
$stackLimit = if ($run.Fields.ContainsKey('stack_limit')) { [int64]$run.Fields['stack_limit'] } else { 0 }
if ($stackLimit -gt 0 -and $stack -gt $stackLimit) {
    Write-Host "  stack=$stack > stack_limit=$stackLimit FAIL"
    $failures++
} elseif ($stack -ge 0) {
    Write-Host "  stack=$stack of $stackLimit (ok)"
}

Write-Host ''
if ($failures -ne 0) {
    Write-Host "FAIL: real-flash image parity ($failures mismatch(es); board output: $($run.TextPath))"
    exit 1
}
Write-Host 'PASS: real internal-flash image is byte-identical to the host model (all 6 fields + rc/errors/verify_ok)'
exit 0
