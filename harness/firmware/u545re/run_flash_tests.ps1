<#
.SYNOPSIS
    On-silicon internal-flash endurance / long-run tests (NUCLEO-U545RE-Q).

.DESCRIPTION
    Builds and flashes one of the two real-flash test images, waits for the
    firmware's DONE flag, reads the RAM result buffer back over SWD and prints
    the RESULT/BOARD lines.

      endurance : real 8 KiB page erase + 128-bit quad-word program cycles with
                  per-cycle verification, time-budgeted (~60 s);
      longrun   : real KVDB + TSDB on internal flash (4+4 x 8 KiB partitions,
                  WRITE_GRAN=128) for ~60 s of operation, per-round verification
                  and periodical remounts.

    Both use the driver in harness/firmware/rust/hwflash.rs and only touch the
    scratch pages 24..31 of bank 1 (0x0803_0000..0x0803_FFFF); the firmware
    image lives in the first pages and is never erased.

.PARAMETER Test          endurance or longrun.
.PARAMETER BuildOnly     Build + print sections only, no hardware.
.PARAMETER NoBuild       Reuse the existing ELF and go straight to flash + run.
.PARAMETER TimeoutSec    Seconds to wait for DONE (default 300: 60 s workload +
                         flash + probe latency).
.PARAMETER OutDir        Artifact directory (default harness/firmware/build/u545re).

.EXAMPLE
    harness/firmware/u545re/run_flash_tests.ps1 -Test endurance
#>
[CmdletBinding()]
param(
    [ValidateSet('endurance', 'longrun')][string]$Test = 'endurance',
    [switch]$BuildOnly,
    [switch]$NoBuild,
    [string]$Chip = 'STM32U545RE',
    [int]$TimeoutSec = 400,
    [string]$OutDir = ''
)

$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\board_common.ps1"
if (-not $OutDir) { $OutDir = $script:RustOut }

$feature = "flash-$Test"
$elf = Join-Path $OutDir (Get-RustElfName -Profile size -Features @($feature))

if ($NoBuild) {
    if (-not (Test-Path $elf)) { throw "missing ELF: $elf (run without -NoBuild)" }
} else {
    Write-Host "== building Rust flash-$Test firmware (size profile, board-u545) =="
    $elf = Build-RustFirmware -Profile size -Features @($feature) -OutDir $OutDir
}

$size = Find-SizeTool
if ($BuildOnly) {
    if ($size) { Write-Host ''; & $size -A $elf | Write-Host }
    Write-Host 'build only: skipping flash/run'
    exit 0
}

Write-Host ''
Write-Host "== forcing flash tests ($Test) on NUCLEO-U545RE-Q =="
$runDir = Join-Path $OutDir "flash-$Test"
$run = Invoke-BoardRun -Elf $elf -Chip $Chip -TimeoutSec $TimeoutSec -OutDir $runDir
Format-BoardRun $run -Title "Rust flash-$Test firmware"

if ([string]$run.Fields['rc'] -ne '0') {
    throw "flash-$Test workload reported a failure (rc != 0); see $($run.TextPath)"
}
Write-Host "PASS: flash-$Test completed with rc=0"
exit 0
