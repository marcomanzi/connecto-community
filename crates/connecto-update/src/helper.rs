//! The hand-over: a small script, started just before Connecto quits, that
//! waits for it to exit, puts the new version in place, starts it, and puts
//! the previous one back if the new one does not say it started (the healthy
//! marker, see [`crate::state`]). A running program cannot replace its own
//! files on Windows, and on macOS the bundle should be swapped while nothing
//! runs from it, so this cannot be done from inside the app.
//!
//! The scripts are plain `sh` (macOS) and PowerShell (Windows), written from
//! the paths below with every path quoted, so a space or a quote in a folder
//! name cannot change what runs.

use std::path::Path;

/// What the macOS hand-over needs.
#[derive(Debug, Clone)]
pub struct MacHandOver<'a> {
    /// The process to wait for (this app).
    pub pid: u32,
    /// The bundle to replace, e.g. `/Applications/Connecto.app`.
    pub target: &'a Path,
    /// The new bundle, checked and ready.
    pub staged: &'a Path,
    /// Where the bundle in use is kept, for a rollback.
    pub previous: &'a Path,
    /// The file the new version writes once it is up.
    pub marker: &'a Path,
    pub log: &'a Path,
    pub wait_secs: u32,
}

/// What the Windows hand-over needs.
#[derive(Debug, Clone)]
pub struct WindowsHandOver<'a> {
    pub pid: u32,
    /// The install folder, `%LOCALAPPDATA%\Programs\Connecto`.
    pub target: &'a Path,
    /// The new folder, checked and ready.
    pub staged: &'a Path,
    /// Where the install folder in use is kept, for a rollback.
    pub previous: &'a Path,
    /// The program to start if the new version does not: the one running now.
    pub fallback: &'a Path,
    pub marker: &'a Path,
    pub log: &'a Path,
    pub wait_secs: u32,
}

/// `text` as one `sh` word.
#[must_use]
pub fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// `text` as one PowerShell single-quoted string.
#[must_use]
pub fn ps_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn sh(path: &Path) -> String {
    sh_quote(&path.to_string_lossy())
}

fn ps(path: &Path) -> String {
    ps_quote(&path.to_string_lossy())
}

/// The macOS hand-over script. An older kept version goes to the Trash, never
/// deleted.
#[must_use]
pub fn mac_script(h: &MacHandOver<'_>) -> String {
    format!(
        r#"#!/bin/sh
# Connecto update hand-over. Written and started by Connecto.
PID={pid}
TARGET={target}
STAGED={staged}
PREVIOUS={previous}
MARK={marker}
exec >>{log} 2>&1
echo "$(date): waiting for Connecto ($PID) to quit"
while kill -0 "$PID" 2>/dev/null; do sleep 0.5; done
rm -f "$MARK"
if [ -e "$PREVIOUS" ]; then
  mv "$PREVIOUS" "$HOME/.Trash/Connecto (previous $(date +%s)).app" || exit 1
fi
mkdir -p "$(dirname "$PREVIOUS")"
if ! mv "$TARGET" "$PREVIOUS"; then
  echo "the app in use could not be moved aside: nothing changed"
  open "$TARGET"
  exit 1
fi
if ! mv "$STAGED" "$TARGET"; then
  echo "the new version could not be put in place: back to the previous one"
  mv "$PREVIOUS" "$TARGET"
  open "$TARGET"
  exit 1
fi
open "$TARGET"
i=0
while [ "$i" -lt {wait} ]; do
  if [ -f "$MARK" ]; then
    echo "$(date): the new version started"
    exit 0
  fi
  sleep 1
  i=$((i + 1))
done
echo "$(date): the new version did not start: rolling back"
pkill -f "$TARGET/Contents/MacOS/connecto-app"
sleep 2
mv "$TARGET" "$HOME/.Trash/Connecto (failed update $(date +%s)).app" || exit 1
mv "$PREVIOUS" "$TARGET"
open "$TARGET"
exit 1
"#,
        pid = h.pid,
        target = sh(h.target),
        staged = sh(h.staged),
        previous = sh(h.previous),
        marker = sh(h.marker),
        log = sh(h.log),
        wait = h.wait_secs,
    )
}

/// The Windows hand-over script.
#[must_use]
pub fn windows_script(h: &WindowsHandOver<'_>) -> String {
    format!(
        r#"# Connecto update hand-over. Written and started by Connecto.
$ErrorActionPreference = 'Stop'
$ConnectoPid = {pid}
$Target = {target}
$Staged = {staged}
$Previous = {previous}
$Fallback = {fallback}
$Mark = {marker}
Start-Transcript -Path {log} -Append | Out-Null
try {{ Wait-Process -Id $ConnectoPid -ErrorAction SilentlyContinue }} catch {{ }}
Start-Sleep -Milliseconds 500
Remove-Item -LiteralPath $Mark -Force -ErrorAction SilentlyContinue
$moved = $false
try {{
  if (Test-Path -LiteralPath $Target) {{
    if (Test-Path -LiteralPath $Previous) {{ Remove-Item -LiteralPath $Previous -Recurse -Force }}
    Move-Item -LiteralPath $Target -Destination $Previous
    $moved = $true
  }}
  New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Target) | Out-Null
  Move-Item -LiteralPath $Staged -Destination $Target
}} catch {{
  Write-Output "the new version could not be put in place: $_"
  if ($moved -and -not (Test-Path -LiteralPath $Target)) {{ Move-Item -LiteralPath $Previous -Destination $Target }}
  Start-Process -FilePath $Fallback
  exit 1
}}
$new = Start-Process -FilePath (Join-Path $Target 'connecto-app.exe') -PassThru
for ($i = 0; $i -lt {wait}; $i++) {{
  if (Test-Path -LiteralPath $Mark) {{ Write-Output 'the new version started'; exit 0 }}
  Start-Sleep -Seconds 1
}}
Write-Output 'the new version did not start: rolling back'
Stop-Process -Id $new.Id -Force -ErrorAction SilentlyContinue
Start-Sleep -Seconds 2
$failed = "$Target.failed-$([DateTimeOffset]::Now.ToUnixTimeSeconds())"
Move-Item -LiteralPath $Target -Destination $failed
if ($moved) {{ Move-Item -LiteralPath $Previous -Destination $Target }}
Start-Process -FilePath $Fallback
exit 1
"#,
        pid = h.pid,
        target = ps(h.target),
        staged = ps(h.staged),
        previous = ps(h.previous),
        fallback = ps(h.fallback),
        marker = ps(h.marker),
        log = ps(h.log),
        wait = h.wait_secs,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn quoting_keeps_any_path_one_word() {
        assert_eq!(sh_quote("/a b/c"), "'/a b/c'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(ps_quote(r"C:\O'Neil\x"), r"'C:\O''Neil\x'");
    }

    #[test]
    fn the_mac_hand_over_swaps_starts_and_rolls_back() {
        let target = PathBuf::from("/Applications/Connecto.app");
        let staged = PathBuf::from(
            "/Users/a b/Library/Application Support/connecto/updates/0.1.9/Connecto.app",
        );
        let previous = PathBuf::from(
            "/Users/a b/Library/Application Support/connecto/updates/previous/Connecto.app",
        );
        let marker =
            PathBuf::from("/Users/a b/Library/Application Support/connecto/updates/healthy-0.1.9");
        let log = PathBuf::from("/tmp/log");
        let script = mac_script(&MacHandOver {
            pid: 4242,
            target: &target,
            staged: &staged,
            previous: &previous,
            marker: &marker,
            log: &log,
            wait_secs: 120,
        });
        assert!(script.starts_with("#!/bin/sh\n"));
        assert!(script.contains("PID=4242\n"));
        assert!(script.contains(
            "STAGED='/Users/a b/Library/Application Support/connecto/updates/0.1.9/Connecto.app'\n"
        ));
        // In order: wait, move aside, move in, start, wait for the marker,
        // then the rollback.
        let at = |needle: &str| script.find(needle).unwrap_or_else(|| panic!("{needle}"));
        assert!(at("kill -0") < at(r#"mv "$TARGET" "$PREVIOUS""#));
        assert!(at(r#"mv "$TARGET" "$PREVIOUS""#) < at(r#"mv "$STAGED" "$TARGET""#));
        assert!(at(r#"mv "$STAGED" "$TARGET""#) < at(r#"[ -f "$MARK" ]"#));
        assert!(at(r#"[ -f "$MARK" ]"#) < at("rolling back"));
        assert!(script.contains(r#"while [ "$i" -lt 120 ]"#));
        // Nothing is ever deleted: old copies go to the Trash.
        assert!(!script.contains("rm -r"), "{script}");
    }

    #[test]
    fn the_windows_hand_over_swaps_starts_and_rolls_back() {
        let target = PathBuf::from(r"C:\Users\Jane O'Neil\AppData\Local\Programs\Connecto");
        let staged = PathBuf::from(r"C:\Users\x\AppData\Roaming\connecto\updates\0.1.9\Connecto");
        let previous = PathBuf::from(r"C:\Users\x\AppData\Local\Programs\Connecto.previous");
        let fallback = PathBuf::from(r"C:\Users\x\Downloads\connecto\connecto-app.exe");
        let marker = PathBuf::from(r"C:\m");
        let log = PathBuf::from(r"C:\l");
        let script = windows_script(&WindowsHandOver {
            pid: 7,
            target: &target,
            staged: &staged,
            previous: &previous,
            fallback: &fallback,
            marker: &marker,
            log: &log,
            wait_secs: 90,
        });
        assert!(
            script.contains(r"$Target = 'C:\Users\Jane O''Neil\AppData\Local\Programs\Connecto'")
        );
        assert!(script.contains("$ConnectoPid = 7\n"));
        assert!(script.contains("-lt 90"));
        let at = |needle: &str| script.find(needle).unwrap_or_else(|| panic!("{needle}"));
        assert!(at("Wait-Process") < at("Move-Item -LiteralPath $Target -Destination $Previous"));
        assert!(
            at("Move-Item -LiteralPath $Staged") < at("Start-Process -FilePath (Join-Path $Target")
        );
        assert!(at("Test-Path -LiteralPath $Mark") < at("rolling back"));
        assert!(script.ends_with("Start-Process -FilePath $Fallback\nexit 1\n"));
    }
}
