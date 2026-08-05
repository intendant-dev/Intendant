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

    The install finishes concretely: it generates the per-user dashboard
    access certificates and imports them into this user's certificate
    stores (CurrentUser\Root + CurrentUser\My; Windows asks for
    confirmation on the root import), adds the binaries to the user
    PATH, creates Desktop and Start-menu shortcuts that open the
    dashboard (starting the daemon first when needed), prints a success
    block with the tokened dashboard URL, and opens that URL in the
    default browser. Without -Service the daemon runs in its own console
    window: closing that window stops it, and nothing restarts it at
    logon -- -Service installs the Task Scheduler entry that does.

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
function Fail([string]$Message) {
    Write-Host "[intendant install] $Message" -ForegroundColor Red
    # Keep the failure on screen: when this console exists only for the
    # install, ending the script would take the diagnostics with it.
    if (-not [Console]::IsInputRedirected) {
        Read-Host "Press Enter to close" | Out-Null
    }
    # throw, never `exit`: the documented one-liner runs this script as a
    # scriptblock inside the user's own PowerShell session, where `exit`
    # terminates that whole session -- the window vanishes with every
    # line of output. A terminating error stops the install and leaves
    # the session (and the transcript) alive.
    throw "intendant install failed: $Message"
}

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

# -- Dashboard access certs + Windows certificate-store import --
# The dashboard defaults to mTLS. Generate the per-user access material
# now (setup is idempotent -- existing material is kept, never
# regenerated) and then actually run the CurrentUser imports that
# `intendant access setup` prints as copy/paste commands, instead of
# leaving them as homework. Every step degrades softly: the dashboard
# stays fully reachable certless from this machine through the per-boot
# tokened loopback URL, so a declined root-trust dialog or a missing pki
# module costs browser mTLS enrollment, never the install.
Say "generating dashboard access certificates (intendant access setup)"
& $daemonExe access setup
if ($LASTEXITCODE -ne 0) {
    Say "note: access setup did not complete -- continuing; the tokened loopback URL still grants this machine's dashboard. Re-run '$daemonExe' access setup later for browser mTLS enrollment."
} else {
    # Same resolution as the daemon's Windows access backend
    # (src/bin/caller/access/backend.rs: dirs::data_dir()/intendant/access-certs).
    $certDir = Join-Path ([Environment]::GetFolderPath("ApplicationData")) "intendant\access-certs"
    $caPath = Join-Path $certDir "ca.crt"
    $p12Path = Join-Path $certDir "client.p12"
    $passPath = Join-Path $certDir "p12_password"
    $pkiReady = (Get-Command Import-Certificate -ErrorAction SilentlyContinue) -and
        (Get-Command Import-PfxCertificate -ErrorAction SilentlyContinue)
    if ((Test-Path $caPath) -and (Test-Path $p12Path) -and (Test-Path $passPath) -and $pkiReady) {
        try {
            $caCert = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2($caPath)
            if (Test-Path ("Cert:\CurrentUser\Root\" + $caCert.Thumbprint)) {
                Say "access CA already trusted by this user -- skipping the root import"
            } else {
                Say "importing the access CA into Cert:\CurrentUser\Root -- Windows will ask you to confirm trusting it"
                Import-Certificate -FilePath $caPath -CertStoreLocation "Cert:\CurrentUser\Root" | Out-Null
            }
            $pfxPassword = ConvertTo-SecureString -String ((Get-Content -Raw -LiteralPath $passPath).Trim()) -AsPlainText -Force
            Import-PfxCertificate -FilePath $p12Path -CertStoreLocation "Cert:\CurrentUser\My" -Password $pfxPassword | Out-Null
            Remove-Variable pfxPassword
            Say "browser client identity imported (Cert:\CurrentUser\My) -- restart Edge/Chrome before relying on mTLS"
        } catch {
            Say "note: certificate import did not complete ($($_.Exception.Message)). The tokened loopback URL still works; 'intendant access setup' re-prints the manual import commands."
        }
    } else {
        Say "note: skipping the certificate-store import (generated material or the pki module is unavailable); 'intendant access setup' prints the manual commands."
    }
}

# -- User PATH --
# install.sh symlinks the binaries into ~/.local/bin; the Windows twin
# appends the release directory to the *user* PATH (no elevation needed,
# and in-place rebuilds keep working). The current session gets it too.
$releaseDir = Join-Path $InstallDir "target\release"
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (-not $userPath) { $userPath = "" }
$alreadyOnPath = $false
foreach ($entry in ($userPath -split ";")) {
    if ($entry.TrimEnd("\") -ieq $releaseDir.TrimEnd("\")) { $alreadyOnPath = $true; break }
}
if ($alreadyOnPath) {
    Say "user PATH already carries $releaseDir"
} else {
    [Environment]::SetEnvironmentVariable("Path", ($userPath.TrimEnd(";") + ";" + $releaseDir).TrimStart(";"), "User")
    Say "added $releaseDir to the user PATH -- new terminals can run plain: intendant"
}
if (($env:Path -split ";") -notcontains $releaseDir) { $env:Path = $env:Path + ";" + $releaseDir }

# -- Launcher + shortcuts --
# The dashboard URL carries a per-boot admission token, so a shortcut
# frozen at install time would go stale at the first daemon restart. The
# Desktop / Start-menu shortcuts instead run a small generated launcher:
# it (re)starts the daemon when nothing is listening, reads the fresh
# tokened URL via `ctl dashboard-url`, and prefers an Edge app-mode
# window for an installed-app feel (default browser otherwise).
$launcherDir = Join-Path $env:LOCALAPPDATA "Intendant"
New-Item -ItemType Directory -Force -Path $launcherDir | Out-Null
$launcherPath = Join-Path $launcherDir "launch-intendant.ps1"
$connectBake = ""
if ($Connect) {
    $connectBake = "`$env:INTENDANT_CONNECT_RENDEZVOUS_URL = '" + ($Connect -replace "'", "''") + "'"
    if ($DaemonId) {
        $connectBake = $connectBake + [Environment]::NewLine +
            "`$env:INTENDANT_CONNECT_DAEMON_ID = '" + ($DaemonId -replace "'", "''") + "'"
    }
}
$launcher = @'
# Generated by install.ps1. Opens the Intendant dashboard, starting the
# daemon first when it is not already running. The dashboard URL carries
# a per-boot loopback admission token, so it is read fresh on every
# click (intendant ctl dashboard-url) and never stored in this file.
$exe = '__DAEMON_EXE__'
__CONNECT_ENV__
function Get-DashboardUrl {
    $line = (& $exe ctl dashboard-url 2>$null | Select-Object -First 1)
    if ($LASTEXITCODE -ne 0 -or -not $line) { return $null }
    $url = "$line".Trim()
    # The token sidecar can outlive a dead daemon (reboot, power loss):
    # trust the URL only when its port actually accepts connections.
    $port = 8765
    if ($url -match ':(\d+)/') { $port = [int]$Matches[1] }
    $client = New-Object System.Net.Sockets.TcpClient
    try {
        if ($client.ConnectAsync("127.0.0.1", $port).Wait(1500) -and $client.Connected) { return $url }
        return $null
    } catch {
        return $null
    } finally {
        $client.Close()
    }
}
$url = Get-DashboardUrl
if (-not $url) {
    Write-Host "Starting the Intendant daemon -- its logs live in the window this opens; closing that window stops it."
    Start-Process -FilePath $exe -ArgumentList '--no-tui' -WorkingDirectory '__INSTALL_DIR__'
    foreach ($i in 1..60) {
        Start-Sleep -Seconds 2
        $url = Get-DashboardUrl
        if ($url) { break }
    }
}
if (-not $url) {
    Write-Host "The dashboard did not come up in time. Check the daemon window, then run: intendant ctl dashboard-url" -ForegroundColor Red
    if (-not [Console]::IsInputRedirected) { Read-Host "Press Enter to close" | Out-Null }
    return
}
$edgeExe = $null
$edgeCmd = Get-Command msedge.exe -ErrorAction SilentlyContinue
if ($edgeCmd) { $edgeExe = $edgeCmd.Source }
if (-not $edgeExe) {
    foreach ($root in @(${env:ProgramFiles(x86)}, $env:ProgramFiles, $env:LOCALAPPDATA)) {
        if (-not $root) { continue }
        $candidate = Join-Path $root "Microsoft\Edge\Application\msedge.exe"
        if (Test-Path $candidate) { $edgeExe = $candidate; break }
    }
}
if ($edgeExe) {
    Start-Process -FilePath $edgeExe -ArgumentList ('--app=' + $url)
} else {
    Start-Process $url
}
'@
$launcher = $launcher.Replace("__DAEMON_EXE__", ($daemonExe -replace "'", "''"))
$launcher = $launcher.Replace("__INSTALL_DIR__", ($InstallDir -replace "'", "''"))
$launcher = $launcher.Replace("__CONNECT_ENV__", $connectBake)
Set-Content -LiteralPath $launcherPath -Value $launcher -Encoding ASCII

function New-DashboardShortcut([string]$LnkPath) {
    $shell = New-Object -ComObject WScript.Shell
    $lnk = $shell.CreateShortcut($LnkPath)
    $lnk.TargetPath = "powershell.exe"
    $lnk.Arguments = "-NoProfile -ExecutionPolicy Bypass -File `"$launcherPath`""
    $lnk.WorkingDirectory = $launcherDir
    $lnk.Description = "Open the Intendant dashboard (starts the daemon when needed)"
    $lnk.IconLocation = "$daemonExe,0"
    $lnk.Save()
}
try {
    New-DashboardShortcut (Join-Path ([Environment]::GetFolderPath("Desktop")) "Intendant.lnk")
    New-DashboardShortcut (Join-Path ([Environment]::GetFolderPath("Programs")) "Intendant.lnk")
    Say "created Desktop and Start-menu shortcuts: Intendant"
} catch {
    Say "note: shortcut creation failed ($($_.Exception.Message)) -- run the launcher directly: $launcherPath"
}

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
} elseif (-not $NoRun) {
    # The daemon gets its own console window: this installer window stays
    # free to wait for readiness and report the outcome, and under the
    # one-liner the user's own session survives both. Start-Process
    # children inherit the INTENDANT_CONNECT_* env set above.
    Say "starting the daemon in its own window -- its one-time Connect code links discovery only and grants no access. Establish owner through this machine's local console or direct mTLS."
    Start-Process -FilePath $daemonExe -ArgumentList $daemonArgs -WorkingDirectory $InstallDir
}

# -- Readiness + tokened dashboard URL --
# `ctl dashboard-url` prints this boot's loopback owner URL
# (https://127.0.0.1:<port>/?token=...) once the gateway is up: certless
# and local-only by design, with the token rotating every daemon boot.
function Get-TokenedDashboardUrl {
    # Shield the probe: under $ErrorActionPreference = "Stop", Windows
    # PowerShell 5.1 can escalate redirected native stderr (the expected
    # "daemon not up yet" noise) into a terminating error.
    $prevEap = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $line = (& $daemonExe ctl dashboard-url 2>$null | Select-Object -First 1)
        if ($LASTEXITCODE -eq 0 -and $line) { return "$line".Trim() }
        return $null
    } catch {
        return $null
    } finally {
        $ErrorActionPreference = $prevEap
    }
}
$dashUrl = $null
if (-not $NoRun) {
    Say "waiting for the dashboard to come up..."
    foreach ($i in 1..60) {
        Start-Sleep -Seconds 2
        $dashUrl = Get-TokenedDashboardUrl
        if ($dashUrl) { break }
    }
    if (-not $dashUrl) {
        Say "note: the dashboard did not report ready within 120s -- check the daemon window (or the service log above); print the URL any time with: intendant ctl dashboard-url"
    }
}

# -- Done --
$stateRoot = Join-Path $HOME ".intendant"
Write-Host ""
Write-Host "============================================================"
Write-Host "  Intendant is installed"
Write-Host "============================================================"
Write-Host "  Checkout:   $InstallDir"
Write-Host "  Binary:     $daemonExe"
Write-Host "              (new terminals can run plain: intendant)"
Write-Host "  Shortcuts:  Desktop + Start menu -- 'Intendant' starts the"
Write-Host "              daemon when needed and opens the dashboard"
if ($dashUrl) {
    Write-Host "  Dashboard:  $dashUrl"
    Write-Host "              (this machine's owner URL; its token rotates each"
    Write-Host "              daemon boot -- reprint with: intendant ctl dashboard-url)"
}
if ($Service) {
    if ($NoRun) {
        Write-Host "  Daemon:     Task Scheduler entry installed, not started (-NoRun);"
        Write-Host "              it starts at the next $(if ($elevated) { "boot" } else { "logon" }). Status: intendant service status"
    } else {
        Write-Host "  Daemon:     running under Task Scheduler and restarting on"
        Write-Host "              failure (claim code / logs: the line printed above)"
    }
    Write-Host "  Logs:       the service log named above; session logs under"
    Write-Host "              $stateRoot\logs"
} else {
    if ($NoRun) {
        Write-Host "  Daemon:     not started (-NoRun). Start it with the desktop"
        Write-Host "              shortcut, or: `"$daemonExe`" $($daemonArgs -join ' ')"
    } else {
        Write-Host "  Daemon:     running in its own console window. Closing that"
        Write-Host "              window stops it; nothing restarts it at logon."
        Write-Host "              Always-on instead:  intendant service install --now"
        Write-Host "              (unelevated = logon task, elevated = at-boot service)"
    }
    Write-Host "  Logs:       the daemon window; session logs under"
    Write-Host "              $stateRoot\logs"
}
Write-Host "============================================================"
Write-Host ""

if ($dashUrl) {
    Say "opening the dashboard in your default browser..."
    try {
        Start-Process $dashUrl
    } catch {
        Say "note: no browser could be opened here -- open the Dashboard URL above yourself."
    }
}
