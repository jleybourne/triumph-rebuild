<#
.SYNOPSIS
  Builds the portable release zip: dist\triumph-rebuild-<version>-windows-x64.zip.

.DESCRIPTION
  1. Checks the repository: no tracked file may be game data or game-derived (title archives, saves,
     shader containers, translated shaders, caches). It refuses and exits 1 if it finds any.
  2. Builds rebuild-launcher.exe and rebuild.exe (cargo build --release).
  3. Takes DXC (dxc.exe, dxcompiler.dll, dxil.dll and its three licence texts) from Microsoft's
     DirectXShaderCompiler GitHub release, pinned by version and SHA-256.
  4. Writes the third-party licence texts of every crate linked into the two exes.
  5. Stages an explicit list of files, checks the stage the same way as the repository (only the listed
     files, no game data, no shader container in any of our binaries), and zips it.

  Nothing from a game folder is ever read. Upscaler DLLs (Streamline, DLSS, FidelityFX) are not bundled:
  see CREDITS.md and docs/user-guide.md.

  Works in Windows PowerShell 5.1 and PowerShell 7.

.PARAMETER OutDir
  Where the zip goes (default: <repo>\dist).
.PARAMETER DxcZip
  A local copy of the pinned DXC release zip; otherwise it is downloaded once into target\dxc\.
.PARAMETER SkipBuild
  Use the exes already in target\release.
.PARAMETER Jobs
  cargo -j (default 4, which leaves cores free on a shared machine).
.PARAMETER CheckOnly
  Only check the repository for game data, then exit (0 clean, 1 refused).

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\package.ps1
#>
[CmdletBinding()]
param(
    [string]$OutDir = "",
    [string]$DxcZip = "",
    [switch]$SkipBuild,
    [int]$Jobs = 4,
    [switch]$CheckOnly
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = "Stop"
$Repo = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
if (-not $OutDir) { $OutDir = Join-Path $Repo "dist" }

# ---- pinned third-party input ----------------------------------------------------------------------------

# DXC 1.8.2502: the same compiler as the Windows SDK 10.0.26100 build every test and run here uses.
$DxcVersion = "v1.8.2502"
$DxcUrl = "https://github.com/microsoft/DirectXShaderCompiler/releases/download/v1.8.2502/dxc_2025_02_20.zip"
$DxcSha256 = "70B1913A1BFCE4A3E1A5311D16246F4ECDF3A3E613ABEC8AA529E57668426F85"

# ---- the game-data guard ---------------------------------------------------------------------------------

# Extensions that are game data, saves, or shader output derived from the title. None may be tracked or shipped.
$DenyExtensions = @(
    ".rpf", ".xvd", ".xvc", ".xsp", ".msixvc", ".eekb", ".pak", ".bk2", ".bik", ".gsc", ".sgd",
    ".cso", ".dxil", ".dxbc", ".fxc", ".spv", ".od", ".samplers", ".bcpack", ".xex", ".xbe"
)
# File names that only ever come from a game package, a title's data or our caches of it.
$DenyNames = @(
    "microsoftgame.config", "appxmanifest.xml", "layout.xml", "settings.xml", "library.bin",
    "bcpack-tables.bin", "shaders.rpf",
    # the title's own DLLs (the console-only ones the runtime serves in their place)
    "d3d12_xs.dll", "xmem.dll", "acphal.dll", "xapu.dll", "chad.dll", "xcrdapi.dll", "xg_xs.dll",
    "xgameruntime.dll", "amd_fidelityfx_loader_xbox.dll", "amd_fidelityfx_upscaler_xbox.dll", "xboxbvhbuilder.dll"
)
# Folders that hold caches of translated shaders or a game's data.
$DenyDirs = @("pipelines-in", "translate", "ondemand", "victory-data", "bcpack-cache", "oracle")
$MaxTrackedBytes = 5MB

$Latin1 = [System.Text.Encoding]::GetEncoding(28591)

# A DXBC/DXIL shader container inside a file: "DXBC", 16-byte digest, version 1.0, a total size that fits,
# 1-64 parts, and the first part right after the part offsets. Our own exes compile their shaders at run time
# and embed none, so any container in them (or in a tracked file) is refused.
function Find-ShaderContainer([string]$Path) {
    $bytes = [System.IO.File]::ReadAllBytes($Path)
    if ($bytes.Length -lt 36) { return $null }
    $s = $Latin1.GetString($bytes)
    $i = $s.IndexOf("DXBC", [System.StringComparison]::Ordinal)
    while ($i -ge 0 -and $i + 36 -le $bytes.Length) {
        $major = [BitConverter]::ToUInt16($bytes, $i + 20)
        $minor = [BitConverter]::ToUInt16($bytes, $i + 22)
        $total = [BitConverter]::ToUInt32($bytes, $i + 24)
        $parts = [BitConverter]::ToUInt32($bytes, $i + 28)
        if ($major -eq 1 -and $minor -eq 0 -and $total -ge 36 -and $total -le ($bytes.Length - $i) -and $parts -ge 1 -and $parts -le 64) {
            $first = [BitConverter]::ToUInt32($bytes, $i + 32)
            if ($first -eq (32 + 4 * $parts)) { return ("a shader container at byte {0}" -f $i) }
        }
        $i = $s.IndexOf("DXBC", $i + 1, [System.StringComparison]::Ordinal)
    }
    return $null
}

# Why a path (relative, forward slashes) must not be tracked or shipped, or $null.
function Get-DenyReason([string]$Rel) {
    $name = [System.IO.Path]::GetFileName($Rel).ToLowerInvariant()
    $ext = [System.IO.Path]::GetExtension($name)
    if ($DenyExtensions -contains $ext) { return "game data or a translated shader ($ext)" }
    if ($DenyNames -contains $name) { return "a game's own file or a cache of it ($name)" }
    foreach ($part in ($Rel.ToLowerInvariant() -split "/")) {
        if ($DenyDirs -contains $part) { return "inside a shader or game-data cache folder ($part)" }
    }
    return $null
}

# The repository: every tracked file (and every untracked file git would add).
function Test-Repository {
    Push-Location $Repo
    try {
        $files = @(& git ls-files --cached --others --exclude-standard)
        if ($LASTEXITCODE -ne 0) { throw "git ls-files failed" }
    } finally { Pop-Location }
    $bad = New-Object System.Collections.Generic.List[string]
    foreach ($rel in $files) {
        $full = Join-Path $Repo $rel
        if (-not (Test-Path -LiteralPath $full -PathType Leaf)) { continue }
        $why = Get-DenyReason $rel
        if (-not $why) {
            $len = (Get-Item -LiteralPath $full).Length
            if ($len -gt $MaxTrackedBytes) { $why = "larger than $($MaxTrackedBytes / 1MB) MB ($len bytes): nothing of ours is" }
        }
        if (-not $why) { $why = Find-ShaderContainer $full }
        if ($why) { $bad.Add("$rel : $why") }
    }
    if ($bad.Count -gt 0) {
        Write-Host "REFUSED: the repository holds files that look like game data or game-derived output:" -ForegroundColor Red
        $bad | ForEach-Object { Write-Host "  $_" }
        exit 1
    }
    Write-Host ("repository   {0} files checked: no game data, no shader containers" -f $files.Count)
}

# The stage: exactly the files we put there, and none of them game data.
function Test-Stage([string]$Stage, [string[]]$Expected, [string[]]$Pinned) {
    $bad = New-Object System.Collections.Generic.List[string]
    $found = @(Get-ChildItem -LiteralPath $Stage -Recurse -File | ForEach-Object { $_.FullName.Substring($Stage.Length + 1).Replace("\", "/") })
    foreach ($rel in $found) {
        if ($Expected -notcontains $rel) { $bad.Add("$rel : not on the package list"); continue }
        $why = Get-DenyReason $rel
        if (-not $why -and $Pinned -notcontains $rel) { $why = Find-ShaderContainer (Join-Path $Stage $rel) }
        if ($why) { $bad.Add("$rel : $why") }
    }
    foreach ($rel in $Expected) { if ($found -notcontains $rel) { $bad.Add("$rel : missing") } }
    if ($bad.Count -gt 0) {
        Write-Host "REFUSED: the package would not be clean:" -ForegroundColor Red
        $bad | ForEach-Object { Write-Host "  $_" }
        exit 1
    }
}

Test-Repository
if ($CheckOnly) { exit 0 }

# ---- version ---------------------------------------------------------------------------------------------

$cargoToml = Get-Content (Join-Path $Repo "Cargo.toml") -Raw
$m = [regex]::Match($cargoToml, '(?ms)^\[workspace\.package\].*?^version\s*=\s*"([^"]+)"')
if (-not $m.Success) { throw "no [workspace.package] version in Cargo.toml" }
$WsVersion = $m.Groups[1].Value
Push-Location $Repo
try {
    $Describe = (& git describe --tags --always --dirty --match "v*").Trim()
    $Commit = (& git rev-parse HEAD).Trim()
} finally { Pop-Location }
# a release tag that names this exact version: "0.1.0"; anything else carries the describe: "0.1.0-9271847"
if ($Describe -eq "v$WsVersion") { $Version = $WsVersion } else { $Version = "$WsVersion-$($Describe -replace '^v', '')" }
$Name = "triumph-rebuild-$Version-windows-x64"
Write-Host "version      $Version (workspace $WsVersion, git $Describe)"

# ---- build -----------------------------------------------------------------------------------------------

if (-not $SkipBuild) {
    # the source paths compiled into panic messages and the PDB name the repository and cargo's folder, not
    # this machine's user folder
    $ch = $env:CARGO_HOME
    if (-not $ch) { $ch = Join-Path $env:USERPROFILE ".cargo" }
    $oldEncoded = $env:CARGO_ENCODED_RUSTFLAGS
    $flags = @()
    if ($env:RUSTFLAGS) { $flags += @($env:RUSTFLAGS -split '\s+' | Where-Object { $_ }) }
    $flags += "--remap-path-prefix=$Repo=triumph-rebuild", "--remap-path-prefix=$ch=cargo"
    $env:CARGO_ENCODED_RUSTFLAGS = $flags -join [char]0x1f     # paths with spaces stay whole
    Push-Location $Repo
    try {
        & cargo build --release -j $Jobs -p rebuild-launcher --bin rebuild-launcher -p rebuild --bin rebuild
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
    } finally { Pop-Location; $env:CARGO_ENCODED_RUSTFLAGS = $oldEncoded }
}
$Release = Join-Path $Repo "target\release"
foreach ($f in "rebuild-launcher.exe", "rebuild.exe", "rebuild.pdb") {
    if (-not (Test-Path (Join-Path $Release $f))) { throw "$f is not in target\release: build first" }
}

# ---- DXC -------------------------------------------------------------------------------------------------

if (-not $DxcZip) {
    $dxcDir = Join-Path $Repo "target\dxc"
    New-Item -ItemType Directory -Force $dxcDir | Out-Null
    $DxcZip = Join-Path $dxcDir ([System.IO.Path]::GetFileName($DxcUrl))
    if (-not (Test-Path $DxcZip) -or (Get-FileHash $DxcZip -Algorithm SHA256).Hash -ne $DxcSha256) {
        Write-Host "dxc          downloading $DxcVersion"
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
        $ProgressPreference = "SilentlyContinue"
        Invoke-WebRequest -Uri $DxcUrl -OutFile $DxcZip -UseBasicParsing
    }
}
$h = (Get-FileHash $DxcZip -Algorithm SHA256).Hash
if ($h -ne $DxcSha256) { throw "DXC zip $DxcZip has SHA-256 $h, expected $DxcSha256" }

# ---- stage -----------------------------------------------------------------------------------------------

$StageRoot = Join-Path $OutDir "stage"
$Stage = Join-Path $StageRoot $Name
if (Test-Path $StageRoot) { Remove-Item -Recurse -Force $StageRoot }
New-Item -ItemType Directory -Force (Join-Path $Stage "licenses") | Out-Null

Copy-Item (Join-Path $Release "rebuild-launcher.exe") $Stage
Copy-Item (Join-Path $Release "rebuild.exe") $Stage
Copy-Item (Join-Path $Release "rebuild.pdb") $Stage   # function names and lines in crash reports
Copy-Item (Join-Path $Repo "LICENSE") (Join-Path $Stage "LICENSE.txt")
Copy-Item (Join-Path $Repo "CREDITS.md") (Join-Path $Stage "CREDITS.md")
Copy-Item (Join-Path $Repo "apps\launcher\assets\fonts\Inter-OFL.txt") (Join-Path $Stage "licenses\Inter-OFL.txt")

Add-Type -AssemblyName System.IO.Compression.FileSystem
$zip = [System.IO.Compression.ZipFile]::OpenRead($DxcZip)
try {
    $take = @{
        "bin/x64/dxc.exe" = "dxc.exe"; "bin/x64/dxcompiler.dll" = "dxcompiler.dll"; "bin/x64/dxil.dll" = "dxil.dll";
        "LICENSE-LLVM.txt" = "licenses/DXC-LICENSE-LLVM.txt"; "LICENSE-MIT.txt" = "licenses/DXC-LICENSE-MIT.txt";
        "LICENSE-MS.txt" = "licenses/DXC-LICENSE-MS.txt"
    }
    foreach ($e in $zip.Entries) {
        if ($take.ContainsKey($e.FullName)) {
            [System.IO.Compression.ZipFileExtensions]::ExtractToFile($e, (Join-Path $Stage $take[$e.FullName]), $true)
        }
    }
} finally { $zip.Dispose() }

# ---- third-party licences of the linked crates -----------------------------------------------------------

Push-Location $Repo
try {
    $tree = @(& cargo tree -p rebuild-launcher -p rebuild -e "normal,no-proc-macro" --target x86_64-pc-windows-msvc --prefix none -f "{p}|{l}")
    if ($LASTEXITCODE -ne 0) { throw "cargo tree failed" }
} finally { Pop-Location }
$cargoHome = $env:CARGO_HOME
if (-not $cargoHome) { $cargoHome = Join-Path $env:USERPROFILE ".cargo" }
$srcRoots = @(Get-ChildItem (Join-Path $cargoHome "registry\src") -Directory -ErrorAction SilentlyContinue)
$crates = [ordered]@{}
foreach ($line in $tree) {
    $line = ($line -replace '\s*\(\*\)\s*$', '').Trim()
    if (-not $line -or $line -match '\(') { continue }        # "(path)" = a workspace crate: ours, GPL-3.0-only
    $p = $line -split '\|', 2
    $id = $p[0].Trim()
    if (-not $crates.Contains($id)) { $crates[$id] = $p[1].Trim() }
}
$needStandard = $false
$std = @{}
$sb = New-Object System.Text.StringBuilder
[void]$sb.AppendLine("Third-party licences: the Rust crates linked into rebuild-launcher.exe and rebuild.exe")
[void]$sb.AppendLine("triumph-rebuild $Version, commit $Commit")
[void]$sb.AppendLine("")
[void]$sb.AppendLine("Each crate's licence (SPDX expression, from its manifest) and the licence files it ships. Where a crate")
[void]$sb.AppendLine("offers a choice (A OR B), it is used under any one of them. The DXC files are under licenses\DXC-*.txt.")
[void]$sb.AppendLine("")
[void]$sb.AppendLine("Summary:")
foreach ($k in $crates.Keys) { [void]$sb.AppendLine(("  {0,-42} {1}" -f $k, $crates[$k])) }
foreach ($k in $crates.Keys) {
    $nm, $ver = $k -split ' v', 2
    $dir = $null
    foreach ($r in $srcRoots) { $c = Join-Path $r.FullName "$nm-$ver"; if (Test-Path $c) { $dir = $c; break } }
    [void]$sb.AppendLine("")
    [void]$sb.AppendLine(("=" * 100))
    [void]$sb.AppendLine("$k  --  $($crates[$k])")
    [void]$sb.AppendLine(("=" * 100))
    $files = @()
    if ($dir) {
        $files = @(Get-ChildItem $dir -File | Where-Object { $_.Name -match '^(LICEN[CS]E|COPYING|COPYRIGHT|NOTICE|UNLICENSE)' } | Sort-Object Name)
        $ld = Join-Path $dir "LICENSES"
        if (Test-Path $ld) { $files += @(Get-ChildItem $ld -File | Sort-Object Name) }
    }
    if ($files.Count -eq 0) {
        $authors = "its authors"
        if ($dir) {
            $am = [regex]::Match((Get-Content -LiteralPath (Join-Path $dir "Cargo.toml") -Raw), '(?s)\nauthors\s*=\s*\[(.*?)\]')
            if ($am.Success) { $authors = (($am.Groups[1].Value -split ',') | ForEach-Object { $_.Trim().Trim('"') } | Where-Object { $_ }) -join ", " }
        }
        [void]$sb.AppendLine("The crate ships no licence file. Copyright (c) $authors.")
        [void]$sb.AppendLine("Licensed under $($crates[$k]); the standard texts are at the end of this file.")
        $script:needStandard = $true
    }
    foreach ($f in $files) {
        $text = (Get-Content -LiteralPath $f.FullName -Raw -Encoding UTF8).TrimEnd()
        if (-not $script:std.Contains("Apache-2.0") -and $text -match '^\s*Apache License\s+Version 2\.0') { $script:std["Apache-2.0"] = $text }
        if (-not $script:std.Contains("BSL-1.0") -and $text -match 'Boost Software License - Version 1\.0') { $script:std["BSL-1.0"] = $text }
        [void]$sb.AppendLine("--- $($f.Name) ---")
        [void]$sb.AppendLine($text)
        [void]$sb.AppendLine("")
    }
}
if ($needStandard) {
    [void]$sb.AppendLine("")
    [void]$sb.AppendLine(("=" * 100))
    [void]$sb.AppendLine("Standard licence texts, for the crates above that ship no licence file of their own")
    [void]$sb.AppendLine(("=" * 100))
    [void]$sb.AppendLine("--- MIT ---")
    [void]$sb.AppendLine(@"
Permission is hereby granted, free of charge, to any person obtaining a copy of this software and associated
documentation files (the "Software"), to deal in the Software without restriction, including without limitation
the rights to use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of the Software, and
to permit persons to whom the Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all copies or substantial portions of
the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO
THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF
CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS
IN THE SOFTWARE.
"@)
    foreach ($id in "Apache-2.0", "BSL-1.0") {
        [void]$sb.AppendLine("")
        [void]$sb.AppendLine("--- $id ---")
        if ($std.Contains($id)) { [void]$sb.AppendLine($std[$id]) } else { [void]$sb.AppendLine("https://spdx.org/licenses/$id.html") }
    }
}
[System.IO.File]::WriteAllText((Join-Path $Stage "licenses\THIRD-PARTY-LICENSES.txt"), $sb.ToString(), (New-Object System.Text.UTF8Encoding $false))

# ---- README.txt ------------------------------------------------------------------------------------------

$readme = @"
triumph-rebuild $Version (Windows x64)
======================================

A compatibility layer that runs an Xbox Series title's own code on Windows, with a desktop launcher.
This is a PREVIEW for testers. The launcher, settings, profiles, saves, storage and mods work. The title
boots and its first-run menus run on developer machines; gameplay is not reached yet. The shader library
the game draws with is translated from your own copy on your PC, and the launcher does not build it yet:
until a release does, Play starts the game but most of its image stays empty.

This zip contains no game files. You need your own, legally obtained copy of the game, installed
as a folder on this PC. Nothing here is affiliated with or endorsed by its developer or publisher.

Requirements
  - Windows 10 or 11, 64-bit
  - a Direct3D 12 graphics card with tiled resources tier 2 or higher and shader model 6.6
    (recent NVIDIA, AMD or Intel cards; up-to-date driver)
  - your own copy of the game

Start
  1. Unzip this folder somewhere you own (Desktop, Documents, another drive). Not Program Files.
  2. Run rebuild-launcher.exe.
  3. Add your game's folder, check Diagnostics, then press Play.
  Everything the launcher and the game write goes to the "data" folder beside rebuild-launcher.exe.
  The game's own folder is only ever read.

Offline only: there is no online play.

The player's guide: https://github.com/jleybourne/triumph-rebuild/blob/master/docs/user-guide.md

What is in this folder
  rebuild-launcher.exe   the launcher
  rebuild.exe            the layer the launcher starts with Play (rebuild.pdb: names in crash reports)
  dxc.exe, dxcompiler.dll, dxil.dll
                         Microsoft's DirectX Shader Compiler $DxcVersion, unmodified, which turns shaders
                         into the PC's format on your machine; licences in licenses\DXC-*.txt
  LICENSE.txt            GNU GPL version 3 (the launcher and the layer)
  CREDITS.md             what this project uses from others, and under which licence
  licenses\              the licence texts of everything bundled
  FILES.txt              every file in this zip with its size and SHA-256

Not included (optional, supplied by you): NVIDIA DLSS / Streamline and AMD FidelityFX DLLs.
The upscaler is not wired into the game yet.

Licence and source
  triumph-rebuild is free software under the GNU General Public License version 3 only (LICENSE.txt),
  with no warranty. The source for exactly this build:
  https://github.com/jleybourne/triumph-rebuild/tree/$Commit
  DXC's dxil.dll is Microsoft's, under licenses\DXC-LICENSE-MS.txt; by using it you agree to those terms.
"@
[System.IO.File]::WriteAllText((Join-Path $Stage "README.txt"), ($readme -replace "`r?`n", "`r`n"), (New-Object System.Text.UTF8Encoding $false))

# ---- FILES.txt, the check, the zip -----------------------------------------------------------------------

$Pinned = @("dxc.exe", "dxcompiler.dll", "dxil.dll")   # verified by the release zip's SHA-256 above
$Expected = @(
    "rebuild-launcher.exe", "rebuild.exe", "rebuild.pdb", "dxc.exe", "dxcompiler.dll", "dxil.dll",
    "LICENSE.txt", "CREDITS.md", "README.txt", "FILES.txt",
    "licenses/DXC-LICENSE-LLVM.txt", "licenses/DXC-LICENSE-MIT.txt", "licenses/DXC-LICENSE-MS.txt",
    "licenses/Inter-OFL.txt", "licenses/THIRD-PARTY-LICENSES.txt"
)
$Licence = @{
    "rebuild-launcher.exe" = "GPL-3.0-only (this project; linked crates: licenses\THIRD-PARTY-LICENSES.txt)";
    "rebuild.exe" = "GPL-3.0-only (this project; linked crates: licenses\THIRD-PARTY-LICENSES.txt)";
    "rebuild.pdb" = "GPL-3.0-only (debug line tables of rebuild.exe)";
    "dxc.exe" = "NCSA (LLVM) and MIT: DXC $DxcVersion, licenses\DXC-LICENSE-LLVM.txt, DXC-LICENSE-MIT.txt";
    "dxcompiler.dll" = "NCSA (LLVM) and MIT: DXC $DxcVersion, licenses\DXC-LICENSE-LLVM.txt, DXC-LICENSE-MIT.txt";
    "dxil.dll" = "Microsoft DirectX Shader Compiler licence (distributable code): licenses\DXC-LICENSE-MS.txt";
}
$fl = New-Object System.Text.StringBuilder
[void]$fl.AppendLine("triumph-rebuild $Version  commit $Commit")
[void]$fl.AppendLine("")
foreach ($rel in ($Expected | Where-Object { $_ -ne "FILES.txt" })) {
    $p = Join-Path $Stage $rel
    $lic = ""
    if ($Licence.ContainsKey($rel)) { $lic = "  " + $Licence[$rel] }
    [void]$fl.AppendLine(("{0}  {1,10}  {2}{3}" -f (Get-FileHash $p -Algorithm SHA256).Hash.ToLowerInvariant(), (Get-Item $p).Length, $rel, $lic))
}
[System.IO.File]::WriteAllText((Join-Path $Stage "FILES.txt"), ($fl.ToString() -replace "`r?`n", "`r`n"), (New-Object System.Text.UTF8Encoding $false))

Test-Stage $Stage $Expected $Pinned
Write-Host ("stage        {0} files: the package list exactly, no game data, no shader containers" -f $Expected.Count)

New-Item -ItemType Directory -Force $OutDir | Out-Null
$ZipPath = Join-Path $OutDir "$Name.zip"
if (Test-Path $ZipPath) { Remove-Item -Force $ZipPath }
[System.IO.Compression.ZipFile]::CreateFromDirectory($Stage, $ZipPath, [System.IO.Compression.CompressionLevel]::Optimal, $true)
$zipHash = (Get-FileHash $ZipPath -Algorithm SHA256).Hash.ToLowerInvariant()
[System.IO.File]::WriteAllText("$ZipPath.sha256", "$zipHash  $Name.zip`n")

Write-Host ""
Write-Host "contents"
Get-ChildItem -LiteralPath $Stage -Recurse -File | Sort-Object FullName | ForEach-Object {
    Write-Host ("  {0,12:N0}  {1}" -f $_.Length, $_.FullName.Substring($Stage.Length + 1))
}
$unpacked = (Get-ChildItem -LiteralPath $Stage -Recurse -File | Measure-Object Length -Sum).Sum
Write-Host ("zip          {0}  {1:N1} MB ({2:N1} MB unpacked)  sha256 {3}" -f $ZipPath, ((Get-Item $ZipPath).Length / 1MB), ($unpacked / 1MB), $zipHash)
if ($env:GITHUB_OUTPUT) {
    Add-Content -Path $env:GITHUB_OUTPUT -Value "zip=$ZipPath"
    Add-Content -Path $env:GITHUB_OUTPUT -Value "name=$Name"
    Add-Content -Path $env:GITHUB_OUTPUT -Value "version=$Version"
}
