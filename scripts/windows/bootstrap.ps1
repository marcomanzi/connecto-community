# Prepares a Windows machine to build the Connecto workspace (this repository).
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\windows\bootstrap.ps1 -CheckOnly
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\windows\bootstrap.ps1
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\windows\bootstrap.ps1 -WithAgents
#
# -ExecutionPolicy Bypass applies to this one process only; the script never
# changes the machine or user execution policy.
#
# UNTESTED: this was written on macOS and has not been run on Windows. Run it with
# -CheckOnly first and fix the script if it misbehaves.
#
# What building the workspace on Windows needs (checked against Cargo.lock and the
# build scripts of the locked crates, not guessed):
#   - Git: source control, and Git Bash for Claude Code's Bash tool (optional).
#   - rustup with the stable MSVC toolchain, at least the rust-version in
#     Cargo.toml (1.95). The target is x86_64-pc-windows-msvc.
#   - Visual Studio 2022 Build Tools with the C++ workload (cl.exe, link.exe) and
#     a Windows SDK (kernel32.lib etc.). The MSVC linker is needed by every crate.
#     ring 0.17.14 (through iroh and rustls) is compiled with cc on MSVC.
#   - Node.js LTS only for -WithAgents, because the Codex CLI is installed with npm.
#   Not needed, and therefore not installed:
#   - NASM and Perl: ring's crates.io package ships pregenerated assembly; those
#     tools are used only for a git checkout of ring.
#   - cmake: no locked crate builds with cmake (no aws-lc-sys, openssl-sys or
#     libsqlite3-sys in Cargo.lock).
#   - clang / libclang: only bindgen (through libproc) needs it, and libproc is
#     macOS-only code that is not yet target-gated in connecto-session. Once that
#     is gated, clang is not needed on x86_64 Windows. On Windows ARM64, ring
#     compiles C with clang (ring's build.rs), so this script stops there.
#
# Exit codes: 0 done (or everything present), 1 an install or check failed,
# 2 winget is missing, 3 -CheckOnly found missing items.

[CmdletBinding()]
param(
  [switch]$CheckOnly,
  [switch]$WithAgents
)

$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$CargoToml = Join-Path $RepoRoot 'Cargo.toml'
$Target = 'x86_64-pc-windows-msvc'
$ProgramFilesX86 = ${env:ProgramFiles(x86)}
$VsWhere = Join-Path $ProgramFilesX86 'Microsoft Visual Studio\Installer\vswhere.exe'

# Winget package IDs. All five were confirmed with `winget show --id <id> -e` on
# Windows 11 on 2026-11-12. Anthropic.ClaudeCode is also the ID on Anthropic's
# official Claude Code install page.
$WingetGit = 'Git.Git'
$WingetRustup = 'Rustlang.Rustup'
$WingetVsBuildTools = 'Microsoft.VisualStudio.2022.BuildTools'
$WingetNode = 'OpenJS.NodeJS.LTS'
$WingetClaude = 'Anthropic.ClaudeCode'

$script:Missing = @()
$script:Installed = $false

function Write-Step([string]$Text) { Write-Host ''; Write-Host "==> $Text" }
function Write-Info([string]$Text) { Write-Host "    $Text" }
function Write-Note([string]$Text) { Write-Host "    NOTE: $Text" -ForegroundColor Yellow }

function Stop-Bootstrap([string]$Message, [int]$Code = 1) {
  Write-Host ''
  Write-Host "FAILED: $Message" -ForegroundColor Red
  exit $Code
}

function Test-Tool([string]$Name) {
  return [bool](Get-Command $Name -ErrorAction SilentlyContinue)
}

# Records an item as missing unless it is present now. Call it after the install
# attempt, so an item that was just installed is not reported as still missing.
function Add-MissingUnless([string]$Label, [bool]$Present) {
  if (-not $Present) { $script:Missing += $Label }
}

# Runs a native tool and returns its stdout lines, or $null when it is missing or fails.
# ErrorActionPreference is relaxed for the call: on Windows PowerShell 5.1 a native
# command's stderr output becomes an error that 'Stop' would turn into an exception.
function Get-NativeOutput([string]$Exe, [string[]]$Arguments) {
  if (-not (Test-Tool $Exe)) { return $null }
  $saved = $ErrorActionPreference
  $ErrorActionPreference = 'Continue'
  try {
    $out = & $Exe @Arguments 2>$null
    if ($LASTEXITCODE -ne 0) { return $null }
    # The leading comma stops PowerShell from unrolling a one-line result into a
    # plain string, which would make $result[0] the first character.
    return ,@($out)
  } catch {
    return $null
  } finally {
    $ErrorActionPreference = $saved
  }
}

# Rebuilds PATH from the machine and user values so tools installed by winget are
# found in this session. A new shell is still the safe way to pick up everything.
function Update-SessionPath {
  $machine = [Environment]::GetEnvironmentVariable('Path', 'Machine')
  $user = [Environment]::GetEnvironmentVariable('Path', 'User')
  $cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
  $env:Path = "$cargoBin;$machine;$user"
}

function Test-Admin {
  $id = [Security.Principal.WindowsIdentity]::GetCurrent()
  return (New-Object Security.Principal.WindowsPrincipal $id).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Invoke-WingetInstall([string]$Id, [string]$Label, [string]$Override = '') {
  if (-not (Test-Tool 'winget')) {
    Stop-Bootstrap "winget is not available. Install or update 'App Installer' from the Microsoft Store, then run this again." 2
  }
  Write-Info "installing $Label ($Id) with winget"
  $wingetArgs = @('install', '--id', $Id, '-e', '--silent',
    '--accept-package-agreements', '--accept-source-agreements')
  if ($Override) { $wingetArgs += @('--override', $Override) }
  & winget @wingetArgs
  if ($LASTEXITCODE -ne 0) {
    Stop-Bootstrap "winget could not install $Label (exit $LASTEXITCODE). Try 'winget install --id $Id -e' by hand to see why."
  }
  $script:Installed = $true
  Update-SessionPath
}

function Get-RustPin {
  foreach ($line in Get-Content $CargoToml) {
    if ($line -match '^rust-version\s*=\s*"([^"]+)"') { return $Matches[1] }
  }
  return $null
}

function Get-VsInstallPath {
  if (-not (Test-Path $VsWhere)) { return $null }
  $found = Get-NativeOutput $VsWhere @('-latest', '-products', '*',
    '-requires', 'Microsoft.VisualStudio.Component.VC.Tools.x86.x64',
    '-property', 'installationPath')
  if ($found) { return ($found | Select-Object -First 1) }
  return $null
}

function Get-ClPath([string]$VsPath) {
  $msvc = Join-Path $VsPath 'VC\Tools\MSVC'
  if (-not (Test-Path $msvc)) { return $null }
  return Get-ChildItem $msvc -Directory | ForEach-Object {
    Join-Path $_.FullName 'bin\Hostx64\x64\cl.exe'
  } | Where-Object { Test-Path $_ } | Select-Object -Last 1
}

function Test-WindowsSdk {
  $lib = Join-Path $ProgramFilesX86 'Windows Kits\10\Lib'
  if (-not (Test-Path $lib)) { return $null }
  $sdk = Get-ChildItem $lib -Directory | Where-Object {
    Test-Path (Join-Path $_.FullName 'um\x64\kernel32.lib')
  } | Select-Object -Last 1
  if ($sdk) { return $sdk.Name }
  return $null
}

# ---------------------------------------------------------------- start

Write-Step 'Connecto Windows bootstrap'
Write-Info "repository: $RepoRoot"
Write-Info ("mode: " + $(if ($CheckOnly) { 'check only, nothing is installed' } else { 'check and install' }))
if ($WithAgents) { Write-Info 'agents: Claude Code and Codex CLI will be installed too' }

if (-not $env:OS -or $env:OS -ne 'Windows_NT') {
  Stop-Bootstrap 'this script runs on Windows only.'
}

# A 32-bit PowerShell on 64-bit Windows reports the real machine in ..._ARCHITEW6432.
$arch = $env:PROCESSOR_ARCHITECTURE
if ($env:PROCESSOR_ARCHITEW6432) { $arch = $env:PROCESSOR_ARCHITEW6432 }
if ($arch -ne 'AMD64') {
  Stop-Bootstrap "processor architecture is $arch. Only x86_64 (AMD64) is covered. On ARM64 ring needs clang, which this script does not install." 1
}

Write-Step 'Administrator rights'
if (Test-Admin) {
  Write-Info 'this shell is elevated'
} else {
  Write-Note 'this shell is NOT elevated. Visual Studio Build Tools and machine-wide installs show a UAC prompt; accept it, or re-run from an elevated PowerShell.'
}

Write-Step 'Git'
$gitOut = Get-NativeOutput 'git' @('--version')
if ($gitOut) { Write-Info "present: $($gitOut[0])" } else {
  Write-Info 'missing'
  if (-not $CheckOnly) { Invoke-WingetInstall $WingetGit 'Git for Windows' }
  Add-MissingUnless 'Git' (Test-Tool 'git')
}

Write-Step 'Visual Studio 2022 Build Tools (C++ workload)'
$vsPath = Get-VsInstallPath
$clPath = $null
if ($vsPath) { $clPath = Get-ClPath $vsPath }
if ($clPath) {
  Write-Info "present: cl.exe at $clPath"
} else {
  Write-Info 'missing (no MSVC x64 toolset found by vswhere)'
  # In install mode a failure below stops the script, so this is recorded for -CheckOnly only.
  if ($CheckOnly) { $script:Missing += 'Visual Studio Build Tools' }
  if (-not $CheckOnly) {
    # VCTools is the C++ workload; --includeRecommended brings the Windows SDK.
    # Workload and option names are from Microsoft's Build Tools docs as remembered:
    # TODO check them on the machine if the install does not add a Windows SDK.
    Invoke-WingetInstall $WingetVsBuildTools 'Visual Studio 2022 Build Tools' `
      '--passive --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended'
    $vsPath = Get-VsInstallPath
    $clPath = if ($vsPath) { Get-ClPath $vsPath } else { $null }
    if (-not $clPath) {
      Stop-Bootstrap 'Build Tools installed but cl.exe is not found. Open the Visual Studio Installer and check the C++ build tools workload, then run this again.'
    }
  }
}

Write-Step 'Windows SDK'
$sdkVersion = Test-WindowsSdk
if ($sdkVersion) { Write-Info "present: Windows Kits 10 Lib $sdkVersion" } else {
  Write-Info 'missing (no Windows Kits 10 um\x64\kernel32.lib)'
  $script:Missing += 'Windows SDK'
  if (-not $CheckOnly) {
    Write-Note 'The Windows SDK comes with the Build Tools workload above. Add it in the Visual Studio Installer (Individual components, Windows 11 SDK) and run this again.'
    Stop-Bootstrap 'Windows SDK not found.'
  }
}

Write-Step 'Rust (rustup, MSVC toolchain)'
$pin = Get-RustPin
Write-Info "repository rust-version: $pin"
if (-not (Test-Tool 'rustup')) {
  Write-Info 'rustup missing'
  if (-not $CheckOnly) { Invoke-WingetInstall $WingetRustup 'rustup' }
  Add-MissingUnless 'rustup' (Test-Tool 'rustup')
}
if (Test-Tool 'rustup') {
  $active = Get-NativeOutput 'rustup' @('show', 'active-toolchain')
  $hasMsvc = $active -and ($active -match 'x86_64-pc-windows-msvc')
  if (-not $hasMsvc -and -not $CheckOnly) {
    Write-Info "installing the stable $Target toolchain"
    & rustup toolchain install "stable-$Target" --profile minimal -c clippy -c rustfmt
    if ($LASTEXITCODE -ne 0) { Stop-Bootstrap 'rustup could not install the stable MSVC toolchain. Check the network and run this again.' }
    & rustup default "stable-$Target"
    if ($LASTEXITCODE -ne 0) { Stop-Bootstrap 'rustup default failed.' }
    $script:Installed = $true
  } elseif (-not $hasMsvc) {
    $script:Missing += 'stable MSVC toolchain'
  }
}
Update-SessionPath
$rustcLine = Get-NativeOutput 'rustc' @('-V')
$rustcVer = $null
if ($rustcLine -and ($rustcLine[0] -match 'rustc (\d+\.\d+(\.\d+)?)')) { $rustcVer = $Matches[1] }
$hostLine = Get-NativeOutput 'rustc' @('-vV')
$hostTriple = $null
if ($hostLine) { foreach ($l in $hostLine) { if ($l -match '^host:\s*(\S+)') { $hostTriple = $Matches[1] } } }
if ($rustcVer) {
  Write-Info "rustc $rustcVer, host $hostTriple"
  if ($pin -and ([version]$rustcVer -lt [version]$pin)) {
    if (-not $CheckOnly -and (Test-Tool 'rustup')) {
      Write-Info "rustc is older than $pin; updating the stable toolchain"
      & rustup update "stable-$Target"
      if ($LASTEXITCODE -ne 0) { Stop-Bootstrap 'rustup update failed.' }
      Update-SessionPath
    } else {
      $script:Missing += "rustc >= $pin"
    }
  }
  if ($hostTriple -and $hostTriple -ne $Target) {
    Write-Note "the default host is $hostTriple, not $Target. Run 'rustup default stable-$Target'."
  }
} else {
  Write-Info 'rustc not found on PATH'
  if (-not $CheckOnly) { Write-Note 'Open a new PowerShell window so PATH is refreshed, then run this again.' }
  $script:Missing += 'rustc'
}

Write-Step 'Node.js LTS (only for -WithAgents, for the Codex CLI)'
if ($WithAgents) {
  $nodeOut = Get-NativeOutput 'node' @('--version')
  if ($nodeOut) { Write-Info "present: node $($nodeOut[0])" } else {
    Write-Info 'missing'
    if (-not $CheckOnly) { Invoke-WingetInstall $WingetNode 'Node.js LTS' }
    Add-MissingUnless 'Node.js' (Test-Tool 'node')
  }
} else {
  Write-Info 'skipped (not needed to build)'
}

Write-Step 'Agent CLIs (only with -WithAgents)'
if ($WithAgents) {
  $claudeOut = Get-NativeOutput 'claude' @('--version')
  if ($claudeOut) { Write-Info "present: $($claudeOut[0])" } else {
    Write-Info 'Claude Code missing'
    if ($CheckOnly) { $script:Missing += 'Claude Code' }
    if (-not $CheckOnly) {
      # Official route (code.claude.com/docs, Windows tab): WinGet, or the PowerShell
      # installer `irm https://claude.ai/install.ps1 | iex`. WinGet is used here.
      # Claude Code uses Git Bash for its Bash tool when Git for Windows is present.
      Invoke-WingetInstall $WingetClaude 'Claude Code'
      Add-MissingUnless 'Claude Code' (Test-Tool 'claude')
    }
  }
  $codexOut = Get-NativeOutput 'codex' @('--version')
  if ($codexOut) { Write-Info "present: $($codexOut[0])" } else {
    Write-Info 'Codex CLI missing'
    if ($CheckOnly) { $script:Missing += 'Codex CLI' }
    if (-not $CheckOnly) {
      # Official npm route from the Codex CLI README: npm install -g @openai/codex.
      # npm.cmd is called directly so the npm.ps1 shim and execution policy are not used.
      # TODO (Windows session): the README has a separate PowerShell installer
      # (chatgpt.com/codex/install.ps1). It was not used here: it needs an execution
      # policy change. Also the README does not say whether Codex is supported natively
      # on Windows (WSL is the alternative). Check both before relying on it.
      if (-not (Test-Tool 'npm')) {
        Stop-Bootstrap 'npm is not on PATH. Open a new PowerShell window (Node.js was just installed) and run this again.'
      }
      Write-Info 'installing Codex CLI with npm (npm install -g @openai/codex)'
      & npm.cmd install -g '@openai/codex'
      if ($LASTEXITCODE -ne 0) { Stop-Bootstrap 'npm install -g @openai/codex failed.' }
      $script:Installed = $true
      Update-SessionPath
      Add-MissingUnless 'Codex CLI' (Test-Tool 'codex')
    }
  }
} else {
  Write-Info 'skipped (run with -WithAgents to install)'
}

Write-Step 'Verification'
$verifyCommands = @(
  @{ Label = 'rustc -vV';      Exe = 'rustc'; Args = @('-vV') },
  @{ Label = 'cargo -V';       Exe = 'cargo'; Args = @('-V') },
  @{ Label = 'git --version';  Exe = 'git';   Args = @('--version') }
)
foreach ($check in $verifyCommands) {
  $out = Get-NativeOutput $check.Exe $check.Args
  if ($out) { Write-Info ($check.Label + ' -> ' + $out[0]) } else { Write-Info ($check.Label + ' -> NOT AVAILABLE') }
}
$vsNow = Get-VsInstallPath
$clNow = if ($vsNow) { Get-ClPath $vsNow } else { $null }
if ($clNow) { Write-Info "cl.exe (vswhere) -> $clNow" } else { Write-Info 'cl.exe (vswhere) -> NOT AVAILABLE' }
Write-Info 'cl.exe is only on PATH inside a Developer shell; cargo finds it through its own MSVC lookup.'

Write-Host ''
if ($script:Installed) {
  Write-Note 'Something was installed. Open a NEW PowerShell window (PATH changed), then run this script with -CheckOnly again.'
}
if ($CheckOnly -and $script:Missing.Count -gt 0) {
  Write-Host ('MISSING: ' + ($script:Missing -join ', ')) -ForegroundColor Yellow
  Write-Host 'Run this script without -CheckOnly to install them.'
  exit 3
}
if ($script:Missing.Count -gt 0) {
  Write-Host ('STILL MISSING: ' + ($script:Missing -join ', ')) -ForegroundColor Red
  Write-Host 'Open a new PowerShell window and run it again.'
  exit 1
}
Write-Host 'All required tools are present. Next: cargo check --workspace.' -ForegroundColor Green
exit 0
