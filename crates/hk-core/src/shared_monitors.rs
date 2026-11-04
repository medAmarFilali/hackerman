//! One monitor plugged into two or more devices at once.
//!
//! Each of them reports it as a monitor of its own, so it would be on the canvas once for each
//! of them. But it only shows one of them at a time, so it's one screen there: the copy of the
//! device it's showing. The others are hidden, which the cursor never goes onto either, since
//! nothing it does there could be seen.
//!
//! A device can't tell whether a monitor is showing its picture, so the user says which device
//! it's showing, and every device learns of that. A monitor is only taken to be the same one if
//! it gives the same serial number everywhere: two monitors of one model otherwise look alike.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::device::Fingerprint;
use crate::layout::{DeviceMonitors, MonitorKey};

/// What a monitor says it is, which is the same whichever device it's plugged into: its maker,
/// model and serial number, like `PKB036E:D2NEE0014200`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorIdentity {
    /// The monitor's ID on the device reporting it.
    pub monitor: String,
    pub hardware: String,
}

/// Far longer than any real one. Other devices send these.
pub const MAX_HARDWARE_LENGTH: usize = 100;

/// One monitor that more than one device reports.
#[derive(Debug, Clone, PartialEq)]
pub struct SharedMonitor {
    pub hardware: String,
    /// The copy that's on the canvas, of the device the monitor is showing.
    pub showing: MonitorKey,
    pub hidden: Vec<MonitorKey>,
}

impl SharedMonitor {
    /// Every device it's plugged into, in order.
    pub fn devices(&self) -> Vec<Fingerprint> {
        let mut devices: Vec<Fingerprint> = std::iter::once(&self.showing)
            .chain(&self.hidden)
            .map(|copy| copy.device)
            .collect();
        devices.sort();
        devices
    }

    pub fn copy_on(&self, device: Fingerprint) -> Option<&MonitorKey> {
        std::iter::once(&self.showing)
            .chain(&self.hidden)
            .find(|copy| copy.device == device)
    }
}

/// Which device each monitor plugged into several of them is showing, as the user last said.
/// Every device keeps a copy, and the latest word on each monitor wins.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Showing {
    choices: Vec<Choice>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Choice {
    hardware: String,
    device: Fingerprint,
    /// When it was chosen, in milliseconds since 1970, and who by, which settles a tie.
    at: u64,
    by: Fingerprint,
}

impl Choice {
    fn is_newer_than(&self, other: &Self) -> bool {
        (self.at, self.by) > (other.at, other.by)
    }
}

impl Showing {
    /// The device this monitor was last said to be showing, if anyone said.
    pub fn device_for(&self, hardware: &str) -> Option<Fingerprint> {
        self.find(hardware).map(|choice| choice.device)
    }

    /// Records that this monitor is showing `device` now. A device's clock can be behind
    /// another's, so this always comes out as the latest word.
    pub fn choose(&mut self, hardware: &str, device: Fingerprint, now: u64, by: Fingerprint) {
        let at = self
            .find(hardware)
            .map_or(now, |choice| now.max(choice.at + 1));
        let choice = Choice {
            hardware: hardware.to_string(),
            device,
            at,
            by,
        };
        match self.choices.iter_mut().find(|c| c.hardware == hardware) {
            Some(existing) => *existing = choice,
            None => self.choices.push(choice),
        }
    }

    /// Takes the newer word on each monitor from another device's copy. Says whether
    /// anything changed.
    pub fn merge(&mut self, other: Showing) -> bool {
        let mut changed = false;
        for choice in other.choices {
            if choice.hardware.len() > MAX_HARDWARE_LENGTH {
                continue;
            }
            match self
                .choices
                .iter_mut()
                .find(|c| c.hardware == choice.hardware)
            {
                Some(existing) if choice.is_newer_than(existing) => {
                    *existing = choice;
                    changed = true;
                }
                Some(_) => {}
                None => {
                    self.choices.push(choice);
                    changed = true;
                }
            }
        }
        changed
    }

    fn find(&self, hardware: &str) -> Option<&Choice> {
        self.choices
            .iter()
            .find(|choice| choice.hardware == hardware)
    }
}

/// The monitors that more than one of these devices reports, and which copy of each is
/// shown. `identities` says what each device's monitors are, as far as it said.
///
/// A monitor is shown as the device it was last said to be showing, as long as that device is
/// there. While it isn't, the monitor will be showing one that is, which most monitors switch
/// to by themselves when the picture they were showing goes. Otherwise, it's the first device
/// in order, so that devices agree before anyone has said.
pub fn find_shared(
    devices: &[DeviceMonitors],
    identities: &BTreeMap<Fingerprint, Vec<MonitorIdentity>>,
    showing: &Showing,
) -> Vec<SharedMonitor> {
    let mut copies: BTreeMap<&str, Vec<(MonitorKey, bool)>> = BTreeMap::new();
    let mut devices: Vec<&DeviceMonitors> = devices.iter().collect();
    devices.sort_by_key(|device| device.device);
    for device in devices {
        let Some(identities) = identities.get(&device.device) else {
            continue;
        };
        for monitor in &device.monitors {
            let Some(identity) = identities.iter().find(|i| i.monitor == monitor.id) else {
                continue;
            };
            let found = copies.entry(identity.hardware.as_str()).or_default();
            // A device reporting it twice is only plugged into it once.
            if found.iter().all(|(copy, _)| copy.device != device.device) {
                let key = MonitorKey {
                    device: device.device,
                    monitor: monitor.id.clone(),
                };
                found.push((key, device.online));
            }
        }
    }

    copies
        .into_iter()
        .filter(|(_, copies)| copies.len() > 1)
        .map(|(hardware, copies)| {
            let chosen = showing.device_for(hardware);
            let shown = copies
                .iter()
                .position(|(copy, online)| *online && Some(copy.device) == chosen)
                .or_else(|| copies.iter().position(|(_, online)| *online))
                .or_else(|| {
                    copies
                        .iter()
                        .position(|(copy, _)| Some(copy.device) == chosen)
                })
                .unwrap_or(0);
            let mut copies: Vec<MonitorKey> = copies.into_iter().map(|(copy, _)| copy).collect();
            let showing = copies.remove(shown);
            SharedMonitor {
                hardware: hardware.to_string(),
                showing,
                hidden: copies,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Rect;
    use crate::layout::Monitor;

    const A: Fingerprint = Fingerprint::from_bytes([1; 32]);
    const B: Fingerprint = Fingerprint::from_bytes([2; 32]);
    const C: Fingerprint = Fingerprint::from_bytes([3; 32]);
    const MAESTRO: &str = "PKB036E:D2NEE0014200";

    fn monitor(id: &str) -> Monitor {
        Monitor {
            id: id.into(),
            name: id.into(),
            bounds: Rect::new(0.0, 0.0, 1920.0, 1080.0),
            scale: 1.0,
            primary: false,
        }
    }

    fn device(device: Fingerprint, monitors: &[&str], online: bool) -> DeviceMonitors {
        DeviceMonitors {
            device,
            monitors: monitors.iter().map(|id| monitor(id)).collect(),
            online,
        }
    }

    fn identity(monitor: &str, hardware: &str) -> MonitorIdentity {
        MonitorIdentity {
            monitor: monitor.into(),
            hardware: hardware.into(),
        }
    }

    fn key(device: Fingerprint, monitor: &str) -> MonitorKey {
        MonitorKey {
            device,
            monitor: monitor.into(),
        }
    }

    /// A and B both have the Maestro, under their own IDs for it, and a screen of their own.
    fn a_and_b(
        online: bool,
    ) -> (
        Vec<DeviceMonitors>,
        BTreeMap<Fingerprint, Vec<MonitorIdentity>>,
    ) {
        let devices = vec![
            device(A, &["a-laptop", "a-maestro"], true),
            device(B, &["b-laptop", "b-maestro"], online),
        ];
        let identities = BTreeMap::from([
            (A, vec![identity("a-maestro", MAESTRO)]),
            (B, vec![identity("b-maestro", MAESTRO)]),
        ]);
        (devices, identities)
    }

    #[test]
    fn the_same_monitor_on_two_devices_is_one_screen() {
        let (devices, identities) = a_and_b(true);
        let shared = find_shared(&devices, &identities, &Showing::default());
        assert_eq!(
            shared,
            [SharedMonitor {
                hardware: MAESTRO.into(),
                showing: key(A, "a-maestro"),
                hidden: vec![key(B, "b-maestro")],
            }]
        );
        assert_eq!(shared[0].devices(), [A, B]);
    }

    #[test]
    fn it_shows_the_device_it_was_last_said_to_be_showing() {
        let (devices, identities) = a_and_b(true);
        let mut showing = Showing::default();
        showing.choose(MAESTRO, B, 1_000, A);
        let shared = find_shared(&devices, &identities, &showing);
        assert_eq!(shared[0].showing, key(B, "b-maestro"));
        assert_eq!(shared[0].hidden, [key(A, "a-maestro")]);
    }

    /// Monitors switch to a picture that's there by themselves, when theirs goes.
    #[test]
    fn while_that_device_is_away_it_shows_one_that_is_there() {
        let (devices, identities) = a_and_b(false);
        let mut showing = Showing::default();
        showing.choose(MAESTRO, B, 1_000, A);
        let shared = find_shared(&devices, &identities, &showing);
        assert_eq!(shared[0].showing, key(A, "a-maestro"));
    }

    #[test]
    fn different_monitors_of_one_model_stay_apart() {
        let devices = vec![
            device(A, &["a-maestro"], true),
            device(B, &["b-maestro"], true),
        ];
        let identities = BTreeMap::from([
            (A, vec![identity("a-maestro", "PKB036E:D2NEE0014200")]),
            (B, vec![identity("b-maestro", "PKB036E:D2NEE0099999")]),
        ]);
        assert!(find_shared(&devices, &identities, &Showing::default()).is_empty());
    }

    #[test]
    fn a_device_that_doesnt_say_shares_nothing() {
        let (devices, mut identities) = a_and_b(true);
        identities.remove(&B);
        assert!(find_shared(&devices, &identities, &Showing::default()).is_empty());
    }

    /// One it said was there, but isn't any more, like an unplugged one.
    #[test]
    fn only_monitors_there_now_count() {
        let (mut devices, identities) = a_and_b(true);
        devices[1]
            .monitors
            .retain(|monitor| monitor.id != "b-maestro");
        assert!(find_shared(&devices, &identities, &Showing::default()).is_empty());
    }

    #[test]
    fn three_devices_can_share_one_monitor() {
        let (mut devices, mut identities) = a_and_b(true);
        devices.push(device(C, &["c-maestro"], true));
        identities.insert(C, vec![identity("c-maestro", MAESTRO)]);
        let mut showing = Showing::default();
        showing.choose(MAESTRO, C, 1_000, A);
        let shared = find_shared(&devices, &identities, &showing);
        assert_eq!(shared[0].showing, key(C, "c-maestro"));
        assert_eq!(shared[0].hidden, [key(A, "a-maestro"), key(B, "b-maestro")]);
        assert_eq!(shared[0].copy_on(B), Some(&key(B, "b-maestro")));
    }

    #[test]
    fn the_latest_word_wins_wherever_it_comes_from() {
        let mut here = Showing::default();
        here.choose(MAESTRO, A, 1_000, A);
        let mut there = Showing::default();
        there.choose(MAESTRO, B, 2_000, B);

        let mut older = here.clone();
        assert!(older.merge(there.clone()));
        assert_eq!(older.device_for(MAESTRO), Some(B));
        assert!(
            !there.clone().merge(here),
            "the older word doesn't replace it"
        );
        assert!(!older.merge(there), "nothing new");
    }

    /// A device whose clock is behind still gets the last word when it's the last to choose.
    #[test]
    fn choosing_again_is_always_newer_than_what_came_before() {
        let mut showing = Showing::default();
        showing.choose(MAESTRO, B, 5_000, B);
        let before = showing.clone();
        showing.choose(MAESTRO, A, 1_000, A);
        let mut elsewhere = before;
        assert!(elsewhere.merge(showing));
        assert_eq!(elsewhere.device_for(MAESTRO), Some(A));
    }

    #[test]
    fn a_word_on_one_monitor_leaves_the_others_alone() {
        let mut showing = Showing::default();
        showing.choose(MAESTRO, A, 1_000, A);
        showing.choose("DEL40A3:ABC", B, 1_000, A);
        assert_eq!(showing.device_for(MAESTRO), Some(A));
        assert_eq!(showing.device_for("DEL40A3:ABC"), Some(B));
        assert_eq!(showing.device_for("nothing"), None);
    }

    #[test]
    fn a_word_too_long_to_be_real_is_ignored() {
        let mut showing = Showing::default();
        let mut sent = Showing::default();
        sent.choose(&"x".repeat(MAX_HARDWARE_LENGTH + 1), A, 1_000, A);
        assert!(!showing.merge(sent));
    }
}
