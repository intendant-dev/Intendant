<#
.SYNOPSIS
    Intendant installer for Windows -- the install.sh counterpart.
    The canonical copy is a per-tag GitHub release asset; a Connect
    rendezvous serves at most a redirect to it, never the script body.

.DESCRIPTION
    Stands up a daemon and optionally links its route to Connect. The
    one-time claim code grants no daemon access and changes no IAM. Establish
    root separately through the machine's local console or direct mTLS. The
    packaged macOS app only bridges its own bundled local daemon; this
    installer never accepts an owner key.

    One-liner (PowerShell):
      & ([scriptblock]::Create((irm https://github.com/intendant-dev/Intendant/releases/latest/download/install.ps1)))

    Release assets are stamped with the release they belong to (tag +
    commit) and sha256-committed to the public transparency log; a stamped
    copy installs exactly that released tree and fails closed
    (RELEASE_PIN_MISMATCH) when the checkout does not match. The copy in
    scripts/ is the unstamped source.

    Dependencies (git, rustup, VS Build Tools, NASM) are handled by
    scripts/setup-windows.ps1 from the cloned repo -- run automatically
    when this shell is elevated, otherwise checked and reported.

.PARAMETER Connect
    Rendezvous URL to register with for discovery. Default: the
    INTENDANT_CONNECT_RENDEZVOUS_URL environment variable, else none
    (the daemon publishes no discovery route; its local dashboard still
    works).

.PARAMETER DaemonId
    Stable daemon id at the rendezvous.

.PARAMETER Ref
    Pin the fresh clone to a tag, branch, or commit. Default: the release
    this installer was stamped with (when fetched as a release asset); an
    unstamped copy falls back to the newest published release tag
    (vX.Y.Z), and to the default branch head only while no release
    exists. An explicit ref you choose skips the release-pin
    verification.

.PARAMETER Service
    Keep the daemon running unattended: installs a Task Scheduler entry
    via `intendant service install` (at boot when elevated, at logon
    otherwise) supervised by the built-in restart loop; the one-time claim code
    lands in the service log the installer prints.

.PARAMETER NoRun
    Build and link only; print how to start it.

.PARAMETER Repo
    Git URL to clone (default: https://github.com/intendant-dev/Intendant).

.PARAMETER InstallDir
    Checkout directory (default: $HOME\intendant).
#>
[CmdletBinding()]
param(
    [string]$Connect = $env:INTENDANT_CONNECT_RENDEZVOUS_URL,
    [string]$DaemonId = "",
    [string]$Ref = "",
    [switch]$Service,
    [switch]$NoRun,
    [string]$Repo = "https://github.com/intendant-dev/Intendant",
    [string]$InstallDir = (Join-Path $HOME "intendant")
)

$ErrorActionPreference = "Stop"

# -- Release identity --
# Stamped by release.yml when this script is packaged as a release asset
# (empty in the repository copy). A stamped installer announces the
# release it belongs to, installs exactly that released tree, and fails
# closed on mismatch; its own bytes are covered by the release manifest
# committed to the transparency log.
$InstallerReleaseTag = ""
$InstallerReleaseCommit = ""

function Say([string]$Message) { Write-Host "[intendant install] $Message" -ForegroundColor White }
function Fail([string]$Message) { Write-Host "[intendant install] $Message" -ForegroundColor Red; exit 1 }

# -- Identity banner --
# Say what this copy IS before doing anything else.
if ($InstallerReleaseCommit) {
    Say "release-pinned installer: $InstallerReleaseTag @ $InstallerReleaseCommit"
} else {
    Say "unstamped source copy (no release pin) -- the canonical, verified installer is the GitHub release asset"
}

$elevated = ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()
    ).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)

# -- Toolchain --
# git is needed before anything else -- the clone is how the repo's own
# setup scripts (scripts\setup-windows.ps1) arrive, so they cannot
# install it for us. A stock box may lack it: try winget, then choco,
# announcing the exact command before it runs -- elevation only ever
# happens through their own visible prompts (UAC), never silently. With
# neither manager present, stop and name the command to run.
if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
    if (Get-Command winget -ErrorAction SilentlyContinue) {
        $gitInstall = @("winget", "install", "--id", "Git.Git", "-e", "--source", "winget",
            "--accept-package-agreements", "--accept-source-agreements")
    } elseif (Get-Command choco -ErrorAction SilentlyContinue) {
        $gitInstall = @("choco", "install", "git", "-y")
    } else {
        Fail "git is required and neither winget nor choco is here to install it. Install it yourself (winget install Git.Git) and re-run -- or run scripts\setup-windows.ps1 from an elevated shell after cloning $Repo."
    }
    $gitInstallShown = $gitInstall -join " "
    Say "git is missing -- installing it via: $gitInstallShown"
    $gitInstallExe, $gitInstallArgs = $gitInstall
    & $gitInstallExe @gitInstallArgs
    if ($LASTEXITCODE -ne 0) {
        Fail "git install failed -- run it yourself ($gitInstallShown), then re-run this installer."
    }
    # The package manager extends PATH for future shells only -- append the
    # machine/user PATH here so this session sees the new git (append, not
    # replace: process-only entries must survive).
    $env:Path = $env:Path + ";" +
        [Environment]::GetEnvironmentVariable("Path", "Machine") + ";" +
        [Environment]::GetEnvironmentVariable("Path", "User")
    if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
        Fail "git was installed but is not on PATH in this session -- open a new PowerShell and re-run this installer."
    }
}

# -- Source --
if (Test-Path (Join-Path $InstallDir ".git")) {
    if ($Ref) { Fail "-Ref pins fresh clones only; $InstallDir already exists -- check out the ref there yourself." }
    Say "using existing checkout at $InstallDir (leaving it exactly as-is)"
    if ($InstallerReleaseCommit) {
        Say "note: the stamped release pin ($InstallerReleaseTag) is not enforced on a checkout you already had"
    }
} else {
    if (-not $Ref) {
        if ($InstallerReleaseCommit) {
            # A stamped release asset installs exactly its own release; the
            # tree is verified against the stamped commit after checkout.
            $Ref = $InstallerReleaseTag
            Say "installing the stamped release: $Ref"
        } else {
            # Unstamped copy: default fresh installs to the newest published
            # release tag (vX.Y.Z only -- pre-releases and peeled refs are
            # filtered) so even this path delivers an immutable, released
            # tree. Falling back to the mutable default-branch head happens
            # only while no release exists, and says so out loud. -Ref
            # overrides either way.
            $tagLines = git ls-remote --tags $Repo "v*"
            if ($LASTEXITCODE -eq 0 -and $tagLines) {
                $Ref = @($tagLines) |
                    ForEach-Object { if ($_ -match 'refs/tags/(v\d+\.\d+(\.\d+){0,2})$') { $Matches[1] } } |
                    Sort-Object { [version]$_.Substring(1) } |
                    Select-Object -Last 1
            }
            if ($Ref) {
                Say "pinning to the latest release tag: $Ref (override with -Ref)"
            } else {
                Say "note: no release tags published yet -- installing the default branch head (mutable; pin with -Ref once releases exist)."
            }
        }
    } elseif ($InstallerReleaseCommit -and $Ref -ne $InstallerReleaseTag) {
        Say "note: explicit ref $Ref overrides the stamped release ($InstallerReleaseTag) -- release-pin verification is skipped for a ref you chose"
    }
    Say "cloning $Repo -> $InstallDir"
    git clone --depth 1 $Repo $InstallDir
    if ($LASTEXITCODE -ne 0) { Fail "git clone failed" }
    if ($Ref) {
        Say "pinning checkout to $Ref"
        git -C $InstallDir fetch --depth 1 origin $Ref
        if ($LASTEXITCODE -ne 0) { Fail "git fetch $Ref failed" }
        git -C $InstallDir checkout --detach FETCH_HEAD
        if ($LASTEXITCODE -ne 0) { Fail "git checkout $Ref failed" }
    }
}
Set-Location $InstallDir

# -- Release-pin verification --
# A stamped installer fails closed unless the tree it just checked out is
# the exact commit its release recorded -- a moved tag, a substituted
# remote, or a tampered mirror all land here, BEFORE anything from the
# tree is executed. Everything the installer runs from here on
# (setup-windows.ps1, the build) comes from the verified tree, and
# `cargo build --locked` extends the pinning to dependency hashes.
if ($InstallerReleaseCommit -and $Ref -eq $InstallerReleaseTag) {
    $actualCommit = git rev-parse HEAD
    if ($LASTEXITCODE -ne 0 -or -not $actualCommit) { Fail "git rev-parse HEAD failed" }
    $actualCommit = "$actualCommit".Trim()
    if ($actualCommit -ne $InstallerReleaseCommit) {
        Fail "RELEASE_PIN_MISMATCH: $InstallerReleaseTag checked out commit $actualCommit, but this installer was published for $InstallerReleaseCommit. Refusing to continue. Re-download the installer from the release page and compare the repository's tags before trusting either."
    }
    Say "release pin verified: $Ref is commit $actualCommit"
}

# -- System dependencies --
# setup-windows.ps1 is the dependency authority (rustup, VS Build Tools
# C++ workload, NASM, ffmpeg, Media Foundation). It needs elevation to
# install; unelevated we only verify and report.
$setup = Join-Path $InstallDir "scripts\setup-windows.ps1"
if ($elevated -and (Test-Path $setup)) {
    Say "installing system dependencies (scripts\setup-windows.ps1 -NoBuild)"
    & $setup -NoBuild
    if ($LASTEXITCODE -ne 0) { Fail "system dependency setup failed" }
} elseif (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Fail "Rust is required. Either re-run this installer from an elevated PowerShell (it will run scripts\setup-windows.ps1 for you) or install rustup from https://rustup.rs and re-run."
} else {
    Say "note: unelevated shell -- skipping dependency setup; if the build fails on a missing native dep, run scripts\setup-windows.ps1 from an elevated PowerShell."
}

# -- Build --
# --locked: build exactly the committed Cargo.lock -- a resolution that
# differs from what CI tested is a failure, not a fallback.
Say "building release binaries (this takes a few minutes on a fresh box)"
cargo build --release --locked
if ($LASTEXITCODE -ne 0) { Fail "cargo build failed" }
$daemonExe = Join-Path $InstallDir "target\release\intendant.exe"

# -- Launch --
$daemonArgs = @("--no-tui")
if ($Connect) {
    $env:INTENDANT_CONNECT_RENDEZVOUS_URL = $Connect
    if ($DaemonId) { $env:INTENDANT_CONNECT_DAEMON_ID = $DaemonId }
    Say "rendezvous: $Connect"
} else {
    Say "note: no -Connect rendezvous URL -- the daemon will not publish a discovery route (its local dashboard still works)."
}

if ($Service) {
    # `service install` writes the Task Scheduler definition, captures the
    # INTENDANT_CONNECT_* env set above, and prints where the one-time claim code
    # lands (the built-in supervisor's log file).
    if (-not $elevated) {
        Say "note: unelevated -- the task starts at logon; re-run elevated for an at-boot service."
    }
    $installArgs = @("service", "install")
    if (-not $NoRun) { $installArgs += "--now" }
    $installArgs += "--"
    $installArgs += $daemonArgs
    & $daemonExe @installArgs
    if ($LASTEXITCODE -ne 0) { Fail "service install failed" }
} elseif ($NoRun) {
    Say "done. Start it with:"
    Say "  `"$daemonExe`" $($daemonArgs -join ' ')"
} else {
    Say "starting the daemon -- its one-time Connect code links discovery only and grants no access. Establish owner through this machine's local console or direct mTLS."
    & $daemonExe @daemonArgs
    exit $LASTEXITCODE
}
