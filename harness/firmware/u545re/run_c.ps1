<#
.SYNOPSIS
    FlashDB-rs real-hardware test -- C FlashDB reference on NUCLEO-U545RE-Q.

.DESCRIPTION
    Builds the C FlashDB reference firmware for the STM32U545RE board
    (arm-none-eabi-gcc -Os, -DFDB_BOARD_U545, harness/firmware/u545re/board.ld),
    flashes it with probe-rs, waits for DONE, reads BOARD_OUT back over SWD and
    prints/verifies the RESULT and BOARD lines -- the same workload and the same
    measurement points as run_rust.ps1, so the two can be compared field by
    field on identical silicon.

    The C compiler is taken from PATH or, when missing, from the GNU Tools for
    STM32 bundled with STM32CubeIDE (auto-detected).

.PARAMETER BuildOnly       Build the ELF and print its sections; no hardware.
.PARAMETER NoBuild         Reuse an existing ELF and go straight to flash + run.
.PARAMETER Chip            probe-rs chip name (default STM32U545RE).
.PARAMETER CoreHz          Override the cycle->us clock (default: firmware value).
.PARAMETER TimeoutSec      Seconds to wait for DONE (default 180).

.EXAMPLE
    harness/firmware/u545re/run_c.ps1

.EXAMPLE
    harness/firmware/u545re/compare.ps1 -BuildOnly
#>
[CmdletBinding()]
param(
    [switch]$BuildOnly,
    [switch]$NoBuild,
    [string]$Chip = 'STM32U545RE',
    [int]$CoreHz = 0,
    [int]$TimeoutSec = 180
)

$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\board_common.ps1"

$elf = Join-Path $script:COut 'fw_c_u545.elf'
if ($NoBuild) {
    if (-not (Test-Path $elf)) { throw "missing ELF: $elf (run without -NoBuild)" }
} else {
    Write-Host '== building C FlashDB reference firmware for thumbv8m.main-none-eabi =='
    $elf = Build-CFirmware
}

$size = Find-SizeTool
if ($BuildOnly) {
    if ($size) { Write-Host ''; & $size -A $elf | Write-Host }
    Write-Host 'build only: skipping flash/run'
    exit 0
}

Write-Host ''
Write-Host "== flash + run on NUCLEO-U545RE-Q (chip $Chip, probe-rs, C) =="
$run = Invoke-BoardRun -Elf $elf -Chip $Chip -TimeoutSec $TimeoutSec -OutDir $script:COut
Format-BoardRun $run -Title 'C FlashDB firmware' -CoreHz $CoreHz

if ([string]$run.Fields['rc'] -ne '0') { throw 'workload reported a failure (rc != 0)' }
Write-Host 'PASS: C FlashDB ran on the NUCLEO-U545RE-Q and rc=0'
exit 0
