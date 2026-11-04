//! What the agent keeps on disk, in one folder.
//!
//! `hackerman.toml` holds settings a person might edit. `devices.json` and
//! `layout.json` are the agent's own records.

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use hk_core::device::Fingerprint;
use hk_core::layout::{LayoutDoc, Monitor};
use hk_core::rules::Rules;
use hk_core::shared_monitors::{MonitorIdentity, Showing};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub const DEFAULT_PORT: u16 = 24850;

/// The folder holding Hackerman's configuration and identity.
pub fn default_config_dir() -> Result<PathBuf> {
    let env_path = |name| {
        std::env::var_os(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    let base = if cfg!(windows) {
        env_path("APPDATA")
    } else if cfg!(target_os = "macos") {
        env_path("HOME").map(|home| home.join("Library/Application Support"))
    } else {
        env_path("XDG_CONFIG_HOME").or_else(|| env_path("HOME").map(|home| home.join(".config")))
    };
    Ok(base
        .context("can't find the user's configuration folder")?
        .join("Hackerman"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Shown to other devices.
    #[serde(default = "computer_name")]
    pub name: String,
    /// The UDP port to listen on.
    #[serde(default = "default_port")]
    pub port: u16,
    /// Whether this computer shares anything. Stays as the user left it across
    /// restarts, because turning it off could be for a reason that hasn't gone away.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub rules: Rules,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

fn default_enabled() -> bool {
    true
}

pub(crate) fn computer_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "My computer".into())
}

impl Settings {
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join("hackerman.toml");
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let settings = Self {
                    name: computer_name(),
                    port: DEFAULT_PORT,
                    enabled: true,
                    rules: Rules::default(),
                };
                settings.save(dir)?;
                Ok(settings)
            }
            Err(error) => Err(error).with_context(|| format!("can't read {}", path.display())),
        }
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        let text = toml::to_string(self).context("can't format the settings")?;
        write_atomically(&dir.join("hackerman.toml"), text.as_bytes())
    }
}

/// A device this one has paired with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairedDevice {
    pub fingerprint: Fingerprint,
    pub name: String,
    /// Where it was last reached, for when discovery doesn't find it.
    #[serde(default)]
    pub addresses: Vec<SocketAddr>,
    /// Its make and model, like "Dell Latitude 7420", if it says.
    #[serde(default)]
    pub model: Option<String>,
    /// Its monitors when last connected, to keep it on the canvas while it's away.
    #[serde(default)]
    pub monitors: Vec<Monitor>,
    /// The user said it can send files here without asking.
    #[serde(default)]
    pub files_allowed: bool,
    /// The user said it can see this computer's screens.
    #[serde(default)]
    pub screens_allowed: bool,
    /// What its monitors said they were, for a monitor plugged into it and another device.
    #[serde(default)]
    pub monitor_identities: Vec<MonitorIdentity>,
}

pub fn load_devices(dir: &Path) -> Result<Vec<PairedDevice>> {
    load_json(&dir.join("devices.json"))
}

pub fn save_devices(dir: &Path, devices: &[PairedDevice]) -> Result<()> {
    save_json(&dir.join("devices.json"), &devices)
}

pub fn load_layout(dir: &Path) -> Result<LayoutDoc> {
    load_json(&dir.join("layout.json"))
}

pub fn save_layout(dir: &Path, layout: &LayoutDoc) -> Result<()> {
    save_json(&dir.join("layout.json"), layout)
}

/// Which device each monitor plugged into several of them is showing.
pub fn load_showing(dir: &Path) -> Result<Showing> {
    load_json(&dir.join("shared-monitors.json"))
}

pub fn save_showing(dir: &Path, showing: &Showing) -> Result<()> {
    save_json(&dir.join("shared-monitors.json"), showing)
}

fn load_json<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match std::fs::read(path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).with_context(|| format!("invalid {}", path.display()))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error).with_context(|| format!("can't read {}", path.display())),
    }
}

fn save_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value).context("can't format saved data")?;
    write_atomically(path, &bytes)
}

/// Replaces the file in one step, so a crash or power cut can't leave half of it.
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("can't create {}", dir.display()))?;
    }
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, bytes)
        .with_context(|| format!("can't write {}", temporary.display()))?;
    std::fs::rename(&temporary, path).with_context(|| format!("can't write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use hk_core::rules::ReceiveFiles;

    use super::*;

    #[test]
    fn settings_from_an_older_version_still_load() {
        // Milestone M1's config file had these other fields.
        let settings: Settings = toml::from_str(
            r#"
            name = "desk"
            listen = "0.0.0.0:24850"

            [peer]
            address = "192.168.1.20:24850"
            fingerprint = "paste here"
            edge = "right"
            "#,
        )
        .unwrap();
        assert_eq!(settings.name, "desk");
        assert_eq!(settings.port, DEFAULT_PORT);
    }

    /// The one version that could say this had it as the default.
    #[test]
    fn a_settings_file_saying_to_always_take_files_now_asks_first() {
        let settings: Settings = toml::from_str(
            r#"
            [rules]
            receiveFiles = "always"
            dropFolder = "D:\\Reçus"
            "#,
        )
        .unwrap();
        assert_eq!(settings.rules.receive_files, ReceiveFiles::AskFirst);
        assert_eq!(settings.rules.drop_folder.as_deref(), Some("D:\\Reçus"));
    }

    #[test]
    fn paired_devices_from_an_older_version_load_and_have_to_ask() {
        let id = Fingerprint::from_bytes([7; 32]);
        let devices: Vec<PairedDevice> =
            serde_json::from_str(&format!(r#"[{{"fingerprint":"{id}","name":"old"}}]"#)).unwrap();
        assert!(!devices[0].files_allowed);
        assert!(!devices[0].screens_allowed);
    }

    #[test]
    fn missing_settings_get_defaults() {
        let settings: Settings = toml::from_str("").unwrap();
        assert_eq!(settings.port, DEFAULT_PORT);
        assert!(!settings.name.is_empty());
    }
}
