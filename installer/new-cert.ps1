# One-time setup: creates B-Rar's self-signed code-signing certificate and trusts it on this PC.
#
# The certificate (with its private key) lives in your personal store, Cert:\CurrentUser\My,
# never in the repository. Trusting it means adding the public part to the machine's
# "Trusted People" store, which needs one admin (UAC) prompt.
#
# Keep a backup of the certificate (certmgr.msc -> Personal -> export with private key):
# updates to an installed MSIX must be signed with the same certificate.
param([string] $Subject = 'CN=Ahmad Anjum')
$ErrorActionPreference = 'Stop'
$friendly = 'B-Rar MSIX signing'

$cert = Get-ChildItem Cert:\CurrentUser\My | Where-Object { $_.Subject -eq $Subject -and $_.FriendlyName -eq $friendly } |
    Sort-Object NotAfter -Descending | Select-Object -First 1
if (-not $cert) {
    $cert = New-SelfSignedCertificate -Type Custom -Subject $Subject -FriendlyName $friendly `
        -KeyUsage DigitalSignature -KeyAlgorithm RSA -KeyLength 3072 -HashAlgorithm SHA256 `
        -CertStoreLocation Cert:\CurrentUser\My -NotAfter (Get-Date).AddYears(5) `
        -TextExtension @('2.5.29.37={text}1.3.6.1.5.5.7.3.3', '2.5.29.19={text}')
    "created certificate $($cert.Thumbprint)"
} else {
    "using existing certificate $($cert.Thumbprint)"
}

$trusted = Get-ChildItem Cert:\LocalMachine\TrustedPeople | Where-Object Thumbprint -eq $cert.Thumbprint
if (-not $trusted) {
    $cer = Join-Path ([IO.Path]::GetTempPath()) 'B-Rar-signing.cer'
    Export-Certificate -Cert $cert -FilePath $cer | Out-Null
    "asking for admin rights to trust the certificate on this PC..."
    $p = Start-Process powershell -Verb RunAs -Wait -PassThru -ArgumentList @(
        '-NoProfile', '-Command', "Import-Certificate -FilePath '$cer' -CertStoreLocation Cert:\LocalMachine\TrustedPeople | Out-Null"
    )
    Remove-Item $cer -ErrorAction SilentlyContinue
    if (-not (Get-ChildItem Cert:\LocalMachine\TrustedPeople | Where-Object Thumbprint -eq $cert.Thumbprint)) {
        throw 'the certificate was not trusted (admin prompt declined?)'
    }
    'certificate trusted on this PC'
} else {
    'certificate already trusted on this PC'
}
$cert.Thumbprint
