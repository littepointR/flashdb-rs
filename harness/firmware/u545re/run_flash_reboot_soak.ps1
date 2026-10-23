<#
.SYNOPSIS
    On-silicon reboot soak: real-flash KVDB + TSDB across 20 self-resets
    (NUCLEO-U545RE-Q).

.DESCRIPTION
    Flashes the `flash-reboot-soak` image once and lets it reset itself with
    `NVIC_SystemReset` between deterministic write batches (a warm reset:
    SRAM is preserved, the firmware restarts from flash).  The round counter
    lives in a NOLOAD `.results`-section static, so every boot:

      * reads the round number and mounts both databases from the internal
        flash image the previous boot left behind;
      * verifies the previous round's batch before writing the next one
        (all 8 KV key generations + TS last timestamp + full KV CRC check);
      * writes one deterministic batch (64 KV updates = 8 keys rewritten 8x,
        20 TS appends with timestamps 1..400 and rollover enabled - the 32 KiB
        TS ring wraps twice across the run);
      * resets again.

    The final boot remounts both databases, verifies the complete expected
    state (all key generations, contiguous TS window ending at timestamp 400,
    exact payloads, CRC and by-time query count), prints `RESULT ... rc=0` and
    sets the DONE flag.  The image clears the persisted round counter after the
    result, so a new run always starts from round 0.

    This complements the host power-cut tests: they inject a failure at every
    flash operation of a single workload; this one proves that the real
    controller's state survives 19 warm resets with the database reopened from
    flash each time.

.PARAMETER BuildOnly        Build the ELF and print its sections; no hardware.
.PARAMETER NoBuild          Reuse the existing ELF and go straight to hardware.
.PARAMETER Chip             probe-rs chip name (default STM32U545RE).
.PARAMETER TimeoutSec       Seconds to wait for DONE (default 240; the run is
                            20 boots of a few hundred ms each).
.PARAMETER ExpectedRounds   Rounds the firmware must report (default 20; must
                            match flash_reboot_soak.rs ROUNDS).
.PARAMETER OutDir           Artifact directory (default harness/firmware/build/u545re).

.EXAMPLE
    harness/firmware/u545re/run_flash_reboot_soak.ps1 -BuildOnly
    harness/firmware/u545re/run_flash_reboot_soak.ps1
    harness/firmware/u545re/run_flash_reboot_soak.ps1 -NoBuild -TimeoutSec 300
#>
[CmdletBinding()]
param(
    [switch]$BuildOnly,
    [switch]$NoBuild,
    [string]$Chip = 'STM32U545RE',
    [int]$TimeoutSec = 240,
    [int]$ExpectedRounds = 20,
    [string]$OutDir = ''
)

$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\board_common.ps1"

if (-not $OutDir) { $OutDir = $script:RustOut }
if (-not [System.IO.Path]::IsPathRooted($OutDir)) { $OutDir = Join-Path $script:RepoRoot $OutDir }
$OutDir = (New-Item -ItemType Directory -Force -Path $OutDir).FullName

# Workload constants of flash_reboot_soak.rs (part of the reported schema).
$KvSetsPerRound = 64
$TsPerRound = 20

$elf = Join-Path $OutDir (Get-RustElfName -Profile size -Features @('flash-reboot-soak'))

if ($NoBuild) {
    if (-not (Test-Path $elf)) { throw "missing ELF: $elf (run without -NoBuild)" }
} else {
    Write-Host '== building Rust flash-reboot-soak firmware (size profile, board-u545) =='
    $elf = Build-RustFirmware -Profile size -Features @('flash-reboot-soak') -OutDir $OutDir
}

$size = Find-SizeTool
if ($BuildOnly) {
    if ($size) { Write-Host ''; & $size -A $elf | Write-Host }
    Write-Host 'build only: skipping hardware'
    exit 0
}

Write-Host ''
Write-Host "== flashing reboot-soak image once and waiting for $ExpectedRounds self-reset rounds =="
Write-Host "   elf: $elf  timeout: $TimeoutSec s"
$run = Invoke-BoardRun -Elf $elf -Chip $Chip -TimeoutSec $TimeoutSec -OutDir (Join-Path $OutDir 'flash-reboot-soak')
Format-BoardRun $run -Title 'Rust flash-reboot-soak firmware'

$f = $run.Fields
function Get-ResultField([string]$Key) {
    if (-not $f.ContainsKey($Key)) { throw "RESULT is missing field '$Key'; see $($run.TextPath)" }
    return $f[$Key]
}

$rc = [string](Get-ResultField 'rc')
$rounds = [int](Get-ResultField 'rounds')
$roundsDone = [int](Get-ResultField 'rounds_done')
$boots = [int](Get-ResultField 'boots')
$resets = [int](Get-ResultField 'resets')
$kvSets = [int](Get-ResultField 'kv_sets')
$tsAppends = [int](Get-ResultField 'ts_appends')
$kvKeys = [int](Get-ResultField 'kv_keys')
$tsEntries = [int](Get-ResultField 'ts_entries')
$tsFirst = [int](Get-ResultField 'ts_first')
$tsLast = [int](Get-ResultField 'ts_last')
$checksum = [string](Get-ResultField 'checksum')

$expectKvSets = $ExpectedRounds * $KvSetsPerRound
$expectTsAppends = $ExpectedRounds * $TsPerRound
$expectKvKeys = 8
$errors = New-Object System.Collections.Generic.List[string]

if ($rc -ne '0') { $errors.Add("rc=$rc (expected 0)") }
if ($rounds -ne $ExpectedRounds) { $errors.Add("rounds=$rounds (expected $ExpectedRounds)") }
if ($roundsDone -ne $ExpectedRounds) { $errors.Add("rounds_done=$roundsDone (expected $ExpectedRounds)") }
if ($resets -ne ($ExpectedRounds - 1)) { $errors.Add("resets=$resets (expected $($ExpectedRounds - 1))") }
if ($boots -lt $ExpectedRounds) { $errors.Add("boots=$boots (expected >= $ExpectedRounds)") }
elseif ($boots -gt $ExpectedRounds) { Write-Host "note: boots=$boots > $ExpectedRounds - run resumed from an earlier interrupted round counter" }
if ($kvSets -ne $expectKvSets) { $errors.Add("kv_sets=$kvSets (expected $expectKvSets)") }
if ($tsAppends -ne $expectTsAppends) { $errors.Add("ts_appends=$tsAppends (expected $expectTsAppends)") }
if ($kvKeys -ne $expectKvKeys) { $errors.Add("kv_keys=$kvKeys (expected $expectKvKeys)") }
if ($tsLast -ne $expectTsAppends) { $errors.Add("ts_last=$tsLast (expected $expectTsAppends)") }
if ($tsEntries -lt 1) { $errors.Add("ts_entries=$tsEntries (expected >= 1)") }
if ($tsFirst -ne ($tsLast - $tsEntries + 1)) { $errors.Add("ts window not contiguous: first=$tsFirst last=$tsLast entries=$tsEntries") }
if ($errors.Count -ne 0) {
    foreach ($e in $errors) { Write-Host "FAIL: $e" }
    throw "flash-reboot-soak failed ($($errors.Count) check(s)); see $($run.TextPath)"
}

Write-Host ''
Write-Host ("   rounds={0} boots={1} resets={2} kv_sets={3} ts_appends={4}" -f `
    $rounds, $boots, $resets, $kvSets, $tsAppends)
Write-Host ("   kv_keys={0} ts_window=[{1}..{2}] ({3} entries) checksum={4}" -f `
    $kvKeys, $tsFirst, $tsLast, $tsEntries, $checksum)
Write-Host ("PASS: {0} reboot rounds persisted and verified on the NUCLEO-U545RE-Q" -f $rounds)
exit 0
