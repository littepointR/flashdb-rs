<#
.SYNOPSIS
    On-silicon deterministic mid-erase reset injection: real-flash KVDB + TSDB
    across 20 active page-erase resets (NUCLEO-U545RE-Q).

.DESCRIPTION
    Flashes the `flash-erase-fault` image once and executes 20 deterministic
    mid-erase reset rounds on the internal STM32U545RE flash scratch area
    (`0x0803_0000..0x0803_FFFF`, pages 24..31).

    Why SRAM execution is required on STM32U5:
      In a single-bank flash layout, initiating a page erase on Bank 1 while
      executing `wait_idle` from Bank 1 Flash stalls CPU instruction fetch
      until the ~1.5 ms page erase finishes.  To trigger a system reset *while
      the flash controller is actively erasing a page*, `RealFlash::arm_erase_reset`
      diverts the target page erase to `ram_erase_and_reset` in `.data` (SRAM1
      `0x2000_xxxx`) with interrupts masked.  From SRAM, the CPU starts the
      erase, polls `FLASH_NSSR.BSY == 1` (`bit 16`) live without AHB instruction
      fetch stalls, spins a swept number of SRAM delay loops inside the active
      erase window, records `[abs_addr, nssr_at_reset, sram_loops]` into the
      `.results` NOLOAD SRAM section, and asserts `SCB->AIRCR = SYSRESETREQ`
      while `BSY == 1`.

    Across 20 rounds:
      * Even rounds (10 rounds) inject the mid-erase reset during a **KVDB
        garbage-collection sector erase**.
      * Odd rounds (10 rounds) inject the mid-erase reset during a **TSDB
        rollover sector erase**.
      * On the boot following each reset, the firmware installs a RAM vector
        table (`SCB->VTOR`) whose NMI handler acknowledges STM32U5 Flash ECC
        double-error NMIs (`FLASH_ECCR.ECCD`) raised when reading partially
        erased 128-bit quad-words, verifies that `nssr_at_reset` had `BSY == 1`
        for the full delay window at the instant of reset, inspects the
        interrupted scratch page, remounts both `Kvdb` and `Tsdb`, runs
        `Kvdb::check()`, validates all 8 KV keys (payload + CRC32) and the TS
        live window, performs post-recovery writes to both databases, and
        advances to the next round.

.PARAMETER BuildOnly        Build the ELF and print its sections; no hardware.
.PARAMETER NoBuild          Reuse the existing ELF and go straight to hardware.
.PARAMETER Chip             probe-rs chip name (default STM32U545RE).
.PARAMETER TimeoutSec       Seconds to wait for DONE (default 240).
.PARAMETER ExpectedRounds   Rounds the firmware must report (default 20; must
                            match flash_erase_fault.rs ROUNDS).
.PARAMETER OutDir           Artifact directory (default harness/firmware/build/u545re).

.EXAMPLE
    harness/firmware/u545re/run_flash_erase_fault.ps1 -BuildOnly
    harness/firmware/u545re/run_flash_erase_fault.ps1
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

$elf = Join-Path $OutDir (Get-RustElfName -Profile size -Features @('flash-erase-fault'))

if ($NoBuild) {
    if (-not (Test-Path $elf)) { throw "missing ELF: $elf (run without -NoBuild)" }
} else {
    Write-Host '== building Rust flash-erase-fault firmware (size profile, board-u545) =='
    $elf = Build-RustFirmware -Profile size -Features @('flash-erase-fault') -OutDir $OutDir
}

$size = Find-SizeTool
if ($BuildOnly) {
    if ($size) { Write-Host ''; & $size -A $elf | Write-Host }
    Write-Host 'build only: skipping hardware'
    exit 0
}

Write-Host ''
Write-Host "== flashing erase-fault image once and waiting for $ExpectedRounds mid-erase reset rounds =="
Write-Host "   elf: $elf  timeout: $TimeoutSec s"
$run = Invoke-BoardRun -Elf $elf -Chip $Chip -TimeoutSec $TimeoutSec -OutDir (Join-Path $OutDir 'flash-erase-fault')
Format-BoardRun $run -Title 'Rust flash-erase-fault firmware'

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
$bsyConfirmed = [int](Get-ResultField 'bsy_confirmed')
$kvEraseResets = [int](Get-ResultField 'kv_erase_resets')
$tsEraseResets = [int](Get-ResultField 'ts_erase_resets')
$pageErasedAfter = [int](Get-ResultField 'page_erased_after')
$pagePartialAfter = [int](Get-ResultField 'page_partial_after')
$eccNmis = [int](Get-ResultField 'ecc_nmis')
$lastAddr = [string](Get-ResultField 'last_addr')
$lastSr = [string](Get-ResultField 'last_sr')
$lastLoops = [int](Get-ResultField 'last_loops')
$kvKeys = [int](Get-ResultField 'kv_keys')
$tsEntries = [int](Get-ResultField 'ts_entries')
$tsFirst = [int](Get-ResultField 'ts_first')
$tsLast = [int](Get-ResultField 'ts_last')
$verifyOk = [int](Get-ResultField 'verify_ok')

$expectKvEraseResets = [int]($ExpectedRounds / 2)
$expectTsEraseResets = $ExpectedRounds - $expectKvEraseResets
$expectKvKeys = 8
$errors = New-Object System.Collections.Generic.List[string]

if ($rc -ne '0') { $errors.Add("rc=$rc (expected 0)") }
if ($verifyOk -ne 1) { $errors.Add("verify_ok=$verifyOk (expected 1)") }
if ($rounds -ne $ExpectedRounds) { $errors.Add("rounds=$rounds (expected $ExpectedRounds)") }
if ($roundsDone -ne $ExpectedRounds) { $errors.Add("rounds_done=$roundsDone (expected $ExpectedRounds)") }
if ($resets -ne $ExpectedRounds) { $errors.Add("resets=$resets (expected $ExpectedRounds)") }
if ($bsyConfirmed -ne $ExpectedRounds) { $errors.Add("bsy_confirmed=$bsyConfirmed (expected $ExpectedRounds)") }
if ($kvEraseResets -ne $expectKvEraseResets) { $errors.Add("kv_erase_resets=$kvEraseResets (expected $expectKvEraseResets)") }
if ($tsEraseResets -ne $expectTsEraseResets) { $errors.Add("ts_erase_resets=$tsEraseResets (expected $expectTsEraseResets)") }
if (($pageErasedAfter + $pagePartialAfter) -ne $ExpectedRounds) {
    $errors.Add("page_erased_after ($pageErasedAfter) + page_partial_after ($pagePartialAfter) != $ExpectedRounds")
}
if ($pagePartialAfter -lt 1) { $errors.Add("page_partial_after=$pagePartialAfter (expected >= 1 partially erased page)") }
if ($boots -lt ($ExpectedRounds + 1)) { $errors.Add("boots=$boots (expected >= $($ExpectedRounds + 1))") }
if ($kvKeys -ne $expectKvKeys) { $errors.Add("kv_keys=$kvKeys (expected $expectKvKeys)") }
if ($tsEntries -lt 1) { $errors.Add("ts_entries=$tsEntries (expected >= 1)") }
if ($tsFirst -ne ($tsLast - $tsEntries + 1)) { $errors.Add("ts window not contiguous: first=$tsFirst last=$tsLast entries=$tsEntries") }
if ($errors.Count -ne 0) {
    foreach ($e in $errors) { Write-Host "FAIL: $e" }
    throw "flash-erase-fault failed ($($errors.Count) check(s)); see $($run.TextPath)"
}

Write-Host ''
Write-Host ("   rounds={0} boots={1} resets={2} bsy_confirmed={3} (kv_erase={4} ts_erase={5})" -f `
    $rounds, $boots, $resets, $bsyConfirmed, $kvEraseResets, $tsEraseResets)
Write-Host ("   page_after_reset: erased={0} partial={1} ecc_nmis={2}  last_erase: addr={3} sr={4} loops={5}" -f `
    $pageErasedAfter, $pagePartialAfter, $eccNmis, $lastAddr, $lastSr, $lastLoops)
Write-Host ("   kv_keys={0} ts_window=[{1}..{2}] ({3} entries)" -f `
    $kvKeys, $tsFirst, $tsLast, $tsEntries)
Write-Host ("PASS: {0} mid-erase resets (FLASH_NSSR.BSY==1) injected and recovered on the NUCLEO-U545RE-Q" -f $rounds)
exit 0
