<#
.SYNOPSIS
    C FlashDB vs Rust flashdb -- side-by-side on the NUCLEO-U545RE-Q.

.DESCRIPTION
    Builds BOTH firmware images for the STM32U545RE, flashes each one in turn
    through the on-board ST-LINK/V3 with probe-rs, runs the identical
    KVDB+TSDB workload, reads the BOARD_OUT buffer back over SWD and prints a
    comparison table plus an acceptance verdict:

      * ROM (text+rodata+data), static RAM (data+bss), stack high-water and
        static+stack must be no worse than the C reference;
      * mount / KV / TS / total DWT cycles must be no worse than C;
      * every logical workload field (write/get/iterate counts, checksums, rc)
        must be byte-identical between the two images.

    Artefacts (ELF, raw result buffers, logs, comparison report) land in
    harness/firmware/build/{c_u545,u545re}/.

.PARAMETER RustProfile     Cargo profile for the Rust image (release or size).
.PARAMETER RustFeatures    Extra Rust crate features (e.g. small-crc).
.PARAMETER BuildOnly       Build both images and print the C section sizes only.
.PARAMETER NoBuild         Reuse existing ELFs (no rebuild).
.PARAMETER CoreHz          Override the cycle->us clock (default: firmware value).
.PARAMETER TimeoutSec      Seconds to wait for DONE per image (default 180).
.PARAMETER Report          Report path (default build/u545re/board_compare.txt).

.EXAMPLE
    harness/firmware/u545re/compare.ps1

.EXAMPLE
    harness/firmware/u545re/compare.ps1 -RustProfile size -RustFeatures small-crc
#>
[CmdletBinding()]
param(
    [switch]$BuildOnly,
    [switch]$NoBuild,
    [switch]$Nightly,
    [string]$Chip = 'STM32U545RE',
    [int]$CoreHz = 0,
    [int]$TimeoutSec = 180,
    [string]$RustProfile = 'release',
    [string[]]$RustFeatures = @(),
    [int]$InlineThreshold = 50,
    [string]$Report = ''
)

$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\board_common.ps1"

if ($Nightly) {
    # Smallest measured layout: nightly -Z build-std + panic=immediate-abort.
    $RustProfile = 'size'
    if ($RustFeatures -notcontains 'small-crc') { $RustFeatures += 'small-crc' }
}
$rustConfig = if ($Nightly) { 'size, nightly build-std' } else {
    $RustProfile + $(if ($RustFeatures) { ' + ' + ($RustFeatures -join ',') } else { '' })
}
$rustConfig += ", inline-threshold=$InlineThreshold"

if (-not $Report) { $Report = Join-Path $script:RustOut 'board_compare.txt' }

# ---------------------------------------------------------------------------
# Build both images
# ---------------------------------------------------------------------------
$cElf = Join-Path $script:COut 'fw_c_u545.elf'
if (-not $NoBuild) {
    Write-Host '== building both firmware images for thumbv8m.main-none-eabi =='
    $cElf = Build-CFirmware
}
if (-not (Test-Path $cElf)) { throw "missing C ELF: $cElf (run without -NoBuild)" }

$rElf = Join-Path $script:RustOut (Get-RustElfName -Profile $RustProfile -Features $RustFeatures -Nightly:$Nightly -InlineThreshold $InlineThreshold)
if (-not $NoBuild) {
    $rElf = Build-RustFirmware -Profile $RustProfile -Features $RustFeatures -Nightly:$Nightly -InlineThreshold $InlineThreshold
}
if (-not (Test-Path $rElf)) { throw "missing Rust ELF: $rElf (run without -NoBuild)" }

$size = Find-SizeTool
if ($size) {
    Write-Host ''
    Write-Host '-- C image sections --'
    & $size -A $cElf | Write-Host
}

if ($BuildOnly) {
    Write-Host ''
    Write-Host 'build only: skipping flash/run'
    exit 0
}

# ---------------------------------------------------------------------------
# Run both images
# ---------------------------------------------------------------------------
Write-Host ''
Write-Host '== running C FlashDB reference on NUCLEO-U545RE-Q =='
$c = Invoke-BoardRun -Elf $cElf -Chip $Chip -TimeoutSec $TimeoutSec -OutDir $script:COut
Format-BoardRun $c -Title 'C FlashDB firmware' -CoreHz $CoreHz

Write-Host ''
Write-Host "== running Rust flashdb ($rustConfig) on NUCLEO-U545RE-Q =="
$r = Invoke-BoardRun -Elf $rElf -Chip $Chip -TimeoutSec $TimeoutSec -OutDir $script:RustOut
Format-BoardRun $r -Title 'Rust firmware' -CoreHz $CoreHz

# ---------------------------------------------------------------------------
# Comparison table + acceptance verdict
# ---------------------------------------------------------------------------
$lines = New-Object System.Collections.Generic.List[string]
function Add-Line([string]$Text = '') { $lines.Add($Text); Write-Host $Text }

Add-Line ''
Add-Line '==============================================================='
Add-Line (" C FlashDB vs Rust flashdb -- NUCLEO-U545RE-Q ({0})" -f $Chip)
Add-Line '==============================================================='
Add-Line (" C image    : {0}" -f $cElf)
Add-Line (" Rust image : {0} ({1})" -f $rElf, $rustConfig)
Add-Line (" logical    : prints sent to the same NOR simulator, same workload")
Add-Line ''

$rows = @(
    @{ Name = 'ROM (text+rodata+data) B'; C = (Get-RomBytes $c.Fields); R = (Get-RomBytes $r.Fields); Lower = $true }
    @{ Name = 'static RAM (data+bss) B'; C = (Get-StaticRam $c.Fields); R = (Get-StaticRam $r.Fields); Lower = $true }
    @{ Name = 'stack high-water B'; C = (Get-Stack $c.Fields); R = (Get-Stack $r.Fields); Lower = $true }
    @{ Name = 'static+stack RAM B'; C = ((Get-StaticRam $c.Fields) + (Get-Stack $c.Fields)); R = ((Get-StaticRam $r.Fields) + (Get-Stack $r.Fields)); Lower = $true }
    @{ Name = 'cycles mount'; C = (Get-Cycles $c.Fields 'cycles_mount'); R = (Get-Cycles $r.Fields 'cycles_mount'); Lower = $true }
    @{ Name = 'cycles KV'; C = (Get-Cycles $c.Fields 'cycles_kv'); R = (Get-Cycles $r.Fields 'cycles_kv'); Lower = $true }
    @{ Name = 'cycles TS'; C = (Get-Cycles $c.Fields 'cycles_ts'); R = (Get-Cycles $r.Fields 'cycles_ts'); Lower = $true }
    @{ Name = 'cycles rollover'; C = (Get-Cycles $c.Fields 'cycles_roll'); R = (Get-Cycles $r.Fields 'cycles_roll'); Lower = $true }
    @{ Name = 'cycles total'; C = (Get-Cycles $c.Fields 'cycles_total'); R = (Get-Cycles $r.Fields 'cycles_total'); Lower = $true }
)

Add-Line ('{0,-26} {1,14} {2,14} {3,16}' -f 'metric', 'C FlashDB', 'Rust', 'Rust vs C')
Add-Line ('{0,-26} {1,14} {2,14} {3,16}' -f '--------------------------', '-----------', '-----------', '-------------')
foreach ($row in $rows) {
    $delta = $row.R - $row.C
    $pct = if ($row.C -ne 0) { 100.0 * $delta / $row.C } else { 0.0 }
    Add-Line ('{0,-26} {1,14} {2,14} {3,9:+0;-0;0} ({4,6:+0.0;-0.0;0.0}%)' -f $row.Name, $row.C, $row.R, $delta, $pct)
}

# Logical fields must be identical (workload + rollover-phase integrity).
$logicalKeys = @('kv_writes', 'kv_gets', 'kv_iterated', 'kv_checksum', 'kv_iter_checksum',
    'ts_appends', 'ts_iterated', 'ts_checksum',
    'ts_roll_appends', 'ts_roll_erases', 'ts_roll_violations', 'ts_roll_count',
    'ts_roll_first_time', 'ts_roll_last_time', 'ts_roll_checksum',
    'ts_roll_remount_count', 'ts_roll_remount_checksum', 'ts_roll_continue_ok',
    'kv_image_checksum', 'ts_image_checksum',
    'rc')
$mismatch = @($logicalKeys | Where-Object { [string]$c.Fields[$_] -ne [string]$r.Fields[$_] })
$clockOk = ([string]$c.Fields['core_hz'] -eq [string]$r.Fields['core_hz'])

Add-Line ''
Add-Line '-- acceptance --'
$failures = 0
function Verdict([string]$Label, [bool]$Ok, [string]$Detail = '') {
    $state = if ($Ok) { 'PASS' } else { 'FAIL' }
    Add-Line ('  [{0}] {1}{2}' -f $state, $Label, $(if ($Detail) { ": $Detail" } else { '' }))
    if (-not $Ok) { $script:failures++ }
}

$script:failures = $failures
Verdict 'workload results identical to C' ($mismatch.Count -eq 0) $(if ($mismatch.Count) { 'differs: ' + ($mismatch -join ', ') } else { "$($logicalKeys.Count) fields, checksums included" })
foreach ($row in $rows) {
    $ok = $row.R -le $row.C
    $detail = '{0} vs {1}' -f $row.R, $row.C
    Verdict ('{0} at least as good as C' -f $row.Name) $ok $detail
}
Verdict 'both images report the same clock' $clockOk ("C={0} Rust={1}" -f $c.Fields['core_hz'], $r.Fields['core_hz'])

Add-Line ''
if ($script:failures -eq 0) {
    Add-Line 'VERDICT: PASS -- on identical silicon the Rust image is at least as good'
    Add-Line '         as the C reference in ROM, RAM, stack and CPU cycles.'
} else {
    Add-Line ("VERDICT: FAIL -- {0} acceptance check(s) not met (see above)." -f $script:failures)
}

Set-Content -Path $Report -Value ($lines -join [Environment]::NewLine) -Encoding utf8
Write-Host ''
Write-Host ("report: {0}" -f $Report)

if ($script:failures -ne 0) { exit 1 }
exit 0
