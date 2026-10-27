# Builds the release binaries and the installer, dist\B-Rar-Setup-<version>.exe.
#
#   powershell -ExecutionPolicy Bypass -File installer\build-setup.ps1
#
# -Quick builds with the `quick` profile instead: much faster after a change, for installers to
# test locally. Releases use the default (full LTO).
#
# Needs Inno Setup 6 (winget install JRSoftware.InnoSetup).
param([switch] $Quick)
# Native tools signal failure through exit codes (checked below); cargo's progress goes to
# stderr, which Windows PowerShell would otherwise turn into a terminating error.
$ErrorActionPreference = 'Continue'
$root = Split-Path $PSScriptRoot -Parent

$iscc = @(
    (Get-Command iscc -ErrorAction SilentlyContinue).Source,
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe",
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
) | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
if (-not $iscc) { Write-Error 'Inno Setup 6 not found. Install it with: winget install JRSoftware.InnoSetup'; exit 1 }

$version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value

Push-Location $root
try {
    $buildProfile = if ($Quick) { 'quick' } else { 'release' }
    cargo build --profile $buildProfile -p brar-cli -p brar-gui
    if ($LASTEXITCODE -ne 0) { Write-Error 'cargo build failed'; exit 1 }
    & $iscc /Qp "/DAppVersion=$version" "/DBinDir=$root\target\$buildProfile" (Join-Path $root 'installer\b-rar.iss')
    if ($LASTEXITCODE -ne 0) { Write-Error 'Inno Setup failed'; exit 1 }
} finally {
    Pop-Location
}
Get-Item (Join-Path $root "dist\B-Rar-Setup-$version.exe") | ForEach-Object { "Built $($_.FullName) ($([math]::Round($_.Length / 1MB, 2)) MB)" }
