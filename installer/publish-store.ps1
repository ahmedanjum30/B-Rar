# Submits the Store package to the Microsoft Store through the Partner Center submission API:
# a new submission copied from the published one, with this package in place of the old one
# and the version's CHANGELOG.md entry as "What's new", and the screenshots in store\images
# (1-*.png to 9-*.png, captions from store\listing.md) in place of the old ones, committed for
# certification. The listing text, logo and options stay as they are.
#
#   installer\publish-store.ps1 -Package dist\B-Rar-<version>-store.msix [-DryRun] [-NoCommit]
#   installer\publish-store.ps1 -ListingOnly [-NoCommit]   (screenshots only, the package stays)
#
# Needs MSSTORE_TENANT_ID, MSSTORE_CLIENT_ID and MSSTORE_CLIENT_SECRET: a Microsoft Entra app
# with the Manager role in Partner Center (Account settings > User management). Without them it
# skips. -DryRun only signs in and shows the app's state; -NoCommit uploads into a new
# submission and deletes it again, to test everything but the submitting.

param([string]$Package, [switch]$DryRun, [switch]$NoCommit, [switch]$ListingOnly)

$ErrorActionPreference = 'Stop'
$appId = '9MZKQXC7PFCH'
$root = Split-Path $PSScriptRoot -Parent

$tenant, $clientId, $secret = $env:MSSTORE_TENANT_ID, $env:MSSTORE_CLIENT_ID, $env:MSSTORE_CLIENT_SECRET
if (-not $tenant -or -not $clientId -or -not $secret) {
    'Microsoft Store credentials not set; skipping.'
    return
}

$token = (Invoke-RestMethod -Method Post "https://login.microsoftonline.com/$tenant/oauth2/v2.0/token" -ContentType 'application/x-www-form-urlencoded' -Body @{
        grant_type = 'client_credentials'
        client_id = $clientId
        client_secret = $secret
        scope = 'https://manage.devcenter.microsoft.com/.default'
    }).access_token
$api = "https://manage.devcenter.microsoft.com/v1.0/my/applications/$appId"

function Api($method, $path, $body) {
    # The API takes only JSON, even for calls without a body.
    $params = @{ Method = $method; Uri = "$api$path"; Headers = @{ Authorization = "Bearer $token" }; ContentType = 'application/json; charset=utf-8' }
    if ($null -ne $body) {
        $params.Body = [Text.Encoding]::UTF8.GetBytes(($body | ConvertTo-Json -Depth 30 -Compress))
    }
    Invoke-RestMethod @params
}

$app = Api GET ''
"Microsoft Store: $($app.primaryName) ($appId), published submission $($app.lastPublishedApplicationSubmission.id), pending submission $(if ($app.pendingApplicationSubmission) { $app.pendingApplicationSubmission.id } else { 'none' })"
if ($DryRun) { return }

# One submission at a time: a release during another's certification waits for the next
# release (Store users simply get the newer one then).
if ($app.pendingApplicationSubmission) {
    "::warning::A Microsoft Store submission ($($app.pendingApplicationSubmission.id)) is still in progress; this version wasn't submitted. The next release will be."
    return
}
if (-not $ListingOnly -and (-not $Package -or -not (Test-Path $Package))) { throw "No Store package at '$Package'." }
$version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value

# "What's new": the version's CHANGELOG.md section, as plain text (the Store shows no markdown).
$changelog = Get-Content (Join-Path $root 'CHANGELOG.md') -Raw
$section = [regex]::Match($changelog, '(?ms)^## ' + [regex]::Escape($version) + ' \(.*?\)\s*$(.*?)(?=^## |\z)').Groups[1].Value
$notes = ($section -replace '\*\*|`', '' -replace '\[([^\]]+)\]\([^)]+\)', '$1' -replace '(?m)^- ', '• ').Trim()
if (-not $notes) { $notes = "B-Rar $version." }
if ($notes.Length -gt 1500) { $notes = $notes.Substring(0, 1497) + '...' }

$submission = Api POST '/submissions'
"Submission $($submission.id) created for $version."
$committed = $false
try {
    # What goes up, in one zip: the package and the screenshots.
    $stage = Join-Path ([IO.Path]::GetTempPath()) "B-Rar-$version-store"
    Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force (Join-Path $stage 'images') | Out-Null

    if (-not $ListingOnly) {
        # The new package in, the old ones out.
        foreach ($p in $submission.applicationPackages) { $p.fileStatus = 'PendingDelete' }
        $name = Split-Path $Package -Leaf
        Copy-Item $Package $stage
        $submission.applicationPackages = @($submission.applicationPackages) + [pscustomobject]@{
            fileName = $name
            fileStatus = 'PendingUpload'
            minimumDirectXVersion = 'None'
            minimumSystemRam = 'None'
        }
    }

    # Screenshots: store\images\<n>-*.png, in order, captioned from listing.md's table.
    $listing = Get-Content (Join-Path $root 'store/listing.md') -Raw
    $shots = Get-ChildItem (Join-Path $root 'store/images') -Filter '*.png' | Where-Object Name -Match '^\d-' | Sort-Object Name
    foreach ($s in $shots) { Copy-Item $s.FullName (Join-Path $stage 'images') }
    foreach ($lang in $submission.listings.PSObject.Properties.Name) {
        $base = $submission.listings.$lang.baseListing
        if (-not $ListingOnly) { $base.releaseNotes = $notes }
        $images = @(foreach ($i in @($base.images | Where-Object { $_ })) {
                if ($i.imageType -eq 'Screenshot') { $i.fileStatus = 'PendingDelete' }
                $i
            })
        foreach ($s in $shots) {
            $caption = [regex]::Match($listing, '(?m)^\| `' + [regex]::Escape($s.Name) + '` \|[^|]*\| ([^|]*?) \|').Groups[1].Value
            $images += [pscustomobject]@{ fileName = "images/$($s.Name)"; fileStatus = 'PendingUpload'; imageType = 'Screenshot'; description = $caption }
        }
        $base.images = $images
    }

    # The zip goes to the blob the submission names.
    $zip = "$stage.zip"
    Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip -Force
    Invoke-RestMethod -Method Put -Uri $submission.fileUploadUrl -InFile $zip -Headers @{ 'x-ms-blob-type' = 'BlockBlob' } | Out-Null
    Remove-Item $zip, $stage -Recurse -Force
    Api PUT "/submissions/$($submission.id)" $submission | Out-Null
    "Uploaded $(if ($ListingOnly) { '' } else { "$name and " })$($shots.Count) screenshots."

    if ($NoCommit) {
        Api DELETE "/submissions/$($submission.id)" | Out-Null
        "-NoCommit: submission $($submission.id) deleted again."
        return
    }
    Api POST "/submissions/$($submission.id)/commit" | Out-Null
    $committed = $true

    # The Store checks the package before certification starts.
    for ($i = 0; ; $i++) {
        $status = Api GET "/submissions/$($submission.id)/status"
        if ($status.status -ne 'CommitStarted') { break }
        if ($i -ge 60) { throw 'The Microsoft Store is taking unusually long to accept the submission.' }
        Start-Sleep 10
    }
    if ($status.status -eq 'CommitFailed') {
        $why = @($status.statusDetails.errors) + @($status.statusDetails.warnings) | ForEach-Object { "$($_.code): $($_.details)" }
        throw "The Microsoft Store refused the submission: $($why -join '; ')"
    }
    "Submitted $(if ($ListingOnly) { 'new screenshots for B-Rar' } else { "B-Rar $version" }) to the Microsoft Store (submission $($submission.id), $($status.status)); certification can take a day or more."
} catch {
    if (-not $committed) {
        try { Api DELETE "/submissions/$($submission.id)" | Out-Null } catch {}
    }
    throw
}
