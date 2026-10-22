<#
.SYNOPSIS
    Rust flashdb firmware -- build + QEMU measurement (Windows port of fw_rust.sh).

.DESCRIPTION
    Builds the bare-metal Rust measurement firmware in harness/firmware/rust/
    for the MPS2-AN385 board (Cortex-M3) with the real `flashdb` crate and runs
    it under qemu-system-arm, printing the ELF section sizes and the RESULT
    line (ROM/RAM/stack high-water + workload checksums).

    This is the fast inner-loop used while optimising the crate: it needs no
    debug probe and takes seconds.  The real-hardware acceptance is
    harness/firmware/u545re/compare.ps1 (STM32U545RE / NUCLEO-U545RE-Q).

.PARAMETER Profile      release (opt-level="s") or size (opt-level="z").
.PARAMETER Phases       Build with the stack-phases feature (per-phase peaks).
.PARAMETER SmallCrc     Enable the crate's small-crc feature (64 B CRC table).
.PARAMETER Features     Extra firmware/crate features.
.PARAMETER BuildOnly    Build + section sizes only, no QEMU run.
.PARAMETER NoRun        Alias for BuildOnly.
.PARAMETER Symbols      Print symbol-level size attribution (needs python3).
.PARAMETER Clean        Remove the cargo target directory first.
.PARAMETER TimeoutSec   QEMU wall-clock timeout (default 120).
.PARAMETER InlineThreshold  LLVM -inline-threshold passed via
                      -C llvm-args=... (default 50: measured best ROM/cycle
                      trade-off for the -z profile on silicon; 225 is LLVM's
                      built-in default.  A codegen knob, never baked in).
                      A negative value means "pass no -inline-threshold at
                      all" (LLVM's own -Oz default) for A/B reference runs.
.PARAMETER Cycles       QEMU cycle/instruction proxy (`qemu-cycles` feature):
                      times the phase boundaries with ARM semihosting
                      SYS_ELAPSED and runs the board rollover workload, under
                      `-icount shift=0` (1 tick == 1 instruction).  Prints a
                      separate QEMU_CYCLES line, so the documented RESULT
                      baseline is unaffected.  Iterates TS/rollover CPU cost
                      without silicon.
.PARAMETER Icf          Link with rust-lld and fold identical code
                      (--icf=all).  GNU ld (the default gcc-driven link) has no
                      ICF, so this switches the linker for the build; the
                      artifact gets an _icf suffix.
.PARAMETER OutDir       Artifact directory (default harness/firmware/build/rust).

.EXAMPLE
    scripts/fw_rust.ps1 -Profile size -SmallCrc -Phases

.EXAMPLE
    scripts/fw_rust.ps1 -Profile size -SmallCrc -Phases -InlineThreshold 100

.EXAMPLE
    scripts/fw_rust.ps1 -Profile size -SmallCrc -Cycles

.EXAMPLE
    scripts/fw_rust.ps1 -Profile size -SmallCrc -Icf
#>
[CmdletBinding()]
param(
    [ValidateSet('release', 'size')][string]$Profile = 'release',
    [switch]$Phases,
    [switch]$SmallCrc,
    [switch]$Cycles,
    [string[]]$Features = @(),
    [switch]$BuildOnly,
    [switch]$Symbols,
    [switch]$Clean,
    [int]$TimeoutSec = 120,
    [int]$InlineThreshold = 50,
    [switch]$Icf,
    [string]$OutDir = ''
)

$ErrorActionPreference = 'Stop'
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$FwDir = Join-Path $RepoRoot 'harness\firmware'
$RustDir = Join-Path $FwDir 'rust'
$Ld = Join-Path $FwDir 'common\mps2_an385.ld'
$Target = 'thumbv7m-none-eabi'
if (-not $OutDir) { $OutDir = Join-Path $FwDir 'build\rust' }
# Resolve relative to the repo root so paths keep working after Push-Location.
if (-not [System.IO.Path]::IsPathRooted($OutDir)) { $OutDir = Join-Path $RepoRoot $OutDir }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

function Find-ArmGcc {
    $cmd = Get-Command arm-none-eabi-gcc -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    $roots = @('D:\ST', 'C:\ST', 'C:\Program Files\STMicroelectronics', 'C:\Program Files (x86)\STMicroelectronics')
    foreach ($root in $roots) {
        if (-not (Test-Path $root)) { continue }
        $hit = Get-ChildItem $root -Recurse -Filter 'arm-none-eabi-gcc.exe' -ErrorAction SilentlyContinue |
            Sort-Object FullName -Descending | Select-Object -First 1
        if ($hit) { return $hit.FullName }
    }
    throw 'arm-none-eabi-gcc not found (install GNU Tools for STM32 or put it on PATH)'
}

function Find-SizeTool {
    foreach ($n in @('arm-none-eabi-size', 'llvm-size')) {
        $c = Get-Command $n -ErrorAction SilentlyContinue
        if ($c) { return $c.Source }
    }
    foreach ($p in @('D:\Program Files\LLVM\bin\llvm-size.exe', 'C:\Program Files\LLVM\bin\llvm-size.exe')) {
        if (Test-Path $p) { return $p }
    }
    throw 'no size tool found (need arm-none-eabi-size or llvm-size)'
}

# Features, mirroring fw_rust.sh: --phases adds the diagnostic instrumentation
# and changes the artifact suffix so it never overwrites the baseline ELF.
# A `-Features a,b` list arrives as one comma-joined string from `pwsh -File`,
# so normalise before building the suffix.
$allFeatures = @()
foreach ($f in $Features) {
    foreach ($part in ($f -split ',')) {
        $t = $part.Trim()
        if ($t -and $allFeatures -notcontains $t) { $allFeatures += $t }
    }
}
$suffix = ''
foreach ($f in $allFeatures) { $suffix += "_$($f -replace '-', '')" }
if ($Phases) { $allFeatures += 'stack-phases'; $suffix += '_phases' }
if ($SmallCrc) { $allFeatures += 'small-crc'; $suffix += '_smallcrc' }
if ($Cycles) { $allFeatures += 'qemu-cycles'; $suffix += '_cycles' }
# Non-default codegen knobs get their own suffix so experiments never overwrite
# a canonical artifact (same convention as scripts/fw_rust.sh).
# A negative threshold means "pass no -inline-threshold at all" (LLVM's own
# -Oz default), which is the reference configuration for A/B measurements.
if ($InlineThreshold -lt 0) { $suffix += '_thdefault' }
elseif ($InlineThreshold -ne 50) { $suffix += "_th$InlineThreshold" }
if ($Icf) { $suffix += '_icf' }

$gcc = Find-ArmGcc
$targetDir = Join-Path $OutDir 'target'
if ($Clean) { Remove-Item $targetDir -Recurse -Force -ErrorAction SilentlyContinue }
$env:CARGO_TARGET_DIR = $targetDir

$featureArg = if ($allFeatures.Count) { '--features', ($allFeatures -join ',') } else { @() }
$profileArg = if ($Profile -eq 'release') { @('--release') } else { @('--profile', 'size') }
$inlineArg = if ($InlineThreshold -ge 0) { @('-C', "llvm-args=-inline-threshold=$InlineThreshold") } else { @() }
if ($Icf) {
    # GNU ld has no --icf; switch to rust-lld (which does) and resolve libgcc
    # from the same GCC toolchain that supplies the C reference image.
    $hostTriple = (& rustc -vV | Select-String '^host: ').ToString().Split(' ')[1].Trim()
    $sysroot = (& rustc --print sysroot).Trim()
    $lld = Join-Path $sysroot "lib\rustlib\$hostTriple\bin\rust-lld.exe"
    if (-not (Test-Path $lld)) { throw "rust-lld not found: $lld" }
    $libgccDir = Split-Path (& $gcc '-print-libgcc-file-name').Trim() -Parent
    if (-not (Test-Path $libgccDir)) { throw "libgcc dir not found: $libgccDir" }
    $linkerExe = $lld
    $env:CARGO_TARGET_THUMBV7M_NONE_EABI_LINKER = $lld
    $linkArgs = @(
        '-C', "linker=$lld",
        '-C', "link-arg=-T$Ld",
        '-C', 'link-arg=--gc-sections',
        '-C', 'link-arg=--icf=all',
        '-C', "link-arg=-L$libgccDir",
        '-C', 'link-arg=-lgcc'
    ) + $inlineArg
} else {
    $linkerExe = $gcc
    $env:CARGO_TARGET_THUMBV7M_NONE_EABI_LINKER = $gcc
    $linkArgs = @(
        '-C', "linker=$gcc",
        '-C', "link-arg=-T$Ld",
        '-C', 'link-arg=-nostartfiles',
        '-C', 'link-arg=-nostdlib',
        '-C', 'link-arg=-Wl,--gc-sections',
        '-C', 'link-arg=-lgcc'
    ) + $inlineArg
}

$buildLog = Join-Path $OutDir "cargo$($suffix).log"
Write-Host "== Rust flashdb firmware ($Profile profile$($suffix -replace '_', ' ')) =="
Write-Host "   rustc : $(& rustc --version)"
Write-Host "   linker: $linkerExe$(if ($Icf) { ' (--icf=all)' } else { ' (GNU ld)' })"
Write-Host "   codegen: $(if ($InlineThreshold -ge 0) { "-C llvm-args=-inline-threshold=$InlineThreshold" } else { 'LLVM -Oz default inliner (no flag)' })"
Write-Host "   target: $Target / Cortex-M3 / MPS2-AN385"
Push-Location $RustDir
try {
    $cargoArgs = @('rustc', '--bin', 'fw-rust', '--target', $Target, '--offline') + $profileArg +
        @($featureArg) + @('--') + $linkArgs
    & cargo @cargoArgs *> $buildLog
    $buildRc = $LASTEXITCODE
} finally {
    Pop-Location
}
if ($buildRc -ne 0) {
    Get-Content $buildLog | Select-Object -Last 40 | Write-Host
    throw "cargo build failed (rc=$buildRc); log: $buildLog"
}

$profileDir = if ($Profile -eq 'release') { 'release' } else { 'size' }
$builtElf = Join-Path $targetDir "$Target\$profileDir\fw-rust"
if (-not (Test-Path $builtElf)) { throw "expected ELF not found: $builtElf" }
$elf = Join-Path $OutDir ("fw_rust_{0}{1}.elf" -f $Profile, $suffix)
Copy-Item $builtElf $elf -Force

Write-Host ''
Write-Host '-- ELF sections (size -A) --'
$size = Find-SizeTool
& $size -A $elf | Write-Host

if ($Symbols) {
    $py = Get-Command python3, python -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($py) {
        & $py.Source (Join-Path $FwDir 'tools\symbol_report.py') $elf --kind rust | Write-Host
    } else {
        Write-Host '   (python not found: skipping symbol report)'
    }
}

if ($BuildOnly) {
    Write-Host ''
    Write-Host "build only: $elf"
    return
}

# ---------------------------------------------------------------------------
# Run under QEMU, capture semihosting output
# ---------------------------------------------------------------------------
$qemuLog = Join-Path $OutDir "qemu$suffix.log"
$qemu = (Get-Command qemu-system-arm -ErrorAction SilentlyContinue)
if (-not $qemu) { throw 'qemu-system-arm not found in PATH' }
Write-Host ''
Write-Host "-- QEMU run (mps2-an385, timeout ${TimeoutSec}s) --"
$psi = [System.Diagnostics.ProcessStartInfo]::new()
$psi.FileName = $qemu.Source
# Deterministic virtual clock for the cycle proxy: with `-icount shift=0` every
# instruction consumes exactly one tick, so semihosting SYS_ELAPSED becomes an
# instruction counter and repeated runs are bit-identical.
$qemuExtra = if ($Cycles) { ' -icount shift=0,align=off' } else { '' }
$psi.Arguments = "-M mps2-an385 -cpu cortex-m3 -nographic -semihosting$qemuExtra -kernel `"$elf`""
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$proc = [System.Diagnostics.Process]::new()
$proc.StartInfo = $psi
[void]$proc.Start()
$out = $proc.StandardOutput.ReadToEnd()
$err = $proc.StandardError.ReadToEnd()
if (-not $proc.WaitForExit($TimeoutSec * 1000)) {
    try { $proc.Kill($true) } catch {}
    throw "QEMU did not finish within ${TimeoutSec}s (hang? see $qemuLog)"
}
$text = $out + $err
Set-Content -Path $qemuLog -Value $text -NoNewline
Write-Host $text
$result = ($text -split "`r?`n" | Where-Object { $_ -match '^RESULT ' } | Select-Object -First 1)
if (-not $result) { throw "firmware did not print a RESULT line (log: $qemuLog)" }
if ($result -notmatch ' rc=0') { throw "workload reported a failure: $result" }
if ($Cycles) {
    $probeLine = ($text -split "`r?`n" | Where-Object { $_ -match '^QEMU_PROBE ' } | Select-Object -First 1)
    # NOTE: `$cycles` would overwrite the $Cycles switch (PowerShell variables
    # are case-insensitive), so keep a distinct name.
    $cyclesLine = ($text -split "`r?`n" | Where-Object { $_ -match '^QEMU_CYCLES ' } | Select-Object -First 1)
    if ($probeLine) { Write-Host ''; Write-Host $probeLine }
    if ($cyclesLine) { Write-Host $cyclesLine } else { throw "no QEMU_CYCLES line in the output (feature qemu-cycles enabled?)" }
}
return $result
