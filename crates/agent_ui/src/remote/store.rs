//! What Praxis Remote remembers between launches.
//!
//! Nothing secret goes on disk in plain text: the GitHub tokens and every
//! phone's key live in the system keychain, always, even though this build's
//! release channel keeps other credentials in a development file. The data
//! directory only holds which account, channel and gist are in use, and the
//! names of the paired phones.

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
    let state = serde_json::from_str(&contents)
        .with_context(|| format!("parsing {}", path.display()))?;
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
    let Some((_, bytes)) = task
        .await
        .context("reading Praxis Remote's sign-in from the system keychain")?
    else {
        return Ok(None);
    };
    let secrets = serde_json::from_slice(&bytes).context("reading Praxis Remote's sign-in")?;
    Ok(Some(secrets))
}

pub(super) async fn save_secrets(login: &str, secrets: &Secrets, cx: &AsyncApp) -> Result<()> {
    let bytes = serde_json::to_vec(secrets)?;
    let task = cx.update(|cx| cx.write_credentials(KEYCHAIN_URL, login, &bytes));
    task.await
        .context("saving Praxis Remote's sign-in in the system keychain")
}

pub(super) async fn delete_secrets(cx: &AsyncApp) -> Result<()> {
    let task = cx.update(|cx| cx.delete_credentials(KEYCHAIN_URL));
    task.await
        .context("removing Praxis Remote's sign-in from the system keychain")
}
