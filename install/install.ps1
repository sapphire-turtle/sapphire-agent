# Install the headless edition of sapphire-agent (the CLI, the app server and
# the MCP server in one binary, for servers and other machines without a GUI)
# from this repository's GitHub releases. The desktop app ships as an installer
# on the same releases. sapphire-bridge, which the app syncs through, is not
# installed here: sapphire-framework has a script for it.
#
#   & ([scriptblock]::Create((irm https://raw.githubusercontent.com/sapphire-turtle/sapphire-agent/main/install/install.ps1)))
#
# Run with -Help for the options. Windows x86_64; Linux and macOS have install.sh.
#
# Each download is checked against the SHA-256 GitHub records for it and, when
# minisign is on PATH, against the organization's signature before it is
# installed. Files go to ~\.local\bin, where the `update` command of each binary
# recognises a release install and can replace it later. A binary installed by
# cargo is left alone, as `update` would.
#
# Made from sapphire-turtle/.github's templates/install/install.ps1: keep the
# two in step, and change only the comment above and the tables below.

# Only the app names are positional: without PositionalBinding = $false, a bare
# name binds to the first free parameter in declaration order (-Dir, say)
# before -App ever sees it.
[CmdletBinding(PositionalBinding = $false)]
param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]] $App = @(),
    [string] $Dir = (Join-Path $HOME '.local\bin'),
    [switch] $RequireSignature,
    # The bridge is no longer installed with the applications; the switch is
    # accepted so that older command lines still run.
    [switch] $NoBridge,
    [switch] $Help
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

# Every application this script installs: repository, command, and the prefix
# of its release tags. The command's release file is
# `<command>-headless-windows-x86_64.exe`. Keep in step with install.sh and with
# the repository's release-plz.toml. With one entry, the name may be left out.
$Apps = [ordered]@{
    'agent' = @('sapphire-agent', 'sapphire-agent', 'v')
}

# Applications installed by another repository's script, and that repository.
$Moved = @{}

$Owner = 'sapphire-turtle'

# The organization's minisign public key (sapphire_framework_update::UPDATE_PUBKEY),
# which signs every release file.
$PubKey = 'RWTryBeqOT+n9BzfWt2B3inpQylvPAJYvSy5tBmOEAauzdqmbUMqayhq'
$PubKeyId = 'F4A73F39AA17C8EB'

function Show-Usage {
    Write-Host 'Usage: install.ps1 [-Dir <dir>] [-RequireSignature] [<app>...]'
    Write-Host ''
    Write-Host "Installs each <app>'s headless edition from its latest release. For the"
    Write-Host "desktop app, download its installer from the releases instead."
    Write-Host ''
    Write-Host 'Apps:'
    foreach ($name in $Apps.Keys) { Write-Host "  $name" }
    Write-Host ''
    Write-Host 'Options:'
    Write-Host '  -Dir <dir>         Install into <dir> (default: ~\.local\bin)'
    Write-Host '  -RequireSignature  Fail, rather than warn, when minisign is not on PATH to'
    Write-Host '                     check the signature'
}

if ($Help) { Show-Usage; exit 0 }
if ($App.Count -eq 0) {
    if ($Apps.Count -eq 1) { $App = @($Apps.Keys) } else { Show-Usage; exit 2 }
}
foreach ($name in $App) {
    if ($Moved.Contains($name)) {
        throw "$name is installed by its own repository's script now:`n  & ([scriptblock]::Create((irm https://raw.githubusercontent.com/$Owner/$($Moved[$name])/main/install/install.ps1)))"
    }
    if ($name.EndsWith('-desktop') -and $Apps.Contains($name.Substring(0, $name.Length - 8))) {
        $repo = $Apps[$name.Substring(0, $name.Length - 8)][0]
        throw "$name is not installed by this script: download the desktop app's installer from https://github.com/$Owner/$repo/releases/latest"
    }
    if (-not $Apps.Contains($name)) { throw "unknown app: $name (see -Help)" }
}
if (-not [Environment]::Is64BitOperatingSystem -or $env:PROCESSOR_ARCHITECTURE -eq 'ARM64') {
    throw 'releases are built for Windows x86_64 only; build from source with cargo'
}

$Minisign = (Get-Command 'minisign.exe' -CommandType Application -ErrorAction SilentlyContinue |
    Select-Object -First 1).Source
if (-not $Minisign) {
    if ($RequireSignature) {
        throw 'minisign is required to check the signature (-RequireSignature): install it from https://jedisct1.github.io/minisign/'
    }
    Write-Warning 'minisign is not on PATH: downloads are checked against their SHA-256 only. Install minisign (https://jedisct1.github.io/minisign/) to check the signature as well.'
}

# A GitHub API response. Unauthenticated requests share a low per-address rate
# limit (CI runners hit it); GITHUB_TOKEN or GH_TOKEN, when set, lifts it.
function Invoke-GitHubApi([string] $Path) {
    $token = if ($env:GITHUB_TOKEN) { $env:GITHUB_TOKEN } else { $env:GH_TOKEN }
    $headers = @{}
    if ($token) { $headers['Authorization'] = "Bearer $token" }
    Invoke-RestMethod -Uri "https://api.github.com/$Path" -Headers $headers
}

# The highest x.y.z among the repository's releases tagged <prefix>x.y.z. Not
# `releases/latest`: a repository that tags several binaries has one "latest"
# among all of them. Pre-release versions (x.y.z-...) are skipped, as `update`
# skips them.
function Get-LatestVersion([string] $Repo, [string] $Prefix) {
    try {
        $releases = Invoke-GitHubApi "repos/$Owner/$Repo/releases?per_page=100"
    } catch {
        throw "could not list the releases of $Owner/$Repo (rate-limited? set GITHUB_TOKEN): $_"
    }
    $versions = foreach ($r in $releases) {
        if ($r.draft -or $r.prerelease) { continue }
        if (-not $r.tag_name.StartsWith($Prefix)) { continue }
        $v = $r.tag_name.Substring($Prefix.Length)
        if ($v -match '^\d+\.\d+\.\d+$') { [version] $v }
    }
    $latest = $versions | Sort-Object -Descending | Select-Object -First 1
    if ($latest) { "$latest" } else { $null }
}

# Check a download against its SHA-256 and the organization's signature, or throw.
function Test-Download([string] $File, $Asset, $Release, [string] $Url) {
    $name = $Asset.name
    if (-not ($Asset.digest -and $Asset.digest.StartsWith('sha256:'))) {
        throw "$name has no SHA-256 recorded on GitHub: not installed"
    }
    $want = $Asset.digest.Substring(7)
    $got = (Get-FileHash -Algorithm SHA256 -LiteralPath $File).Hash.ToLowerInvariant()
    if ($got -ne $want) { throw "$name does not match its SHA-256 (expected $want, got $got): not installed" }

    if (-not ($Release.assets | Where-Object { $_.name -eq "$name.sig" })) {
        throw "$name is not signed: not installed"
    }
    if (-not $Minisign) { return }

    # The .sig file is a minisign signature, base64-encoded as cargo-packager
    # writes it.
    $sig = Join-Path ([IO.Path]::GetTempPath()) "sapphire-install.$([guid]::NewGuid().ToString('N')).minisig"
    try {
        $encoded = (Invoke-WebRequest -Uri "$Url.sig" -UseBasicParsing).Content
        if ($encoded -is [byte[]]) { $encoded = [Text.Encoding]::ASCII.GetString($encoded) }
        [IO.File]::WriteAllBytes($sig, [Convert]::FromBase64String($encoded.Trim()))
        # -Q prints the trusted comment alone: timestamp:<n>`tfile:<name>.
        $comment = & $Minisign -V -Q -P $PubKey -m $File -x $sig 2>&1 | Out-String
        if ($LASTEXITCODE) {
            throw "$name does not carry a valid signature from the organization's key (${PubKeyId}): not installed"
        }
    } finally {
        Remove-Item -Force -ErrorAction SilentlyContinue $sig
    }
    $signed = ($comment.Trim() -split "`t" | Where-Object { $_.StartsWith('file:') } |
        Select-Object -First 1)
    if ($signed) { $signed = $signed.Substring(5) }
    if ($signed -ne $name) {
        throw "the signature on $name was made for $(if ($signed) { $signed } else { 'another file' }): not installed"
    }
    Write-Host "  Signature verified ($PubKeyId)."
}

# A binary under cargo's bin directory was installed by `cargo install`: leave
# it to cargo, as sapphire-framework-update's `install_method` does.
function Test-CargoInstall([string] $Path) {
    $cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $HOME '.cargo' }
    $cargoBin = (Join-Path $cargoHome 'bin') + '\'
    $Path.StartsWith($cargoBin, [StringComparison]::OrdinalIgnoreCase)
}

# Where the application's desktop package is installed, or $null. The package
# (Velopack, installed per user under %LocalAppData%\<packId>) carries the same
# CLI and app server and updates them itself: a second copy here would run a
# second server. The packId is <app>-desktop, never the app's data directory
# (%LocalAppData%\<app>), which the uninstall would delete (corundum-framework#57).
function Get-DesktopInstall([string] $Base) {
    if (-not $env:LOCALAPPDATA) { return $null }
    $root = Join-Path $env:LOCALAPPDATA "$Base-desktop"
    if (Test-Path (Join-Path $root 'Update.exe')) { $root } else { $null }
}

$Installed = @()

function Install-App([string] $Name) {
    $repo, $base, $prefix = $Apps[$Name]

    $desktop = Get-DesktopInstall $base
    if ($desktop) {
        Write-Host "Skipping ${base}: the desktop app at $desktop already includes it and keeps it up to date."
        return
    }

    $existing = (Get-Command "$base.exe" -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1).Source
    if ($existing -and (Test-CargoInstall $existing)) {
        Write-Host "Leaving $existing alone: it was installed by cargo. Update it the same way."
        return
    }

    $v = Get-LatestVersion $repo $prefix
    if (-not $v) { throw "$repo has no ${prefix}x.y.z release of $base yet" }
    try {
        $release = Invoke-GitHubApi "repos/$Owner/$repo/releases/tags/$prefix$v"
    } catch {
        throw "could not read the release $prefix$v of $Owner/$repo (rate-limited? set GITHUB_TOKEN): $_"
    }
    $asset = $release.assets | Where-Object { $_.name -eq "$base-headless-windows-x86_64.exe" }
    if (-not $asset) { throw "$repo $prefix$v has no $base for windows-x86_64" }
    $url = "https://github.com/$Owner/$repo/releases/download/$prefix$v/$($asset.name)"
    $dest = Join-Path $Dir "$base.exe"

    Write-Host "Installing $base $v (windows-x86_64) to $Dir"
    New-Item -ItemType Directory -Force -Path $Dir | Out-Null
    $tmp = Join-Path $Dir ".$base.$([guid]::NewGuid().ToString('N')).exe"
    try {
        try {
            Invoke-WebRequest -Uri $url -OutFile $tmp -UseBasicParsing
        } catch {
            throw "could not download ${url}: $_"
        }
        Test-Download $tmp $asset $release $url
    } catch {
        Remove-Item -Force -ErrorAction SilentlyContinue $tmp
        throw
    }
    # A running executable cannot be overwritten on Windows, but it can be
    # renamed: move the old file aside, then put the new one in its place. The
    # old file is removed when nothing runs it any more, or on the next install.
    Get-ChildItem -Path $Dir -Filter ".$base.old-*.exe" -ErrorAction SilentlyContinue |
        Remove-Item -Force -ErrorAction SilentlyContinue
    if (Test-Path $dest) {
        Move-Item -Force $dest (Join-Path $Dir ".$base.old-$([guid]::NewGuid().ToString('N')).exe")
    }
    Move-Item -Force $tmp $dest

    if ($existing -and $existing -ne $dest) {
        Write-Host "Note: $existing comes first on PATH and was not replaced; remove it, or install"
        Write-Host "      with -Dir $(Split-Path $existing) to replace it."
    }
    $script:Installed += $base
}

foreach ($name in $App) {
    Install-App $name
}

if ($Installed.Count -eq 0) { exit 0 }
Write-Host ''
Write-Host "Installed: $($Installed -join ' ')"

$userPath = [Environment]::GetEnvironmentVariable('PATH', 'User')
if (($userPath -split ';') -notcontains $Dir) {
    [Environment]::SetEnvironmentVariable('PATH', "$Dir;$userPath", 'User')
    Write-Host ''
    Write-Host "Added $Dir to your PATH. Open a new terminal to use it."
}

if ($Installed -contains 'sapphire-bridge') {
    Write-Host ''
    Write-Host 'Run the bridge as a service, so it starts with your session:'
    Write-Host '  sapphire-bridge service install'
    Write-Host 'If the service was already running, restart it to run the new version.'
} elseif (-not (Get-Command 'sapphire-bridge.exe' -CommandType Application -ErrorAction SilentlyContinue) -and
          -not (Test-Path (Join-Path $Dir 'sapphire-bridge.exe'))) {
    # The applications sync through the host's bridge, which is installed on its own.
    Write-Host ''
    Write-Host 'To sync, install sapphire-bridge on this host as well:'
    Write-Host "  & ([scriptblock]::Create((irm https://raw.githubusercontent.com/$Owner/sapphire-framework/main/install/install.ps1))) bridge"
}
