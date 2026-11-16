//! What Praxis Remote remembers between launches.
//!
//! The GitHub tokens and every phone's key live in the system keychain, even
//! though this build's release channel keeps other credentials in a
//! development file. Where no keychain can be used, as on a Linux server or
//! VM without a Secret Service, they go in a file only this user can read,
//! as GitHub's own CLI does. The data directory otherwise only holds which
//! account, channel and gist are in use, and the names of the paired phones.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use gpui::AsyncApp;
use serde::{Deserialize, Serialize};

use super::github::Tokens;

const KEYCHAIN_URL: &str = "https://api.github.com/praxis-remote";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct LocalState {
    pub version: u32,
    pub login: String,
    /// Identifies this computer's channel, even if its gist is recreated.
    pub channel: String,
    #[serde(default)]
    pub gist_id: Option<String>,
    #[serde(default)]
    pub phones: Vec<PhoneInfo>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct PhoneInfo {
    pub id: String,
    pub name: String,
    pub paired_at: DateTime<Utc>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Secrets {
    #[serde(default)]
    pub tokens: Option<Tokens>,
    /// Each paired phone's key, base64.
    #[serde(default)]
    pub phone_keys: BTreeMap<String, String>,
}

pub(super) fn state_path() -> PathBuf {
    paths::data_dir().join("remote").join("state.json")
}

fn secrets_path() -> PathBuf {
    paths::data_dir().join("remote").join("secrets.json")
}

/// Where the sign-in is kept, when the keychain could not be used.
pub(super) fn secrets_file_in_use() -> Option<PathBuf> {
    let path = secrets_path();
    path.is_file().then_some(path)
}

pub(super) fn acquire_channel_lock() -> Result<std::fs::File> {
    lock_channel_file(&state_path().with_extension("lock"))
}

fn lock_channel_file(path: &std::path::Path) -> Result<std::fs::File> {
    let parent = path
        .parent()
        .context("Praxis Remote's lock has no folder")?;
    std::fs::create_dir_all(parent)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!(
            "Another Praxis process owns this phone connection. Open the folder in that process; existing sessions are unchanged."
        ),
        Err(std::fs::TryLockError::Error(error)) => {
            Err(error).with_context(|| format!("locking {}", path.display()))
        }
    }
}

/// The configuration file of the first version, which needed a repository.
pub(super) fn legacy_config_path() -> PathBuf {
    paths::data_dir().join("remote").join("config.json")
}

pub(super) fn load_state() -> Result<Option<LocalState>> {
    let path = state_path();
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let state =
        serde_json::from_str(&contents).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(state))
}

pub(super) fn save_state(state: &LocalState) -> Result<()> {
    let path = state_path();
    let directory = path
        .parent()
        .context("Praxis Remote's state has no folder")?;
    std::fs::create_dir_all(directory)
        .with_context(|| format!("creating {}", directory.display()))?;
    // Written beside it and moved into place, so a crash never leaves half a
    // file behind.
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(state)?)
        .with_context(|| format!("writing {}", temporary.display()))?;
    std::fs::rename(&temporary, &path).with_context(|| format!("writing {}", path.display()))
}

pub(super) fn delete_state() -> Result<()> {
    match std::fs::remove_file(state_path()) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(error).context("removing Praxis Remote's state")
        }
        _ => Ok(()),
    }
}

pub(super) async fn load_secrets(cx: &AsyncApp) -> Result<Option<Secrets>> {
    let task = cx.update(|cx| cx.read_credentials(KEYCHAIN_URL));
    let bytes = match task.await {
        Ok(Some((_, bytes))) => bytes,
        Ok(None) => match read_secrets_file(&secrets_path())? {
            Some(bytes) => bytes,
            None => return Ok(None),
        },
        Err(error) => match read_secrets_file(&secrets_path())? {
            Some(bytes) => bytes,
            None => {
                return Err(error)
                    .context("reading Praxis Remote's sign-in from the system keychain");
            }
        },
    };
    let secrets = serde_json::from_slice(&bytes).context("reading Praxis Remote's sign-in")?;
    Ok(Some(secrets))
}

pub(super) async fn save_secrets(login: &str, secrets: &Secrets, cx: &AsyncApp) -> Result<()> {
    let bytes = serde_json::to_vec(secrets)?;
    let task = cx.update(|cx| cx.write_credentials(KEYCHAIN_URL, login, &bytes));
    match task.await {
        // A copy left from a time without a keychain must not outlive it.
        Ok(()) => remove_secrets_file(&secrets_path()),
        Err(error) => {
            let path = secrets_path();
            log::warn!(
                "Praxis Remote could not use the system keychain ({error:#}), so it keeps its sign-in in {}",
                path.display()
            );
            write_secrets_file(&path, &bytes)
        }
    }
}

pub(super) async fn delete_secrets(cx: &AsyncApp) -> Result<()> {
    let task = cx.update(|cx| cx.delete_credentials(KEYCHAIN_URL));
    let keychain = task.await;
    let path = secrets_path();
    let had_file = path.is_file();
    remove_secrets_file(&path)?;
    match keychain {
        // Without a keychain, the file was the only copy.
        Err(error) if had_file => {
            log::info!("Praxis Remote's keychain entry could not be removed: {error:#}");
            Ok(())
        }
        result => result.context("removing Praxis Remote's sign-in from the system keychain"),
    }
}

fn read_secrets_file(path: &std::path::Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_secrets_file(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;

    let directory = path
        .parent()
        .context("Praxis Remote's sign-in has no folder")?;
    std::fs::create_dir_all(directory)
        .with_context(|| format!("creating {}", directory.display()))?;
    let temporary = path.with_extension("json.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    // Created unreadable to others from the start, rather than narrowed
    // after the secret is already written.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("writing {}", temporary.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        // A leftover temporary file keeps its old mode when reopened.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("protecting {}", temporary.display()))?;
    }
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {}", temporary.display()))?;
    drop(file);
    std::fs::rename(&temporary, path).with_context(|| format!("writing {}", path.display()))
}

fn remove_secrets_file(path: &std::path::Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(error).with_context(|| format!("removing {}", path.display()))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_ownership_is_exclusive_and_released_on_drop() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("state.lock");
        let first = lock_channel_file(&path).expect("first process owns channel");
        assert!(lock_channel_file(&path).is_err());
        drop(first);
        let second = lock_channel_file(&path).expect("ownership released");
        assert!(lock_channel_file(&path).is_err());
        drop(second);
        assert!(
            path.exists(),
            "never unlink a lock file while another process may open it"
        );
    }

    #[test]
    fn test_remote_secrets_file_round_trips_and_is_private() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("remote").join("secrets.json");
        assert_eq!(read_secrets_file(&path).expect("missing file reads"), None);

        write_secrets_file(&path, b"first").expect("written");
        write_secrets_file(&path, b"second").expect("replaced");
        assert_eq!(
            read_secrets_file(&path).expect("read").as_deref(),
            Some(&b"second"[..])
        );
        assert!(!path.with_extension("json.tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "only the owner may read the sign-in");
        }

        remove_secrets_file(&path).expect("removed");
        remove_secrets_file(&path).expect("removing twice is fine");
        assert_eq!(read_secrets_file(&path).expect("read"), None);
    }
}
