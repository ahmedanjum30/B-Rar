# Builds the release binaries and the installer, dist\B-Rar-Setup-<version>.exe, with B-Installer
# (https://github.com/ahmedanjum30/B-Installer) from installer\b-rar.toml.
#
#   powershell -ExecutionPolicy Bypass -File installer\build-setup.ps1
#
# -Quick builds with the `quick` profile instead: much faster after a change, for installers to
# test locally. Releases use the default (full LTO).
#
# B-Installer's bi.exe and bi-setup.exe come from its release below, checked against its
# SHA-256 sums, into target\b-installer. -Bi <path to bi.exe> uses another build of it (with
# bi-setup.exe next to it).
param([switch] $Quick, [string] $Bi)
# Native tools signal failure through exit codes (checked below); cargo's progress goes to
# stderr, which Windows PowerShell would otherwise turn into a terminating error.
$ErrorActionPreference = 'Continue'
$root = Split-Path $PSScriptRoot -Parent

$biVersion = 'v0.1.1'
$biSums = @{
    'bi.exe'       = 'aef2cbcf4190570aa3725eb1a2f9894bfdcf84406252dfad93b44cdda0efd9df'
    'bi-setup.exe' = 'f574b437c3c3642724c52ecd431c5a5c856673969182dcef8c49497e039edfb7'
}

if (-not $Bi) {
    $tools = Join-Path $root "target\b-installer\$biVersion"
    New-Item -ItemType Directory -Force $tools | Out-Null
    foreach ($name in $biSums.Keys) {
        $file = Join-Path $tools $name
        if (-not (Test-Path $file)) {
            Invoke-WebRequest -UseBasicParsing "https://github.com/ahmedanjum30/B-Installer/releases/download/$biVersion/$name" -OutFile $file
        }
        if ((Get-FileHash $file -Algorithm SHA256).Hash.ToLower() -ne $biSums[$name]) {
            Remove-Item $file
            Write-Error "$name from B-Installer $biVersion doesn't match its SHA-256"; exit 1
        }
    }
    $Bi = Join-Path $tools 'bi.exe'
}

$version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value

Push-Location $root
try {
    $buildProfile = if ($Quick) { 'quick' } else { 'release' }
    cargo build --profile $buildProfile -p brar-cli -p brar-gui
    if ($LASTEXITCODE -ne 0) { Write-Error 'cargo build failed'; exit 1 }
    & $Bi (Join-Path $root 'installer\b-rar.toml') -D "version=$version" -D "bindir=$root\target\$buildProfile"
    if ($LASTEXITCODE -ne 0) { Write-Error 'B-Installer failed'; exit 1 }
} finally {
    Pop-Location
}
Get-Item (Join-Path $root "dist\B-Rar-Setup-$version.exe") | ForEach-Object { "Built $($_.FullName) ($([math]::Round($_.Length / 1MB, 2)) MB)" }
