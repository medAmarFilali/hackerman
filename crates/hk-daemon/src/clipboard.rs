//! Keeps track of which clipboard is the newest and whether this device has it.

use std::time::{SystemTime, UNIX_EPOCH};

use hk_core::clipboard::ClipVersion;
use hk_core::device::Fingerprint;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

#[derive(Debug)]
pub struct ClipboardSync {
    me: Fingerprint,
    /// The newest clipboard known on any device.
    latest: Option<ClipVersion>,
    /// The version this device's own clipboard holds.
    held: Option<ClipVersion>,
    /// The version already asked for, so it's only asked for once.
    fetching: Option<ClipVersion>,
}

impl ClipboardSync {
    pub fn new(me: Fingerprint) -> Self {
        Self {
            me,
            latest: None,
            held: None,
            fetching: None,
        }
    }

    /// Something new was copied on this device. Returns the version to announce.
    pub fn copied(&mut self) -> ClipVersion {
        let newest_seen = self.latest.map_or(0, |latest| latest.counter);
        let version = ClipVersion {
            counter: now_ms().max(newest_seen + 1),
            owner: self.me,
        };
        self.latest = Some(version);
        self.held = Some(version);
        self.fetching = None;
        version
    }

    /// Another device announced a version. Returns whether it's the newest so far.
    pub fn offered(&mut self, version: ClipVersion) -> bool {
        if self.latest.is_some_and(|latest| latest >= version) {
            return false;
        }
        self.latest = Some(version);
        true
    }

    /// The version to ask for now, if this device is behind. Each one is only
    /// returned once, until something changes.
    pub fn next_to_fetch(&mut self) -> Option<ClipVersion> {
        let latest = self.latest?;
        if self.held == Some(latest) || self.fetching == Some(latest) || latest.owner == self.me {
            return None;
        }
        self.fetching = Some(latest);
        Some(latest)
    }

    /// Whether content that arrived is still the one wanted.
    #[cfg(test)]
    fn wants(&self, version: ClipVersion) -> bool {
        self.latest == Some(version)
    }

    /// The version that content arriving from `device` should be, if any.
    pub fn next_wanted(&self, device: Fingerprint) -> Option<ClipVersion> {
        self.latest.filter(|latest| latest.owner == device)
    }

    /// The content of `version` is now on this device's clipboard.
    pub fn applied(&mut self, version: ClipVersion) {
        self.held = Some(version);
        if self.fetching == Some(version) {
            self.fetching = None;
        }
    }

    /// Whether this device can supply `version`.
    pub fn owns(&self, version: ClipVersion) -> bool {
        version.owner == self.me && self.held == Some(version)
    }

    /// This device's clipboard version, if it's the newest, to tell a device that
    /// has just connected.
    pub fn own_offer(&self) -> Option<ClipVersion> {
        self.held
            .filter(|held| held.owner == self.me && self.latest == Some(*held))
    }

    /// This device's clipboard changed while sharing was off. That stays private, so
    /// it isn't announced, and this device mustn't supply it for an older version. It's
    /// also what the user wants here now, so nothing offered earlier should replace it.
    pub fn changed_privately(&mut self) {
        self.held = self.latest.filter(|latest| latest.owner != self.me);
        self.fetching = None;
    }

    /// A device went away, so anything being fetched from it isn't coming.
    pub fn device_gone(&mut self, device: Fingerprint) {
        if self.fetching.is_some_and(|version| version.owner == device) {
            self.fetching = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: Fingerprint = Fingerprint::from_bytes([1; 32]);
    const OTHER: Fingerprint = Fingerprint::from_bytes([2; 32]);

    fn from_other(counter: u64) -> ClipVersion {
        ClipVersion {
            counter,
            owner: OTHER,
        }
    }

    #[test]
    fn a_private_change_cancels_what_was_going_to_be_fetched() {
        let mut sync = ClipboardSync::new(ME);
        assert!(sync.offered(from_other(1)));
        sync.changed_privately();
        assert_eq!(sync.next_to_fetch(), None);
        assert_eq!(sync.own_offer(), None);

        // Something newer still comes through.
        assert!(sync.offered(from_other(2)));
        assert_eq!(sync.next_to_fetch(), Some(from_other(2)));
    }

    #[test]
    fn a_private_change_is_never_supplied_as_an_older_version() {
        let mut sync = ClipboardSync::new(ME);
        let shared = sync.copied();
        assert!(sync.owns(shared));
        sync.changed_privately();
        assert!(!sync.owns(shared));
        assert_eq!(sync.own_offer(), None);
        assert_eq!(sync.next_to_fetch(), None);
    }

    #[test]
    fn each_copy_is_newer_than_everything_seen() {
        let mut sync = ClipboardSync::new(ME);
        let first = sync.copied();
        // Even from a device whose clock is ahead of this one's.
        let ahead = from_other(first.counter + 3_600_000);
        sync.offered(ahead);
        let second = sync.copied();
        assert!(second > first);
        assert!(second > ahead);
    }

    /// This used to count up from 1 after every start, so the first copies after a
    /// restart lost to whatever the other devices had already seen, and went nowhere.
    #[test]
    fn a_copy_after_a_restart_still_counts_as_the_newest() {
        let mut other_device = ClipboardSync::new(OTHER);
        for _ in 0..50 {
            other_device.copied();
        }
        let before = other_device.latest.unwrap();

        let mut restarted = ClipboardSync::new(ME);
        // Fifty copies in a moment put the count a little ahead of the clock.
        std::thread::sleep(std::time::Duration::from_millis(100));
        let copy = restarted.copied();
        assert!(other_device.offered(copy), "{copy:?} vs {before:?}");
    }

    #[test]
    fn older_offers_are_ignored() {
        let mut sync = ClipboardSync::new(ME);
        assert!(sync.offered(from_other(5)));
        assert!(!sync.offered(from_other(5)));
        assert!(!sync.offered(from_other(4)));
        assert!(sync.offered(from_other(6)));
    }

    #[test]
    fn fetches_the_newest_once() {
        let mut sync = ClipboardSync::new(ME);
        assert_eq!(sync.next_to_fetch(), None);
        sync.offered(from_other(1));
        assert_eq!(sync.next_to_fetch(), Some(from_other(1)));
        assert_eq!(sync.next_to_fetch(), None, "already asked");

        sync.offered(from_other(2));
        assert_eq!(sync.next_to_fetch(), Some(from_other(2)));
    }

    #[test]
    fn nothing_to_fetch_once_it_is_here() {
        let mut sync = ClipboardSync::new(ME);
        sync.offered(from_other(1));
        sync.next_to_fetch();
        assert!(sync.wants(from_other(1)));
        sync.applied(from_other(1));
        assert_eq!(sync.next_to_fetch(), None);
    }

    #[test]
    fn content_overtaken_by_a_newer_copy_is_not_wanted() {
        let mut sync = ClipboardSync::new(ME);
        sync.offered(from_other(1));
        sync.next_to_fetch();
        sync.offered(from_other(2));
        assert!(!sync.wants(from_other(1)));
        assert!(sync.wants(from_other(2)));
    }

    #[test]
    fn copying_here_cancels_any_fetch() {
        let mut sync = ClipboardSync::new(ME);
        sync.offered(from_other(1));
        sync.next_to_fetch();
        sync.copied();
        assert_eq!(sync.next_to_fetch(), None);
        assert!(!sync.wants(from_other(1)));
    }

    #[test]
    fn can_ask_again_if_the_owner_went_away() {
        let mut sync = ClipboardSync::new(ME);
        sync.offered(from_other(1));
        sync.next_to_fetch();
        sync.device_gone(OTHER);
        assert_eq!(sync.next_to_fetch(), Some(from_other(1)));
    }

    #[test]
    fn only_supplies_what_it_holds_and_owns() {
        let mut sync = ClipboardSync::new(ME);
        let mine = sync.copied();
        assert!(sync.owns(mine));
        assert_eq!(sync.own_offer(), Some(mine));

        // Newer content arrived from elsewhere: there's nothing of ours to offer.
        let newer = from_other(mine.counter + 1);
        sync.offered(newer);
        sync.next_to_fetch();
        sync.applied(newer);
        assert!(!sync.owns(mine));
        assert!(!sync.owns(newer), "it belongs to the other device");
        assert_eq!(sync.own_offer(), None);
    }
}
