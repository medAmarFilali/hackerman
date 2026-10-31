//! What the lock-screen helper runs from.
//!
//! The helper is this same agent, running as the system on the protected desktop that the
//! lock and sign-in screens are on. It has to be the same computer to other devices, so it
//! needs this computer's identity, paired devices and layout, in a copy of its own.
//!
//! The user's folder is one that any of the user's programs can write to, so what's in it
//! can't decide who gets to type at the sign-in screen. So this computer's identity is only
//! copied when an administrator sets the helper up, and devices are only copied if an
//! administrator has approved them, which the caller of [`copy_settings`] says. A device gone
//! from the user's list goes straight away, since that can only take access away.
//!
//! It only ever receives keyboard and mouse input: see [`options`] and [`rules_while_locked`].

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use hk_core::device::Fingerprint;
use hk_core::rules::{MediaShortcuts, ReceiveFiles, Rules};
use serde::Deserialize;

use crate::config::{DEFAULT_PORT, PairedDevice, Settings, computer_name, write_atomically};
use crate::engine::Options;

const IDENTITY_FILES: [&str; 2] = ["identity/certificate.der", "identity/private-key.der"];

/// Far more than any of them should be. These are read from a folder other programs can write
/// to, and this is done by the system.
const MAX_FILE_SIZE: u64 = 8 * 1024 * 1024;

/// How the helper runs. It captures nothing: anyone sitting at a locked PC could otherwise
/// reach the user's other PCs with its keyboard, and it couldn't get in the way of typing a
/// password on it either.
pub fn options(config_dir: PathBuf, channel: String) -> Options {
    Options {
        config_dir,
        port: None,
        channel,
        capture: false,
        discovery: true,
        headless_pairing: false,
    }
}

/// Nothing leaves a locked PC or lands on it, just keyboard and mouse input coming in.
fn rules_while_locked() -> Rules {
    Rules {
        clipboard: false,
        shortcuts: false,
        stay_in_fullscreen: false,
        receive_files: ReceiveFiles::Never,
        drop_folder: None,
        notify_files: false,
        share_apps: false,
        media_control: false,
        app_control: false,
        media_shortcuts: MediaShortcuts {
            enabled: false,
            ..MediaShortcuts::default()
        },
        ..Rules::default()
    }
}

/// What the helper takes from the user's settings. Reading only this means a settings file
/// from any version, or a damaged one, can't stop it starting.
#[derive(Default, Deserialize)]
struct CarriedOver {
    name: Option<String>,
    port: Option<u16>,
    enabled: Option<bool>,
}

/// Copies this computer's identity. Only for when an administrator is setting this up.
pub fn copy_identity(from: &Path, to: &Path) -> Result<()> {
    for file in IDENTITY_FILES {
        let bytes = read_small(&from.join(file))?
            .with_context(|| format!("there's no {file} in {}", from.display()))?;
        write_if_changed(&to.join(file), &bytes)?;
    }
    Ok(())
}

/// The devices a user has paired with, and what they're called.
pub fn paired_devices(from: &Path) -> Result<Vec<(Fingerprint, String)>> {
    let devices = read_devices(from)?.unwrap_or_default();
    Ok(devices
        .into_iter()
        .map(|device| (device.fingerprint, device.name))
        .collect())
}

fn read_devices(from: &Path) -> Result<Option<Vec<PairedDevice>>> {
    let path = from.join("devices.json");
    let Some(bytes) = read_small(&path)? else {
        return Ok(None);
    };
    let devices =
        serde_json::from_slice(&bytes).with_context(|| format!("invalid {}", path.display()))?;
    Ok(Some(devices))
}

/// Brings the helper's copy up to date with a user's settings, leaving out any device that
/// isn't in `approved`. It goes as far as it can, and reports the first thing that went wrong.
pub fn copy_settings(from: &Path, to: &Path, approved: &[Fingerprint]) -> Result<()> {
    let devices = copy_devices(from, to, approved);
    let layout = copy_layout(from, to);
    let settings = write_settings(from, to);
    devices.and(layout).and(settings)
}

fn copy_devices(from: &Path, to: &Path, approved: &[Fingerprint]) -> Result<()> {
    let target = to.join("devices.json");
    let Some(mut devices) = read_devices(from)? else {
        return remove_if_there(&target);
    };
    devices.retain(|device| approved.contains(&device.fingerprint));
    // What a locked PC is showing is nobody's business, whoever could see it unlocked.
    for device in &mut devices {
        device.screens_allowed = false;
    }
    let bytes = serde_json::to_vec_pretty(&devices).context("can't format the devices")?;
    write_if_changed(&target, &bytes)
}

fn copy_layout(from: &Path, to: &Path) -> Result<()> {
    let target = to.join("layout.json");
    match read_small(&from.join("layout.json"))? {
        Some(bytes) => write_if_changed(&target, &bytes),
        None => remove_if_there(&target),
    }
}

fn write_settings(from: &Path, to: &Path) -> Result<()> {
    let bytes = read_small(&from.join("hackerman.toml"))?.unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    let carried: CarriedOver = toml::from_str(&text).unwrap_or_default();
    let settings = Settings {
        name: carried.name.unwrap_or_else(computer_name),
        port: carried.port.unwrap_or(DEFAULT_PORT),
        // Turned off by the user means off here as well.
        enabled: carried.enabled.unwrap_or(true),
        rules: rules_while_locked(),
    };
    let text = toml::to_string(&settings).context("can't format the settings")?;
    write_if_changed(&to.join("hackerman.toml"), text.as_bytes())
}

/// What's in a file, if it's there, as long as it isn't too large to be what it should be.
fn read_small(path: &Path) -> Result<Option<Vec<u8>>> {
    let read_error = || format!("can't read {}", path.display());
    match std::fs::metadata(path) {
        Ok(found) if found.len() > MAX_FILE_SIZE => {
            bail!("{} is too large to be what it should be", path.display())
        }
        Ok(_) => std::fs::read(path).map(Some).with_context(read_error),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(read_error),
    }
}

fn remove_if_there(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != ErrorKind::NotFound => {
            Err(error).with_context(|| format!("can't remove {}", path.display()))
        }
        _ => Ok(()),
    }
}

fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<()> {
    if std::fs::read(path).is_ok_and(|existing| existing == bytes) {
        return Ok(());
    }
    write_atomically(path, bytes)
}

/// Whether the copy has this computer's identity. Without it, the helper would make up a new
/// one, and be a stranger to every other device.
pub fn has_settings(dir: &Path) -> bool {
    IDENTITY_FILES.iter().all(|file| dir.join(file).is_file())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use hk_core::rules::CrossModifier;

    use super::*;

    const APPROVED: Fingerprint = Fingerprint::from_bytes([1; 32]);
    const ANOTHER: Fingerprint = Fingerprint::from_bytes([2; 32]);

    /// A user's folder and a helper's, emptied first.
    fn folders(name: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join("hackerman-unit-files").join(name);
        let _ = fs::remove_dir_all(&root);
        let (user, helper) = (root.join("user"), root.join("helper"));
        fs::create_dir_all(user.join("identity")).unwrap();
        (user, helper)
    }

    fn devices_text(devices: &[Fingerprint]) -> String {
        let entries: Vec<String> = devices
            .iter()
            .enumerate()
            .map(|(at, device)| format!(r#"{{"fingerprint":"{device}","name":"pc {at}"}}"#))
            .collect();
        format!("[{}]", entries.join(","))
    }

    fn with_everything(user: &Path) {
        fs::write(user.join("identity/certificate.der"), "certificate").unwrap();
        fs::write(user.join("identity/private-key.der"), "key").unwrap();
        fs::write(user.join("devices.json"), devices_text(&[APPROVED])).unwrap();
        fs::write(user.join("layout.json"), "{}").unwrap();
    }

    fn settings_in(dir: &Path) -> Settings {
        toml::from_str(&fs::read_to_string(dir.join("hackerman.toml")).unwrap()).unwrap()
    }

    fn fingerprints_in(dir: &Path) -> Vec<Fingerprint> {
        paired_devices(dir)
            .unwrap()
            .into_iter()
            .map(|(fingerprint, _)| fingerprint)
            .collect()
    }

    #[test]
    fn the_computer_stays_the_same_computer() {
        let (user, helper) = folders("same-computer");
        with_everything(&user);
        fs::write(
            user.join("hackerman.toml"),
            "name = \"bureau\"\nport = 25000\nenabled = false\n",
        )
        .unwrap();

        copy_identity(&user, &helper).unwrap();
        copy_settings(&user, &helper, &[APPROVED]).unwrap();

        assert!(has_settings(&helper));
        assert_eq!(
            fs::read(helper.join("identity/private-key.der")).unwrap(),
            b"key"
        );
        assert_eq!(fingerprints_in(&helper), [APPROVED]);
        assert_eq!(fs::read(helper.join("layout.json")).unwrap(), b"{}");
        let settings = settings_in(&helper);
        assert_eq!(settings.name, "bureau");
        assert_eq!(settings.port, 25000);
        assert!(!settings.enabled, "turned off stays off");
    }

    /// Any of the user's programs could add a device to that file.
    #[test]
    fn being_in_the_users_list_is_not_enough_to_reach_the_lock_screen() {
        let (user, helper) = folders("not-approved");
        with_everything(&user);
        fs::write(
            user.join("devices.json"),
            devices_text(&[APPROVED, ANOTHER]),
        )
        .unwrap();

        copy_settings(&user, &helper, &[APPROVED]).unwrap();
        assert_eq!(fingerprints_in(&helper), [APPROVED]);

        copy_settings(&user, &helper, &[]).unwrap();
        assert_eq!(fingerprints_in(&helper), []);
    }

    /// Any of the user's programs could change that too.
    #[test]
    fn keeping_the_copy_up_to_date_never_touches_the_identity() {
        let (user, helper) = folders("identity-stays");
        with_everything(&user);
        copy_identity(&user, &helper).unwrap();

        fs::write(user.join("identity/private-key.der"), "someone else's").unwrap();
        copy_settings(&user, &helper, &[APPROVED]).unwrap();
        assert_eq!(
            fs::read(helper.join("identity/private-key.der")).unwrap(),
            b"key"
        );
    }

    #[test]
    fn copying_settings_alone_gives_no_identity() {
        let (user, helper) = folders("no-identity-copied");
        with_everything(&user);
        copy_settings(&user, &helper, &[APPROVED]).unwrap();
        assert!(!has_settings(&helper));
    }

    #[test]
    fn nothing_but_input_gets_past_a_locked_pc_whatever_the_user_chose() {
        let (user, helper) = folders("only-input");
        with_everything(&user);
        fs::write(
            user.join("hackerman.toml"),
            "name = \"bureau\"\n[rules]\nclipboard = true\nreceiveFiles = \"anyDevice\"\n\
             dropFolder = \"D:\\\\Reçus\"\nstayInFullscreen = true\ncrossModifier = \"ctrl\"\n",
        )
        .unwrap();

        copy_settings(&user, &helper, &[APPROVED]).unwrap();

        let rules = settings_in(&helper).rules;
        assert!(!rules.clipboard);
        assert!(!rules.shortcuts);
        assert!(!rules.stay_in_fullscreen);
        assert!(!rules.notify_files);
        assert!(!rules.share_apps);
        assert!(!rules.media_control);
        assert!(!rules.media_shortcuts.enabled);
        assert_eq!(rules.receive_files, ReceiveFiles::Never);
        assert_eq!(rules.drop_folder, None);
        assert_eq!(rules.cross_modifier, CrossModifier::None);
    }

    #[test]
    fn it_captures_nothing_from_a_locked_pc() {
        let options = options(PathBuf::from("somewhere"), "channel".into());
        assert!(!options.capture);
        assert!(!options.headless_pairing, "no pairing with nobody watching");
    }

    /// Forgetting a device has to reach the helper, or it would still let that device in.
    #[test]
    fn the_helper_never_lets_a_device_see_the_screens() {
        let (user, helper) = folders("screens");
        with_everything(&user);
        fs::write(
            user.join("devices.json"),
            format!(r#"[{{"fingerprint":"{APPROVED}","name":"desk","screens_allowed":true}}]"#),
        )
        .unwrap();
        copy_settings(&user, &helper, &[APPROVED]).unwrap();
        let copied = read_devices(&helper).unwrap().unwrap();
        assert_eq!(copied.len(), 1);
        assert!(!copied[0].screens_allowed);
    }

    #[test]
    fn a_device_forgotten_by_the_user_goes_even_if_it_was_approved() {
        let (user, helper) = folders("forgotten");
        with_everything(&user);
        copy_settings(&user, &helper, &[APPROVED]).unwrap();
        assert_eq!(fingerprints_in(&helper), [APPROVED]);

        fs::write(user.join("devices.json"), devices_text(&[])).unwrap();
        copy_settings(&user, &helper, &[APPROVED]).unwrap();
        assert_eq!(fingerprints_in(&helper), []);

        fs::write(user.join("devices.json"), devices_text(&[APPROVED])).unwrap();
        copy_settings(&user, &helper, &[APPROVED]).unwrap();
        fs::remove_file(user.join("devices.json")).unwrap();
        copy_settings(&user, &helper, &[APPROVED]).unwrap();
        assert!(!helper.join("devices.json").exists());
    }

    /// Whoever damaged it, the devices already approved carry on working.
    #[test]
    fn a_damaged_list_of_devices_changes_nothing_but_is_reported() {
        let (user, helper) = folders("damaged-devices");
        with_everything(&user);
        copy_settings(&user, &helper, &[APPROVED]).unwrap();

        fs::write(user.join("devices.json"), "this isn't a list").unwrap();
        fs::write(user.join("layout.json"), "{\"moved\":true}").unwrap();
        assert!(copy_settings(&user, &helper, &[APPROVED]).is_err());
        assert_eq!(fingerprints_in(&helper), [APPROVED]);
        assert_eq!(
            fs::read(helper.join("layout.json")).unwrap(),
            b"{\"moved\":true}",
            "the rest still gets done"
        );
    }

    #[test]
    fn changes_reach_the_copy() {
        let (user, helper) = folders("changes");
        with_everything(&user);
        copy_settings(&user, &helper, &[APPROVED]).unwrap();
        fs::write(user.join("layout.json"), "{\"moved\":true}").unwrap();
        copy_settings(&user, &helper, &[APPROVED]).unwrap();
        assert_eq!(
            fs::read(helper.join("layout.json")).unwrap(),
            b"{\"moved\":true}"
        );
    }

    /// A damaged or newer settings file mustn't stop the lock screen working.
    #[test]
    fn settings_that_make_no_sense_are_left_behind() {
        for (name, text) in [
            ("damaged", "this isn't [ settings"),
            (
                "newer",
                "name = \"bureau\"\n[rules]\nreceiveFiles = \"who knows\"\n",
            ),
        ] {
            let (user, helper) = folders(name);
            with_everything(&user);
            fs::write(user.join("hackerman.toml"), text).unwrap();
            copy_settings(&user, &helper, &[APPROVED]).unwrap();
            let settings = settings_in(&helper);
            assert_eq!(settings.port, DEFAULT_PORT);
            assert!(settings.enabled);
            assert_eq!(settings.rules, rules_while_locked());
        }
    }

    #[test]
    fn no_settings_file_is_fine_too() {
        let (user, helper) = folders("no-settings");
        with_everything(&user);
        copy_settings(&user, &helper, &[APPROVED]).unwrap();
        assert!(!settings_in(&helper).name.is_empty());
    }

    #[test]
    fn a_user_who_has_never_run_it_has_nothing_to_set_up_from() {
        let (user, helper) = folders("never-run");
        assert!(copy_identity(&user, &helper).is_err());
        assert!(!has_settings(&helper));
        assert_eq!(paired_devices(&user).unwrap(), []);
    }

    #[test]
    fn half_an_identity_is_no_identity() {
        let (user, helper) = folders("half");
        fs::write(user.join("identity/certificate.der"), "certificate").unwrap();
        assert!(copy_identity(&user, &helper).is_err());
        assert!(!has_settings(&helper));
    }

    #[test]
    fn a_huge_file_is_refused() {
        let (user, helper) = folders("huge");
        with_everything(&user);
        let huge = fs::File::create(user.join("layout.json")).unwrap();
        huge.set_len(MAX_FILE_SIZE + 1).unwrap();
        drop(huge);
        assert!(copy_settings(&user, &helper, &[APPROVED]).is_err());
        assert!(!helper.join("layout.json").exists());
    }

    #[test]
    fn copying_again_leaves_files_alone_when_nothing_changed() {
        let (user, helper) = folders("unchanged");
        with_everything(&user);
        copy_settings(&user, &helper, &[APPROVED]).unwrap();
        let path = helper.join("layout.json");
        let before = fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        copy_settings(&user, &helper, &[APPROVED]).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), before);
    }
}
