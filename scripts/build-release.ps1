<#
.SYNOPSIS
  Builds the WinBaobab release: tests, installer and portable exe in dist\.
.EXAMPLE
  ./scripts/build-release.ps1   (version is read from Cargo.toml; -Version x.y.z optionally cross-checks it)
#>
param(
    [ValidatePattern('^\d+\.\d+\.\d+$')][string]$Version,
    [string]$Iscc = "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo
# Cargo.toml is the single source of the version; -Version is only an optional cross-check.
$cargoVersion = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
if (-not $cargoVersion) { throw 'Could not read version from Cargo.toml.' }
if ($Version -and $Version -ne $cargoVersion) { throw "-Version $Version does not match Cargo.toml version $cargoVersion. Bump Cargo.toml first." }
$Version = $cargoVersion
if (-not (Test-Path $Iscc)) { throw "ISCC.exe not found at '$Iscc'. Install Inno Setup 6 or pass -Iscc <path>." }

# Pin this process (and children: cargo, rustc, ISCC) to CPUs 16-31 (second CCD on a 16-core Ryzen) so games keep the first; skipped on smaller CPUs.
if ([Environment]::ProcessorCount -ge 32) { (Get-Process -Id $PID).ProcessorAffinity = [IntPtr][long]0xFFFF0000L }
function Invoke-Cargo {
    & "$PSScriptRoot\cargo.ps1" @args
    if ($LASTEXITCODE -ne 0) { throw "cargo $($args -join ' ') failed ($LASTEXITCODE)" }
}

Invoke-Cargo build --release
Invoke-Cargo test --release

New-Item -ItemType Directory -Force dist | Out-Null
Remove-Item -Force -ErrorAction SilentlyContinue "dist\WinBaobab-Setup-$Version.exe", "dist\WinBaobab-$Version-portable.exe"
& $Iscc "/DAppVersion=$Version" /Qp "installer\winbaobab.iss"
if ($LASTEXITCODE -ne 0) { throw "ISCC failed ($LASTEXITCODE)" }

Copy-Item target\release\winbaobab.exe "dist\WinBaobab-$Version-portable.exe" -Force
foreach ($f in "dist\WinBaobab-Setup-$Version.exe", "dist\WinBaobab-$Version-portable.exe") {
    $h = Get-FileHash $f -Algorithm SHA256
    "{0}  {1}" -f $h.Hash.ToLower(), (Split-Path $f -Leaf)
}
