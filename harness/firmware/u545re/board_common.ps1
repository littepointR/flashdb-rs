<#
.SYNOPSIS
    Shared helpers for the NUCLEO-U545RE-Q board runners.

.DESCRIPTION
    Dot-source from run_rust.ps1 / run_c.ps1 / compare.ps1:

        . "$PSScriptRoot\board_common.ps1"

    Provides:
      * toolchain discovery (Rust/LLVM, arm-none-eabi-*, probe-rs, CubeIDE GCC);
      * Build-RustFirmware / Build-CFirmware for the `board-u545` image;
      * Invoke-BoardRun: flash over probe-rs, wait for the DONE flag, read the
        BOARD_OUT RAM buffer back over SWD and parse RESULT/BOARD fields;
      * Format-BoardRun and Get-RomBytes/Get-StaticRam/Get-Stack accessors.

    Both firmware images use the same harness contract (see
    harness/firmware/common/board_u545.h and
    harness/firmware/rust/main.rs `mod board`), so the numbers below are
    directly comparable on identical silicon.
#>

$script:RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..\..')).Path
$script:FwDir = Join-Path $RepoRoot 'harness\firmware'
$script:RefDir = Join-Path $RepoRoot 'reference\FlashDB'
$script:BoardLd = Join-Path $FwDir 'u545re\board.ld'
$script:RustSrc = Join-Path $FwDir 'rust'
$script:RustOut = Join-Path $FwDir 'build\u545re'
$script:COut = Join-Path $FwDir 'build\c_u545'
$script:Target = 'thumbv8m.main-none-eabi'

# BoardOut = { done: u32, len: u32, buf: [u8; 4096] }
$script:BoardOutFallback = 0x20009000
$script:BoardOutSize = 8 + 4096
$script:DoneMagic = '454e4f44' # "DONE" little-endian

function Need([string]$Name) {
    if (-not (Get-Command $Name -ErrorAction SilentlyContinue)) {
        throw "$Name not found in PATH"
    }
}

function Find-HostTriple { (& rustc -vV | Select-String '^host: ').ToString().Split(' ')[1].Trim() }

function Find-RustLld {
    $hostTriple = Find-HostTriple
    $sysroot = (& rustc --print sysroot).Trim()
    foreach ($name in @('rust-lld.exe', 'rust-lld')) {
        $p = Join-Path $sysroot "lib\rustlib\$hostTriple\bin\$name"
        if (Test-Path $p) { return $p }
    }
    $cmd = Get-Command rust-lld -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    throw 'rust-lld not found in the Rust sysroot'
}

function Find-ArmGcc {
    $cmd = Get-Command arm-none-eabi-gcc -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    # STM32CubeIDE bundles the GNU Tools for STM32; find the newest one.
    $roots = @('D:\ST', 'C:\ST', 'C:\Program Files\STMicroelectronics', 'C:\Program Files (x86)\STMicroelectronics')
    foreach ($root in $roots) {
        if (-not (Test-Path $root)) { continue }
        $hit = Get-ChildItem $root -Recurse -Filter 'arm-none-eabi-gcc.exe' -ErrorAction SilentlyContinue |
            Sort-Object FullName -Descending | Select-Object -First 1
        if ($hit) { return $hit.FullName }
    }
    throw 'arm-none-eabi-gcc not found (install GNU Tools for STM32 or put it on PATH)'
}

function Find-Tool([string[]]$Names, [string[]]$FallbackPaths = @()) {
    foreach ($n in $Names) {
        $c = Get-Command $n -ErrorAction SilentlyContinue
        if ($c) { return $c.Source }
    }
    foreach ($p in $FallbackPaths) {
        if (Test-Path $p) { return $p }
    }
    return $null
}

function Find-SymbolTool {
    $t = Find-Tool @('arm-none-eabi-nm', 'llvm-nm') @('D:\Program Files\LLVM\bin\llvm-nm.exe', 'C:\Program Files\LLVM\bin\llvm-nm.exe')
    if (-not $t) { throw 'no symbol tool found (need arm-none-eabi-nm or llvm-nm)' }
    return $t
}

function Find-SizeTool {
    return Find-Tool @('arm-none-eabi-size', 'llvm-size') @('D:\Program Files\LLVM\bin\llvm-size.exe', 'C:\Program Files\LLVM\bin\llvm-size.exe')
}

function Get-SymbolAddress([string]$Tool, [string]$ElfPath, [string]$Symbol) {
    $line = & $Tool $ElfPath 2>$null | Select-String -Pattern "\s$([regex]::Escape($Symbol))$" | Select-Object -First 1
    if (-not $line) { throw "symbol $Symbol not found in $ElfPath" }
    return [Convert]::ToUInt32(($line.ToString().Trim() -split '\s+')[0], 16)
}

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------
function Get-RustElfName {
    param(
        [string]$Profile = 'release',
        [string[]]$Features = @(),
        [switch]$Nightly,
        [int]$InlineThreshold = 50
    )
    # Non-default codegen knobs get a suffix so an experiment never overwrites a
    # canonical artifact; the default (50) keeps the documented names.
    # A negative threshold means "pass no -inline-threshold at all" (LLVM's own
    # -Oz default inliner), the reference configuration for A/B measurements.
    $thSuffix = if ($InlineThreshold -lt 0) { '_thdefault' }
                elseif ($InlineThreshold -ne 50) { "_th$InlineThreshold" }
                else { '' }
    if ($Nightly) { return "fw_rust_u545re_size_nightly$thSuffix.elf" }
    $suffix = if ($Profile -eq 'release') { '' } else { "_$Profile" }
    # Distinct artifact per measured configuration:  board-u545 is implicit
    # (always added by Build-RustFirmware) and never part of the name; the known
    # optional features get their established short names; anything else is
    # sanitised.  Canonical names:
    #   fw_rust_u545re_size_smallcrc.elf          (board baseline)
    #   fw_rust_u545re_size_smallcrc_phases.elf   (stack attribution)
    # `-Features small-crc,stack-phases` from `pwsh -File` arrives as one
    # comma-joined string, so split before mapping.
    $all = @()
    foreach ($f in $Features) {
        foreach ($part in ($f -split ',')) {
            $t = $part.Trim()
            if ($t -and $t -ne 'board-u545' -and $all -notcontains $t) { $all += $t }
        }
    }
    $priority = @('small-crc', 'stack-phases')
    $ordered = @()
    foreach ($p in $priority) { if ($all -contains $p) { $ordered += $p } }
    foreach ($o in @($all | Where-Object { $priority -notcontains $_ } | Sort-Object)) { $ordered += $o }
    foreach ($f in $ordered) {
        $suffix += "_" + $(switch ($f) {
            'small-crc'    { 'smallcrc' }
            'stack-phases' { 'phases' }
            default        { ($f -replace '[^A-Za-z0-9]', '') }
        })
    }
    return "fw_rust_u545re$suffix$thSuffix.elf"
}

function Build-RustFirmware {
    [CmdletBinding()]
    param(
        [string]$Profile = 'release',          # release (opt-level="s") or size (opt-level="z")
        [string[]]$Features = @(),             # extra crate features, e.g. 'small-crc'
        [switch]$Nightly,                      # nightly -Z build-std size build (see docs/OPTIMIZATION.md)
        # LLVM -inline-threshold.  The -z profile's default inliner is too
        # conservative for this workload: raising it to 50 measured -84 B ROM,
        # -24 B stack and TS 531033 -> 414319 / rollover 3184271 -> 2502609
        # board cycles (docs/OPTIMIZATION.md, 2026-10-23).  Override per run,
        # never bake into the crate/profile: it is a codegen flag.
        [int]$InlineThreshold = 50,
        # Nightly-only ROM experiments (measurement knobs, not defaults):
        #   -OptimizeForSize -> `-Z build-std-features=optimize_for_size`
        #   -LtoO3           -> `-C link-arg=--lto-O3`
        [switch]$OptimizeForSize,
        [switch]$LtoO3,
        # Measurement-only: link without --icf=all to quantify the ICF win.
        [switch]$NoIcf,
        [string]$OutDir = $script:RustOut
    )
    Need 'cargo'; Need 'rustc'
    $installed = (& rustup target list --installed) -join ' '
    if ($installed -notmatch [regex]::Escape($script:Target)) {
        throw "Rust target $($script:Target) is not installed. Install it with: rustup target add $($script:Target)"
    }
    $lld = Find-RustLld
    $featureArg = @('board-u545') + $Features
    New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
    # LLVM -inline-threshold: a codegen knob.  A negative value omits the flag
    # entirely (LLVM's own -Oz default inliner), for A/B reference runs.
    $inlineArgs = if ($InlineThreshold -ge 0) { @('-C', "llvm-args=-inline-threshold=$InlineThreshold") } else { @() }
    $variantSuffix = ''
    if ($OptimizeForSize) { $variantSuffix += '_ofs' }
    if ($LtoO3) { $variantSuffix += '_ltool3' }
    if ($NoIcf) { $variantSuffix += '_noicf' }
    $icfArgs = if ($NoIcf) { @() } else { @('-C', 'link-arg=--icf=all') }

    if ($Nightly) {
        # Smallest reproducibly measured layout: nightly toolchain, core rebuilt
        # at -Oz (`-Z build-std`), panic=immediate-abort, LLD ICF and no debug
        # location detail.  Needs a temp copy of the firmware crate because
        # `cargo-features` and `panic="immediate-abort"` must be edited in.
        $toolchains = (& rustup toolchain list) -join "`n"
        if ($toolchains -notmatch '(?m)^nightly') {
            Write-Host '   nightly toolchain missing, installing (with rust-src)...'
            & rustup toolchain install nightly --profile minimal --component rust-src | Out-Host
            if ($LASTEXITCODE -ne 0) { throw 'rustup install nightly failed' }
        } else {
            $comps = (& rustup component list --toolchain nightly --installed) -join ' '
            if ($comps -notmatch 'rust-src') {
                & rustup component add rust-src --toolchain nightly | Out-Host
                if ($LASTEXITCODE -ne 0) { throw 'rustup component add rust-src --toolchain nightly failed' }
            }
        }
        $work = Join-Path $OutDir 'nightly-src'
        $workRust = Join-Path $work 'rust'
        Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
        New-Item -ItemType Directory -Force -Path $work | Out-Null
        Copy-Item $script:RustSrc $workRust -Recurse
        Copy-Item (Join-Path $script:FwDir 'common') (Join-Path $work 'common') -Recurse
        $toml = Join-Path $workRust 'Cargo.toml'
        $s = Get-Content $toml -Raw
        $s = 'cargo-features = ["panic-immediate-abort"]' + "`r`n" + $s
        $crate = (Join-Path $script:RepoRoot 'crates\flashdb') -replace '\\', '/'
        $s = $s.Replace('flashdb = { path = "../../../crates/flashdb" }', "flashdb = { path = `"$crate`" }")
        $s = $s.Replace('panic = "abort"', 'panic = "immediate-abort"')
        Set-Content -Path $toml -Value $s -NoNewline
        $buildLog = Join-Path $OutDir 'cargo-nightly.log'
        $env:CARGO_TARGET_DIR = Join-Path $work 'target'
        Push-Location $workRust
        try {
            $buildStdArgs = @()
            if ($OptimizeForSize) { $buildStdArgs += @('-Z', 'build-std-features=optimize_for_size') }
            $cargoArgs = @(
                '+nightly', 'rustc', '--bin', 'fw-rust', '--target', $script:Target,
                '--profile', 'size', '-Z', 'build-std=core'
            ) + $buildStdArgs + @(
                '--features', ($featureArg -join ','), '--',
                '-C', "linker=$lld",
                '-C', "link-arg=-T$($script:BoardLd)",
                '-C', 'link-arg=--gc-sections',
                '-Z', 'location-detail=none'
            ) + $icfArgs + $(if ($LtoO3) { @('-C', 'link-arg=--lto-O3') } else { @() }) + $inlineArgs
            & cargo @cargoArgs *> $buildLog
            $rc = $LASTEXITCODE
        } finally {
            Pop-Location
        }
        if ($rc -ne 0) {
            Get-Content $buildLog | Select-Object -Last 40 | Write-Host
            throw "nightly cargo build failed (rc=$rc); log: $buildLog"
        }
        $built = Join-Path $work "target\$($script:Target)\size\fw-rust"
        if (-not (Test-Path $built)) { throw "expected nightly ELF not found: $built" }
        $elf = Join-Path $OutDir ((Get-RustElfName -Profile $Profile -Features $Features -Nightly -InlineThreshold $InlineThreshold) -replace '\.elf$', "$variantSuffix.elf")
        Copy-Item $built $elf -Force
        Write-Host ("   rust: ok (nightly build-std{0} + ICF + inline-threshold={1}, features: {2}) -> {3}" -f $variantSuffix, $InlineThreshold, ($featureArg -join '+'), $elf)
        return $elf
    }

    $profileArg = if ($Profile -eq 'release') { '--release' } else { '--profile', $Profile }
    if ($profileArg -is [string]) { $profileArg = @($profileArg) }
    $buildLog = Join-Path $OutDir 'cargo.log'
    $env:CARGO_TARGET_DIR = Join-Path $OutDir 'target'
    Push-Location $script:RustSrc
    try {
        $cargoArgs = @('rustc', '--bin', 'fw-rust', '--target', $script:Target) + $profileArg + @(
            '--offline', '--features', ($featureArg -join ','), '--',
            '-C', "linker=$lld",
            '-C', "link-arg=-T$($script:BoardLd)",
            '-C', 'link-arg=--gc-sections'
        ) + $icfArgs + $(if ($LtoO3) { @('-C', 'link-arg=--lto-O3') } else { @() }) + $inlineArgs
        & cargo @cargoArgs *> $buildLog
        $rc = $LASTEXITCODE
    } finally {
        Pop-Location
    }
    if ($rc -ne 0) {
        Get-Content $buildLog | Select-Object -Last 40 | Write-Host
        throw "cargo build failed (rc=$rc); log: $buildLog"
    }
    $profileDir = if ($Profile -eq 'release') { 'release' } else { $Profile }
    $built = Join-Path $OutDir "target\$($script:Target)\$profileDir\fw-rust"
    if (-not (Test-Path $built)) { throw "expected ELF not found: $built" }
    $elf = Join-Path $OutDir ((Get-RustElfName -Profile $Profile -Features $Features -InlineThreshold $InlineThreshold) -replace '\.elf$', "$variantSuffix.elf")
    Copy-Item $built $elf -Force
    Write-Host ("   rust: ok ({0}, ICF, inline-threshold={1}, features: {2}) -> {3}" -f $Profile, $InlineThreshold, ($featureArg -join '+'), $elf)
    return $elf
}

function Build-CFirmware {
    [CmdletBinding()]
    param([string]$OutDir = $script:COut)
    $gcc = Find-ArmGcc
    if (-not (Test-Path (Join-Path $script:RefDir 'src\fdb.c'))) {
        throw "reference FlashDB not found at $($script:RefDir) (run scripts/fetch_reference.sh)"
    }
    $objDir = Join-Path $OutDir 'obj'
    New-Item -ItemType Directory -Force -Path $objDir | Out-Null

    $srcs = @(
        (Join-Path $script:RefDir 'src\fdb.c')
        (Join-Path $script:RefDir 'src\fdb_kvdb.c')
        (Join-Path $script:RefDir 'src\fdb_tsdb.c')
        (Join-Path $script:RefDir 'src\fdb_utils.c')
        (Join-Path $script:RefDir 'port\fal\src\fal.c')
        (Join-Path $script:RefDir 'port\fal\src\fal_flash.c')
        (Join-Path $script:RefDir 'port\fal\src\fal_partition.c')
        (Join-Path $script:FwDir 'common\startup_armv7m.S')
        (Join-Path $script:FwDir 'common\semihost.c')
        (Join-Path $script:FwDir 'common\stack.c')
        (Join-Path $script:FwDir 'common\board_u545.c')
        (Join-Path $script:FwDir 'c\main.c')
        (Join-Path $script:FwDir 'c\fal_ram_sim.c')
        (Join-Path $script:FwDir 'c\libc_stubs.c')
    )
    $cflags = @(
        '-mcpu=cortex-m33', '-mthumb', '-mfloat-abi=soft', '-Os', '-std=gnu11', '-Wall',
        '-ffunction-sections', '-fdata-sections', '-fno-common', '-fno-builtin',
        '-fno-stack-protector', '-ffreestanding', '-fno-asynchronous-unwind-tables',
        '-fno-unwind-tables', '-DFDB_WRITE_GRAN=8', '-DFDB_BOARD_U545',
        "-I$(Join-Path $script:FwDir 'common')",
        "-I$(Join-Path $script:FwDir 'c\include')",
        "-I$(Join-Path $script:RefDir 'inc')",
        "-I$(Join-Path $script:RefDir 'port\fal\inc')"
    )
    $compileLog = Join-Path $OutDir 'compile.log'
    Remove-Item $compileLog -ErrorAction SilentlyContinue
    $objs = @()
    $failed = $false
    foreach ($src in $srcs) {
        $o = Join-Path $objDir ((Split-Path $src -LeafBase) + '.o')
        $err = & $gcc @cflags -c $src -o $o 2>&1
        if ($LASTEXITCODE -ne 0) {
            Add-Content $compileLog "FAILED: $src`n$($err -join "`n")"
            $failed = $true
        } elseif ($err) {
            Add-Content $compileLog "$src`n$($err -join "`n")"
        }
        $objs += $o
    }
    if ($failed) {
        Get-Content $compileLog | Select-Object -Last 30 | Write-Host
        throw "C compilation failed (log: $compileLog)"
    }
    $elf = Join-Path $OutDir 'fw_c_u545.elf'
    $map = Join-Path $OutDir 'fw_c_u545.map'
    $linkArgs = @(
        '-mcpu=cortex-m33', '-mthumb', '-mfloat-abi=soft', '-nostdlib', '-nostartfiles',
        '-Wl,--gc-sections', "-Wl,-Map=$map", '-T', $script:BoardLd, '-o', $elf
    )
    $linkOut = & $gcc @linkArgs @objs '-lgcc' 2>&1
    if ($LASTEXITCODE -ne 0) {
        Write-Host ($linkOut | Out-String)
        throw "C link failed (map: $map)"
    }
    Write-Host "   c   : ok (arm-none-eabi-gcc -Os, -DFDB_BOARD_U545) -> $elf"
    return $elf
}

# ---------------------------------------------------------------------------
# Flash + run + read back
# ---------------------------------------------------------------------------
function Invoke-BoardRun {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$Elf,
        [string]$Chip = 'STM32U545RE',
        [int]$TimeoutSec = 120,
        [string]$OutDir = $script:RustOut,
        [switch]$Quiet
    )
    Need 'probe-rs'
    New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
    $nm = Find-SymbolTool
    $size = Find-SizeTool
    $boardOut = Get-SymbolAddress $nm $Elf 'BOARD_OUT'
    $estack = Get-SymbolAddress $nm $Elf '_estack'

    $log = Join-Path $OutDir 'probe_download.log'
    $dump = Join-Path $OutDir 'board_out.bin'
    $textFile = Join-Path $OutDir 'fw_output.txt'
    Remove-Item $dump -ErrorAction SilentlyContinue

    if (-not $Quiet) {
        Write-Host ("   elf       : {0}" -f $Elf)
        Write-Host ("   BOARD_OUT : 0x{0:X8} (0x{1:X} bytes)  _estack: 0x{2:X8}" -f $boardOut, $script:BoardOutSize, $estack)
        if ($size) {
            Write-Host ''
            & $size -A $Elf | Write-Host
        }
        Write-Host '-- flashing --'
    }
    $flashOut = & probe-rs download --chip $Chip --verify --reset --log-file $log $Elf 2>&1 | Out-String
    if ($LASTEXITCODE -ne 0) { Write-Host $flashOut; throw "probe-rs download failed (rc=$LASTEXITCODE); log: $log" }
    if (-not $Quiet) { Write-Host '   flash + verify: ok' }

    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    $seen = $false
    while ((Get-Date) -lt $deadline) {
        $w = & probe-rs read --chip $Chip b32 ('0x{0:X}' -f $boardOut) 1 2>$null
        if ($w -match $script:DoneMagic) { $seen = $true; break }
        Start-Sleep -Milliseconds 200
    }
    if (-not $seen) {
        throw ("firmware did not set the DONE flag within {0}s -- is the target running? Try a physical USB replug." -f $TimeoutSec)
    }
    $elapsed = $sw.Elapsed.TotalSeconds
    if (-not $Quiet) { Write-Host ("   done after ~{0:N1}s (reset -> DONE)" -f $elapsed) }

    $readOut = & probe-rs read --chip $Chip b8 ('0x{0:X}' -f $boardOut) $script:BoardOutSize -o $dump -f binary 2>&1 | Out-String
    if ($LASTEXITCODE -ne 0) { Write-Host $readOut; throw "probe-rs read failed (rc=$LASTEXITCODE)" }
    if (-not (Test-Path $dump)) { throw "result buffer was not read back to $dump" }

    $raw = [System.IO.File]::ReadAllBytes($dump)
    $done = [BitConverter]::ToUInt32($raw, 0)
    $len = [BitConverter]::ToUInt32($raw, 4)
    $text = [System.Text.Encoding]::ASCII.GetString($raw, 8, [Math]::Min($len, 4096))
    Set-Content -Path $textFile -Value $text -NoNewline -Encoding ascii

    $fields = @{}
    foreach ($line in $text -split "`r?`n") {
        if ($line -notmatch '^(RESULT|BOARD) ') { continue }
        foreach ($tok in ($line -split '\s+')) {
            if ($tok -match '^([A-Za-z0-9_]+)=(.*)$') { $fields[$Matches[1]] = $Matches[2] }
        }
    }
    if ($fields.Count -eq 0) {
        $doneText = if ($done -eq 0x454E4F44) { 'OK' } else { 'MISSING' }
        throw ("no RESULT line in the result buffer (done=0x{0:X8} {1}; firmware crashed?)" -f $done, $doneText)
    }

    return [pscustomobject]@{
        Done     = $done
        Len      = $len
        Text     = $text
        Fields   = $fields
        Elapsed  = $elapsed
        Elf      = $Elf
        DumpPath = $dump
        TextPath = $textFile
        LogPath  = $log
    }
}

# ---------------------------------------------------------------------------
# Result helpers
# ---------------------------------------------------------------------------
function Get-RomBytes($Fields) { [int64]$Fields['text'] + [int64]$Fields['rodata'] + [int64]$Fields['data'] }
function Get-StaticRam($Fields) { [int64]$Fields['data'] + [int64]$Fields['bss'] }
function Get-Stack($Fields) { [int64]$Fields['stack'] }

function Get-Cycles($Fields, [string]$Key) { if ($Fields.ContainsKey($Key)) { [int64]$Fields[$Key] } else { 0 } }

function Format-BoardRun($Run, [string]$Title = 'firmware', [int]$CoreHz = 0) {
    Write-Host ''
    Write-Host "-- $Title output --"
    Write-Host $Run.Text.TrimEnd()
    Write-Host '-- parsed --'
    $keys = $Run.Fields.Keys | Where-Object { $_ -match '^(kv_|ts_|rc$)' }
    Write-Host ('RESULT fields: ' + (($keys | Sort-Object | ForEach-Object { "$_=$($Run.Fields[$_])" }) -join ' '))
    $hz = if ($CoreHz -gt 0) { $CoreHz } elseif ($Run.Fields.ContainsKey('core_hz')) { [int]$Run.Fields['core_hz'] } else { 0 }
    foreach ($k in @('cycles_mount', 'cycles_kv', 'cycles_ts', 'cycles_roll', 'cycles_total')) {
        if (-not $Run.Fields.ContainsKey($k)) { continue }
        $c = [int64]$Run.Fields[$k]
        if ($hz -gt 0) {
            Write-Host ('{0,-14} = {1,12} cycles  ({2,11:F1} us @ {3} MHz)' -f $k, $c, ($c * 1e6 / $hz), ($hz / 1e6))
        } else {
            Write-Host ('{0,-14} = {1,12} cycles  (core_hz unknown)' -f $k, $c)
        }
    }
    $doneText = if ($Run.Done -eq 0x454E4F44) { 'OK' } else { 'MISSING' }
    Write-Host ('ROM {0} B | static RAM {1} B | stack {2} B | done 0x{3:X8} ({4})' -f (Get-RomBytes $Run.Fields), (Get-StaticRam $Run.Fields), (Get-Stack $Run.Fields), $Run.Done, $doneText)
    Write-Host ('result bin: {0}' -f $Run.DumpPath)
}
