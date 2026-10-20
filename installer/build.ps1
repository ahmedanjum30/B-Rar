# Builds B-Rar in release mode and packages it as dist\B-Rar-<version>.msix, signed with the
# certificate from new-cert.ps1 (run that once first). -Install also installs it on this PC.
param([switch] $Install)
# Native tools signal failure through exit codes (checked below); cargo's progress goes to
# stderr, which Windows PowerShell would otherwise turn into a terminating error.
$ErrorActionPreference = 'Continue'
$root = Split-Path $PSScriptRoot -Parent
$subject = 'CN=Ahmad Anjum'
$package = 'AhmadAnjum.B-Rar'

function Fail([string] $why) { Write-Error $why; exit 1 }

$sdk = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\makeappx.exe" -ErrorAction SilentlyContinue |
    Sort-Object FullName -Descending | Select-Object -First 1
if (-not $sdk) { Fail 'Windows SDK not found (winget install Microsoft.WindowsSDK.10.0.26100)' }
$makeappx = $sdk.FullName
$signtool = Join-Path $sdk.DirectoryName 'signtool.exe'

$cert = Get-ChildItem Cert:\CurrentUser\My | Where-Object { $_.Subject -eq $subject -and $_.FriendlyName -eq 'B-Rar MSIX signing' } |
    Sort-Object NotAfter -Descending | Select-Object -First 1
if (-not $cert) { Fail 'signing certificate not found: run installer\new-cert.ps1 once' }

# MSIX wants four version parts.
$version = (Select-String -Path "$root\Cargo.toml" -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$msixVersion = "$version.0"

Push-Location $root
try {
    cargo build --release -p brar-cli -p brar-gui -p brar-shell
    if ($LASTEXITCODE -ne 0) { Fail 'cargo build failed' }

    $layout = Join-Path $root 'target\msix\layout'
    if (Test-Path $layout) { Remove-Item -Recurse -Force $layout }
    New-Item -ItemType Directory -Force "$layout\Assets" | Out-Null
    Copy-Item 'target\release\B-Rar.exe', 'target\release\B-Rar-GUI.exe' $layout
    # Rust names the DLL after the crate (brar_shell); ship it under the product name.
    Copy-Item 'target\release\brar_shell.dll' "$layout\B-Rar-Shell.dll"
    Copy-Item 'installer\unrar-license.txt' $layout
    Copy-Item 'assets\msix\*.png' "$layout\Assets"
    (Get-Content 'installer\AppxManifest.xml' -Raw).Replace('{{VERSION}}', $msixVersion) |
        Set-Content -Encoding utf8 "$layout\AppxManifest.xml"

    New-Item -ItemType Directory -Force 'dist' | Out-Null
    $msix = Join-Path $root "dist\B-Rar-$version.msix"
    & $makeappx pack /o /h SHA256 /d $layout /p $msix | Out-Null
    if ($LASTEXITCODE -ne 0) { & $makeappx pack /o /h SHA256 /d $layout /p $msix; Fail 'makeappx failed' }
    & $signtool sign /q /fd SHA256 /sha1 $cert.Thumbprint /s My $msix
    if ($LASTEXITCODE -ne 0) { Fail 'signtool failed' }
    Get-Item $msix

    if ($Install) {
        # Windows won't reinstall the same version with new contents (a dev rebuild): remove it first.
        Get-AppxPackage -Name $package | Where-Object { $_.Version -eq $msixVersion } | Remove-AppxPackage
        Add-AppxPackage -Path $msix -ForceApplicationShutdown -ForceUpdateFromAnyVersion
        Get-AppxPackage -Name $package | Select-Object Name, Version, InstallLocation
    }
} finally { Pop-Location }
