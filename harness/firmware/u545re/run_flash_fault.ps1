<#
.SYNOPSIS
    Real-flash power-loss injection on NUCLEO-U545RE-Q (internal STM32U5 flash).

.DESCRIPTION
    The gap this covers: FlashDB's power-cut recovery is simulated on the host,
    but the reset has never interrupted a real internal-flash program/erase on
    silicon.  This runner repeats N cycles of:

      1. build both images once (writer: `flash-fault-write`, verifier:
         `flash-fault-verify`, size profile + `board-u545`);
      2. flash the *writer* image, which churns a KVDB (16 keys x 32 B) and a
         TSDB (32 B payloads, rollover) on the real flash controller forever;
      3. sleep a randomized delay (default 20..250 ms, so resets land inside
         128-bit program and 8 KiB page-erase windows; cycle 0 is primed with
         -PrimeMs so the first injection hits the steady-state workload and not
         the initial format/reformat of foreign scratch-page contents);
      4. `probe-rs reset` while the writer runs: an external reset in the middle
         of flash activity;
      5. flash the *verifier* image and require `rc=0`.  It mounts both
         databases from the interrupted flash state and checks: mount succeeds,
         every committed KV record passes CRC, every live value is
         generation-consistent, the TS window is contiguous with exact payloads
         (torn records never committed), `last_time` is right, reverse iteration
         mirrors forward, new writes are accepted, and the continued state
         survives a remount.  It reports the writer's post-mortem trace
         (`trace_op`: 5 = erase, 6 = program) so the log shows which cycles were
         reset inside erase/program windows.

    PASS only if every cycle's verifier reports rc=0 and the writer did not
    publish an error through the trace.

    Images and the shared trace block are described in
    harness/firmware/rust/flash_fault_write.rs / flash_fault_verify.rs and
    hwflash.rs (scratch pages 24..31 only; the firmware image is never touched).

.PARAMETER BuildOnly        Build both ELFs and print their sections; no hardware.
.PARAMETER NoBuild          Reuse the existing ELFs and go straight to hardware.
.PARAMETER Iterations       Number of reset-injection cycles (default 20).
.PARAMETER Seed             PRNG seed for the delays (default 0 = random; the
                            resolved seed is printed so a run can be replayed).
.PARAMETER MinDelayMs       Minimum writer run time before the reset (default 20).
.PARAMETER MaxDelayMs       Maximum writer run time before the reset (default 250).
.PARAMETER PrimeMs          Delay for cycle 0 (default 800 ms; the first writer
                            boot may have to format foreign scratch-page data).
.PARAMETER NoPrime          Use a random delay for cycle 0 as well.
.PARAMETER Chip             probe-rs chip name (default STM32U545RE).
.PARAMETER TimeoutSec       Seconds to wait for each verifier's DONE flag (default 60).
.PARAMETER OutDir           Artifact directory (default harness/firmware/build/u545re).

.EXAMPLE
    harness/firmware/u545re/run_flash_fault.ps1 -BuildOnly
    harness/firmware/u545re/run_flash_fault.ps1 -Iterations 20 -Seed 1234
    harness/firmware/u545re/run_flash_fault.ps1 -Iterations 10 -MinDelayMs 15 -MaxDelayMs 120
#>
[CmdletBinding()]
param(
    [switch]$BuildOnly,
    [switch]$NoBuild,
    [int]$Iterations = 20,
    [int]$Seed = 0,
    [int]$MinDelayMs = 20,
    [int]$MaxDelayMs = 250,
    [int]$PrimeMs = 800,
    [switch]$NoPrime,
    [string]$Chip = 'STM32U545RE',
    [int]$TimeoutSec = 60,
    [string]$OutDir = ''
)

$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\board_common.ps1"

if (-not $OutDir) { $OutDir = $script:RustOut }
if (-not [System.IO.Path]::IsPathRooted($OutDir)) { $OutDir = Join-Path $script:RepoRoot $OutDir }
$OutDir = (New-Item -ItemType Directory -Force -Path $OutDir).FullName

if ($Iterations -lt 1) { throw "-Iterations must be >= 1" }
if ($MinDelayMs -lt 1) { throw "-MinDelayMs must be >= 1" }
if ($MaxDelayMs -lt $MinDelayMs) { throw "-MaxDelayMs must be >= -MinDelayMs" }

$writerElf = Join-Path $OutDir (Get-RustElfName -Profile size -Features @('flash-fault-write'))
$verifyElf = Join-Path $OutDir (Get-RustElfName -Profile size -Features @('flash-fault-verify'))

if ($NoBuild) {
    foreach ($elf in @($writerElf, $verifyElf)) {
        if (-not (Test-Path $elf)) { throw "missing ELF: $elf (run without -NoBuild)" }
    }
} else {
    Write-Host '== building Rust flash-fault-write firmware (size profile, board-u545) =='
    $writerElf = Build-RustFirmware -Profile size -Features @('flash-fault-write') -OutDir $OutDir
    Write-Host '== building Rust flash-fault-verify firmware (size profile, board-u545) =='
    $verifyElf = Build-RustFirmware -Profile size -Features @('flash-fault-verify') -OutDir $OutDir
}

$size = Find-SizeTool
if ($BuildOnly) {
    foreach ($elf in @($writerElf, $verifyElf)) {
        if ($size) { Write-Host ''; Write-Host "== $([System.IO.Path]::GetFileName($elf)) =="; & $size -A $elf | Write-Host }
    }
    Write-Host 'build only: skipping hardware'
    exit 0
}

# ---- hardware helpers ------------------------------------------------------

# Flash an image and (re)start it; no DONE polling (the writer never finishes).
function Invoke-BoardFlash {
    param([string]$Elf, [string]$Log, [string]$Dir)
    New-Item -ItemType Directory -Force -Path $Dir | Out-Null
    $out = & probe-rs download --chip $Chip --verify --reset --log-file $Log $Elf 2>&1 | Out-String
    if ($LASTEXITCODE -ne 0) {
        Write-Host $out
        throw "probe-rs download failed for $Elf (rc=$LASTEXITCODE; log: $Log)"
    }
}

# Reset the running target.  `probe-rs reset` starts the core again after the
# reset; the exact option spelling has changed across probe-rs releases, so try
# the documented forms and report the last failure if none works.
function Invoke-TargetReset {
    $candidates = @(
        @('reset', '--chip', $Chip),
        @('reset', '--chip', $Chip, '--halt'),
        @('reset')
    )
    $last = ''
    foreach ($resetArgs in $candidates) {
        $out = & probe-rs @resetArgs 2>&1 | Out-String
        if ($LASTEXITCODE -eq 0) { return }
        $last = "probe-rs $($resetArgs -join ' ') -> rc=$LASTEXITCODE`n$out"
    }
    throw "probe-rs reset failed for every known invocation:`n$last"
}

# ---- reset-injection loop --------------------------------------------------

if ($Seed -eq 0) { $Seed = [int](Get-Random -Minimum 1 -Maximum 2147483647) }
$rng = New-Object System.Random($Seed)
Write-Host ''
Write-Host "== power-loss injection: $Iterations cycles on NUCLEO-U545RE-Q (chip $Chip) =="
Write-Host ("   seed={0} delays={1}..{2} ms prime={3} ms timeout={4} s" -f `
    $Seed, $MinDelayMs, $MaxDelayMs, $(if ($NoPrime) { 'random' } else { "$PrimeMs" }), $TimeoutSec)
Write-Host "   writer  : $writerElf"
Write-Host "   verifier: $verifyElf"

$runDir = Join-Path $OutDir 'flash-fault'
New-Item -ItemType Directory -Force -Path $runDir | Out-Null
$summaryPath = Join-Path $runDir 'summary.txt'
$summary = New-Object System.Collections.Generic.List[string]
$summary.Add(("cycle delay_ms rc trace_valid trace_op trace_addr trace_erases trace_programs trace_seq trace_error kv_keys ts_entries ts_prewrite"))

$opNames = @{ 0 = 'between-ops'; 5 = 'erase'; 6 = 'program'; 0x100 = 'boot' }
$opCounts = @{ erase = 0; program = 0; between = 0; boot = 0; unknown = 0 }
$failures = 0

for ($i = 0; $i -lt $Iterations; $i++) {
    if ($i -eq 0 -and -not $NoPrime) {
        $delayMs = $PrimeMs
    } else {
        $delayMs = $rng.Next($MinDelayMs, $MaxDelayMs + 1)
    }
    $cycleDir = Join-Path $runDir ("cycle-{0:D2}" -f ($i + 1))

    Write-Host ("[{0,2}/{1}] flash writer, run {2} ms, reset ..." -f ($i + 1), $Iterations, $delayMs) -NoNewline
    Invoke-BoardFlash -Elf $writerElf -Log (Join-Path $cycleDir 'writer_flash.log') -Dir $cycleDir
    Start-Sleep -Milliseconds $delayMs
    Invoke-TargetReset

    $run = Invoke-BoardRun -Elf $verifyElf -Chip $Chip -TimeoutSec $TimeoutSec -OutDir $cycleDir -Quiet
    $f = $run.Fields
    $rc = [string]$f['rc']
    $traceValid = [int]$f['trace_valid']
    $traceOp = if ($f.ContainsKey('trace_op')) { [int]$f['trace_op'] } else { 0 }
    $traceAddr = [string]$f['trace_addr']
    $traceErases = [int]$f['trace_erases']
    $tracePrograms = [int]$f['trace_programs']
    $traceSeq = [int]$f['trace_seq']
    $traceError = [string]$f['trace_error']

    $opName = if ($opNames.ContainsKey($traceOp)) { $opNames[$traceOp] } else { 'unknown' }
    if ($opName -eq 'between-ops') { $opCounts['between']++ }
    elseif ($opName -eq 'unknown') { $opCounts['unknown']++ }
    else { $opCounts[$opName]++ }

    $summary.Add(("{0} {1} {2} {3} {4} {5} {6} {7} {8} {9} {10} {11} {12}" -f `
        ($i + 1), $delayMs, $rc, $traceValid, $traceOp, $traceAddr, $traceErases, `
        $tracePrograms, $traceSeq, $traceError, $f['kv_keys'], $f['ts_entries'], $f['ts_prewrite']))

    Write-Host (" rc={0} trace={1}/{2} (op={3} addr={4}, +{5} erases +{6} programs, seq={7})" -f `
        $rc, $traceValid, $traceError, $opName, $traceAddr, $traceErases, $tracePrograms, $traceSeq)

    if ($rc -ne '0') {
        $failures++
        Write-Host '-- verifier output --'
        Write-Host $run.Text.TrimEnd()
        Write-Host "-- verifier fields --"
        Write-Host (($f.Keys | Sort-Object | ForEach-Object { "$_=$($f[$_])" }) -join ' ')
        Write-Host "result bin: $($run.DumpPath)  text: $($run.TextPath)"
        throw ("cycle {0} did not recover: verifier rc={1} (delay {2} ms, trace op {3})" -f `
            ($i + 1), $rc, $delayMs, $opName)
    }
    if ($traceValid -eq 1 -and "$traceError" -ne '0x00000000' -and "$traceError" -ne '') {
        throw ("cycle {0}: writer reported a flash/driver failure (trace_error={1}; stage = error >> 16)" -f `
            ($i + 1), $traceError)
    }
    if ($traceValid -eq 1 -and $traceSeq -eq 0) {
        Write-Host ("   note: cycle {0} reset before the writer reached its first flash operation" -f ($i + 1))
    }
    if ($traceValid -eq 0) {
        Write-Host ("   note: cycle {0} trace invalid - reset landed outside a traceable window or SRAM did not survive the reset" -f ($i + 1))
    }
}

$summary | Set-Content -Path $summaryPath -Encoding ascii

Write-Host ''
Write-Host '-- reset windows (writer trace of the interrupted operation) --'
Write-Host ('   erases={0} programs={1} between-ops={2} before-workload={3} unknown={4}' -f `
    $opCounts['erase'], $opCounts['program'], $opCounts['between'], $opCounts['boot'], $opCounts['unknown'])
Write-Host '   note: a reset during an erase/program also aborts the controller; the next verifier mount is exactly the recovery path under test.'
Write-Host ''
Write-Host "summary: $summaryPath"
foreach ($line in $summary) { Write-Host "  $line" }

if ($failures -ne 0) { throw "$failures/$Iterations cycles failed" }
Write-Host ''
Write-Host ("PASS: {0}/{1} injected resets recovered with rc=0 on the NUCLEO-U545RE-Q" -f $Iterations, $Iterations)
exit 0
