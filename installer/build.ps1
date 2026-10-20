# Builds brar in release mode and packages dist\brar-setup-<version>.exe with Inno Setup 6.
# Native tools signal failure through exit codes (checked below); cargo's progress goes to
# stderr, which Windows PowerShell would otherwise turn into a terminating error.
$ErrorActionPreference = 'Continue'
$root = Split-Path $PSScriptRoot -Parent

$iscc = @(
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe",
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { throw 'Inno Setup 6 not found (winget install JRSoftware.InnoSetup)' }

$version = (Select-String -Path "$root\Cargo.toml" -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value

Push-Location $root
try {
    cargo build --release -p brar-cli -p brar-gui
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
    & $iscc "/DAppVersion=$version" /Qp "$PSScriptRoot\brar.iss"
    if ($LASTEXITCODE -ne 0) { throw 'ISCC failed' }
} finally { Pop-Location }

Get-Item "$root\dist\brar-setup-$version.exe"
