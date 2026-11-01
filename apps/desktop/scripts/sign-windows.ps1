# Authenticode signing of one file with SSL.com eSigner's CodeSignTool (P4-REL-01).
#
# Called by the Tauri bundler for scull.exe and each installer through
# `bundle.windows.signCommand`, which .github/workflows/release-desktop.yml sets only when the
# SSLDOTCOM_* secrets are present. The same provider and secrets as the CLI pipeline (dist's
# `ssldotcom-windows-sign`). Secrets come from the environment and are never printed.
#
#   pwsh -File sign-windows.ps1 <file>
#
# CodeSignTool is downloaded once per job from SSL.com (the URL dist uses). Set the repository
# variable CODESIGNTOOL_SHA256 to pin the download to a reviewed release.
param([Parameter(Mandatory = $true)][string]$File)
$ErrorActionPreference = 'Stop'

foreach ($v in 'SSLDOTCOM_USERNAME', 'SSLDOTCOM_PASSWORD', 'SSLDOTCOM_CREDENTIAL_ID', 'SSLDOTCOM_TOTP_SECRET') {
    if (-not [Environment]::GetEnvironmentVariable($v)) {
        Write-Host "::notice title=Windows signing skipped::$v is not set; $File is NOT signed."
        exit 0
    }
}

$base = Join-Path ($env:RUNNER_TEMP ?? [IO.Path]::GetTempPath()) 'codesigntool'
$tool = Get-ChildItem $base -Recurse -Filter 'CodeSignTool.bat' -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $tool) {
    New-Item -ItemType Directory -Force $base | Out-Null
    $zip = Join-Path $base 'CodeSignTool.zip'
    Invoke-WebRequest 'https://www.ssl.com/download/codesigntool-for-windows/' -OutFile $zip
    if ($env:CODESIGNTOOL_SHA256) {
        $hash = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($hash -ne $env:CODESIGNTOOL_SHA256.ToLowerInvariant()) { throw "CodeSignTool.zip SHA-256 $hash does not match CODESIGNTOOL_SHA256" }
    } else {
        Write-Host "::warning title=CodeSignTool not pinned::set the CODESIGNTOOL_SHA256 repository variable to pin SSL.com's download"
    }
    Expand-Archive $zip -DestinationPath $base -Force
    $tool = Get-ChildItem $base -Recurse -Filter 'CodeSignTool.bat' | Select-Object -First 1
    if (-not $tool) { throw 'CodeSignTool.bat not found in the download' }
    # Production endpoints (as in dist and SSL.com's own GitHub action).
    $conf = Join-Path $tool.DirectoryName 'conf/code_sign_tool.properties'
    Set-Content -Path $conf -Encoding ascii -Value @(
        'CLIENT_ID=kaXTRACNijSWsFdRKg_KAfD3fqrBlzMbWs6TwWHwAn8',
        'OAUTH2_ENDPOINT=https://login.ssl.com/oauth2/token',
        'CSC_API_ENDPOINT=https://cs.ssl.com',
        'TSA_URL=http://ts.ssl.com',
        'TSA_LEGACY_URL=http://ts.ssl.com/legacy'
    )
}

$full = (Resolve-Path $File).Path
Push-Location $tool.DirectoryName
try {
    & $tool.FullName sign "-input_file_path=$full" "-username=$env:SSLDOTCOM_USERNAME" `
        "-password=$env:SSLDOTCOM_PASSWORD" "-credential_id=$env:SSLDOTCOM_CREDENTIAL_ID" `
        "-totp_secret=$env:SSLDOTCOM_TOTP_SECRET" '-override=true' | Out-Host
    if ($LASTEXITCODE -ne 0) { throw "CodeSignTool failed for $full (exit $LASTEXITCODE)" }
} finally {
    Pop-Location
}
$sig = Get-AuthenticodeSignature $full
if ($sig.Status -ne 'Valid') { throw "$full signature: $($sig.Status)" }
Write-Host "signed: $full ($($sig.SignerCertificate.Subject))"
