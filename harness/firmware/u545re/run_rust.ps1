<#
.SYNOPSIS
    FlashDB-rs real-hardware test -- Rust firmware on NUCLEO-U545RE-Q.

.DESCRIPTION
    Builds the Rust measurement firmware with the `board-u545` feature, flashes
    it through the on-board ST-LINK/V3 with probe-rs, waits for the DONE flag,
    reads the BOARD_OUT RAM buffer back over SWD and prints/verifies the RESULT
    and BOARD lines.

    Artefacts land in harness/firmware/build/u545re/ (ELF, probe log, raw
    result buffer, decoded output).  Shared helpers live in board_common.ps1.

.PARAMETER BuildOnly       Build the ELF and print its sections; no hardware.
.PARAMETER NoBuild         Reuse an existing ELF and go straight to flash + run.
.PARAMETER Chip            probe-rs chip name (default STM32U545RE).
.PARAMETER Profile         Cargo profile: release (opt-level="s") or size ("z").
.PARAMETER Features        Extra crate features, e.g. -Features small-crc.
.PARAMETER CoreHz          Override the cycle->us clock (default: firmware value).
.PARAMETER TimeoutSec      Seconds to wait for DONE (default 120).
.PARAMETER InlineThreshold LLVM -inline-threshold (default 50: measured best ROM+cycles
                           trade-off under the size profile; 100/200 under test).
.PARAMETER OutDir          Artifact directory (default harness/firmware/build/u545re).

.EXAMPLE
    harness/firmware/u545re/run_rust.ps1

.EXAMPLE
    harness/firmware/u545re/run_rust.ps1 -Profile size -Features small-crc

.EXAMPLE
    harness/firmware/u545re/run_rust.ps1 -BuildOnly
#>
[CmdletBinding()]
param(
    [switch]$BuildOnly,
    [switch]$NoBuild,
    [switch]$Nightly,
    [string]$Chip = 'STM32U545RE',
    [string]$Profile = 'release',
    [string[]]$Features = @(),
    [int]$CoreHz = 0,
    [int]$TimeoutSec = 120,
    [int]$InlineThreshold = 50,
    [string]$OutDir = ''
)

$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\board_common.ps1"

if (-not $OutDir) { $OutDir = $script:RustOut }
if (-not [System.IO.Path]::IsPathRooted($OutDir)) { $OutDir = Join-Path $script:RepoRoot $OutDir }

if ($Nightly) {
    # Smallest measured layout: nightly -Z build-std + panic=immediate-abort.
    $Profile = 'size'
    if ($Features -notcontains 'small-crc') { $Features += 'small-crc' }
}
$elf = Join-Path $OutDir (Get-RustElfName -Profile $Profile -Features $Features -Nightly:$Nightly -InlineThreshold $InlineThreshold)

if ($NoBuild) {
    if (-not (Test-Path $elf)) { throw "missing ELF: $elf (run without -NoBuild)" }
} else {
    Write-Host '== building Rust firmware for thumbv8m.main-none-eabi (board-u545) =='
    $elf = Build-RustFirmware -Profile $Profile -Features $Features -Nightly:$Nightly -InlineThreshold $InlineThreshold -OutDir $OutDir
}

$size = Find-SizeTool
if ($BuildOnly) {
    if ($size) { Write-Host ''; & $size -A $elf | Write-Host }
    Write-Host 'build only: skipping flash/run'
    exit 0
}

Write-Host ''
Write-Host "== flash + run on NUCLEO-U545RE-Q (chip $Chip, probe-rs, Rust) =="
$run = Invoke-BoardRun -Elf $elf -Chip $Chip -TimeoutSec $TimeoutSec -OutDir $OutDir
Format-BoardRun $run -Title 'Rust firmware' -CoreHz $CoreHz

if ([string]$run.Fields['rc'] -ne '0') { throw 'workload reported a failure (rc != 0)' }
Write-Host 'PASS: Rust flashdb ran on the NUCLEO-U545RE-Q and rc=0'
exit 0
