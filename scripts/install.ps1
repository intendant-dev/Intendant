<#
.SYNOPSIS
    Intendant bootstrap installer for Windows — the install.sh counterpart.
    Served by every Intendant Connect rendezvous at /install.ps1.

.DESCRIPTION
    Stands up a daemon that is OWNED from first boot and holds no secrets:
      1. -Owner pins root authority to your browser identity key (the
         fingerprint is public — shown in the dashboard's Access drawer).
      2. The daemon prints its claim phrase; claim it from the browser you
         are already holding.
      3. The first dashboard session fuels it with credential leases from
         your vault. Nothing sensitive ever appears on this machine's disk,
         in this command, or on the wire.

    One-liner (PowerShell):
      & ([scriptblock]::Create((irm https://intendant.dev/install.ps1))) -Owner <your-key>

    Dependencies (git, rustup, VS Build Tools, NASM) are handled by
    scripts/setup-windows.ps1 from the cloned repo — run automatically
    when this shell is elevated, otherwise checked and reported.

.PARAMETER Owner
    Client-key fingerprint to pin root authority to from first boot.

.PARAMETER Connect
    Rendezvous URL to register with.

.PARAMETER DaemonId
    Stable daemon id at the rendezvous.

.PARAMETER Service
    Keep the daemon running unattended: installs a Task Scheduler entry
    via `intendant service install` (at boot when elevated, at logon
    otherwise) supervised by the built-in restart loop; the claim phrase
    lands in the service log the installer prints.

.PARAMETER NoRun
    Build and link only; print how to start it.

.PARAMETER Repo
    Git URL to clone (default: https://github.com/lovon-spec/intendant).

.PARAMETER InstallDir
    Checkout directory (default: $HOME\intendant).
#>
[CmdletBinding()]
param(
    [string]$Owner = "",
    [string]$Connect = "",
    [string]$DaemonId = "",
    [switch]$Service,
    [switch]$NoRun,
    [string]$Repo = "https://github.com/lovon-spec/intendant",
    [string]$InstallDir = (Join-Path $HOME "intendant")
)

$ErrorActionPreference = "Stop"

function Say([string]$Message) { Write-Host "[intendant install] $Message" -ForegroundColor White }
function Fail([string]$Message) { Write-Host "[intendant install] $Message" -ForegroundColor Red; exit 1 }

if (-not $Owner) {
    Say "note: no -Owner given — the daemon will start unowned; pass your client-key fingerprint (Access drawer) to own it from first boot."
}

$elevated = ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()
    ).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)

# ── Toolchain ──
if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
    Fail "git is required. Install it (winget install Git.Git) and re-run — or run scripts\setup-windows.ps1 from an elevated shell after cloning $Repo."
}

# ── Source ──
if (Test-Path (Join-Path $InstallDir ".git")) {
    Say "using existing checkout at $InstallDir (leaving it exactly as-is)"
} else {
    Say "cloning $Repo -> $InstallDir"
    git clone --depth 1 $Repo $InstallDir
    if ($LASTEXITCODE -ne 0) { Fail "git clone failed" }
}
Set-Location $InstallDir

# ── System dependencies ──
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
    Say "note: unelevated shell — skipping dependency setup; if the build fails on a missing native dep, run scripts\setup-windows.ps1 from an elevated PowerShell."
}

# ── Build ──
Say "building release binaries (this takes a few minutes on a fresh box)"
cargo build --release
if ($LASTEXITCODE -ne 0) { Fail "cargo build failed" }
$daemonExe = Join-Path $InstallDir "target\release\intendant.exe"

# ── Launch ──
$daemonArgs = @("--no-tui")
if ($Owner) { $daemonArgs += @("--owner", $Owner) }
if ($Connect) {
    $env:INTENDANT_CONNECT_RENDEZVOUS_URL = $Connect
    if ($DaemonId) { $env:INTENDANT_CONNECT_DAEMON_ID = $DaemonId }
    Say "rendezvous: $Connect"
} else {
    Say "note: no -Connect rendezvous URL — hosted claiming needs one (the daemon still serves its local dashboard)."
}

if ($Service) {
    # `service install` writes the Task Scheduler definition, captures the
    # INTENDANT_CONNECT_* env set above, and prints where the claim phrase
    # lands (the built-in supervisor's log file).
    if (-not $elevated) {
        Say "note: unelevated — the task starts at logon; re-run elevated for an at-boot service."
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
    Say "starting the daemon — it will print its claim phrase; claim it from your browser, then fuel it from the vault."
    & $daemonExe @daemonArgs
    exit $LASTEXITCODE
}
