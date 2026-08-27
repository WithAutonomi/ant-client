# Quick-start installer for the Autonomi `ant` CLI.
#
# Usage:
#   irm https://raw.githubusercontent.com/WithAutonomi/ant-client/main/install.ps1 | iex
#
# Environment variables:
#   ANT_CHANNEL   - release channel to install from: "stable" (default) or "beta".
#   ANT_VERSION   - install a specific version (e.g. "0.1.1"). Overrides ANT_CHANNEL.
#   INSTALL_DIR   - override install directory (default: %LOCALAPPDATA%\ant\bin).
#
# Examples:
#   irm <this-script> | iex                                   # newest stable
#   $env:ANT_CHANNEL="beta"; irm <this-script> | iex          # newest beta-eligible
#   $env:ANT_VERSION="0.3.3"; irm <this-script> | iex         # a specific version
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

# Whether a version is eligible for a release channel.
#
# Deliberately mirrors version_matches_channel in ant-core/src/channel.rs, which in turn mirrors
# ant-node's own copy. If these drift, the installer hands someone a build that `ant update` would
# then refuse to move off, or vice versa.
#
#   stable - final releases only, i.e. no pre-release component at all.
#   beta   - final releases, plus pre-releases whose first identifier is exactly `beta`.
#
# Every other pre-release suffix is rejected on both channels. `-rc.*` in particular is NOT a beta
# candidate: release candidates are published before the release gates have reported, and semver
# ranks `-rc` above `-beta`, so accepting them would pull beta users onto un-gated code. Both
# 0.3.4-beta.1 and 0.3.4-rc.1 routinely exist at once, which is exactly when this matters.
function Test-VersionMatchesChannel {
    param([string]$Version, [string]$Channel)

    $dash = $Version.IndexOf('-')
    if ($dash -lt 0) { return $true }          # a final release suits every channel
    if ($Channel -ne "beta") { return $false }

    $pre = $Version.Substring($dash + 1)
    # Compare the whole first identifier, so `betamax.1` is not mistaken for a beta.
    return ($pre.Split('.')[0] -eq "beta")
}

# Compare two versions by semver precedence. Returns -1, 0 or 1.
#
# [version] is not used: it cannot parse a pre-release suffix at all, and would either throw on
# "0.3.4-beta.1" or silently compare the wrong thing.
function Compare-SemVer {
    param([string]$A, [string]$B)

    $splitVersion = {
        param([string]$v)
        $dash = $v.IndexOf('-')
        if ($dash -lt 0) { return @{ Core = $v; Pre = "" } }
        return @{ Core = $v.Substring(0, $dash); Pre = $v.Substring($dash + 1) }
    }

    $left = & $splitVersion $A
    $right = & $splitVersion $B

    $leftFields = $left.Core.Split('.')
    $rightFields = $right.Core.Split('.')
    for ($i = 0; $i -lt 3; $i++) {
        $l = if ($i -lt $leftFields.Count) { [int]$leftFields[$i] } else { 0 }
        $r = if ($i -lt $rightFields.Count) { [int]$rightFields[$i] } else { 0 }
        if ($l -gt $r) { return 1 }
        if ($l -lt $r) { return -1 }
    }

    # Same core version: a final release outranks any pre-release of it.
    if ($left.Pre -eq "" -and $right.Pre -ne "") { return 1 }
    if ($left.Pre -ne "" -and $right.Pre -eq "") { return -1 }
    if ($left.Pre -eq "" -and $right.Pre -eq "") { return 0 }

    # Both are pre-releases, and only `beta.N` reaches this far, so the trailing number decides.
    $parseTrailing = {
        param([string]$pre)
        $last = $pre.Split('.')[-1]
        $n = 0
        if ([int]::TryParse($last, [ref]$n)) { return $n }
        return 0
    }
    $leftN = & $parseTrailing $left.Pre
    $rightN = & $parseTrailing $right.Pre
    if ($leftN -gt $rightN) { return 1 }
    if ($leftN -lt $rightN) { return -1 }
    return 0
}

# Newest stable release, via the endpoint that already excludes pre-releases.
function Get-LatestStableVersion {
    $response = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest"
    if ($response.tag_name -match "^ant-cli-v(.+)$") {
        return $Matches[1]
    }
    Err "Could not parse version from tag: $($response.tag_name)"
}

# Highest release eligible for $Channel, scanning the release list.
#
# /releases/latest cannot serve the beta channel: GitHub excludes pre-releases from it entirely, so
# it would always return the newest stable build. The list endpoint includes them, and the highest
# *eligible* entry wins - which is neither the newest by date nor the highest by raw semver.
function Get-LatestChannelVersion {
    param([string]$Channel)

    $releases = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases?per_page=100"
    $best = $null

    foreach ($release in $releases) {
        if ($release.tag_name -notmatch "^ant-cli-v(.+)$") { continue }
        $version = $Matches[1]
        if (-not (Test-VersionMatchesChannel -Version $version -Channel $Channel)) { continue }
        if ($null -eq $best -or (Compare-SemVer -A $version -B $best) -gt 0) {
            $best = $version
        }
    }

    if ($null -eq $best) { Err "No $Channel-eligible ant-cli release found" }
    return $best
}

function Resolve-Version {
    param([string]$Channel)
    switch ($Channel) {
        "stable" { return Get-LatestStableVersion }
        "beta"   { return Get-LatestChannelVersion -Channel "beta" }
        default  { Err "Unknown channel '$Channel' (expected 'stable' or 'beta')" }
    }
}

function Get-DefaultInstallDir {
    return Join-Path $env:LOCALAPPDATA "ant\bin"
}

function Get-ConfigDir {
    return Join-Path $env:APPDATA "ant"
}

# --- main -------------------------------------------------------------------

$Channel = if ($env:ANT_CHANNEL) { $env:ANT_CHANNEL } else { "stable" }
$Version = if ($env:ANT_VERSION) { $env:ANT_VERSION } else { Resolve-Version -Channel $Channel }
$InstallDir = if ($env:INSTALL_DIR) { $env:INSTALL_DIR } else { Get-DefaultInstallDir }
$Target = "x86_64-pc-windows-msvc"

if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") {
    Say "WARNING: No native ARM64 build available. Installing x86_64 binary (runs under emulation)."
}

if ($env:ANT_VERSION) {
    Say "Installing ant $Version for $Target..."
} else {
    Say "Installing ant $Version for $Target ($Channel channel)..."
}

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
