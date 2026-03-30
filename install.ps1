# Quick-start installer for the Autonomi `ant` CLI.
#
# Usage:
#   irm https://raw.githubusercontent.com/WithAutonomi/ant-client/main/install.ps1 | iex
#
# Environment variables:
#   ANT_VERSION   - install a specific version (e.g. "0.1.1"). Defaults to latest.
#   INSTALL_DIR   - override install directory (default: %LOCALAPPDATA%\ant\bin).
#
# Verification:
#   Release archives are signed with ML-DSA-65 post-quantum signatures.
#   Download ant-keygen from https://github.com/WithAutonomi/ant-keygen/releases
#   and the public key from resources/release-signing-key.pub in the repository, then:
#     ant-keygen verify --key release-signing-key.pub --input <file> --signature <file>.sig --context ant-release-v1

$ErrorActionPreference = "Stop"

$Repo = "WithAutonomi/ant-client"
$BinaryName = "ant"

# --- helpers ----------------------------------------------------------------

function Say($msg) { Write-Host $msg }
function Err($msg) { Write-Error $msg; exit 1 }

function Get-LatestVersion {
    $response = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest"
    if ($response.tag_name -match "^ant-cli-v(.+)$") {
        return $Matches[1]
    }
    Err "Could not parse version from tag: $($response.tag_name)"
}

function Get-DefaultInstallDir {
    return Join-Path $env:LOCALAPPDATA "ant\bin"
}

function Get-ConfigDir {
    return Join-Path $env:APPDATA "ant"
}

# --- main -------------------------------------------------------------------

$Version = if ($env:ANT_VERSION) { $env:ANT_VERSION } else { Get-LatestVersion }
$InstallDir = if ($env:INSTALL_DIR) { $env:INSTALL_DIR } else { Get-DefaultInstallDir }
$Target = "x86_64-pc-windows-msvc"

if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") {
    Say "WARNING: No native ARM64 build available. Installing x86_64 binary (runs under emulation)."
}

Say "Installing ant $Version for $Target..."

$Archive = "$BinaryName-$Version-$Target.zip"
$Url = "https://github.com/$Repo/releases/download/ant-cli-v$Version/$Archive"

$TempDir = Join-Path ([System.IO.Path]::GetTempPath()) "ant-install-$([System.Guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $TempDir -Force | Out-Null

try {
    Say "Downloading $Url..."
    Invoke-WebRequest -Uri $Url -OutFile (Join-Path $TempDir $Archive) -UseBasicParsing

    Say "Extracting..."
    Expand-Archive -Path (Join-Path $TempDir $Archive) -DestinationPath $TempDir

    # Install binary
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    $ExtractedDir = Join-Path $TempDir "$BinaryName-$Version-$Target"
    Copy-Item (Join-Path $ExtractedDir "$BinaryName.exe") -Destination (Join-Path $InstallDir "$BinaryName.exe") -Force
    Say "Installed $BinaryName.exe to $InstallDir"

    # Install bootstrap config
    $ConfigDir = Get-ConfigDir
    New-Item -ItemType Directory -Path $ConfigDir -Force | Out-Null
    $ConfigFile = Join-Path $ConfigDir "bootstrap_peers.toml"
    if (-not (Test-Path $ConfigFile)) {
        Copy-Item (Join-Path $ExtractedDir "bootstrap_peers.toml") -Destination $ConfigFile
        Say "Installed bootstrap config to $ConfigFile"
    } else {
        Say "Bootstrap config already exists at $ConfigFile - skipping"
    }

    # Add to user PATH if not already there
    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($UserPath -notlike "*$InstallDir*") {
        Say ""
        Say "Adding $InstallDir to your user PATH..."
        [Environment]::SetEnvironmentVariable("Path", "$InstallDir;$UserPath", "User")
        $env:Path = "$InstallDir;$env:Path"
        Say "Restart your terminal for the PATH change to take effect."
    }
} finally {
    Remove-Item -Path $TempDir -Recurse -Force -ErrorAction SilentlyContinue
}

Say ""
Say "Done! Run 'ant --help' to get started."
