//! What an administrator agreed to, kept in Program Files where only administrators can
//! change it, while anyone can read it. There's nothing secret in it.

use std::path::PathBuf;

use anyhow::{Context, Result};
use hk_core::device::Fingerprint;
use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Approval {
    /// The folder of the user who set this up. It's the only one the service reads from, so
    /// that nobody else signing in to this computer has any say in what the helper does.
    pub(crate) settings_from: PathBuf,
    /// The only devices that can reach the lock screen, for as long as that user stays paired
    /// with them.
    pub(crate) devices: Vec<Fingerprint>,
    /// That user's account, as its security ID: the only one the input helper works for.
    /// Setting this up before there was an input helper didn't say.
    #[serde(default)]
    pub(crate) account: Option<String>,
}

impl Approval {
    pub(crate) fn load() -> Result<Self> {
        let path = paths::approval_file()?;
        let bytes =
            std::fs::read(&path).with_context(|| format!("can't read {}", path.display()))?;
        serde_json::from_slice(&bytes).with_context(|| format!("invalid {}", path.display()))
    }

    pub(crate) fn save(&self) -> Result<()> {
        let path = paths::approval_file()?;
        let bytes = serde_json::to_vec_pretty(self).context("can't format the approval")?;
        std::fs::write(&path, bytes).with_context(|| format!("can't write {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_reads_back_what_it_wrote() {
        let approval = Approval {
            settings_from: PathBuf::from(r"C:\Users\été\AppData\Roaming\Hackerman"),
            devices: vec![Fingerprint::from_bytes([7; 32])],
            account: Some("S-1-5-21-1-2-3-1001".into()),
        };
        let text = serde_json::to_string(&approval).unwrap();
        assert!(text.contains("settingsFrom"));
        assert_eq!(serde_json::from_str::<Approval>(&text).unwrap(), approval);
    }

    /// From before there was an input helper. Without an account, it works for nobody.
    #[test]
    fn an_older_approval_still_reads() {
        let approval: Approval = serde_json::from_str(
            r#"{"settingsFrom":"C:\\Users\\a\\AppData\\Roaming\\Hackerman","devices":[]}"#,
        )
        .unwrap();
        assert_eq!(approval.account, None);
    }
}
