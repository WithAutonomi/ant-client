# Update the winget manifest and optionally submit to winget-pkgs.
#
# Usage:
#   .\update-winget.ps1 -Version "0.1.1" -Submit
#
# Requires: wingetcreate (install with `winget install wingetcreate`)

param(
    [Parameter(Mandatory)]
    [string]$Version,

    [switch]$Submit,

    [string]$Token
)

$ErrorActionPreference = "Stop"

$repo = "WithAutonomi/ant-client"
$url = "https://github.com/$repo/releases/download/ant-cli-v$Version/ant-$Version-x86_64-pc-windows-msvc.zip"

Write-Host "Downloading archive to compute SHA256..."
$tempFile = [System.IO.Path]::GetTempFileName()
try {
    Invoke-WebRequest -Uri $url -OutFile $tempFile
    $hash = (Get-FileHash -Path $tempFile -Algorithm SHA256).Hash
    Write-Host "SHA256: $hash"
} finally {
    Remove-Item $tempFile -ErrorAction SilentlyContinue
}

# Read template and substitute values
$template = Get-Content "$PSScriptRoot\Autonomi.ant.yaml" -Raw
$manifest = $template `
    -replace '\$\{VERSION\}', $Version `
    -replace '\$\{SHA256\}', $hash

$outPath = "$PSScriptRoot\Autonomi.ant.installer.yaml"
$manifest | Set-Content $outPath -Encoding UTF8
Write-Host "Manifest written to $outPath"

if ($Submit) {
    if (-not $Token) {
        Write-Error "A GitHub personal access token (-Token) is required to submit."
        exit 1
    }
    Write-Host "Submitting to winget-pkgs..."
    wingetcreate update Autonomi.ant `
        --version $Version `
        --urls $url `
        --submit `
        --token $Token
}
