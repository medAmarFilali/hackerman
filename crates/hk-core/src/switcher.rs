//! Decides, for every local input event, which device it goes to.
//!
//! The device you physically touch is in charge. While it controls another
//! device, it parks its own cursor and keeps track of where the cursor is on
//! the other device's desktop, sending absolute positions. That way it alone
//! decides when the cursor crosses to the next device, with no round trips, and
//! going from one remote device to a third works the same as coming home.
//!
//! All positions here are in a device's own desktop pixels. Where the cursor goes
//! when it leaves a monitor is worked out on the canvas by [`Topology`], and the
//! canvas outranks the operating system. Someone can pull a device's two monitors
//! apart on the canvas, with other devices between them, to match their desk: the
//! cursor then visits the devices in between, even though the operating system
//! would go straight from one monitor to the other. Only where the canvas shows
//! nothing past an edge does the operating system's arrangement apply, so that a
//! monitor can never become unreachable.

use std::collections::HashSet;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::apps::MediaCommand;
use crate::device::Fingerprint;
use crate::geometry::{Point, Rect};
use crate::input::{InputEvent, KeyCode, MouseButton};
use crate::layout::{Crossing, Side, Topology, clamp_to_pixels};
use crate::rules::{CrossModifier, Rules, Shortcut};

/// How far from where it was parked the cursor has to be for something to have moved it.
/// Nearer than this can be a single report from the mouse.
const PARKED_CURSOR_MOVED: f64 = 150.0;

/// Input from this machine's own keyboard and mouse. On Windows it can come from another
/// program on the same machine, which is what serializing it is for.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum LocalEvent {
    /// The cursor is about to move from one position to another. Where it is now
    /// has to come from the operating system at that moment: a remembered position
    /// goes stale whenever another device moves this device's cursor.
    Motion {
        from: Point,
        to: Point,
    },
    Button {
        button: MouseButton,
        pressed: bool,
    },
    Scroll {
        dx: f64,
        dy: f64,
    },
    Key {
        #[serde(with = "crate::input::key_code_by_name")]
        code: KeyCode,
        pressed: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    /// Tell `device` it is now being controlled by this machine.
    Enter(Fingerprint),
    /// Tell `device` it is no longer being controlled.
    Leave(Fingerprint),
    Forward(Fingerprint, InputEvent),
    /// Move this machine's own cursor out of the way and hide it, while input
    /// goes to another device.
    ParkCursor(Point),
    /// Show this machine's own cursor again, at this position.
    RestoreCursor(Point),
    /// Something the user should be able to see has changed.
    StateChanged,
    /// A media shortcut was pressed. It's for whatever is playing, wherever that is.
    Media(MediaCommand),
}

#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    /// Whether the event must be hidden from this machine.
    pub swallow: bool,
    pub actions: Vec<Action>,
}

impl Outcome {
    fn pass() -> Self {
        Self::default()
    }

    fn swallow(actions: Vec<Action>) -> Self {
        Self {
            swallow: true,
            actions,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Remote {
    device: Fingerprint,
    cursor: Point,
}

#[derive(Debug)]
pub struct Switcher {
    me: Fingerprint,
    topology: Topology,
    park: Point,
    /// Set while this machine controls another device.
    remote: Option<Remote>,
    /// Where the local cursor goes if control returns without crossing an edge.
    left_from: Point,
    /// Where the local cursor last was while input stayed on this machine.
    last_position: Point,
    // Each release must go where its press went. A key pressed here and
    // released after crossing would otherwise stay stuck down on this machine.
    local_keys: HashSet<KeyCode>,
    remote_keys: HashSet<KeyCode>,
    local_buttons: HashSet<MouseButton>,
    remote_buttons: HashSet<MouseButton>,
    /// Keys used up by a shortcut, whose release must be hidden too.
    shortcut_keys: HashSet<KeyCode>,
    rules: Rules,
    /// The user asked for the cursor to stay on the current device.
    locked: bool,
    /// A full-screen app is in front on this machine.
    fullscreen: bool,
    /// Which edge the cursor is pushing against, and since when.
    edge_contact: Option<(Side, Duration)>,
    /// What the Ctrl+Alt+number shortcuts lead to: the first is number 1.
    jump_order: Vec<Fingerprint>,
    /// Something somewhere can be played or paused. If nothing can, media shortcuts stay
    /// with the program in front, which may have a use for those keys.
    media_available: bool,
    /// The user is choosing a new shortcut, so the keys are for the window asking.
    shortcuts_paused: bool,
    now: Duration,
}

impl Switcher {
    /// `park` is where the local cursor waits while input goes elsewhere. It
    /// must be inside the desktop and away from its edges, so that movement in
    /// every direction still registers.
    pub fn new(me: Fingerprint, park: Point) -> Self {
        Self {
            me,
            topology: Topology::default(),
            park,
            remote: None,
            left_from: park,
            last_position: park,
            local_keys: HashSet::new(),
            remote_keys: HashSet::new(),
            local_buttons: HashSet::new(),
            remote_buttons: HashSet::new(),
            shortcut_keys: HashSet::new(),
            rules: Rules::default(),
            locked: false,
            fullscreen: false,
            edge_contact: None,
            jump_order: Vec::new(),
            media_available: false,
            shortcuts_paused: false,
            now: Duration::ZERO,
        }
    }

    /// The device this machine is controlling, if any.
    pub fn controlling(&self) -> Option<Fingerprint> {
        self.remote.map(|remote| remote.device)
    }

    pub fn locked(&self) -> bool {
        self.locked
    }

    pub fn set_locked(&mut self, locked: bool) {
        self.locked = locked;
    }

    pub fn set_rules(&mut self, rules: Rules) {
        self.rules = rules;
    }

    pub fn set_jump_order(&mut self, order: Vec<Fingerprint>) {
        self.jump_order = order;
    }

    /// Says whether anything on any device can be played or paused.
    pub fn set_media_available(&mut self, available: bool) {
        self.media_available = available;
    }

    /// Leaves every shortcut alone while the user is choosing a new one. Ctrl+Alt+Home isn't
    /// one of these: it's the way back from anything.
    pub fn set_shortcuts_paused(&mut self, paused: bool) {
        self.shortcuts_paused = paused;
    }

    /// Says whether a full-screen app is in front on this machine.
    pub fn set_fullscreen(&mut self, fullscreen: bool) {
        self.fullscreen = fullscreen;
    }

    /// Whether the cursor is being kept here because of a full-screen app.
    pub fn held_by_fullscreen(&self) -> bool {
        self.fullscreen && self.rules.stay_in_fullscreen && self.remote.is_none()
    }

    /// Monitors, the layout or connected devices changed.
    pub fn set_environment(&mut self, topology: Topology, park: Point) -> Vec<Action> {
        self.topology = topology;
        self.park = park;
        let Some(remote) = self.remote else {
            return vec![];
        };
        let online = self
            .topology
            .device_screens(remote.device)
            .any(|screen| screen.online);
        if online {
            vec![]
        } else {
            self.return_home_to(self.left_from)
        }
    }

    /// Another device started controlling this one, so this one must let go.
    pub fn peer_took_control(&mut self) -> Vec<Action> {
        self.return_home_to(self.left_from)
    }

    /// Input capture is about to stop, so control comes back here. Nothing can stay
    /// marked as held, since its release won't be seen.
    pub fn capture_stopping(&mut self) -> Vec<Action> {
        let actions = self.return_home_to(self.left_from);
        self.local_keys.clear();
        self.local_buttons.clear();
        self.shortcut_keys.clear();
        self.edge_contact = None;
        actions
    }

    /// `now` is the time of the event, measured from any fixed starting point.
    pub fn handle_local(&mut self, event: LocalEvent, now: Duration) -> Outcome {
        self.now = now;
        if let LocalEvent::Key { code, pressed } = event
            && let Some(outcome) = self.handle_shortcut(code, pressed)
        {
            return outcome;
        }
        match self.remote {
            None => self.handle_at_home(event),
            Some(remote) => self.handle_remote(remote, event),
        }
    }

    /// Ctrl + left Alt + L locks the cursor to the current device, and Ctrl + left
    /// Alt + a number jumps to a device. Only the left Alt counts, because Windows
    /// reports AltGr as Ctrl + right Alt, and many keyboard layouts use AltGr with
    /// the number row and other keys to type characters such as @ and #.
    fn handle_shortcut(&mut self, code: KeyCode, pressed: bool) -> Option<Outcome> {
        if !pressed {
            return self
                .shortcut_keys
                .remove(&code)
                .then(|| Outcome::swallow(vec![]));
        }
        // A held key repeats, which shouldn't toggle the lock back and forth.
        if self.shortcut_keys.contains(&code) {
            return Some(Outcome::swallow(vec![]));
        }
        if self.shortcuts_paused {
            return None;
        }
        let held = |code| self.local_keys.contains(&code) || self.remote_keys.contains(&code);
        let modifiers =
            (held(KeyCode::ControlLeft) || held(KeyCode::ControlRight)) && held(KeyCode::AltLeft);
        if self.rules.shortcuts && modifiers {
            if code == KeyCode::KeyL {
                self.shortcut_keys.insert(code);
                self.locked = !self.locked;
                return Some(Outcome::swallow(vec![Action::StateChanged]));
            }
            if let Some(index) = jump_index(code) {
                self.shortcut_keys.insert(code);
                let actions = match self.jump_order.get(index).copied() {
                    Some(device) => self.jump_to(device),
                    None => vec![],
                };
                return Some(Outcome::swallow(actions));
            }
        }
        let command = self.media_shortcut(code)?;
        self.shortcut_keys.insert(code);
        Some(Outcome::swallow(vec![Action::Media(command)]))
    }

    /// Which media shortcut this key completes, if any. It has to be just those modifiers,
    /// so that a shortcut on Ctrl+Space doesn't also take Ctrl+Shift+Space. As above, AltGr
    /// doesn't count as anything: Windows reports it as Ctrl + right Alt.
    fn media_shortcut(&self, code: KeyCode) -> Option<MediaCommand> {
        let shortcuts = &self.rules.media_shortcuts;
        if !shortcuts.enabled || !self.media_available {
            return None;
        }
        let held = |code| self.local_keys.contains(&code) || self.remote_keys.contains(&code);
        let ctrl = held(KeyCode::ControlLeft) || held(KeyCode::ControlRight);
        if ctrl && held(KeyCode::AltRight) {
            return None;
        }
        let pressed = Shortcut {
            ctrl,
            alt: held(KeyCode::AltLeft) || held(KeyCode::AltRight),
            shift: held(KeyCode::ShiftLeft) || held(KeyCode::ShiftRight),
            meta: held(KeyCode::MetaLeft) || held(KeyCode::MetaRight),
            key: Some(code),
        };
        [
            (shortcuts.play_pause, MediaCommand::PlayPause),
            (shortcuts.back, MediaCommand::Back),
            (shortcuts.forward, MediaCommand::Forward),
        ]
        .into_iter()
        .find_map(|(shortcut, command)| (shortcut == pressed).then_some(command))
    }

    /// Moves control straight to the middle of `device`'s main monitor. A
    /// deliberate jump isn't subject to the lock, which is about stray movement.
    fn jump_to(&mut self, device: Fingerprint) -> Vec<Action> {
        if device == self.me {
            return self.return_home_to(self.left_from);
        }
        if self.controlling() == Some(device) {
            return vec![];
        }
        let screens = || self.topology.device_screens(device).filter(|s| s.online);
        let Some(screen) = screens()
            .find(|screen| screen.monitor.primary)
            .or_else(|| screens().next())
        else {
            return vec![];
        };
        let b = screen.monitor.bounds;
        let cursor = Point::new(
            b.x + (b.width / 2.0).floor(),
            b.y + (b.height / 2.0).floor(),
        );

        let mut actions = Vec::with_capacity(3);
        match self.remote {
            Some(remote) => {
                self.remote_keys.clear();
                self.remote_buttons.clear();
                actions.push(Action::Leave(remote.device));
                actions.push(Action::Enter(device));
            }
            None => {
                self.left_from = self.last_position;
                actions.push(Action::Enter(device));
                actions.push(Action::ParkCursor(self.park));
            }
        }
        actions.push(position_event(device, cursor));
        self.remote = Some(Remote { device, cursor });
        self.edge_contact = None;
        actions
    }

    /// Whether the settings let the cursor cross to another device from here: with
    /// the right key held if one is needed, and away from the corners of the screen.
    fn crossing_allowed(&self, side: Side, at: Point, bounds: Rect) -> bool {
        let held = |code| self.local_keys.contains(&code) || self.remote_keys.contains(&code);
        let key_is_held = match self.rules.cross_modifier {
            CrossModifier::None => true,
            CrossModifier::Ctrl => held(KeyCode::ControlLeft) || held(KeyCode::ControlRight),
            CrossModifier::Shift => held(KeyCode::ShiftLeft) || held(KeyCode::ShiftRight),
            CrossModifier::Alt => held(KeyCode::AltLeft) || held(KeyCode::AltRight),
        };
        let to_corner = match side {
            Side::Left | Side::Right => (at.y - bounds.y).min(bounds.bottom() - 1.0 - at.y),
            Side::Top | Side::Bottom => (at.x - bounds.x).min(bounds.right() - 1.0 - at.x),
        };
        key_is_held && to_corner >= f64::from(self.rules.corner_guard)
    }

    /// With a crossing delay set, whether the cursor has now been pushing against
    /// `side` for long enough.
    fn pushed_long_enough(&mut self, side: Side) -> bool {
        let delay = Duration::from_millis(u64::from(self.rules.edge_delay_ms));
        if delay.is_zero() {
            return true;
        }
        match self.edge_contact {
            Some((touching, since)) if touching == side => self.now.saturating_sub(since) >= delay,
            _ => {
                self.edge_contact = Some((side, self.now));
                false
            }
        }
    }

    fn handle_at_home(&mut self, event: LocalEvent) -> Outcome {
        match event {
            LocalEvent::Motion { from, to } => {
                let outcome = self.motion_at_home(from, to);
                if !outcome.swallow {
                    self.last_position = to;
                }
                outcome
            }
            LocalEvent::Button { button, pressed } => {
                if pressed {
                    self.local_buttons.insert(button);
                } else {
                    self.local_buttons.remove(&button);
                }
                Outcome::pass()
            }
            LocalEvent::Scroll { .. } => Outcome::pass(),
            LocalEvent::Key { code, pressed } => {
                if pressed {
                    self.local_keys.insert(code);
                } else {
                    self.local_keys.remove(&code);
                }
                Outcome::pass()
            }
        }
    }

    fn motion_at_home(&mut self, from: Point, to: Point) -> Outcome {
        let Some(screen) = self
            .topology
            .screen_at(self.me, from)
            .or_else(|| self.topology.nearest_screen(self.me, from))
            .cloned()
        else {
            return Outcome::pass();
        };
        let bounds = screen.monitor.bounds;
        // The operating system is about to carry the cursor to another monitor here.
        let entering = self
            .topology
            .screen_at(self.me, to)
            .filter(|target| target.monitor.id != screen.monitor.id)
            .map(|target| target.monitor.id.clone());

        let exit = Side::ALL
            .into_iter()
            .filter(|&side| {
                if entering.is_some() {
                    is_past(to, bounds, side)
                } else {
                    // The operating system stops the cursor at the edge of the
                    // desktop, so reaching the last pixel row or column counts.
                    is_on_edge(to, bounds, side)
                        && self
                            .topology
                            .screen_at(self.me, outward(clamp_to_pixels(to, bounds), side))
                            .is_none()
                }
            })
            .find_map(|side| {
                let crossing = self.topology.crossing(&screen, side, to)?;
                Some((side, crossing))
            });
        let Some((side, crossing)) = exit else {
            self.edge_contact = None;
            return Outcome::pass();
        };

        if crossing.device == self.me {
            // If this is where the operating system was going anyway, let it.
            let target = &self.topology.screens[crossing.screen].monitor.id;
            if entering.as_ref() == Some(target) && is_level(crossing.entry, to, side) {
                return Outcome::pass();
            }
            // One pixel inside the edge, so moving along it doesn't immediately cross again.
            let cursor = inset_entry(crossing.entry, side);
            self.last_position = cursor;
            return Outcome::swallow(vec![Action::RestoreCursor(cursor)]);
        }

        // Crossing in the middle of a drag would leave the drag half done. Staying
        // on this device is still fine, so the operating system can carry on.
        if !self.local_buttons.is_empty()
            || self.locked
            || self.held_by_fullscreen()
            || !self.crossing_allowed(side, to, bounds)
        {
            return Outcome::pass();
        }
        if !self.pushed_long_enough(side) {
            // Held at the edge, rather than slipping onto the next local monitor.
            return if entering.is_some() {
                Outcome::swallow(vec![])
            } else {
                Outcome::pass()
            };
        }
        self.edge_contact = None;
        self.left_from = inset(clamp_to_pixels(to, bounds), side);
        self.remote = Some(Remote {
            device: crossing.device,
            cursor: crossing.entry,
        });
        Outcome::swallow(vec![
            Action::Enter(crossing.device),
            Action::ParkCursor(self.park),
            position_event(crossing.device, crossing.entry),
        ])
    }

    fn handle_remote(&mut self, remote: Remote, event: LocalEvent) -> Outcome {
        match event {
            LocalEvent::Motion { from, to: position } => {
                // The local cursor stays parked, so every move is an offset from the park point.
                let dx = position.x - self.park.x;
                let dy = position.y - self.park.y;
                if dx == 0.0 && dy == 0.0 {
                    return Outcome::swallow(vec![]);
                }
                // Unless something else has moved it, or Windows dropped the input hook for a
                // while and the mouse did. Measuring from the park point would then send the
                // other cursor flying with every report, so it goes back, and the move counts
                // from where it really started.
                let away = (from.x - self.park.x)
                    .abs()
                    .max((from.y - self.park.y).abs());
                if away > PARKED_CURSOR_MOVED {
                    let moved = Point::new(
                        remote.cursor.x + position.x - from.x,
                        remote.cursor.y + position.y - from.y,
                    );
                    let mut actions = vec![Action::ParkCursor(self.park)];
                    actions.extend(self.move_remote_cursor(remote, moved));
                    return Outcome::swallow(actions);
                }
                let moved = Point::new(remote.cursor.x + dx, remote.cursor.y + dy);
                Outcome::swallow(self.move_remote_cursor(remote, moved))
            }
            LocalEvent::Button { button, pressed } => {
                if pressed {
                    self.remote_buttons.insert(button);
                } else {
                    self.remote_buttons.remove(&button);
                }
                let event = InputEvent::Button { button, pressed };
                Outcome::swallow(vec![Action::Forward(remote.device, event)])
            }
            LocalEvent::Scroll { dx, dy } => {
                let event = InputEvent::Scroll { dx, dy };
                Outcome::swallow(vec![Action::Forward(remote.device, event)])
            }
            LocalEvent::Key { code, pressed } => {
                if !pressed && self.local_keys.remove(&code) {
                    return Outcome::pass();
                }
                // Ctrl+Alt+Home always brings control back, even if the peer misbehaves.
                if pressed && code == KeyCode::Home && self.ctrl_and_alt_held() {
                    return Outcome::swallow(self.return_home_to(self.left_from));
                }
                if pressed {
                    self.remote_keys.insert(code);
                } else {
                    self.remote_keys.remove(&code);
                }
                let event = InputEvent::Key { code, pressed };
                Outcome::swallow(vec![Action::Forward(remote.device, event)])
            }
        }
    }

    fn move_remote_cursor(&mut self, remote: Remote, moved: Point) -> Vec<Action> {
        let device = remote.device;
        let Some(screen) = self
            .topology
            .screen_at(device, remote.cursor)
            .or_else(|| self.topology.nearest_screen(device, remote.cursor))
            .cloned()
        else {
            return self.return_home_to(self.left_from);
        };

        let bounds = screen.monitor.bounds;
        if bounds.contains(moved) {
            self.edge_contact = None;
            return self.place_remote_cursor(remote, moved);
        }

        let exit = Side::ALL
            .into_iter()
            .filter(|&side| is_past(moved, bounds, side))
            .find_map(|side| {
                let crossing = self.topology.crossing(&screen, side, moved)?;
                Some((side, crossing))
            });
        // The monitor the device's own arrangement would put the cursor on, if any.
        let entering = self.topology.screen_at(device, moved);

        if let Some((side, crossing)) = exit {
            if crossing.device == device {
                let target = &self.topology.screens[crossing.screen].monitor.id;
                let going_there_anyway = entering
                    .is_some_and(|screen| &screen.monitor.id == target)
                    && is_level(crossing.entry, moved, side);
                self.edge_contact = None;
                let cursor = if going_there_anyway {
                    moved
                } else {
                    crossing.entry
                };
                return self.place_remote_cursor(remote, cursor);
            }
            if self.remote_buttons.is_empty()
                && !self.locked
                && self.crossing_allowed(side, moved, bounds)
            {
                if self.pushed_long_enough(side) {
                    return self.cross_from_remote(remote, side, crossing);
                }
                // Held at the edge until it has been pushed for long enough.
                return self.place_remote_cursor(remote, clamp_to_pixels(moved, bounds));
            }
        }

        let cursor = if entering.is_some() {
            moved
        } else {
            clamp_to_pixels(moved, bounds)
        };
        self.place_remote_cursor(remote, cursor)
    }

    fn place_remote_cursor(&mut self, remote: Remote, cursor: Point) -> Vec<Action> {
        let device = remote.device;
        self.remote = Some(Remote { device, cursor });
        if cursor == remote.cursor {
            vec![]
        } else {
            vec![position_event(device, cursor)]
        }
    }

    fn cross_from_remote(&mut self, remote: Remote, side: Side, crossing: Crossing) -> Vec<Action> {
        self.edge_contact = None;
        if crossing.device == self.me {
            // One pixel inside the edge, so moving along it doesn't immediately cross again.
            return self.return_home_to(inset_entry(crossing.entry, side));
        }
        self.remote_keys.clear();
        self.remote = Some(Remote {
            device: crossing.device,
            cursor: crossing.entry,
        });
        vec![
            Action::Leave(remote.device),
            Action::Enter(crossing.device),
            position_event(crossing.device, crossing.entry),
        ]
    }

    fn return_home_to(&mut self, cursor: Point) -> Vec<Action> {
        let Some(remote) = self.remote.take() else {
            return vec![];
        };
        // The device releases whatever is still pressed when it hears we left.
        self.remote_keys.clear();
        self.remote_buttons.clear();
        self.last_position = cursor;
        vec![Action::Leave(remote.device), Action::RestoreCursor(cursor)]
    }

    fn ctrl_and_alt_held(&self) -> bool {
        let held = |code| self.local_keys.contains(&code) || self.remote_keys.contains(&code);
        (held(KeyCode::ControlLeft) || held(KeyCode::ControlRight))
            && (held(KeyCode::AltLeft) || held(KeyCode::AltRight))
    }
}

/// Which entry in the jump order a number key means: the 1 key is the first.
fn jump_index(code: KeyCode) -> Option<usize> {
    const NUMBER_KEYS: [KeyCode; 9] = [
        KeyCode::Digit1,
        KeyCode::Digit2,
        KeyCode::Digit3,
        KeyCode::Digit4,
        KeyCode::Digit5,
        KeyCode::Digit6,
        KeyCode::Digit7,
        KeyCode::Digit8,
        KeyCode::Digit9,
    ];
    NUMBER_KEYS.iter().position(|&key| key == code)
}

fn position_event(device: Fingerprint, at: Point) -> Action {
    Action::Forward(device, InputEvent::PointerPosition { x: at.x, y: at.y })
}

/// Whether `p` is beyond `side` of `bounds`.
fn is_past(p: Point, bounds: Rect, side: Side) -> bool {
    match side {
        Side::Left => p.x < bounds.x,
        Side::Right => p.x >= bounds.right(),
        Side::Top => p.y < bounds.y,
        Side::Bottom => p.y >= bounds.bottom(),
    }
}

/// Whether `p` is on the last pixel row or column at `side` of `bounds`, or beyond.
fn is_on_edge(p: Point, bounds: Rect, side: Side) -> bool {
    match side {
        Side::Left => p.x <= bounds.x,
        Side::Right => p.x >= bounds.right() - 1.0,
        Side::Top => p.y <= bounds.y,
        Side::Bottom => p.y >= bounds.bottom() - 1.0,
    }
}

/// Whether two positions on either side of an edge line up, near enough that
/// nobody would notice the difference. Mixed scales round slightly differently.
fn is_level(a: Point, b: Point, side: Side) -> bool {
    const TOLERANCE: f64 = 2.0;
    match side {
        Side::Left | Side::Right => (a.y - b.y).abs() <= TOLERANCE,
        Side::Top | Side::Bottom => (a.x - b.x).abs() <= TOLERANCE,
    }
}

/// One pixel past `side` of `p`.
fn outward(p: Point, side: Side) -> Point {
    match side {
        Side::Left => Point::new(p.x - 1.0, p.y),
        Side::Right => Point::new(p.x + 1.0, p.y),
        Side::Top => Point::new(p.x, p.y - 1.0),
        Side::Bottom => Point::new(p.x, p.y + 1.0),
    }
}

/// One pixel back from `side`: where to put the cursor that left through it, so
/// that it's near the edge again without touching it.
fn inset(p: Point, side: Side) -> Point {
    match side {
        Side::Left => Point::new(p.x + 1.0, p.y),
        Side::Right => Point::new(p.x - 1.0, p.y),
        Side::Top => Point::new(p.x, p.y + 1.0),
        Side::Bottom => Point::new(p.x, p.y - 1.0),
    }
}

/// `entry` is on the edge of the monitor the cursor arrives at, which is the
/// opposite of the `side` it left through.
fn inset_entry(entry: Point, side: Side) -> Point {
    match side {
        Side::Left => Point::new(entry.x - 1.0, entry.y),
        Side::Right => Point::new(entry.x + 1.0, entry.y),
        Side::Top => Point::new(entry.x, entry.y - 1.0),
        Side::Bottom => Point::new(entry.x, entry.y + 1.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{DeviceMonitors, LayoutDoc, Monitor, Placement};

    const ME: Fingerprint = Fingerprint::from_bytes([1; 32]);
    const PEER: Fingerprint = Fingerprint::from_bytes([2; 32]);
    const THIRD: Fingerprint = Fingerprint::from_bytes([3; 32]);
    const PARK: Point = Point::new(960.0, 540.0);

    fn monitor(id: &str, x: f64, y: f64, width: f64, height: f64) -> Monitor {
        Monitor {
            id: id.into(),
            name: id.into(),
            bounds: Rect::new(x, y, width, height),
            scale: 1.0,
            primary: x == 0.0 && y == 0.0,
        }
    }

    fn device(device: Fingerprint, monitors: Vec<Monitor>) -> DeviceMonitors {
        DeviceMonitors {
            device,
            monitors,
            online: true,
        }
    }

    fn place(device: Fingerprint, monitor: &str, x: f64, y: f64) -> Placement {
        Placement {
            device,
            monitor: monitor.into(),
            x,
            y,
            width: None,
        }
    }

    fn switcher_with(devices: &[DeviceMonitors], placements: Vec<Placement>) -> Switcher {
        let mut layout = LayoutDoc::default();
        layout.apply_edit(ME, devices, placements);
        let mut switcher = Switcher::new(ME, PARK);
        switcher.set_environment(Topology::build(devices, &layout), PARK);
        switcher
    }

    /// This 1920x1080 machine with a 2560x1440 peer against its right edge.
    fn peer_on_the_right() -> Switcher {
        switcher_with(
            &[
                device(ME, vec![monitor("me", 0.0, 0.0, 1920.0, 1080.0)]),
                device(PEER, vec![monitor("peer", 0.0, 0.0, 2560.0, 1440.0)]),
            ],
            vec![place(ME, "me", 0.0, 0.0), place(PEER, "peer", 1920.0, 0.0)],
        )
    }

    impl Switcher {
        /// Handles an event at the same moment as the last one.
        fn local(&mut self, event: LocalEvent) -> Outcome {
            self.handle_local(event, self.now)
        }
    }

    fn motion(x: f64, y: f64) -> LocalEvent {
        let position = Point::new(x, y);
        LocalEvent::Motion {
            from: position,
            to: position,
        }
    }

    /// A move from one position to another, at home.
    fn moving(from_x: f64, from_y: f64, to_x: f64, to_y: f64) -> LocalEvent {
        LocalEvent::Motion {
            from: Point::new(from_x, from_y),
            to: Point::new(to_x, to_y),
        }
    }

    /// Moves the parked cursor by (dx, dy) while controlling another device.
    fn nudge(dx: f64, dy: f64) -> LocalEvent {
        moving(PARK.x, PARK.y, PARK.x + dx, PARK.y + dy)
    }

    fn key(code: KeyCode, pressed: bool) -> LocalEvent {
        LocalEvent::Key { code, pressed }
    }

    fn at(device: Fingerprint, x: f64, y: f64) -> Action {
        position_event(device, Point::new(x, y))
    }

    /// What happens if Windows drops the input hook while the cursor is parked: the mouse
    /// moves it away, and then the hook comes back.
    #[test]
    fn a_parked_cursor_that_got_moved_goes_back_without_throwing_the_other_cursor_off() {
        let mut s = peer_on_the_right();
        s.local(motion(1919.0, 540.0));
        assert_eq!(s.controlling(), Some(PEER));

        // Measured from where it was parked, this one report would be a leap of over 800 pixels.
        let far_away = Point::new(PARK.x + 800.0, PARK.y + 400.0);
        let outcome = s.local(LocalEvent::Motion {
            from: far_away,
            to: Point::new(far_away.x + 3.0, far_away.y - 2.0),
        });
        assert!(outcome.swallow);
        assert_eq!(
            outcome.actions,
            [Action::ParkCursor(PARK), at(PEER, 3.0, 538.0)]
        );

        // And it carries on from the park point as usual.
        assert_eq!(s.local(nudge(5.0, 5.0)).actions, [at(PEER, 8.0, 543.0)]);
    }

    #[test]
    fn a_fast_move_of_a_parked_cursor_is_just_a_fast_move() {
        let mut s = peer_on_the_right();
        s.local(motion(1919.0, 540.0));
        assert_eq!(s.local(nudge(120.0, 0.0)).actions, [at(PEER, 120.0, 540.0)]);
    }

    const PLAY: KeyCode = KeyCode::Space;

    /// With something to play, and the default shortcuts.
    fn with_media() -> Switcher {
        let mut s = peer_on_the_right();
        s.set_media_available(true);
        s
    }

    fn press_all(s: &mut Switcher, keys: &[KeyCode]) {
        for &code in keys {
            s.local(key(code, true));
        }
    }

    #[test]
    fn a_media_shortcut_is_hidden_and_reported() {
        let mut s = with_media();
        press_all(&mut s, &[KeyCode::ControlLeft, KeyCode::AltLeft]);
        assert_eq!(
            s.local(key(PLAY, true)),
            Outcome::swallow(vec![Action::Media(MediaCommand::PlayPause)])
        );
        // Holding it down doesn't start and stop the music over and over.
        assert_eq!(s.local(key(PLAY, true)), Outcome::swallow(vec![]));
        assert_eq!(s.local(key(PLAY, false)), Outcome::swallow(vec![]));
        assert_eq!(
            s.local(key(KeyCode::PageUp, true)).actions,
            [Action::Media(MediaCommand::Back)]
        );
        assert_eq!(
            s.local(key(KeyCode::PageDown, true)).actions,
            [Action::Media(MediaCommand::Forward)]
        );
    }

    #[test]
    fn a_media_shortcut_works_from_another_device_too() {
        let mut s = with_media();
        s.local(motion(1919.0, 540.0));
        assert_eq!(s.controlling(), Some(PEER));
        press_all(&mut s, &[KeyCode::ControlRight, KeyCode::AltLeft]);
        assert_eq!(
            s.local(key(PLAY, true)),
            Outcome::swallow(vec![Action::Media(MediaCommand::PlayPause)])
        );
        assert_eq!(s.controlling(), Some(PEER), "the cursor stays where it is");
    }

    #[test]
    fn with_nothing_to_play_the_keys_stay_with_the_program_in_front() {
        let mut s = peer_on_the_right();
        press_all(&mut s, &[KeyCode::ControlLeft, KeyCode::AltLeft]);
        assert_eq!(s.local(key(PLAY, true)), Outcome::pass());
        s.local(key(PLAY, false));

        s.set_media_available(true);
        assert!(s.local(key(PLAY, true)).swallow);
    }

    #[test]
    fn media_shortcuts_can_be_turned_off_together_or_one_at_a_time() {
        let mut s = with_media();
        let mut rules = Rules::default();
        rules.media_shortcuts.play_pause = Shortcut::default();
        s.set_rules(rules.clone());
        press_all(&mut s, &[KeyCode::ControlLeft, KeyCode::AltLeft]);
        assert_eq!(s.local(key(PLAY, true)), Outcome::pass());
        assert!(s.local(key(KeyCode::PageUp, true)).swallow);
        s.local(key(KeyCode::PageUp, false));

        rules.media_shortcuts.enabled = false;
        s.set_rules(rules);
        assert_eq!(s.local(key(KeyCode::PageUp, true)), Outcome::pass());
    }

    #[test]
    fn a_media_shortcut_needs_exactly_its_own_modifiers() {
        let mut s = with_media();
        assert_eq!(s.local(key(PLAY, true)), Outcome::pass(), "just the key");
        s.local(key(PLAY, false));

        s.local(key(KeyCode::ControlLeft, true));
        assert_eq!(s.local(key(PLAY, true)), Outcome::pass(), "only Ctrl");
        s.local(key(PLAY, false));

        press_all(&mut s, &[KeyCode::AltLeft, KeyCode::ShiftLeft]);
        assert_eq!(s.local(key(PLAY, true)), Outcome::pass(), "and also Shift");
    }

    /// Windows reports AltGr as Ctrl + right Alt, and it's for typing characters.
    #[test]
    fn altgr_never_completes_a_media_shortcut() {
        let mut s = with_media();
        press_all(&mut s, &[KeyCode::ControlLeft, KeyCode::AltRight]);
        assert_eq!(s.local(key(PLAY, true)), Outcome::pass());
    }

    #[test]
    fn a_media_shortcut_can_be_any_key_chosen() {
        let mut s = with_media();
        let mut rules = Rules::default();
        rules.media_shortcuts.play_pause = Shortcut {
            key: Some(KeyCode::MediaPlayPause),
            ..Shortcut::default()
        };
        rules.media_shortcuts.forward = Shortcut {
            meta: true,
            shift: true,
            key: Some(KeyCode::KeyN),
            ..Shortcut::default()
        };
        s.set_rules(rules);

        assert_eq!(
            s.local(key(KeyCode::MediaPlayPause, true)).actions,
            [Action::Media(MediaCommand::PlayPause)]
        );
        press_all(&mut s, &[KeyCode::MetaRight, KeyCode::ShiftRight]);
        assert_eq!(
            s.local(key(KeyCode::KeyN, true)).actions,
            [Action::Media(MediaCommand::Forward)]
        );
    }

    #[test]
    fn choosing_a_new_shortcut_leaves_all_of_them_alone() {
        let mut s = with_media();
        s.set_jump_order(vec![ME, PEER]);
        s.set_shortcuts_paused(true);
        press_all(&mut s, &[KeyCode::ControlLeft, KeyCode::AltLeft]);
        for code in [PLAY, KeyCode::KeyL, KeyCode::Digit2] {
            assert_eq!(s.local(key(code, true)), Outcome::pass(), "{code}");
            s.local(key(code, false));
        }
        assert_eq!(s.controlling(), None);
        assert!(!s.locked());

        s.set_shortcuts_paused(false);
        assert!(s.local(key(PLAY, true)).swallow);
    }

    /// A key pressed just before choosing began has to be let go of the same way.
    #[test]
    fn letting_go_of_a_shortcut_still_counts_while_choosing_a_new_one() {
        let mut s = with_media();
        press_all(&mut s, &[KeyCode::ControlLeft, KeyCode::AltLeft]);
        assert!(s.local(key(PLAY, true)).swallow);
        s.set_shortcuts_paused(true);
        assert!(s.local(key(PLAY, false)).swallow);
    }

    /// One a user picked that's taken by these is theirs to sort out, but these stay as
    /// they were.
    #[test]
    fn the_shortcuts_that_were_there_first_come_first() {
        let mut s = with_media();
        let mut rules = Rules::default();
        rules.media_shortcuts.play_pause = Shortcut {
            ctrl: true,
            alt: true,
            key: Some(KeyCode::KeyL),
            ..Shortcut::default()
        };
        s.set_rules(rules);
        press_all(&mut s, &[KeyCode::ControlLeft, KeyCode::AltLeft]);
        assert_eq!(
            s.local(key(KeyCode::KeyL, true)).actions,
            [Action::StateChanged]
        );
        assert!(s.locked());
    }

    #[test]
    fn crosses_at_a_touching_edge() {
        let mut s = peer_on_the_right();
        assert_eq!(s.local(motion(1000.0, 540.0)), Outcome::pass());
        assert_eq!(
            s.local(motion(1919.0, 540.0)),
            Outcome::swallow(vec![
                Action::Enter(PEER),
                Action::ParkCursor(PARK),
                at(PEER, 0.0, 540.0),
            ])
        );
        assert_eq!(s.controlling(), Some(PEER));
    }

    #[test]
    fn stays_home_at_an_edge_with_nothing_beyond_it() {
        let mut s = peer_on_the_right();
        assert_eq!(s.local(motion(0.0, 540.0)), Outcome::pass());
        assert_eq!(s.local(motion(500.0, 0.0)), Outcome::pass());
        assert_eq!(s.local(motion(500.0, 1079.0)), Outcome::pass());
        assert_eq!(s.controlling(), None);
    }

    #[test]
    fn stays_home_when_the_peer_is_offline() {
        let mut peer = device(PEER, vec![monitor("peer", 0.0, 0.0, 1920.0, 1080.0)]);
        peer.online = false;
        let mut s = switcher_with(
            &[
                device(ME, vec![monitor("me", 0.0, 0.0, 1920.0, 1080.0)]),
                peer,
            ],
            vec![place(PEER, "peer", 1920.0, 0.0)],
        );
        assert_eq!(s.local(motion(1919.0, 540.0)), Outcome::pass());
    }

    #[test]
    fn the_canvas_outranks_the_operating_system_about_what_comes_next() {
        // Two monitors side by side in this machine's own display settings, with
        // the peer placed between them on the canvas.
        let mut s = switcher_with(
            &[
                device(
                    ME,
                    vec![
                        monitor("left", 0.0, 0.0, 1920.0, 1080.0),
                        monitor("right", 1920.0, 0.0, 1920.0, 1080.0),
                    ],
                ),
                device(PEER, vec![monitor("peer", 0.0, 0.0, 1920.0, 1080.0)]),
            ],
            vec![
                place(ME, "left", 0.0, 0.0),
                place(ME, "right", 5000.0, 0.0),
                place(PEER, "peer", 1920.0, 0.0),
            ],
        );
        // Reaching the last column isn't enough, because moving further wouldn't
        // hit the edge of the desktop.
        assert_eq!(s.local(motion(1919.0, 500.0)), Outcome::pass());
        assert_eq!(s.controlling(), None);
        // Moving past it goes to the peer, not to the other monitor.
        assert_eq!(
            s.local(moving(1919.0, 500.0, 1921.0, 500.0)),
            Outcome::swallow(vec![
                Action::Enter(PEER),
                Action::ParkCursor(PARK),
                at(PEER, 0.0, 500.0),
            ])
        );
    }

    /// The desk this is all about: a computer with one screen at each end, and two
    /// other computers between them. Each screen is 1000 pixels square.
    fn screens_at_both_ends_with_two_devices_between() -> Switcher {
        switcher_with(
            &[
                device(
                    ME,
                    vec![
                        monitor("first", 0.0, 0.0, 1000.0, 1000.0),
                        monitor("last", 1000.0, 0.0, 1000.0, 1000.0),
                    ],
                ),
                device(PEER, vec![monitor("peer", 0.0, 0.0, 1000.0, 1000.0)]),
                device(THIRD, vec![monitor("third", 0.0, 0.0, 1000.0, 1000.0)]),
            ],
            vec![
                place(ME, "first", 0.0, 0.0),
                place(PEER, "peer", 1000.0, 0.0),
                place(THIRD, "third", 2000.0, 0.0),
                place(ME, "last", 3000.0, 0.0),
            ],
        )
    }

    #[test]
    fn the_cursor_visits_the_devices_between_two_separated_screens() {
        let mut s = screens_at_both_ends_with_two_devices_between();

        // Windows would go straight on to the other screen here.
        assert_eq!(
            s.local(moving(999.0, 500.0, 1001.0, 500.0)).actions,
            [
                Action::Enter(PEER),
                Action::ParkCursor(PARK),
                at(PEER, 0.0, 500.0)
            ]
        );
        s.local(nudge(999.0, 0.0));
        assert_eq!(
            s.local(nudge(1.0, 0.0)).actions,
            [
                Action::Leave(PEER),
                Action::Enter(THIRD),
                at(THIRD, 0.0, 500.0)
            ]
        );
        s.local(nudge(999.0, 0.0));
        // And on to the far screen, which is this machine's own.
        assert_eq!(
            s.local(nudge(1.0, 0.0)).actions,
            [
                Action::Leave(THIRD),
                Action::RestoreCursor(Point::new(1001.0, 500.0))
            ]
        );
        assert_eq!(s.controlling(), None);
    }

    #[test]
    fn the_way_back_from_a_separated_screen_goes_the_same_way() {
        let mut s = screens_at_both_ends_with_two_devices_between();
        // On the far screen, moving along its left edge stays put...
        assert_eq!(
            s.local(moving(1001.0, 500.0, 1000.0, 600.0)),
            Outcome::pass()
        );
        // ...and leaving through it goes to the device beside it on the canvas.
        assert_eq!(
            s.local(moving(1000.0, 600.0, 999.0, 600.0)).actions,
            [
                Action::Enter(THIRD),
                Action::ParkCursor(PARK),
                at(THIRD, 999.0, 600.0)
            ]
        );
    }

    #[test]
    fn a_separated_device_being_controlled_is_treated_the_same() {
        let mut s = switcher_with(
            &[
                device(ME, vec![monitor("me", 0.0, 0.0, 1000.0, 1000.0)]),
                device(
                    PEER,
                    vec![
                        monitor("first", 0.0, 0.0, 1000.0, 1000.0),
                        monitor("last", 1000.0, 0.0, 1000.0, 1000.0),
                    ],
                ),
                device(THIRD, vec![monitor("third", 0.0, 0.0, 1000.0, 1000.0)]),
            ],
            vec![
                place(ME, "me", 0.0, 0.0),
                place(PEER, "first", 1000.0, 0.0),
                place(THIRD, "third", 2000.0, 0.0),
                place(PEER, "last", 3000.0, 0.0),
            ],
        );
        s.local(motion(999.0, 500.0));
        assert_eq!(s.controlling(), Some(PEER));
        s.local(nudge(999.0, 0.0));
        // The peer's own display settings would put this on its second monitor.
        assert_eq!(
            s.local(nudge(1.0, 0.0)).actions,
            [
                Action::Leave(PEER),
                Action::Enter(THIRD),
                at(THIRD, 0.0, 500.0)
            ]
        );
        s.local(nudge(999.0, 0.0));
        // Arriving on the far screen of the same peer.
        assert_eq!(
            s.local(nudge(1.0, 0.0)).actions,
            [
                Action::Leave(THIRD),
                Action::Enter(PEER),
                at(PEER, 1000.0, 500.0)
            ]
        );
        assert_eq!(s.controlling(), Some(PEER));
    }

    #[test]
    fn with_nothing_on_the_canvas_the_operating_system_decides() {
        // If a device in between goes offline, the far screen must stay reachable.
        let mut peer = device(PEER, vec![monitor("peer", 0.0, 0.0, 1000.0, 1000.0)]);
        peer.online = false;
        let mut s = switcher_with(
            &[
                device(
                    ME,
                    vec![
                        monitor("first", 0.0, 0.0, 1000.0, 1000.0),
                        monitor("last", 1000.0, 0.0, 1000.0, 1000.0),
                    ],
                ),
                peer,
            ],
            vec![
                place(ME, "first", 0.0, 0.0),
                place(PEER, "peer", 1000.0, 0.0),
                place(ME, "last", 2000.0, 0.0),
            ],
        );
        assert_eq!(
            s.local(moving(999.0, 500.0, 1001.0, 500.0)),
            Outcome::pass()
        );
        assert_eq!(s.controlling(), None);
    }

    #[test]
    fn dragging_or_the_lock_stays_on_this_device_when_separated() {
        let mut s = screens_at_both_ends_with_two_devices_between();
        s.set_locked(true);
        assert_eq!(
            s.local(moving(999.0, 500.0, 1001.0, 500.0)),
            Outcome::pass()
        );
        s.set_locked(false);

        s.local(LocalEvent::Button {
            button: MouseButton::Left,
            pressed: true,
        });
        assert_eq!(
            s.local(moving(999.0, 500.0, 1001.0, 500.0)),
            Outcome::pass()
        );
        assert_eq!(s.controlling(), None);
    }

    #[test]
    fn the_crossing_delay_holds_the_cursor_instead_of_letting_it_slip_to_the_far_screen() {
        let mut s = with_edge_delay(screens_at_both_ends_with_two_devices_between(), 200);
        let leaving = moving(999.0, 500.0, 1001.0, 500.0);
        // Hidden from Windows, so it doesn't jump to the other monitor meanwhile.
        assert_eq!(at_time(&mut s, leaving, 1000), Outcome::swallow(vec![]));
        assert_eq!(at_time(&mut s, leaving, 1100), Outcome::swallow(vec![]));
        assert_eq!(s.controlling(), None);
        assert!(!at_time(&mut s, leaving, 1200).actions.is_empty());
        assert_eq!(s.controlling(), Some(PEER));
    }

    #[test]
    fn the_canvas_can_connect_monitors_the_operating_system_has_elsewhere() {
        // Side by side in the display settings, but stacked on the canvas.
        let mut s = switcher_with(
            &[device(
                ME,
                vec![
                    monitor("a", 0.0, 0.0, 1000.0, 1000.0),
                    monitor("b", 1000.0, 0.0, 1000.0, 1000.0),
                ],
            )],
            vec![place(ME, "a", 0.0, 0.0), place(ME, "b", 0.0, 1000.0)],
        );
        // The bottom of the desktop leads to the monitor drawn below it.
        assert_eq!(
            s.local(moving(300.0, 998.0, 300.0, 999.0)),
            Outcome::swallow(vec![Action::RestoreCursor(Point::new(1300.0, 1.0))])
        );
        // And back from there, without bouncing on arrival.
        assert_eq!(s.local(moving(1300.0, 1.0, 1310.0, 1.0)), Outcome::pass());
        assert_eq!(
            s.local(moving(1310.0, 1.0, 1310.0, 0.0)),
            Outcome::swallow(vec![Action::RestoreCursor(Point::new(310.0, 998.0))])
        );
    }

    #[test]
    fn own_monitors_placed_as_usual_are_left_to_the_operating_system() {
        let mut s = switcher_with(
            &[device(
                ME,
                vec![
                    monitor("a", 0.0, 0.0, 1000.0, 1000.0),
                    monitor("b", 1000.0, 0.0, 1000.0, 1000.0),
                ],
            )],
            vec![],
        );
        assert_eq!(
            s.local(moving(999.0, 500.0, 1002.0, 500.0)),
            Outcome::pass()
        );
    }

    #[test]
    fn own_monitors_lined_up_differently_on_the_canvas_follow_the_canvas() {
        // Touching in both, but 300 pixels lower on the canvas.
        let mut s = switcher_with(
            &[device(
                ME,
                vec![
                    monitor("a", 0.0, 0.0, 1000.0, 1000.0),
                    monitor("b", 1000.0, 0.0, 1000.0, 1000.0),
                ],
            )],
            vec![place(ME, "a", 0.0, 0.0), place(ME, "b", 1000.0, 300.0)],
        );
        assert_eq!(
            s.local(moving(999.0, 500.0, 1002.0, 500.0)),
            Outcome::swallow(vec![Action::RestoreCursor(Point::new(1001.0, 200.0))])
        );
    }

    #[test]
    fn a_cursor_moved_by_another_device_is_taken_from_where_it_really_is() {
        let mut s = screens_at_both_ends_with_two_devices_between();
        // The last move this machine saw itself was on the first screen.
        s.local(motion(100.0, 100.0));
        // Another device then left the cursor on the far screen. Moving it within
        // that screen must not look like crossing between the two.
        assert_eq!(
            s.local(moving(1500.0, 500.0, 1501.0, 500.0)),
            Outcome::pass()
        );
        assert_eq!(s.controlling(), None);
    }

    #[test]
    fn stays_home_while_dragging() {
        let mut s = peer_on_the_right();
        let left = |pressed| LocalEvent::Button {
            button: MouseButton::Left,
            pressed,
        };
        s.local(left(true));
        assert_eq!(s.local(motion(1919.0, 540.0)), Outcome::pass());
        s.local(left(false));
        assert!(s.local(motion(1919.0, 540.0)).swallow);
    }

    #[test]
    fn remote_motion_follows_offsets_from_the_park_point_and_stops_at_edges() {
        let mut s = peer_on_the_right();
        s.local(motion(1919.0, 540.0));

        assert_eq!(
            s.local(nudge(10.0, -5.0)),
            Outcome::swallow(vec![at(PEER, 10.0, 535.0)])
        );
        // Nothing past the peer's right edge, so the cursor stops there.
        assert_eq!(
            s.local(nudge(5000.0, 0.0)),
            Outcome::swallow(vec![at(PEER, 2559.0, 535.0)])
        );
        // A move to the park point itself (e.g. our own warp) changes nothing.
        assert_eq!(s.local(nudge(0.0, 0.0)), Outcome::swallow(vec![]));
    }

    #[test]
    fn remote_cursor_moves_freely_between_the_peers_own_monitors() {
        let mut s = switcher_with(
            &[
                device(ME, vec![monitor("me", 0.0, 0.0, 1920.0, 1080.0)]),
                device(
                    PEER,
                    vec![
                        monitor("one", 0.0, 0.0, 1920.0, 1080.0),
                        monitor("two", 1920.0, 0.0, 1920.0, 1080.0),
                    ],
                ),
            ],
            vec![
                place(ME, "me", 0.0, 0.0),
                place(PEER, "one", 1920.0, 0.0),
                place(PEER, "two", 3840.0, 0.0),
            ],
        );
        s.local(motion(1919.0, 540.0));
        assert_eq!(
            s.local(nudge(2000.0, 0.0)).actions,
            [at(PEER, 2000.0, 540.0)]
        );
    }

    #[test]
    fn returns_home_when_pushing_back_over_the_edge() {
        let mut s = peer_on_the_right();
        s.local(motion(1919.0, 540.0));

        assert_eq!(
            s.local(nudge(-1.0, 0.0)),
            Outcome::swallow(vec![
                Action::Leave(PEER),
                Action::RestoreCursor(Point::new(1918.0, 540.0))
            ])
        );
        assert_eq!(s.controlling(), None);

        // Moving along the edge doesn't bounce straight back...
        assert_eq!(s.local(motion(1918.0, 600.0)), Outcome::pass());
        // ...but moving outward again does.
        assert!(s.local(motion(1919.0, 600.0)).swallow);
    }

    #[test]
    fn hops_from_one_remote_device_to_another() {
        let mut s = switcher_with(
            &[
                device(ME, vec![monitor("me", 0.0, 0.0, 1920.0, 1080.0)]),
                device(PEER, vec![monitor("peer", 0.0, 0.0, 1920.0, 1080.0)]),
                device(THIRD, vec![monitor("third", 0.0, 0.0, 1920.0, 1080.0)]),
            ],
            vec![
                place(ME, "me", 0.0, 0.0),
                place(PEER, "peer", 1920.0, 0.0),
                place(THIRD, "third", 3840.0, 0.0),
            ],
        );
        s.local(motion(1919.0, 540.0));
        s.local(nudge(1919.0, 0.0));
        assert_eq!(
            s.local(nudge(1.0, 0.0)).actions,
            [
                Action::Leave(PEER),
                Action::Enter(THIRD),
                at(THIRD, 0.0, 540.0)
            ]
        );
        assert_eq!(s.controlling(), Some(THIRD));

        // ...and back through both to get home.
        assert_eq!(
            s.local(nudge(-1.0, 0.0)).actions,
            [
                Action::Leave(THIRD),
                Action::Enter(PEER),
                at(PEER, 1919.0, 540.0)
            ]
        );
        s.local(nudge(-1919.0, 0.0));
        assert_eq!(
            s.local(nudge(-1.0, 0.0)).actions,
            [
                Action::Leave(PEER),
                Action::RestoreCursor(Point::new(1918.0, 540.0))
            ]
        );
        assert_eq!(s.controlling(), None);
    }

    #[test]
    fn stays_on_peer_while_a_button_is_held_there() {
        let mut s = peer_on_the_right();
        s.local(motion(1919.0, 540.0));
        s.local(LocalEvent::Button {
            button: MouseButton::Left,
            pressed: true,
        });
        let outcome = s.local(nudge(-50.0, 0.0));
        assert_eq!(outcome, Outcome::swallow(vec![]));
        assert_eq!(s.controlling(), Some(PEER));
    }

    #[test]
    fn keys_pressed_at_home_are_released_at_home() {
        let mut s = peer_on_the_right();
        s.local(key(KeyCode::ShiftLeft, true));
        s.local(motion(1919.0, 540.0));

        assert_eq!(s.local(key(KeyCode::ShiftLeft, false)), Outcome::pass());
        assert_eq!(
            s.local(key(KeyCode::KeyA, true)),
            Outcome::swallow(vec![Action::Forward(
                PEER,
                InputEvent::Key {
                    code: KeyCode::KeyA,
                    pressed: true
                }
            )])
        );
    }

    #[test]
    fn ctrl_alt_home_returns_to_where_the_cursor_left() {
        let mut s = peer_on_the_right();
        s.local(motion(1919.0, 540.0));
        s.local(key(KeyCode::ControlLeft, true));
        s.local(key(KeyCode::AltRight, true));

        assert_eq!(
            s.local(key(KeyCode::Home, true)),
            Outcome::swallow(vec![
                Action::Leave(PEER),
                Action::RestoreCursor(Point::new(1918.0, 540.0))
            ])
        );
        assert_eq!(s.controlling(), None);
    }

    #[test]
    fn going_offline_returns_home() {
        let me = device(ME, vec![monitor("me", 0.0, 0.0, 1920.0, 1080.0)]);
        let mut s = peer_on_the_right();
        s.local(motion(1919.0, 540.0));

        let actions = s.set_environment(
            Topology::build(std::slice::from_ref(&me), &LayoutDoc::default()),
            PARK,
        );
        assert_eq!(
            actions,
            [
                Action::Leave(PEER),
                Action::RestoreCursor(Point::new(1918.0, 540.0))
            ]
        );
        assert_eq!(s.local(motion(1919.0, 540.0)), Outcome::pass());
    }

    #[test]
    fn another_device_taking_control_returns_home() {
        let mut s = peer_on_the_right();
        assert!(s.peer_took_control().is_empty());
        s.local(motion(1919.0, 540.0));
        assert_eq!(
            s.peer_took_control(),
            [
                Action::Leave(PEER),
                Action::RestoreCursor(Point::new(1918.0, 540.0))
            ]
        );
    }

    fn press(s: &mut Switcher, code: KeyCode) -> Outcome {
        s.local(key(code, true))
    }

    /// Three devices in a row: this one, then the peer, then the third.
    fn three_in_a_row() -> Switcher {
        let mut s = switcher_with(
            &[
                device(ME, vec![monitor("me", 0.0, 0.0, 1920.0, 1080.0)]),
                device(PEER, vec![monitor("peer", 0.0, 0.0, 1920.0, 1080.0)]),
                device(THIRD, vec![monitor("third", 0.0, 0.0, 2560.0, 1440.0)]),
            ],
            vec![
                place(ME, "me", 0.0, 0.0),
                place(PEER, "peer", 1920.0, 0.0),
                place(THIRD, "third", 3840.0, 0.0),
            ],
        );
        s.set_jump_order(vec![ME, PEER, THIRD]);
        s
    }

    fn hold_shortcut_modifiers(s: &mut Switcher) {
        s.local(key(KeyCode::ControlLeft, true));
        s.local(key(KeyCode::AltLeft, true));
    }

    #[test]
    fn the_lock_keeps_the_cursor_at_home() {
        let mut s = peer_on_the_right();
        s.set_locked(true);
        assert_eq!(s.local(motion(1919.0, 540.0)), Outcome::pass());
        s.set_locked(false);
        assert!(s.local(motion(1919.0, 540.0)).swallow);
    }

    #[test]
    fn the_lock_keeps_the_cursor_on_a_remote_device_too() {
        let mut s = peer_on_the_right();
        s.local(motion(1919.0, 540.0));
        s.set_locked(true);
        // Pushing back over the edge the cursor came through does nothing.
        assert_eq!(s.local(nudge(-50.0, 0.0)), Outcome::swallow(vec![]));
        assert_eq!(s.controlling(), Some(PEER));
        s.set_locked(false);
        assert!(matches!(
            s.local(nudge(-50.0, 0.0)).actions[..],
            [Action::Leave(PEER), Action::RestoreCursor(_)]
        ));
    }

    #[test]
    fn ctrl_alt_l_toggles_the_lock_and_hides_the_key() {
        let mut s = peer_on_the_right();
        hold_shortcut_modifiers(&mut s);
        assert_eq!(
            press(&mut s, KeyCode::KeyL),
            Outcome::swallow(vec![Action::StateChanged])
        );
        assert!(s.locked());
        // Holding the key down repeats it, which mustn't toggle it back.
        assert_eq!(press(&mut s, KeyCode::KeyL), Outcome::swallow(vec![]));
        assert!(s.locked());
        // The release is hidden too, since the press was.
        assert_eq!(s.local(key(KeyCode::KeyL, false)), Outcome::swallow(vec![]));
        press(&mut s, KeyCode::KeyL);
        assert!(!s.locked());
    }

    #[test]
    fn shortcuts_can_be_turned_off() {
        let mut s = three_in_a_row();
        s.set_rules(Rules {
            shortcuts: false,
            ..Rules::default()
        });
        hold_shortcut_modifiers(&mut s);
        assert_eq!(press(&mut s, KeyCode::KeyL), Outcome::pass());
        assert_eq!(press(&mut s, KeyCode::Digit2), Outcome::pass());
        assert!(!s.locked());
        assert_eq!(s.controlling(), None);
    }

    #[test]
    fn altgr_stays_available_for_typing_characters() {
        // Windows reports AltGr as Ctrl + right Alt. On a French keyboard,
        // AltGr + 0 is @ and AltGr + 3 is #.
        let mut s = three_in_a_row();
        s.local(key(KeyCode::ControlLeft, true));
        s.local(key(KeyCode::AltRight, true));
        assert_eq!(press(&mut s, KeyCode::Digit3), Outcome::pass());
        assert_eq!(press(&mut s, KeyCode::KeyL), Outcome::pass());
        assert_eq!(s.controlling(), None);
        assert!(!s.locked());
    }

    #[test]
    fn plain_keys_are_not_shortcuts() {
        let mut s = three_in_a_row();
        assert_eq!(press(&mut s, KeyCode::Digit2), Outcome::pass());
        assert_eq!(s.local(key(KeyCode::Digit2, false)), Outcome::pass());
    }

    #[test]
    fn number_shortcuts_jump_to_the_middle_of_a_device() {
        let mut s = three_in_a_row();
        s.local(motion(700.0, 300.0));
        hold_shortcut_modifiers(&mut s);

        assert_eq!(
            press(&mut s, KeyCode::Digit2),
            Outcome::swallow(vec![
                Action::Enter(PEER),
                Action::ParkCursor(PARK),
                at(PEER, 960.0, 540.0),
            ])
        );
        assert_eq!(s.controlling(), Some(PEER));
        s.local(key(KeyCode::Digit2, false));

        // On to another device, directly.
        assert_eq!(
            press(&mut s, KeyCode::Digit3),
            Outcome::swallow(vec![
                Action::Leave(PEER),
                Action::Enter(THIRD),
                at(THIRD, 1280.0, 720.0),
            ])
        );
        s.local(key(KeyCode::Digit3, false));

        // Jumping to where the cursor already is does nothing.
        assert_eq!(press(&mut s, KeyCode::Digit3), Outcome::swallow(vec![]));
        s.local(key(KeyCode::Digit3, false));

        // Number 1 is home, where the cursor comes back to where it was.
        assert_eq!(
            press(&mut s, KeyCode::Digit1),
            Outcome::swallow(vec![
                Action::Leave(THIRD),
                Action::RestoreCursor(Point::new(700.0, 300.0)),
            ])
        );
        assert_eq!(s.controlling(), None);
    }

    #[test]
    fn jumping_ignores_devices_that_are_offline_or_unknown() {
        let mut peer = device(PEER, vec![monitor("peer", 0.0, 0.0, 1920.0, 1080.0)]);
        peer.online = false;
        let mut s = switcher_with(
            &[
                device(ME, vec![monitor("me", 0.0, 0.0, 1920.0, 1080.0)]),
                peer,
            ],
            vec![],
        );
        s.set_jump_order(vec![ME, PEER]);
        hold_shortcut_modifiers(&mut s);
        assert_eq!(press(&mut s, KeyCode::Digit2), Outcome::swallow(vec![]));
        assert_eq!(press(&mut s, KeyCode::Digit9), Outcome::swallow(vec![]));
        assert_eq!(s.controlling(), None);
    }

    #[test]
    fn stopping_capture_brings_control_home_and_forgets_what_was_held() {
        let mut s = peer_on_the_right();
        hold_shortcut_modifiers(&mut s);
        assert!(s.local(motion(1919.0, 500.0)).swallow);
        assert_eq!(s.controlling(), Some(PEER));

        let actions = s.capture_stopping();
        assert_eq!(s.controlling(), None);
        assert!(actions.contains(&Action::Leave(PEER)));

        // The releases of those keys were never seen, so a number on its own is
        // only a number, and not the shortcut it would be with them held.
        assert_eq!(press(&mut s, KeyCode::Digit2), Outcome::pass());
    }

    fn with_rules(mut s: Switcher, change: impl FnOnce(&mut Rules)) -> Switcher {
        let mut rules = Rules::default();
        change(&mut rules);
        s.set_rules(rules);
        s
    }

    #[test]
    fn a_key_can_be_required_to_cross() {
        let mut s = with_rules(peer_on_the_right(), |rules| {
            rules.cross_modifier = CrossModifier::Ctrl;
        });
        assert!(!s.local(motion(1919.0, 500.0)).swallow, "no key held");
        assert_eq!(s.controlling(), None);

        press(&mut s, KeyCode::ControlRight);
        assert!(s.local(motion(1919.0, 500.0)).swallow);
        assert_eq!(s.controlling(), Some(PEER));

        // The key keeps being needed to go on from there, or to come back.
        s.local(LocalEvent::Key {
            code: KeyCode::ControlRight,
            pressed: false,
        });
        for _ in 0..3 {
            s.local(motion(0.0, 500.0));
        }
        assert_eq!(s.controlling(), Some(PEER), "stays without the key");
    }

    #[test]
    fn each_kind_of_key_counts_on_either_side_of_the_keyboard() {
        for (modifier, code) in [
            (CrossModifier::Ctrl, KeyCode::ControlLeft),
            (CrossModifier::Shift, KeyCode::ShiftLeft),
            (CrossModifier::Shift, KeyCode::ShiftRight),
            (CrossModifier::Alt, KeyCode::AltLeft),
            (CrossModifier::Alt, KeyCode::AltRight),
        ] {
            let mut s = with_rules(peer_on_the_right(), |rules| rules.cross_modifier = modifier);
            press(&mut s, code);
            assert!(
                s.local(motion(1919.0, 500.0)).swallow,
                "{modifier:?} {code:?}"
            );
        }
        let mut s = with_rules(peer_on_the_right(), |rules| {
            rules.cross_modifier = CrossModifier::Shift;
        });
        press(&mut s, KeyCode::ControlLeft);
        assert!(!s.local(motion(1919.0, 500.0)).swallow, "the wrong key");
    }

    #[test]
    fn corners_of_the_screen_can_be_kept_from_crossing() {
        let guarded = || with_rules(peer_on_the_right(), |rules| rules.corner_guard = 50);
        for (y, crosses) in [
            (0.0, false),
            (49.0, false),
            (50.0, true),
            (540.0, true),
            (1029.0, true),
            (1030.0, false),
            (1079.0, false),
        ] {
            let mut s = guarded();
            assert_eq!(s.local(motion(1919.0, y)).swallow, crosses, "at row {y}");
        }
    }

    #[test]
    fn corners_are_guarded_on_the_way_back_too() {
        let mut s = with_rules(peer_on_the_right(), |rules| rules.corner_guard = 100);
        assert!(s.local(motion(1919.0, 500.0)).swallow);
        assert_eq!(s.controlling(), Some(PEER));
        // Up to the corner of the other device's screen, then left: it stays there.
        s.local(nudge(0.0, -540.0));
        s.local(nudge(-1.0, 0.0));
        assert_eq!(s.controlling(), Some(PEER));
    }

    #[test]
    fn neither_setting_gets_in_the_way_of_a_deliberate_jump() {
        let mut s = with_rules(peer_on_the_right(), |rules| {
            rules.cross_modifier = CrossModifier::Alt;
            rules.corner_guard = 1000;
        });
        s.set_jump_order(vec![ME, PEER]);
        hold_shortcut_modifiers(&mut s);
        press(&mut s, KeyCode::Digit2);
        assert_eq!(s.controlling(), Some(PEER));
    }

    #[test]
    fn jumping_works_even_when_locked() {
        let mut s = three_in_a_row();
        s.set_locked(true);
        hold_shortcut_modifiers(&mut s);
        press(&mut s, KeyCode::Digit2);
        assert_eq!(s.controlling(), Some(PEER));
    }

    fn with_edge_delay(mut s: Switcher, ms: u32) -> Switcher {
        s.set_rules(Rules {
            edge_delay_ms: ms,
            ..Rules::default()
        });
        s
    }

    fn at_time(s: &mut Switcher, event: LocalEvent, ms: u64) -> Outcome {
        s.handle_local(event, Duration::from_millis(ms))
    }

    #[test]
    fn a_crossing_delay_needs_the_cursor_to_keep_pushing() {
        let mut s = with_edge_delay(peer_on_the_right(), 200);
        let edge = motion(1919.0, 540.0);
        assert_eq!(at_time(&mut s, edge, 1000), Outcome::pass());
        assert_eq!(at_time(&mut s, edge, 1100), Outcome::pass());
        assert!(at_time(&mut s, edge, 1200).swallow);
        assert_eq!(s.controlling(), Some(PEER));
    }

    #[test]
    fn leaving_the_edge_starts_the_delay_over() {
        let mut s = with_edge_delay(peer_on_the_right(), 200);
        let edge = motion(1919.0, 540.0);
        at_time(&mut s, edge, 1000);
        at_time(&mut s, motion(1000.0, 540.0), 1100);
        assert_eq!(at_time(&mut s, edge, 1250), Outcome::pass());
        assert_eq!(at_time(&mut s, edge, 1300), Outcome::pass());
        assert!(at_time(&mut s, edge, 1450).swallow);
    }

    #[test]
    fn the_crossing_delay_applies_on_the_way_back_too() {
        let mut s = with_edge_delay(peer_on_the_right(), 200);
        let edge = motion(1919.0, 540.0);
        at_time(&mut s, edge, 0);
        at_time(&mut s, edge, 200);
        assert_eq!(s.controlling(), Some(PEER));

        // Pushing against the peer's edge holds the cursor there at first.
        assert_eq!(
            at_time(&mut s, nudge(-5.0, 0.0), 1000),
            Outcome::swallow(vec![])
        );
        assert_eq!(s.controlling(), Some(PEER));
        at_time(&mut s, nudge(-5.0, 0.0), 1200);
        assert_eq!(s.controlling(), None);
    }

    #[test]
    fn a_fullscreen_app_holds_the_cursor_only_if_the_rule_is_on() {
        let mut s = peer_on_the_right();
        s.set_fullscreen(true);
        assert!(!s.held_by_fullscreen());
        assert!(s.local(motion(1919.0, 540.0)).swallow);

        let mut s = peer_on_the_right();
        s.set_fullscreen(true);
        s.set_rules(Rules {
            stay_in_fullscreen: true,
            ..Rules::default()
        });
        assert!(s.held_by_fullscreen());
        assert_eq!(s.local(motion(1919.0, 540.0)), Outcome::pass());

        s.set_fullscreen(false);
        assert!(s.local(motion(1919.0, 540.0)).swallow);
    }

    #[test]
    fn enters_the_correct_side_for_every_edge() {
        let devices = [
            device(ME, vec![monitor("me", 0.0, 0.0, 1920.0, 1080.0)]),
            device(PEER, vec![monitor("peer", 0.0, 0.0, 1920.0, 1080.0)]),
        ];
        let cases = [
            (
                Point::new(-1920.0, 0.0),
                motion(0.0, 540.0),
                at(PEER, 1919.0, 540.0),
            ),
            (
                Point::new(0.0, -1080.0),
                motion(960.0, 0.0),
                at(PEER, 960.0, 1079.0),
            ),
            (
                Point::new(0.0, 1080.0),
                motion(960.0, 1079.0),
                at(PEER, 960.0, 0.0),
            ),
        ];
        for (peer_at, leaving, entered) in cases {
            let mut s = switcher_with(
                &devices,
                vec![
                    place(ME, "me", 0.0, 0.0),
                    place(PEER, "peer", peer_at.x, peer_at.y),
                ],
            );
            assert_eq!(s.local(leaving).actions[2], entered);
        }
    }

    #[test]
    fn comes_home_one_pixel_inside_the_edge_for_every_side() {
        let devices = [
            device(ME, vec![monitor("me", 0.0, 0.0, 1920.0, 1080.0)]),
            device(PEER, vec![monitor("peer", 0.0, 0.0, 1920.0, 1080.0)]),
        ];
        let cases = [
            (
                Point::new(-1920.0, 0.0),
                motion(0.0, 540.0),
                nudge(1.0, 0.0),
                Point::new(1.0, 540.0),
            ),
            (
                Point::new(0.0, -1080.0),
                motion(960.0, 0.0),
                nudge(0.0, 1.0),
                Point::new(960.0, 1.0),
            ),
            (
                Point::new(0.0, 1080.0),
                motion(960.0, 1079.0),
                nudge(0.0, -1.0),
                Point::new(960.0, 1078.0),
            ),
        ];
        for (peer_at, leaving, back, expected) in cases {
            let mut s = switcher_with(
                &devices,
                vec![
                    place(ME, "me", 0.0, 0.0),
                    place(PEER, "peer", peer_at.x, peer_at.y),
                ],
            );
            s.local(leaving);
            assert_eq!(
                s.local(back).actions,
                [Action::Leave(PEER), Action::RestoreCursor(expected)]
            );
        }
    }
}
