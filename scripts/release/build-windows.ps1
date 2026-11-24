# Build Connecto for Windows into dist\connecto-VERSION-windows-x86_64.zip.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\release\build-windows.ps1
#
# Run it on Windows x86_64 with the MSVC Rust toolchain (scripts\windows\bootstrap.ps1
# sets a machine up; releases use build-windows-cross.sh instead). Works in Windows
# PowerShell 5.1 and PowerShell 7. See README.md for what is different on Windows.
#
# Output (VERSION is the workspace version):
#   dist\connecto-VERSION-windows-x86_64.zip
#   dist\SHA256SUMS   one line per artifact, the format `shasum -a 256 -c` reads
#                     (lowercase hex, two spaces, name, LF; no BOM)
#
# Other variables:
#   CONNECTO_DIST      output folder (default .\dist)
#   CONNECTO_VERSION   overrides the version for a one-off build
#   CARGO_TARGET_DIR   honoured (as cargo does) when the programs are picked up
#
# The relay access token built into the app is required (see README.md):
#   CONNECTO_RELAY_TOKEN       the token, or
#   scripts\release\relay-token.local   a file holding it (untracked)
#   CONNECTO_NO_RELAY_TOKEN=1  build on purpose without one (no built-in access)
#
# Authenticode signing (optional; see README.md, "Signing (Windows)"). Without any
# of these the build is UNSIGNED and SmartScreen warns on the first start:
#   Azure Artifact Signing (formerly Trusted Signing):
#     CONNECTO_WIN_ARTIFACT_SIGNING_ENDPOINT  e.g. https://weu.codesigning.azure.net
#     CONNECTO_WIN_ARTIFACT_SIGNING_ACCOUNT   the Artifact Signing account name
#     CONNECTO_WIN_ARTIFACT_SIGNING_PROFILE   the certificate profile name
#     CONNECTO_WIN_ARTIFACT_SIGNING_DLIB      path to the x64 Azure.CodeSigning.Dlib.dll
#     (Azure sign-in: AZURE_TENANT_ID, AZURE_CLIENT_ID, AZURE_CLIENT_SECRET, or any
#      other credential DefaultAzureCredential finds)
#   A certificate in the Windows certificate store (OV or EV, on a USB token or a
#   cloud HSM whose key storage provider is installed):
#     CONNECTO_WIN_CERT_SHA1       the certificate's thumbprint
#   A .pfx file (only for certificates whose key may live in a file):
#     CONNECTO_WIN_PFX, CONNECTO_WIN_PFX_PASSWORD
#   Common:
#     CONNECTO_WIN_TIMESTAMP_URL   RFC 3161 timestamp server (default for Artifact
#                                  Signing: http://timestamp.acs.microsoft.com;
#                                  required with a certificate: use your CA's)
#     CONNECTO_SIGNTOOL            signtool.exe to use (default: the newest x64 one
#                                  in the Windows 10/11 SDK)
#     CONNECTO_WIN_REQUIRE_SIGNED=1  fail instead of building unsigned (releases)
$ErrorActionPreference = 'Stop'
$Root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$Dist = if ($env:CONNECTO_DIST) { $env:CONNECTO_DIST } else { Join-Path $Root 'dist' }

# The workspace version in Cargo.toml ([workspace.package]).
$inPkg = $false; $Version = $null
foreach ($line in Get-Content (Join-Path $Root 'Cargo.toml')) {
  if ($line -match '^\[workspace\.package\]') { $inPkg = $true; continue }
  if ($line -match '^\[') { $inPkg = $false }
  if ($inPkg -and $line -match '^version\s*=\s*"([^"]+)"') { $Version = $Matches[1]; break }
}
if ($env:CONNECTO_VERSION) { $Version = $env:CONNECTO_VERSION }
if (-not $Version) { throw 'could not read the workspace version from Cargo.toml' }

# The relay access token built into the app (the app reads CONNECTO_RELAY_TOKEN at
# compile time). From CONNECTO_RELAY_TOKEN, or else from relay-token.local beside
# this script (untracked, git-ignored). A release build fails without one.
# CONNECTO_NO_RELAY_TOKEN=1 builds on purpose without one. The value is never printed.
if ($env:CONNECTO_NO_RELAY_TOKEN -eq '1') {
  Remove-Item Env:CONNECTO_RELAY_TOKEN -ErrorAction SilentlyContinue
  Write-Host '==> CONNECTO_NO_RELAY_TOKEN=1: this build has no built-in relay access token'
} else {
  $TokenFile = Join-Path $PSScriptRoot 'relay-token.local'
  if (-not $env:CONNECTO_RELAY_TOKEN -and (Test-Path $TokenFile)) {
    $env:CONNECTO_RELAY_TOKEN = ([string](Get-Content -Raw $TokenFile)) -replace '\s', ''
  }
  if (-not $env:CONNECTO_RELAY_TOKEN) {
    throw 'no relay access token: set CONNECTO_RELAY_TOKEN, or put it in scripts\release\relay-token.local (untracked). To build on purpose without one, set CONNECTO_NO_RELAY_TOKEN=1.'
  }
  Write-Host '==> built-in relay access token: set'
}

# Run a native program with its output shown as plain lines. Windows PowerShell
# 5.1 turns each stderr line of a native program (cargo's progress) into an error
# record, which stops the script under ErrorActionPreference Stop when the output
# is redirected (as in CI), so Continue applies to this one call. Returns the
# exit code.
function Invoke-Shown([string]$Program, [string[]]$Arguments) {
  $previous = $ErrorActionPreference
  $ErrorActionPreference = 'Continue'
  try {
    & $Program @Arguments 2>&1 | ForEach-Object { Write-Host "$_" }
  } finally {
    $ErrorActionPreference = $previous
  }
  $LASTEXITCODE
}

# How to sign, decided before the long build so a bad setup fails at once.
$Sign = $null
if ($env:CONNECTO_WIN_ARTIFACT_SIGNING_ENDPOINT -or $env:CONNECTO_WIN_ARTIFACT_SIGNING_ACCOUNT -or $env:CONNECTO_WIN_ARTIFACT_SIGNING_PROFILE) {
  foreach ($v in 'CONNECTO_WIN_ARTIFACT_SIGNING_ENDPOINT', 'CONNECTO_WIN_ARTIFACT_SIGNING_ACCOUNT', 'CONNECTO_WIN_ARTIFACT_SIGNING_PROFILE', 'CONNECTO_WIN_ARTIFACT_SIGNING_DLIB') {
    if (-not [Environment]::GetEnvironmentVariable($v)) { throw "Artifact Signing needs $v as well" }
  }
  if (-not (Test-Path $env:CONNECTO_WIN_ARTIFACT_SIGNING_DLIB)) { throw "CONNECTO_WIN_ARTIFACT_SIGNING_DLIB not found: $env:CONNECTO_WIN_ARTIFACT_SIGNING_DLIB" }
  $Sign = 'artifact'
} elseif ($env:CONNECTO_WIN_CERT_SHA1) {
  $Sign = 'store'
} elseif ($env:CONNECTO_WIN_PFX) {
  if (-not (Test-Path $env:CONNECTO_WIN_PFX)) { throw "CONNECTO_WIN_PFX not found: $env:CONNECTO_WIN_PFX" }
  $Sign = 'pfx'
}
if (-not $Sign -and $env:CONNECTO_WIN_REQUIRE_SIGNED -eq '1') {
  throw 'CONNECTO_WIN_REQUIRE_SIGNED=1 but no signing is set up (see the header of this script)'
}
$Timestamp = $env:CONNECTO_WIN_TIMESTAMP_URL
if ($Sign -eq 'artifact' -and -not $Timestamp) { $Timestamp = 'http://timestamp.acs.microsoft.com' }
if ($Sign -and -not $Timestamp) { throw 'signing with a certificate needs CONNECTO_WIN_TIMESTAMP_URL (your CA names its timestamp server)' }
$SignTool = $null
if ($Sign) {
  if ($env:CONNECTO_SIGNTOOL) {
    $SignTool = $env:CONNECTO_SIGNTOOL
  } else {
    # The newest x64 signtool of the installed Windows 10/11 SDKs.
    $Kits = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
    $found = @(Get-ChildItem -Path $Kits -Directory -ErrorAction SilentlyContinue |
      Where-Object { $_.Name -match '^10\.' -and (Test-Path (Join-Path $_.FullName 'x64\signtool.exe')) } |
      Sort-Object { [version]$_.Name } -Descending)
    if ($found.Count -gt 0) { $SignTool = Join-Path $found[0].FullName 'x64\signtool.exe' }
  }
  if (-not $SignTool -or -not (Test-Path $SignTool)) { throw 'signtool.exe not found: install the Windows SDK or set CONNECTO_SIGNTOOL' }
  Write-Host "==> signing: $Sign, with $SignTool"
}

# Sign one program and check the signature. Nothing secret is printed: the
# password goes to signtool only, and its command line is not echoed.
function Invoke-Sign([string]$File) {
  $signArgs = @('sign', '/fd', 'sha256', '/tr', $Timestamp, '/td', 'sha256', '/d', 'Connecto', '/du', 'https://connecto.studio')
  switch ($Sign) {
    'artifact' {
      $meta = Join-Path $Dist 'work\artifact-signing.json'
      $json = @{ Endpoint = $env:CONNECTO_WIN_ARTIFACT_SIGNING_ENDPOINT; CodeSigningAccountName = $env:CONNECTO_WIN_ARTIFACT_SIGNING_ACCOUNT; CertificateProfileName = $env:CONNECTO_WIN_ARTIFACT_SIGNING_PROFILE } | ConvertTo-Json
      [IO.File]::WriteAllText($meta, $json, (New-Object Text.UTF8Encoding($false)))
      $signArgs += @('/dlib', $env:CONNECTO_WIN_ARTIFACT_SIGNING_DLIB, '/dmdf', $meta)
    }
    'store' { $signArgs += @('/sha1', $env:CONNECTO_WIN_CERT_SHA1) }
    'pfx' {
      $signArgs += @('/f', $env:CONNECTO_WIN_PFX)
      if ($env:CONNECTO_WIN_PFX_PASSWORD) { $signArgs += @('/p', $env:CONNECTO_WIN_PFX_PASSWORD) }
    }
  }
  $signArgs += $File
  $code = Invoke-Shown $SignTool $signArgs
  if ($code -ne 0) { throw "signtool sign failed for $(Split-Path -Leaf $File) (exit code $code)" }
  $code = Invoke-Shown $SignTool @('verify', '/pa', '/tw', $File)
  if ($code -ne 0) { throw "the signature of $(Split-Path -Leaf $File) does not verify (exit code $code)" }
}

$Target = 'x86_64-pc-windows-msvc'
$Name = "connecto-$Version-windows-x86_64"

if (Get-Command rustup -ErrorAction SilentlyContinue) {
  $previous = $ErrorActionPreference
  $ErrorActionPreference = 'Continue'
  try { $installed = @(& rustup target list --installed 2>$null) } finally { $ErrorActionPreference = $previous }
  if ($installed -notcontains $Target) { throw "missing Rust target: rustup target add $Target" }
}

Push-Location $Root
try {
  # The shipped programs must not turn on a feature that disables TLS checks
  # (connecto-net's test-relay, or iroh's test-utils). Same check as
  # scripts/release/check-release-features.sh. Only stdout is read (see Invoke-Shown).
  $previous = $ErrorActionPreference
  $ErrorActionPreference = 'Continue'
  try {
    $tree = (& cargo tree --edges features --locked --target $Target -p connecto-app -p connecto-hook 2>$null | Out-String)
  } finally {
    $ErrorActionPreference = $previous
  }
  if ($LASTEXITCODE -ne 0) { throw "cargo tree failed for $Target (exit code $LASTEXITCODE)" }
  if ($tree -match 'feature "(test-relay|test-utils)"') {
    throw "the release feature graph for $Target turns on a TLS-off feature: $($Matches[0])"
  }
  Write-Host "==> release feature graph for $Target has no test-relay or test-utils"
  Write-Host "==> cargo build --release --locked --target $Target"
  $code = Invoke-Shown 'cargo' @('build', '--release', '--locked', '--target', $Target, '-p', 'connecto-app', '-p', 'connecto-hook')
  if ($code -ne 0) { throw "cargo build failed (exit code $code)" }
} finally { Pop-Location }

# Where cargo put the programs: CARGO_TARGET_DIR (relative to the workspace, where
# cargo ran) or target\.
$TargetDir = Join-Path $Root 'target'
if ($env:CARGO_TARGET_DIR) {
  $TargetDir = if ([IO.Path]::IsPathRooted($env:CARGO_TARGET_DIR)) { $env:CARGO_TARGET_DIR } else { Join-Path $Root $env:CARGO_TARGET_DIR }
}
$Built = Join-Path $TargetDir "$Target\release"

New-Item -ItemType Directory -Force -Path $Dist | Out-Null
$Work = Join-Path $Dist "work\$Name"
if (Test-Path $Work) { Remove-Item -Recurse -Force $Work }
New-Item -ItemType Directory -Force -Path $Work | Out-Null
Copy-Item (Join-Path $Built 'connecto-app.exe') $Work
Copy-Item (Join-Path $Built 'connecto-hook.exe') $Work
if ($Sign) {
  Invoke-Sign (Join-Path $Work 'connecto-app.exe')
  Invoke-Sign (Join-Path $Work 'connecto-hook.exe')
  $meta = Join-Path $Dist 'work\artifact-signing.json'
  if (Test-Path $meta) { Remove-Item -Force $meta }
}
# The `connecto` command is the helper under another name (copied after signing,
# so it carries the same signature).
Copy-Item (Join-Path $Work 'connecto-hook.exe') (Join-Path $Work 'connecto.exe')

# README.txt: plain ASCII with CRLF, so every Windows editor shows it as written.
$Readme = (@(
  "Connecto $Version for Windows (x86_64)"
  'https://connecto.studio'
  ''
  'Files (keep them together in one folder):'
  '  connecto-app.exe   the desktop app: start this one'
  '  connecto-hook.exe  the helper for agent terminals; the app copies it into its'
  '                     data folder (%APPDATA%\connecto\cli\bin) when it starts'
  '  connecto.exe       the same helper under the name of the `connecto` command,'
  "                     which works inside Connecto's terminals"
  ''
  'Requirements: Windows 10 version 1809 or newer, or Windows 11, on x86_64 (64-bit'
  'Intel or AMD). ARM64 Windows is not supported.'
  ''
  'Install: unzip into %LOCALAPPDATA%\Programs, so the programs are in'
  '%LOCALAPPDATA%\Programs\Connecto, and start connecto-app.exe there. Connecto'
  'updates itself in that folder (it keeps the previous version beside it as'
  'Connecto.previous); a copy started from elsewhere installs its first update'
  'there.'
) + $(if ($Sign) { @(
  'The programs are signed. Until a new release has built up its reputation,'
  'Windows SmartScreen may still warn on the first start ("Windows protected your'
  'PC"): choose "More info", then "Run anyway".'
) } else { @(
  'This build is UNSIGNED, so Windows SmartScreen warns'
  'on the first start ("Windows protected your PC"): choose "More info", then'
  '"Run anyway". (Or, before unzipping: right-click the zip, Properties, tick'
  '"Unblock".)'
) }) + @(
  'Setup then finds Claude Code and Codex, or installs them with their'
  'official installers. PowerShell 7 is optional (setup offers it); without it the'
  'terminals use Windows PowerShell 5.1.'
  ''
  'Data: %APPDATA%\connecto (settings, projects, the helper, and the keys and'
  'tokens in secrets\secrets.json). To remove Connecto, delete'
  '%LOCALAPPDATA%\Programs\Connecto and %APPDATA%\connecto.'
)) -join "`r`n"
[IO.File]::WriteAllText((Join-Path $Work 'README.txt'), $Readme + "`r`n", [Text.Encoding]::ASCII)

# The zip, one top folder `Connecto`, so unzipping into %LOCALAPPDATA%\Programs
# gives the install folder the updater uses. Entries use `/` (Compress-Archive in
# Windows PowerShell 5.1 writes `\`, which other systems' unzip tools get wrong).
Add-Type -AssemblyName System.IO.Compression, System.IO.Compression.FileSystem
$Zip = Join-Path $Dist "$Name.zip"
if (Test-Path $Zip) { Remove-Item -Force $Zip }
$archive = [IO.Compression.ZipFile]::Open($Zip, [IO.Compression.ZipArchiveMode]::Create)
try {
  foreach ($file in @('connecto-app.exe', 'connecto-hook.exe', 'connecto.exe', 'README.txt')) {
    [void][IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
      $archive, (Join-Path $Work $file), "Connecto/$file", [IO.Compression.CompressionLevel]::Optimal)
  }
} finally { $archive.Dispose() }
Remove-Item -Recurse -Force $Work

# dist\SHA256SUMS: replace this zip's line, keep the others, sort by name, LF, no
# BOM (what common.sh's record_checksum writes, so `shasum -a 256 -c` works).
$Hash = (Get-FileHash -Algorithm SHA256 $Zip).Hash.ToLowerInvariant()
$Sums = Join-Path $Dist 'SHA256SUMS'
$Lines = New-Object System.Collections.Generic.List[string]
if (Test-Path $Sums) {
  foreach ($l in [IO.File]::ReadAllLines($Sums)) {
    $l = $l.TrimEnd("`r")
    if ($l -eq '' -or $l.EndsWith("  $Name.zip")) { continue }
    if (-not $Lines.Contains($l)) { $Lines.Add($l) }
  }
}
$Lines.Add("$Hash  $Name.zip")
$Sorted = $Lines.ToArray()
$Keys = [string[]]($Sorted | ForEach-Object { ($_ -split '  ', 2)[-1] })
[Array]::Sort($Keys, $Sorted, [StringComparer]::Ordinal)
[IO.File]::WriteAllText($Sums, (($Sorted -join "`n") + "`n"), (New-Object Text.UTF8Encoding($false)))

Write-Host ''
Write-Host "built:      $Zip"
Write-Host "size:       $((Get-Item $Zip).Length) bytes"
Write-Host "sha256:     $Hash  $Name.zip"
if ($Sign) {
  Write-Host "signed:     yes ($Sign)"
} else {
  Write-Host 'signed:     no'
  Write-Host 'This build is UNSIGNED (no Authenticode signing is set up): Windows SmartScreen'
  Write-Host 'warns on the first start on other machines ("More info", then "Run anyway").'
}
