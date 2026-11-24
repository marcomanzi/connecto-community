//! Local, process-scoped staging. Each Flutter isolate calls into this same
//! native library. Copies are bounded and disappear when the app exits.

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use connecto_net::wire::{MAX_ATTACHMENT_BYTES, MAX_ATTACHMENTS, MAX_STR};
use serde_json::{Value, json};

type Scope<'a> = (&'a str, &'a str, &'a str);
type OwnedScope = (String, String, String);

const GLOBAL_BYTES: usize = 4 * MAX_ATTACHMENT_BYTES;
const GLOBAL_FILES: usize = 4 * MAX_ATTACHMENTS;
const OVER_LIMIT: &str = "Attachments for one message can use up to 10 MiB combined.";

pub(crate) struct Staged {
    pub token: String,
    pub name: String,
    pub bytes: Vec<u8>,
}

static STAGED: Mutex<Vec<(OwnedScope, Arc<Staged>)>> = Mutex::new(Vec::new());

fn staged() -> MutexGuard<'static, Vec<(OwnedScope, Arc<Staged>)>> {
    STAGED.lock().unwrap_or_else(PoisonError::into_inner)
}

fn in_scope(saved: &OwnedScope, scope: Scope<'_>) -> bool {
    (saved.0.as_str(), saved.1.as_str(), saved.2.as_str()) == scope
}

#[cfg(unix)]
fn open_file(path: &Path) -> std::io::Result<File> {
    use rustix::fs::{Mode, OFlags, open};
    Ok(open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)?)
}

#[cfg(windows)]
fn open_file(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

pub(crate) fn stage(scope: Scope<'_>, path: &Path) -> Result<Value, String> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| {
            !name.is_empty()
                && name.len() <= MAX_STR
                && !name.chars().any(char::is_control)
                && !name.contains(['/', '\\'])
        })
        .ok_or("Choose a file with a valid name.")?;
    if !path.is_absolute() {
        return Err("Choose a file on this device.".into());
    }
    let unreadable = || format!("Could not read {name}.");
    let not_regular = || format!("{name} is not a regular file.");
    let metadata = path.symlink_metadata().map_err(|_| unreadable())?;
    if !metadata.is_file() {
        return Err(not_regular());
    }
    if metadata.len() > MAX_ATTACHMENT_BYTES as u64 {
        return Err(format!("{name} is larger than the 10 MiB limit."));
    }
    let mut files = staged();
    let (mut count, mut bytes, mut global) = (0, 0, 0);
    for (saved, file) in files.iter() {
        global += file.bytes.len();
        if in_scope(saved, scope) {
            count += 1;
            bytes += file.bytes.len();
        }
    }
    if count >= MAX_ATTACHMENTS {
        return Err("You can attach up to 4 files to one message.".into());
    }
    if files.len() >= GLOBAL_FILES {
        return Err(
            "Attachment staging is full. Send or remove queued files and try again.".into(),
        );
    }
    let remaining = MAX_ATTACHMENT_BYTES - bytes;
    let global_remaining = GLOBAL_BYTES - global;
    let file = open_file(path).map_err(|_| unreadable())?;
    let actual = file.metadata().map_err(|_| unreadable())?;
    if !actual.is_file() {
        return Err(not_regular());
    }
    let mut content = Vec::new();
    file.take(remaining.min(global_remaining) as u64 + 1)
        .read_to_end(&mut content)
        .map_err(|_| unreadable())?;
    if content.len() > remaining {
        return Err(OVER_LIMIT.into());
    }
    if content.len() > global_remaining {
        return Err(
            "Attachment staging is full. Send or remove queued files and try again.".into(),
        );
    }
    let mut random = [0; 16];
    getrandom::fill(&mut random).map_err(|_| "Could not queue the attachment. Try again.")?;
    let token = format!("{:032x}", u128::from_be_bytes(random));
    let response = json!({"token": token, "name": name, "bytes": content.len()});
    files.push((
        (scope.0.into(), scope.1.into(), scope.2.into()),
        Arc::new(Staged {
            token,
            name: name.into(),
            bytes: content,
        }),
    ));
    Ok(response)
}

pub(crate) fn resolve(scope: Scope<'_>, tokens: &Value) -> Result<Vec<Arc<Staged>>, String> {
    if tokens.is_null() {
        return Ok(Vec::new());
    }
    const MISSING: &str = "One or more attachments are no longer queued. Add them again.";
    let tokens = tokens
        .as_array()
        .filter(|tokens| tokens.len() <= MAX_ATTACHMENTS)
        .ok_or(MISSING)?;
    let files = staged();
    let mut resolved: Vec<Arc<Staged>> = Vec::with_capacity(tokens.len());
    for token in tokens {
        let token = token.as_str().ok_or(MISSING)?;
        if resolved.iter().any(|file| file.token == token) {
            return Err(MISSING.into());
        }
        let (_, file) = files
            .iter()
            .find(|(saved, file)| in_scope(saved, scope) && file.token == token)
            .ok_or(MISSING)?;
        resolved.push(Arc::clone(file));
    }
    Ok(resolved)
}

pub(crate) fn detach(scope: Scope<'_>, token: &str) {
    staged().retain(|(saved, file)| !in_scope(saved, scope) || file.token != token);
}

pub(crate) fn clear_sent(scope: Scope<'_>, files: &[Arc<Staged>]) {
    staged().retain(|(saved, staged)| {
        !in_scope(saved, scope) || !files.iter().any(|file| Arc::ptr_eq(file, staged))
    });
}

pub(crate) fn message(text: &str, host_paths: &[String]) -> Result<String, String> {
    let paths =
        serde_json::to_string(host_paths).map_err(|_| "Could not prepare the attachments.")?;
    Ok(if text.trim().is_empty() {
        format!("Please review these attached files on this host: {paths}")
    } else {
        format!("{text} Attached files on this host: {paths}")
    })
}

pub(crate) fn forget(host: &str) {
    staged().retain(|(scope, _)| scope.0 != host);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_only_message_contains_the_quoted_host_reference() {
        let paths = vec!["/host/Library/Application Support/clip.bin".into()];
        assert_eq!(
            message("", &paths).unwrap(),
            "Please review these attached files on this host: [\"/host/Library/Application Support/clip.bin\"]"
        );
        assert!(
            message("Read this", &paths)
                .unwrap()
                .starts_with("Read this Attached files")
        );
    }

    #[test]
    fn stages_a_local_copy_and_keeps_its_token_for_failed_sends() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("selected notes.txt");
        std::fs::write(&file, b"binary\0content").unwrap();
        let scope = ("Test Host", "project", "1");
        let staged = stage(scope, &file).unwrap();
        assert_eq!(staged["name"], "selected notes.txt");
        assert_eq!(staged["bytes"], 14);
        assert!(staged.get("path").is_none());
        std::fs::remove_file(&file).unwrap();
        let tokens = json!([staged["token"]]);
        assert!(resolve(("Test Host", "other-project", "1"), &tokens).is_err());
        assert_eq!(
            resolve(scope, &tokens).unwrap()[0].bytes,
            b"binary\0content"
        );
        let files = resolve(scope, &tokens).unwrap();
        clear_sent(scope, &files);
        assert!(resolve(scope, &tokens).is_err());
    }

    #[test]
    fn checks_combined_limit_and_detaches_without_needing_the_file() {
        use std::fs::OpenOptions;
        let dir = tempfile::tempdir().unwrap();
        let scope = ("Size Host", "project", "1");
        let file = dir.path().join("large.dat");
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&file)
            .unwrap()
            .set_len((MAX_ATTACHMENT_BYTES / 2 + 1) as u64)
            .unwrap();
        let staged = stage(scope, &file).unwrap();
        assert_eq!(stage(scope, &file), Err(OVER_LIMIT.into()));
        detach(scope, staged["token"].as_str().unwrap());
        assert!(stage(scope, &file).is_ok());
        forget(scope.0);
        assert!(
            stage(scope, dir.path())
                .unwrap_err()
                .contains("not a regular file")
        );
    }
}
