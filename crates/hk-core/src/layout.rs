//! Where every monitor of every device sits on the shared canvas.
//!
//! Each monitor has its own position on the canvas, in logical pixels (physical
//! pixels divided by the monitor's scale), so monitors with different DPI line
//! up the way they look. The canvas only decides what happens when the cursor
//! *leaves* a device: between a device's own monitors, its operating system
//! moves the cursor according to its own display settings.
//!
//! A [`Topology`] is the resolved picture for the devices known right now. A
//! [`LayoutDoc`] is the saved document it is resolved from, which all peers
//! share.

use std::collections::{HashMap, VecDeque};

use serde::{Deserialize, Serialize};

use crate::device::Fingerprint;
use crate::geometry::{Point, Rect};

/// A monitor still counts as being past an edge if it overlaps that edge by this
/// little on the canvas, in logical pixels.
const TOUCH_TOLERANCE: f64 = 1.0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Monitor {
    /// Stays the same across reboots and replugging.
    pub id: String,
    pub name: String,
    /// Position and size on its own device's desktop, in physical pixels.
    pub bounds: Rect,
    /// The operating system's display scale: 1.0 is 100%, 1.5 is 150%.
    pub scale: f64,
    pub primary: bool,
}

impl Monitor {
    /// Whether the numbers make sense. Monitors reported by other devices are
    /// checked before use, since all the geometry divides by them.
    pub fn is_valid(&self) -> bool {
        let b = self.bounds;
        [b.x, b.y, b.width, b.height, self.scale]
            .iter()
            .all(|value| value.is_finite())
            && b.width >= 1.0
            && b.height >= 1.0
            && self.scale > 0.0
    }

    fn canvas_size(&self) -> (f64, f64) {
        (
            self.bounds.width / self.scale,
            self.bounds.height / self.scale,
        )
    }
}

/// An edge of a monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    pub const ALL: [Side; 4] = [Side::Left, Side::Right, Side::Top, Side::Bottom];
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MonitorKey {
    pub device: Fingerprint,
    pub monitor: String,
}

/// Where a monitor sits on the canvas, and how large it is drawn there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Placement {
    pub device: Fingerprint,
    pub monitor: String,
    pub x: f64,
    pub y: f64,
    /// A width the user chose. The height follows, keeping the shape of the screen.
    /// Without one, the size comes from its resolution and scale.
    #[serde(default)]
    pub width: Option<f64>,
}

impl Placement {
    fn key(&self) -> MonitorKey {
        MonitorKey {
            device: self.device,
            monitor: self.monitor.clone(),
        }
    }
}

/// The arrangement saved for one exact set of monitors, like "docked" or
/// "laptop only". It is selected automatically when that set is present again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Profile {
    /// Sorted, so equal sets compare equal.
    monitors: Vec<MonitorKey>,
    placements: Vec<Placement>,
}

/// The saved layout. Peers exchange it and keep whichever copy is newest.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LayoutDoc {
    counter: u64,
    author: Option<Fingerprint>,
    profiles: Vec<Profile>,
    /// The last position of every monitor ever placed, used for monitor sets
    /// that have no profile of their own.
    last_known: Vec<Placement>,
}

impl LayoutDoc {
    /// Whether this copy should replace `other`. The author breaks ties, so
    /// all peers agree on the winner when two of them edit at the same time.
    pub fn is_newer_than(&self, other: &Self) -> bool {
        (self.counter, self.author) > (other.counter, other.author)
    }

    /// Records a change made by `author` while `devices` were known. Monitors
    /// the edit doesn't mention stay where they are currently shown.
    pub fn apply_edit(
        &mut self,
        author: Fingerprint,
        devices: &[DeviceMonitors],
        placements: Vec<Placement>,
    ) {
        let mut arrangement: Vec<Placement> = Topology::build(devices, self)
            .screens
            .iter()
            .map(|screen| Placement {
                device: screen.device,
                monitor: screen.monitor.id.clone(),
                x: screen.canvas.x,
                y: screen.canvas.y,
                width: screen.custom_width,
            })
            .collect();
        for placement in placements {
            upsert(&mut arrangement, placement);
        }

        self.counter += 1;
        self.author = Some(author);
        for placement in &arrangement {
            upsert(&mut self.last_known, placement.clone());
        }
        let monitors = monitor_set(devices);
        match self
            .profiles
            .iter_mut()
            .find(|profile| profile.monitors == monitors)
        {
            Some(profile) => profile.placements = arrangement,
            None => self.profiles.push(Profile {
                monitors,
                placements: arrangement,
            }),
        }
    }

    /// Replaces this copy with a newer one from another device, keeping anything only this
    /// copy knows. That was arranged while the two were apart, for monitors the other copy
    /// has never seen, so nothing in the newer copy replaces it.
    pub fn adopt(&mut self, mut newer: Self) {
        for profile in self.profiles.drain(..) {
            if !newer
                .profiles
                .iter()
                .any(|p| p.monitors == profile.monitors)
            {
                newer.profiles.push(profile);
            }
        }
        for placement in self.last_known.drain(..) {
            if !newer.last_known.iter().any(|p| p.key() == placement.key()) {
                newer.last_known.push(placement);
            }
        }
        *self = newer;
    }

    /// Whether an arrangement is saved for exactly these monitors. Without one, they sit
    /// where each of them last was.
    pub fn is_saved_for(&self, devices: &[DeviceMonitors]) -> bool {
        let monitors = monitor_set(devices);
        self.profiles
            .iter()
            .any(|profile| profile.monitors == monitors)
    }

    /// How many sets of monitors have an arrangement saved.
    pub fn saved_count(&self) -> usize {
        self.profiles.len()
    }

    fn placements_for(&self, monitors: &[MonitorKey]) -> HashMap<MonitorKey, &Placement> {
        let mut found: HashMap<MonitorKey, &Placement> = self
            .last_known
            .iter()
            .map(|placement| (placement.key(), placement))
            .collect();
        if let Some(profile) = self
            .profiles
            .iter()
            .find(|profile| profile.monitors == monitors)
        {
            found.extend(
                profile
                    .placements
                    .iter()
                    .map(|placement| (placement.key(), placement)),
            );
        }
        found
    }
}

fn upsert(placements: &mut Vec<Placement>, placement: Placement) {
    match placements
        .iter_mut()
        .find(|existing| existing.key() == placement.key())
    {
        Some(existing) => *existing = placement,
        None => placements.push(placement),
    }
}

fn monitor_set(devices: &[DeviceMonitors]) -> Vec<MonitorKey> {
    let mut set: Vec<MonitorKey> = devices
        .iter()
        .flat_map(|device| {
            device.monitors.iter().map(|monitor| MonitorKey {
                device: device.device,
                monitor: monitor.id.clone(),
            })
        })
        .collect();
    set.sort();
    set
}

/// The part of a monitor's ID naming its model, if it has one. On Windows an ID looks like
/// `\\?\DISPLAY#DEL40A3#5&2a8a&0&UID4352#{e6f07b5f}`, where the second part is the model
/// and the third depends on the port it's plugged into.
fn model_in_id(id: &str) -> Option<&str> {
    let mut parts = id.split('#');
    parts.next()?;
    let model = parts.next()?;
    parts.next()?;
    (!model.is_empty()).then_some(model)
}

/// Where each of a device's monitors was saved, if it was.
///
/// A monitor plugged into another port has a new ID. So one with no place of its own takes
/// the place of a monitor of the same model that isn't there any more. Those are gone
/// through in order of ID, so that every peer arrives at the same answer.
fn saved_places<'a>(
    device: &DeviceMonitors,
    saved: &HashMap<MonitorKey, &'a Placement>,
) -> Vec<Option<&'a Placement>> {
    let mut places: Vec<Option<&Placement>> = device
        .monitors
        .iter()
        .map(|monitor| {
            saved
                .get(&MonitorKey {
                    device: device.device,
                    monitor: monitor.id.clone(),
                })
                .copied()
        })
        .collect();

    let mut gone: Vec<&Placement> = saved
        .values()
        .copied()
        .filter(|placement| {
            placement.device == device.device
                && !device.monitors.iter().any(|m| m.id == placement.monitor)
        })
        .collect();
    gone.sort_by(|a, b| a.monitor.cmp(&b.monitor));

    for (monitor, place) in device.monitors.iter().zip(&mut places) {
        let model = model_in_id(&monitor.id);
        if place.is_none()
            && model.is_some()
            && let Some(at) = gone.iter().position(|p| model_in_id(&p.monitor) == model)
        {
            *place = Some(gone.remove(at));
        }
    }
    places
}

/// Whether two screens cover the same part of the canvas, by more than counts as touching.
fn on_top_of(a: Rect, b: Rect) -> bool {
    a.x < b.right() - TOUCH_TOLERANCE
        && b.x < a.right() - TOUCH_TOLERANCE
        && a.y < b.bottom() - TOUCH_TOLERANCE
        && b.y < a.bottom() - TOUCH_TOLERANCE
}

/// Moves a screen right or down, whichever is the shorter way, until it's on top of none of
/// the others. Neither way undoes the other, so it soon comes out somewhere free.
fn clear_of(mut screen: Rect, others: &[Rect]) -> Rect {
    for _ in 0..=others.len() * 2 {
        let Some(other) = others.iter().find(|other| on_top_of(screen, **other)) else {
            return screen;
        };
        if other.right() - screen.x <= other.bottom() - screen.y {
            screen.x = other.right();
        } else {
            screen.y = other.bottom();
        }
    }
    screen.x = others
        .iter()
        .map(|other| other.right())
        .fold(screen.x, f64::max);
    screen
}

/// One device's monitors, as far as we know them.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceMonitors {
    pub device: Fingerprint,
    pub monitors: Vec<Monitor>,
    /// Whether the device can receive input right now. Devices that are away
    /// keep their place on the canvas but can't be crossed into.
    pub online: bool,
}

/// A monitor placed on the canvas.
#[derive(Debug, Clone, PartialEq)]
pub struct Screen {
    pub device: Fingerprint,
    pub monitor: Monitor,
    pub canvas: Rect,
    /// The width the user gave it on the canvas, if they resized it.
    pub custom_width: Option<f64>,
    pub online: bool,
}

impl Screen {
    fn to_canvas(&self, pixel: Point) -> Point {
        let (b, c) = (self.monitor.bounds, self.canvas);
        Point::new(
            c.x + (pixel.x - b.x) * c.width / b.width,
            c.y + (pixel.y - b.y) * c.height / b.height,
        )
    }

    fn canvas_x_to_pixel(&self, x: f64) -> f64 {
        let (b, c) = (self.monitor.bounds, self.canvas);
        b.x + (x - c.x) * b.width / c.width
    }

    fn canvas_y_to_pixel(&self, y: f64) -> f64 {
        let (b, c) = (self.monitor.bounds, self.canvas);
        b.y + (y - c.y) * b.height / c.height
    }

    fn is_same_monitor(&self, other: &Screen) -> bool {
        self.device == other.device && self.monitor.id == other.monitor.id
    }
}

/// Where the cursor ends up after leaving a monitor through an edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Crossing {
    pub device: Fingerprint,
    /// The monitor it arrives on, as an index into [`Topology::screens`].
    pub screen: usize,
    /// In the target device's desktop pixels.
    pub entry: Point,
}

/// Every known monitor placed on the canvas.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Topology {
    pub screens: Vec<Screen>,
}

impl Topology {
    /// Resolves each monitor's position: from the profile for this exact set of
    /// monitors, else from where it last was, else automatically.
    ///
    /// Automatic placement must come out the same on every peer, so that they
    /// agree on the layout before anyone edits it. Devices are therefore
    /// processed in fingerprint order.
    ///
    /// No screen ends up on top of another, because the cursor can't cross between
    /// those. Monitors can bring that about by changing: a higher resolution makes a
    /// screen larger, and a new one can appear where another device's screen is. Nothing
    /// saved changes, so a screen going back to its old size goes back to its old place.
    pub fn build(devices: &[DeviceMonitors], layout: &LayoutDoc) -> Self {
        let saved = layout.placements_for(&monitor_set(devices));
        let mut devices: Vec<&DeviceMonitors> = devices.iter().collect();
        devices.sort_by_key(|device| device.device);

        let mut screens: Vec<Screen> = Vec::new();
        let mut with_a_place: Vec<usize> = Vec::new();
        let mut per_device = Vec::new();
        for device in devices {
            let arranged = arrange_from_os(&device.monitors);
            let places = saved_places(device, &saved);
            let mut anchor = None;
            let mut without_a_place = Vec::new();
            for ((monitor, &arranged), place) in device.monitors.iter().zip(&arranged).zip(places) {
                let index = screens.len();
                match place {
                    Some(_) => {
                        with_a_place.push(index);
                        anchor.get_or_insert((index, arranged));
                    }
                    None => without_a_place.push((index, arranged)),
                }
                // Other devices can send this, and the geometry divides by it.
                let custom_width = place
                    .and_then(|saved| saved.width)
                    .filter(|width| width.is_finite() && *width >= 1.0);
                let (width, height) = match custom_width {
                    Some(width) => (width, width * monitor.bounds.height / monitor.bounds.width),
                    None => monitor.canvas_size(),
                };
                let (x, y) = place.map_or((0.0, 0.0), |saved| (saved.x, saved.y));
                screens.push(Screen {
                    device: device.device,
                    monitor: monitor.clone(),
                    canvas: Rect::new(x, y, width, height),
                    custom_width,
                    online: device.online,
                });
            }
            per_device.push((anchor, without_a_place));
        }

        // Going left to right, a screen that grew pushes the ones it grew into along with
        // it, which keeps them next to each other.
        with_a_place.sort_by(|&a, &b| {
            let (a, b) = (&screens[a], &screens[b]);
            a.canvas
                .x
                .total_cmp(&b.canvas.x)
                .then(a.canvas.y.total_cmp(&b.canvas.y))
                .then_with(|| (a.device, &a.monitor.id).cmp(&(b.device, &b.monitor.id)))
        });
        let mut taken: Vec<Rect> = Vec::new();
        for index in with_a_place {
            let canvas = clear_of(screens[index].canvas, &taken);
            screens[index].canvas = canvas;
            taken.push(canvas);
        }

        for (anchor, without_a_place) in per_device {
            // Monitors with no saved position keep their operating-system
            // position relative to one that has a saved position...
            let offset = anchor.map_or_else(
                // ...and a device with none at all goes to the right of everything else.
                || {
                    let right = taken
                        .iter()
                        .map(|rect| rect.right())
                        .fold(f64::NEG_INFINITY, f64::max);
                    let left = without_a_place
                        .iter()
                        .map(|(_, point)| point.x)
                        .fold(f64::INFINITY, f64::min);
                    let top = without_a_place
                        .iter()
                        .map(|(_, point)| point.y)
                        .fold(f64::INFINITY, f64::min);
                    let x = if right.is_finite() { right } else { 0.0 };
                    Point::new(x - left, -top)
                },
                |(index, arranged)| {
                    let at: Rect = screens[index].canvas;
                    Point::new(at.x - arranged.x, at.y - arranged.y)
                },
            );
            for (index, arranged) in without_a_place {
                let wanted = Rect {
                    x: arranged.x + offset.x,
                    y: arranged.y + offset.y,
                    ..screens[index].canvas
                };
                let canvas = clear_of(wanted, &taken);
                screens[index].canvas = canvas;
                taken.push(canvas);
            }
        }
        Self { screens }
    }

    pub fn device_screens(&self, device: Fingerprint) -> impl Iterator<Item = &Screen> {
        self.screens.iter().filter(move |s| s.device == device)
    }

    /// The monitor of `device` that contains `pixel`.
    pub fn screen_at(&self, device: Fingerprint, pixel: Point) -> Option<&Screen> {
        self.device_screens(device)
            .find(|screen| screen.monitor.bounds.contains(pixel))
    }

    /// The monitor of `device` closest to `pixel`.
    pub fn nearest_screen(&self, device: Fingerprint, pixel: Point) -> Option<&Screen> {
        self.device_screens(device).min_by(|a, b| {
            let distance = |screen: &Screen| {
                let nearest = clamp_to_pixels(pixel, screen.monitor.bounds);
                (nearest.x - pixel.x).powi(2) + (nearest.y - pixel.y).powi(2)
            };
            distance(a).total_cmp(&distance(b))
        })
    }

    /// Where the cursor goes when it leaves `from` through `side`, level with
    /// `pixel`: the nearest screen straight out from that point on the canvas.
    ///
    /// Screens don't have to touch, however far apart they are. That screen can
    /// be on another device, or be another monitor of the same one, since the
    /// canvas decides the order of the screens whatever the device's own display
    /// settings say. Devices that are offline are looked straight through.
    pub fn crossing(&self, from: &Screen, side: Side, pixel: Point) -> Option<Crossing> {
        let at = from.to_canvas(clamp_to_pixels(pixel, from.monitor.bounds));
        let edge = from.canvas;
        let spans = |start: f64, end: f64, value: f64| value >= start && value < end;

        let mut nearest: Option<(f64, Crossing)> = None;
        for (index, screen) in self.screens.iter().enumerate() {
            if !screen.online || screen.is_same_monitor(from) {
                continue;
            }
            let target = screen.canvas;
            let bounds = screen.monitor.bounds;
            let (gap, level, entry) = match side {
                Side::Right => (
                    target.x - edge.right(),
                    spans(target.y, target.bottom(), at.y),
                    Point::new(bounds.x, screen.canvas_y_to_pixel(at.y)),
                ),
                Side::Left => (
                    edge.x - target.right(),
                    spans(target.y, target.bottom(), at.y),
                    Point::new(bounds.right() - 1.0, screen.canvas_y_to_pixel(at.y)),
                ),
                Side::Bottom => (
                    target.y - edge.bottom(),
                    spans(target.x, target.right(), at.x),
                    Point::new(screen.canvas_x_to_pixel(at.x), bounds.y),
                ),
                Side::Top => (
                    edge.y - target.bottom(),
                    spans(target.x, target.right(), at.x),
                    Point::new(screen.canvas_x_to_pixel(at.x), bounds.bottom() - 1.0),
                ),
            };
            let is_ahead = gap >= -TOUCH_TOLERANCE;
            let is_nearer = nearest.is_none_or(|(best, _)| gap < best);
            if is_ahead && level && is_nearer {
                let crossing = Crossing {
                    device: screen.device,
                    screen: index,
                    entry: clamp_to_pixels(entry, bounds),
                };
                nearest = Some((gap, crossing));
            }
        }
        nearest.map(|(_, crossing)| crossing)
    }
}

/// The nearest pixel position inside `rect`.
///
/// Uses `max` and `min` rather than `clamp`, which panics on an empty range. A
/// panic inside an input hook would take the whole program down.
pub fn clamp_to_pixels(p: Point, rect: Rect) -> Point {
    Point::new(
        p.x.min(rect.right() - 1.0).max(rect.x),
        p.y.min(rect.bottom() - 1.0).max(rect.y),
    )
}

/// Canvas positions for one device's monitors that preserve how they touch in
/// the operating system's arrangement, even when their scales differ.
///
/// Physical pixel coordinates can't simply be divided by a scale: a 4K monitor
/// at 150% is 2560 logical pixels wide, but its neighbour starts at physical
/// x = 3840. So the arrangement is walked outward from one monitor, placing
/// each neighbour against the monitor it touches.
fn arrange_from_os(monitors: &[Monitor]) -> Vec<Point> {
    let mut placed: Vec<Option<Point>> = vec![None; monitors.len()];
    let Some(start) = monitors
        .iter()
        .position(|monitor| monitor.primary)
        .or((!monitors.is_empty()).then_some(0))
    else {
        return Vec::new();
    };
    placed[start] = Some(Point::new(0.0, 0.0));

    let mut queue = VecDeque::from([start]);
    while let Some(current) = queue.pop_front() {
        let from = &monitors[current];
        let from_at = placed[current].unwrap_or(Point::new(0.0, 0.0));
        let (from_width, from_height) = from.canvas_size();
        for (index, monitor) in monitors.iter().enumerate() {
            if placed[index].is_some() {
                continue;
            }
            let (width, height) = monitor.canvas_size();
            let along_x = from_at.x + (monitor.bounds.x - from.bounds.x) / from.scale;
            let along_y = from_at.y + (monitor.bounds.y - from.bounds.y) / from.scale;
            let position = if monitor.bounds.x == from.bounds.right() {
                Some(Point::new(from_at.x + from_width, along_y))
            } else if monitor.bounds.right() == from.bounds.x {
                Some(Point::new(from_at.x - width, along_y))
            } else if monitor.bounds.y == from.bounds.bottom() {
                Some(Point::new(along_x, from_at.y + from_height))
            } else if monitor.bounds.bottom() == from.bounds.y {
                Some(Point::new(along_x, from_at.y - height))
            } else {
                None
            };
            if position.is_some() {
                placed[index] = position;
                queue.push_back(index);
            }
        }
    }

    // Anything not connected to the others keeps its scaled desktop position.
    placed
        .into_iter()
        .zip(monitors)
        .map(|(point, monitor)| {
            point.unwrap_or(Point::new(
                monitor.bounds.x / monitor.scale,
                monitor.bounds.y / monitor.scale,
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const A: Fingerprint = Fingerprint::from_bytes([1; 32]);
    const B: Fingerprint = Fingerprint::from_bytes([2; 32]);
    const C: Fingerprint = Fingerprint::from_bytes([3; 32]);

    fn monitor(id: &str, x: f64, y: f64, width: f64, height: f64, scale: f64) -> Monitor {
        Monitor {
            id: id.into(),
            name: id.into(),
            bounds: Rect::new(x, y, width, height),
            scale,
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

    fn placement(device: Fingerprint, monitor: &str, x: f64, y: f64) -> Placement {
        Placement {
            device,
            monitor: monitor.into(),
            x,
            y,
            width: None,
        }
    }

    /// A monitor of this device that doesn't touch its neighbour on the canvas
    /// can't be reached through the edge between them there.
    #[test]
    fn crossings_can_lead_to_another_monitor_of_the_same_device() {
        let devices = [device(
            A,
            vec![
                monitor("one", 0.0, 0.0, 1920.0, 1080.0, 1.0),
                monitor("two", 1920.0, 0.0, 1920.0, 1080.0, 1.0),
            ],
        )];
        // In the display settings they touch, and on the canvas that's kept.
        let topology = Topology::build(&devices, &LayoutDoc::default());
        let one = topology.screens[0].clone();
        assert_eq!(one.monitor.id, "one");
        let crossing = topology
            .crossing(&one, Side::Right, Point::new(1919.0, 500.0))
            .unwrap();
        assert_eq!(crossing.screen, 1);
        assert_eq!(crossing.device, A);
        assert_eq!(crossing.entry, Point::new(1920.0, 500.0));

        // Moved off to one side on the canvas, nothing lies past that edge any more.
        let mut layout = LayoutDoc::default();
        layout.apply_edit(A, &devices, vec![placement(A, "two", 1920.0, 5000.0)]);
        let topology = Topology::build(&devices, &layout);
        let one = topology.screens[0].clone();
        assert!(
            topology
                .crossing(&one, Side::Right, Point::new(1919.0, 500.0))
                .is_none()
        );
    }

    /// Three screens in a row with gaps: 1000 wide, at x = 0, 3000 and 9000.
    fn row_with_gaps() -> (Topology, LayoutDoc, [DeviceMonitors; 3]) {
        let devices = [
            device(A, vec![monitor("a", 0.0, 0.0, 1000.0, 1000.0, 1.0)]),
            device(B, vec![monitor("b", 0.0, 0.0, 1000.0, 1000.0, 1.0)]),
            device(C, vec![monitor("c", 0.0, 0.0, 1000.0, 1000.0, 1.0)]),
        ];
        let mut layout = LayoutDoc::default();
        layout.apply_edit(
            A,
            &devices,
            vec![
                placement(A, "a", 0.0, 0.0),
                placement(B, "b", 3000.0, 0.0),
                placement(C, "c", 9000.0, 0.0),
            ],
        );
        (Topology::build(&devices, &layout), layout, devices)
    }

    #[test]
    fn screens_connect_across_any_gap_in_all_directions() {
        for (side, offset, from, expected) in [
            (
                Side::Right,
                (5000.0, 0.0),
                Point::new(999.0, 400.0),
                Point::new(0.0, 400.0),
            ),
            (
                Side::Left,
                (-5000.0, 0.0),
                Point::new(0.0, 400.0),
                Point::new(999.0, 400.0),
            ),
            (
                Side::Bottom,
                (0.0, 5000.0),
                Point::new(400.0, 999.0),
                Point::new(400.0, 0.0),
            ),
            (
                Side::Top,
                (0.0, -5000.0),
                Point::new(400.0, 0.0),
                Point::new(400.0, 999.0),
            ),
        ] {
            let devices = [
                device(A, vec![monitor("a", 0.0, 0.0, 1000.0, 1000.0, 1.0)]),
                device(B, vec![monitor("b", 0.0, 0.0, 1000.0, 1000.0, 1.0)]),
            ];
            let mut layout = LayoutDoc::default();
            layout.apply_edit(
                A,
                &devices,
                vec![
                    placement(A, "a", 0.0, 0.0),
                    placement(B, "b", offset.0, offset.1),
                ],
            );
            let topology = Topology::build(&devices, &layout);
            let screen = topology.device_screens(A).next().unwrap();
            let crossing = topology.crossing(screen, side, from).unwrap();
            assert_eq!(crossing.device, B);
            assert_eq!(crossing.entry, expected, "going {side:?}");
        }
    }

    #[test]
    fn the_nearest_screen_in_line_wins() {
        let (topology, ..) = row_with_gaps();
        let a = topology.device_screens(A).next().unwrap();
        let b = topology.device_screens(B).next().unwrap();
        let point = Point::new(999.0, 500.0);
        assert_eq!(topology.crossing(a, Side::Right, point).unwrap().device, B);
        assert_eq!(topology.crossing(b, Side::Right, point).unwrap().device, C);
        assert_eq!(
            topology
                .crossing(b, Side::Left, Point::new(0.0, 500.0))
                .unwrap()
                .device,
            A
        );
    }

    #[test]
    fn offline_screens_in_the_way_are_looked_through() {
        let (_, layout, mut devices) = row_with_gaps();
        devices[1].online = false;
        let topology = Topology::build(&devices, &layout);
        let a = topology.device_screens(A).next().unwrap();
        let crossing = topology
            .crossing(a, Side::Right, Point::new(999.0, 500.0))
            .unwrap();
        assert_eq!(crossing.device, C);
    }

    #[test]
    fn a_tall_screen_reaches_past_a_short_one_in_front_of_it() {
        // A is 1000 tall. The nearer screen only covers its top half, the farther
        // one all of it.
        let devices = [
            device(A, vec![monitor("a", 0.0, 0.0, 1000.0, 1000.0, 1.0)]),
            device(B, vec![monitor("b", 0.0, 0.0, 1000.0, 500.0, 1.0)]),
            device(C, vec![monitor("c", 0.0, 0.0, 1000.0, 1000.0, 1.0)]),
        ];
        let mut layout = LayoutDoc::default();
        layout.apply_edit(
            A,
            &devices,
            vec![
                placement(A, "a", 0.0, 0.0),
                placement(B, "b", 2000.0, 0.0),
                placement(C, "c", 5000.0, 0.0),
            ],
        );
        let topology = Topology::build(&devices, &layout);
        let a = topology.device_screens(A).next().unwrap();
        let at = |y| {
            topology
                .crossing(a, Side::Right, Point::new(999.0, y))
                .unwrap()
                .device
        };
        assert_eq!(at(100.0), B);
        assert_eq!(at(499.0), B);
        assert_eq!(at(500.0), C);
        assert_eq!(at(999.0), C);
    }

    #[test]
    fn screens_behind_are_not_ahead() {
        // B is to the left of A, so going right doesn't reach it.
        let devices = [
            device(A, vec![monitor("a", 0.0, 0.0, 1000.0, 1000.0, 1.0)]),
            device(B, vec![monitor("b", 0.0, 0.0, 1000.0, 1000.0, 1.0)]),
        ];
        let mut layout = LayoutDoc::default();
        layout.apply_edit(
            A,
            &devices,
            vec![placement(A, "a", 0.0, 0.0), placement(B, "b", -1000.0, 0.0)],
        );
        let topology = Topology::build(&devices, &layout);
        let a = topology.device_screens(A).next().unwrap();
        assert!(
            topology
                .crossing(a, Side::Right, Point::new(999.0, 500.0))
                .is_none()
        );
    }

    /// Two PCs next to each other, on the canvas and with their saved places.
    fn side_by_side(width: f64) -> ([DeviceMonitors; 2], LayoutDoc) {
        let devices = [
            device(A, vec![monitor("a", 0.0, 0.0, width, 1080.0, 1.0)]),
            device(B, vec![monitor("b", 0.0, 0.0, 1920.0, 1080.0, 1.0)]),
        ];
        let mut layout = LayoutDoc::default();
        layout.apply_edit(
            A,
            &devices,
            vec![placement(A, "a", 0.0, 0.0), placement(B, "b", 1920.0, 0.0)],
        );
        (devices, layout)
    }

    #[test]
    fn a_screen_that_grows_keeps_its_crossing() {
        let (_, layout) = side_by_side(1920.0);
        // A higher resolution makes it wider, and it would cover part of the other.
        let larger = [
            device(A, vec![monitor("a", 0.0, 0.0, 2560.0, 1080.0, 1.0)]),
            device(B, vec![monitor("b", 0.0, 0.0, 1920.0, 1080.0, 1.0)]),
        ];
        let topology = Topology::build(&larger, &layout);
        assert_eq!(canvas_of(&topology, B, "b").x, 2560.0, "it's pushed along");

        let a = topology.device_screens(A).next().unwrap();
        let crossing = topology.crossing(a, Side::Right, Point::new(2559.0, 500.0));
        assert_eq!(crossing.unwrap().device, B);
    }

    #[test]
    fn a_screen_that_grows_pushes_the_whole_row_and_comes_back_when_it_shrinks() {
        let row = |width: f64| {
            [
                device(A, vec![monitor("a", 0.0, 0.0, width, 1080.0, 1.0)]),
                device(B, vec![monitor("b", 0.0, 0.0, 1920.0, 1080.0, 1.0)]),
                device(C, vec![monitor("c", 0.0, 0.0, 1920.0, 1080.0, 1.0)]),
            ]
        };
        let mut layout = LayoutDoc::default();
        layout.apply_edit(
            A,
            &row(1920.0),
            vec![
                placement(A, "a", 0.0, 0.0),
                placement(B, "b", 1920.0, 0.0),
                placement(C, "c", 3840.0, 0.0),
            ],
        );

        let larger = Topology::build(&row(2560.0), &layout);
        assert_eq!(canvas_of(&larger, B, "b").x, 2560.0);
        assert_eq!(canvas_of(&larger, C, "c").x, 4480.0);

        let back = Topology::build(&row(1920.0), &layout);
        assert_eq!(canvas_of(&back, B, "b").x, 1920.0);
        assert_eq!(canvas_of(&back, C, "c").x, 3840.0);
    }

    #[test]
    fn a_screen_turned_upright_pushes_down_what_is_below_it() {
        let top = |width, height| device(A, vec![monitor("a", 0.0, 0.0, width, height, 1.0)]);
        let below = device(B, vec![monitor("b", 0.0, 0.0, 1920.0, 1080.0, 1.0)]);
        let mut layout = LayoutDoc::default();
        layout.apply_edit(
            A,
            &[top(1920.0, 1080.0), below.clone()],
            vec![placement(A, "a", 0.0, 0.0), placement(B, "b", 0.0, 1080.0)],
        );
        let topology = Topology::build(&[top(1080.0, 1920.0), below], &layout);
        assert_eq!(
            canvas_of(&topology, B, "b"),
            Rect::new(0.0, 1920.0, 1920.0, 1080.0)
        );
    }

    /// In the display settings the new one is to the right, which on the canvas is
    /// exactly where the other PC is.
    #[test]
    fn a_new_screen_does_not_land_on_another_pcs_screen() {
        let (devices, layout) = side_by_side(1920.0);
        let with_new_screen = [
            device(
                A,
                vec![
                    monitor("a", 0.0, 0.0, 1920.0, 1080.0, 1.0),
                    monitor("new", 1920.0, 0.0, 1920.0, 1080.0, 1.0),
                ],
            ),
            devices[1].clone(),
        ];
        let topology = Topology::build(&with_new_screen, &layout);
        assert_eq!(canvas_of(&topology, B, "b").x, 1920.0, "it stays put");

        // Down is the shorter way out, and it's still somewhere the cursor can get to.
        assert_eq!(
            canvas_of(&topology, A, "new"),
            Rect::new(1920.0, 1080.0, 1920.0, 1080.0)
        );
        let b = topology.device_screens(B).next().unwrap();
        let crossing = topology.crossing(b, Side::Bottom, Point::new(500.0, 1079.0));
        assert_eq!(crossing.unwrap().device, A);
    }

    #[test]
    fn a_new_pc_goes_past_screens_saved_for_pcs_listed_after_it() {
        let later = device(C, vec![monitor("c", 0.0, 0.0, 1920.0, 1080.0, 1.0)]);
        let mut layout = LayoutDoc::default();
        layout.apply_edit(
            A,
            std::slice::from_ref(&later),
            vec![placement(C, "c", 0.0, 0.0)],
        );

        // B comes before C in fingerprint order, and nothing is saved for it.
        let new = device(B, vec![monitor("b", 0.0, 0.0, 1920.0, 1080.0, 1.0)]);
        let topology = Topology::build(&[new, later], &layout);
        assert_eq!(canvas_of(&topology, C, "c").x, 0.0);
        assert_eq!(canvas_of(&topology, B, "b").x, 1920.0);
    }

    const ON_PORT_ONE: &str = r"\\?\DISPLAY#DEL40A3#5&2a8a&0&UID4352#{e6f07b5f}";
    const ON_PORT_TWO: &str = r"\\?\DISPLAY#DEL40A3#5&2a8a&0&UID4353#{e6f07b5f}";
    const OTHER_MODEL: &str = r"\\?\DISPLAY#GSM5B09#5&2a8a&0&UID4352#{e6f07b5f}";

    #[test]
    fn a_monitor_moved_to_another_port_keeps_its_place_and_size() {
        let laptop = monitor("laptop", 0.0, 0.0, 1920.0, 1080.0, 1.0);
        let before = [device(
            A,
            vec![
                laptop.clone(),
                monitor(ON_PORT_ONE, 1920.0, 0.0, 2560.0, 1440.0, 1.0),
            ],
        )];
        let mut layout = LayoutDoc::default();
        layout.apply_edit(
            A,
            &before,
            vec![Placement {
                width: Some(1000.0),
                ..placement(A, ON_PORT_ONE, 5000.0, -300.0)
            }],
        );

        let after = [device(
            A,
            vec![
                laptop,
                monitor(ON_PORT_TWO, 1920.0, 0.0, 2560.0, 1440.0, 1.0),
            ],
        )];
        let topology = Topology::build(&after, &layout);
        let moved = canvas_of(&topology, A, ON_PORT_TWO);
        assert_eq!((moved.x, moved.y, moved.width), (5000.0, -300.0, 1000.0));
    }

    #[test]
    fn a_monitor_only_takes_the_place_of_one_of_the_same_model_that_has_gone() {
        let first = monitor(ON_PORT_ONE, 0.0, 0.0, 1920.0, 1080.0, 1.0);
        let mut layout = LayoutDoc::default();
        layout.apply_edit(
            A,
            &[device(A, vec![first.clone()])],
            vec![placement(A, ON_PORT_ONE, 500.0, 300.0)],
        );

        // A second one like it is a second monitor, not the first one moved.
        let second = monitor(ON_PORT_TWO, 1920.0, 0.0, 1920.0, 1080.0, 1.0);
        let topology = Topology::build(&[device(A, vec![first, second])], &layout);
        assert_eq!(canvas_of(&topology, A, ON_PORT_ONE).x, 500.0);
        assert_eq!(canvas_of(&topology, A, ON_PORT_TWO).x, 2420.0);

        // Nor does another model take over, or an ID that doesn't say what model it is.
        for id in [OTHER_MODEL, "no-model-in-this"] {
            let replaced = [device(A, vec![monitor(id, 0.0, 0.0, 1920.0, 1080.0, 1.0)])];
            let topology = Topology::build(&replaced, &layout);
            assert_eq!(canvas_of(&topology, A, id).x, 0.0, "{id}");
        }
    }

    #[test]
    fn another_device_has_no_say_in_where_a_moved_monitor_goes() {
        let mut layout = LayoutDoc::default();
        layout.apply_edit(
            A,
            &[device(
                B,
                vec![monitor(ON_PORT_ONE, 0.0, 0.0, 1920.0, 1080.0, 1.0)],
            )],
            vec![placement(B, ON_PORT_ONE, 7000.0, 0.0)],
        );
        // The same model on another PC is a different monitor.
        let here = [device(
            A,
            vec![monitor(ON_PORT_TWO, 0.0, 0.0, 1920.0, 1080.0, 1.0)],
        )];
        let topology = Topology::build(&here, &layout);
        assert_eq!(canvas_of(&topology, A, ON_PORT_TWO).x, 0.0);
    }

    #[test]
    fn it_says_whether_these_exact_screens_have_a_saved_arrangement() {
        let (devices, mut layout) = side_by_side(1920.0);
        assert!(layout.is_saved_for(&devices));
        assert_eq!(layout.saved_count(), 1);

        let fewer = [devices[0].clone()];
        assert!(!layout.is_saved_for(&fewer));

        // Saving them as they are is an edit that moves nothing.
        let before = Topology::build(&fewer, &layout);
        layout.apply_edit(A, &fewer, vec![]);
        assert!(layout.is_saved_for(&fewer));
        assert_eq!(layout.saved_count(), 2);
        assert_eq!(Topology::build(&fewer, &layout), before);
    }

    #[test]
    fn arrangements_made_while_apart_survive_a_newer_copy() {
        let peer = device(B, vec![monitor("b", 0.0, 0.0, 1920.0, 1080.0, 1.0)]);
        let laptop = monitor("laptop", 0.0, 0.0, 1920.0, 1080.0, 1.0);
        let external = monitor("external", 1920.0, 0.0, 2560.0, 1440.0, 1.0);
        let docked = [device(A, vec![laptop.clone(), external]), peer.clone()];
        let undocked = [device(A, vec![laptop]), peer];

        // This one arranged the screens while docked, and the other, with a later count,
        // arranged them with only the laptop, knowing nothing about that.
        let mut here = LayoutDoc::default();
        here.apply_edit(A, &docked, vec![placement(B, "b", 8000.0, 0.0)]);
        let mut there = LayoutDoc::default();
        there.apply_edit(B, &undocked, vec![placement(B, "b", -1920.0, 0.0)]);
        there.apply_edit(B, &undocked, vec![placement(B, "b", -2000.0, 0.0)]);
        assert!(there.is_newer_than(&here));

        here.adopt(there.clone());
        assert_eq!(
            canvas_of(&Topology::build(&docked, &here), B, "b").x,
            8000.0,
            "still there"
        );
        assert_eq!(
            canvas_of(&Topology::build(&undocked, &here), B, "b").x,
            -2000.0
        );
        assert!(!there.is_newer_than(&here), "it doesn't go back and forth");
        assert!(!here.is_newer_than(&there));
    }

    #[test]
    fn a_newer_copy_wins_for_screens_both_copies_know() {
        let (devices, older) = side_by_side(1920.0);
        let mut newer = older.clone();
        newer.apply_edit(B, &devices, vec![placement(B, "b", 2000.0, 0.0)]);
        let mut here = older;
        here.adopt(newer);
        assert_eq!(
            canvas_of(&Topology::build(&devices, &here), B, "b").x,
            2000.0
        );
    }

    #[test]
    fn a_resized_monitor_keeps_its_shape() {
        let devices = [device(
            A,
            vec![monitor("a", 0.0, 0.0, 1920.0, 1200.0, 1.25)],
        )];
        let mut layout = LayoutDoc::default();
        layout.apply_edit(
            A,
            &devices,
            vec![Placement {
                width: Some(3200.0),
                ..placement(A, "a", 0.0, 0.0)
            }],
        );
        let topology = Topology::build(&devices, &layout);
        assert_eq!(canvas_of(&topology, A, "a").width, 3200.0);
        assert_eq!(canvas_of(&topology, A, "a").height, 2000.0);
        assert_eq!(topology.screens[0].custom_width, Some(3200.0));
    }

    #[test]
    fn crossing_a_resized_monitor_lines_up_with_how_it_is_drawn() {
        // Each is 1000 pixels tall, but this one is drawn half as large.
        let devices = [
            device(A, vec![monitor("a", 0.0, 0.0, 2000.0, 1000.0, 1.0)]),
            device(B, vec![monitor("b", 0.0, 0.0, 2000.0, 1000.0, 1.0)]),
        ];
        let mut layout = LayoutDoc::default();
        layout.apply_edit(
            A,
            &devices,
            vec![
                placement(A, "a", 0.0, 0.0),
                Placement {
                    width: Some(1000.0),
                    ..placement(B, "b", 2000.0, 0.0)
                },
            ],
        );
        let topology = Topology::build(&devices, &layout);
        let from = topology.device_screens(A).next().unwrap();
        // A quarter of the way down the large one is halfway down the small one.
        let crossing = topology
            .crossing(from, Side::Right, Point::new(1999.0, 250.0))
            .unwrap();
        assert_eq!(crossing.entry, Point::new(0.0, 500.0));
        // Below the bottom of the small one, nothing lines up.
        assert!(
            topology
                .crossing(from, Side::Right, Point::new(1999.0, 700.0))
                .is_none()
        );

        // And going back lands where it left from.
        let to = topology.device_screens(B).next().unwrap();
        let back = topology.crossing(to, Side::Left, crossing.entry).unwrap();
        assert_eq!(back.entry, Point::new(1999.0, 250.0));
    }

    #[test]
    fn nonsense_sizes_from_another_device_are_ignored() {
        let devices = [device(A, vec![monitor("a", 0.0, 0.0, 1920.0, 1080.0, 1.0)])];
        for width in [0.0, -5.0, f64::NAN, f64::INFINITY] {
            let mut layout = LayoutDoc::default();
            layout.apply_edit(
                A,
                &devices,
                vec![Placement {
                    width: Some(width),
                    ..placement(A, "a", 0.0, 0.0)
                }],
            );
            let topology = Topology::build(&devices, &layout);
            assert_eq!(canvas_of(&topology, A, "a").width, 1920.0);
        }
    }

    #[test]
    fn moving_a_screen_keeps_its_custom_size_and_layouts_saved_before_sizes_still_load() {
        let devices = [device(A, vec![monitor("a", 0.0, 0.0, 1920.0, 1080.0, 1.0)])];
        let mut layout = LayoutDoc::default();
        layout.apply_edit(
            A,
            &devices,
            vec![Placement {
                width: Some(1000.0),
                ..placement(A, "a", 0.0, 0.0)
            }],
        );
        // The edit only mentions some other monitor: this one stays as it is.
        layout.apply_edit(A, &devices, vec![]);
        let topology = Topology::build(&devices, &layout);
        assert_eq!(canvas_of(&topology, A, "a").width, 1000.0);

        // A layout file written by an earlier version has no sizes.
        let old_file = format!(
            r#"{{"counter":3,"author":null,"profiles":[],"last_known":[{{"device":"{A}","monitor":"a","x":5.0,"y":6.0}}]}}"#
        );
        let old: LayoutDoc = serde_json::from_str(&old_file).unwrap();
        let topology = Topology::build(&devices, &old);
        assert_eq!(
            canvas_of(&topology, A, "a"),
            Rect::new(5.0, 6.0, 1920.0, 1080.0)
        );
    }

    fn canvas_of(topology: &Topology, device: Fingerprint, id: &str) -> Rect {
        topology
            .screens
            .iter()
            .find(|screen| screen.device == device && screen.monitor.id == id)
            .unwrap()
            .canvas
    }

    #[test]
    fn mixed_dpi_neighbours_touch_on_the_canvas() {
        // A 4K monitor at 150% with a 1080p monitor to its right.
        let devices = [device(
            A,
            vec![
                monitor("4k", 0.0, 0.0, 3840.0, 2160.0, 1.5),
                monitor("hd", 3840.0, 0.0, 1920.0, 1080.0, 1.0),
            ],
        )];
        let topology = Topology::build(&devices, &LayoutDoc::default());
        assert_eq!(
            canvas_of(&topology, A, "4k"),
            Rect::new(0.0, 0.0, 2560.0, 1440.0)
        );
        assert_eq!(
            canvas_of(&topology, A, "hd"),
            Rect::new(2560.0, 0.0, 1920.0, 1080.0)
        );
    }

    #[test]
    fn new_devices_go_to_the_right_regardless_of_the_order_given() {
        let a = device(A, vec![monitor("a", 0.0, 0.0, 1920.0, 1080.0, 1.0)]);
        let b = device(B, vec![monitor("b", 0.0, 0.0, 2560.0, 1440.0, 1.0)]);
        let forward = Topology::build(&[a.clone(), b.clone()], &LayoutDoc::default());
        let backward = Topology::build(&[b, a], &LayoutDoc::default());
        assert_eq!(forward, backward);
        assert_eq!(canvas_of(&forward, B, "b").x, 1920.0);
    }

    #[test]
    fn crossing_keeps_the_visual_height_across_different_scales() {
        // 4K at 150% (2560x1440 logical) on the left, 1080p at 100% on the right.
        let devices = [
            device(A, vec![monitor("a", 0.0, 0.0, 3840.0, 2160.0, 1.5)]),
            device(B, vec![monitor("b", 0.0, 0.0, 1920.0, 1080.0, 1.0)]),
        ];
        let topology = Topology::build(&devices, &LayoutDoc::default());
        let from = topology.device_screens(A).next().unwrap();
        // 600 logical pixels down is physical row 900 on the 4K monitor.
        let crossing = topology
            .crossing(from, Side::Right, Point::new(3839.0, 900.0))
            .unwrap();
        assert_eq!(crossing.device, B);
        assert_eq!(crossing.entry, Point::new(0.0, 600.0));
    }

    #[test]
    fn no_crossing_where_monitors_do_not_line_up() {
        let mut layout = LayoutDoc::default();
        let devices = [
            device(A, vec![monitor("a", 0.0, 0.0, 1920.0, 1080.0, 1.0)]),
            device(B, vec![monitor("b", 0.0, 0.0, 1920.0, 1080.0, 1.0)]),
        ];
        // B is to the right of A but 800 pixels lower, overlapping the lower 280.
        layout.apply_edit(A, &devices, vec![placement(B, "b", 1920.0, 800.0)]);
        let topology = Topology::build(&devices, &layout);
        let from = topology.device_screens(A).next().unwrap();
        assert!(
            topology
                .crossing(from, Side::Right, Point::new(1919.0, 500.0))
                .is_none()
        );
        assert_eq!(
            topology
                .crossing(from, Side::Right, Point::new(1919.0, 900.0))
                .unwrap()
                .entry,
            Point::new(0.0, 100.0)
        );
    }

    #[test]
    fn each_edge_can_lead_to_a_different_device() {
        let mut layout = LayoutDoc::default();
        let devices = [
            device(A, vec![monitor("a", 0.0, 0.0, 1920.0, 1080.0, 1.0)]),
            device(B, vec![monitor("b", 0.0, 0.0, 1920.0, 1080.0, 1.0)]),
            device(C, vec![monitor("c", 0.0, 0.0, 1920.0, 1080.0, 1.0)]),
        ];
        layout.apply_edit(
            A,
            &devices,
            vec![
                placement(B, "b", 1920.0, 0.0),
                placement(C, "c", 0.0, -1080.0),
            ],
        );
        let topology = Topology::build(&devices, &layout);
        let from = topology.device_screens(A).next().unwrap();
        let center = Point::new(960.0, 540.0);
        assert_eq!(
            topology.crossing(from, Side::Right, center).unwrap().device,
            B
        );
        assert_eq!(
            topology.crossing(from, Side::Top, center).unwrap().device,
            C
        );
        assert!(topology.crossing(from, Side::Left, center).is_none());
        assert!(topology.crossing(from, Side::Bottom, center).is_none());
    }

    #[test]
    fn offline_devices_keep_their_place_but_cannot_be_crossed_into() {
        let mut b = device(B, vec![monitor("b", 0.0, 0.0, 1920.0, 1080.0, 1.0)]);
        b.online = false;
        let devices = [
            device(A, vec![monitor("a", 0.0, 0.0, 1920.0, 1080.0, 1.0)]),
            b,
        ];
        let topology = Topology::build(&devices, &LayoutDoc::default());
        assert_eq!(canvas_of(&topology, B, "b").x, 1920.0);
        let from = topology.device_screens(A).next().unwrap();
        assert!(
            topology
                .crossing(from, Side::Right, Point::new(1919.0, 500.0))
                .is_none()
        );
    }

    #[test]
    fn replugged_monitor_returns_to_its_saved_place() {
        let laptop = monitor("laptop", 0.0, 0.0, 1920.0, 1080.0, 1.0);
        let external = monitor("external", 1920.0, 0.0, 2560.0, 1440.0, 1.0);
        let docked = [device(A, vec![laptop.clone(), external.clone()])];
        let mut layout = LayoutDoc::default();
        layout.apply_edit(A, &docked, vec![placement(A, "external", 5000.0, -300.0)]);

        let undocked = [device(A, vec![laptop.clone()])];
        let topology = Topology::build(&undocked, &layout);
        assert_eq!(topology.screens.len(), 1);

        let topology = Topology::build(&docked, &layout);
        assert_eq!(canvas_of(&topology, A, "external").x, 5000.0);
    }

    #[test]
    fn each_set_of_monitors_keeps_its_own_arrangement() {
        let laptop = monitor("laptop", 0.0, 0.0, 1920.0, 1080.0, 1.0);
        let external = monitor("external", 1920.0, 0.0, 2560.0, 1440.0, 1.0);
        let peer = device(B, vec![monitor("b", 0.0, 0.0, 1920.0, 1080.0, 1.0)]);
        let docked = [device(A, vec![laptop.clone(), external]), peer.clone()];
        let undocked = [device(A, vec![laptop]), peer];

        let mut layout = LayoutDoc::default();
        layout.apply_edit(A, &docked, vec![placement(B, "b", 4480.0, 0.0)]);
        layout.apply_edit(A, &undocked, vec![placement(B, "b", -1920.0, 0.0)]);

        assert_eq!(
            canvas_of(&Topology::build(&undocked, &layout), B, "b").x,
            -1920.0
        );
        assert_eq!(
            canvas_of(&Topology::build(&docked, &layout), B, "b").x,
            4480.0
        );
    }

    #[test]
    fn unknown_monitor_appears_next_to_its_os_neighbour() {
        let first = monitor("first", 0.0, 0.0, 1920.0, 1080.0, 1.0);
        let second = monitor("second", 1920.0, 0.0, 1920.0, 1080.0, 1.0);
        let mut layout = LayoutDoc::default();
        layout.apply_edit(
            A,
            &[device(A, vec![first.clone()])],
            vec![placement(A, "first", 500.0, 300.0)],
        );
        let topology = Topology::build(&[device(A, vec![first, second])], &layout);
        assert_eq!(
            canvas_of(&topology, A, "second"),
            Rect::new(2420.0, 300.0, 1920.0, 1080.0)
        );
    }

    #[test]
    fn newest_document_wins_and_ties_go_to_the_author() {
        let empty = LayoutDoc::default();
        let devices = [device(A, vec![monitor("a", 0.0, 0.0, 100.0, 100.0, 1.0)])];
        let mut by_a = empty.clone();
        by_a.apply_edit(A, &devices, vec![]);
        let mut by_b = empty.clone();
        by_b.apply_edit(B, &devices, vec![]);
        assert!(by_a.is_newer_than(&empty));
        assert!(!empty.is_newer_than(&by_a));
        assert!(by_b.is_newer_than(&by_a));
        assert!(!by_a.is_newer_than(&by_b));
        assert!(!by_a.is_newer_than(&by_a.clone()));
    }

    #[test]
    fn saved_documents_load_back() {
        let mut layout = LayoutDoc::default();
        let devices = [device(A, vec![monitor("a", 0.0, 0.0, 100.0, 100.0, 1.0)])];
        layout.apply_edit(A, &devices, vec![placement(A, "a", 10.0, 20.0)]);
        let bytes = postcard::to_allocvec(&layout).unwrap();
        assert_eq!(postcard::from_bytes::<LayoutDoc>(&bytes).unwrap(), layout);
    }

    proptest! {
        /// However they're saved and whatever size they've become, screens end up clear
        /// of each other, the same on every peer, and an edit moving nothing keeps them.
        #[test]
        fn screens_never_end_up_on_top_of_each_other(
            screens in prop::collection::vec(
                (-4000.0f64..4000.0, -4000.0f64..4000.0, 400.0f64..4000.0, 300.0f64..3000.0,
                 prop::bool::ANY),
                1..9,
            ),
        ) {
            let ids: Vec<Fingerprint> = (0..screens.len())
                .map(|at| Fingerprint::from_bytes([at as u8 + 1; 32]))
                .collect();
            let devices: Vec<DeviceMonitors> = screens
                .iter()
                .zip(&ids)
                .map(|(&(.., width, height, _), &id)| {
                    device(id, vec![monitor("m", 0.0, 0.0, width, height, 1.0)])
                })
                .collect();
            let mut layout = LayoutDoc::default();
            let placements = screens
                .iter()
                .zip(&ids)
                .filter(|((.., saved), _)| *saved)
                .map(|(&(x, y, ..), &id)| placement(id, "m", x, y))
                .collect();
            layout.apply_edit(A, &devices, placements);

            let topology = Topology::build(&devices, &layout);
            prop_assert_eq!(topology.screens.len(), screens.len());
            for (at, first) in topology.screens.iter().enumerate() {
                for second in &topology.screens[at + 1..] {
                    prop_assert!(
                        !on_top_of(first.canvas, second.canvas),
                        "{:?} and {:?}", first.canvas, second.canvas
                    );
                }
            }

            let reversed: Vec<DeviceMonitors> = devices.iter().rev().cloned().collect();
            prop_assert_eq!(&Topology::build(&reversed, &layout), &topology);

            layout.apply_edit(A, &devices, vec![]);
            prop_assert_eq!(&Topology::build(&devices, &layout), &topology);
        }

        /// Wherever two monitors share an edge, every row along it crosses
        /// over, and crossing back lands within a pixel or so of the start.
        #[test]
        fn shared_edges_have_no_dead_zones_and_round_trip(
            width_a in 640u32..8000, height_a in 480u32..5000,
            width_b in 640u32..8000, height_b in 480u32..5000,
            scale_a in prop::sample::select(vec![1.0, 1.25, 1.5, 1.75, 2.0, 3.0]),
            scale_b in prop::sample::select(vec![1.0, 1.25, 1.5, 1.75, 2.0, 3.0]),
            offset in 0.0f64..1.0,
            row in 0.0f64..1.0,
        ) {
            let (width_a, height_a) = (f64::from(width_a), f64::from(height_a));
            let (width_b, height_b) = (f64::from(width_b), f64::from(height_b));
            let a_monitor = monitor("a", 0.0, 0.0, width_a, height_a, scale_a);
            let b_monitor = monitor("b", 0.0, 0.0, width_b, height_b, scale_b);
            let devices = [device(A, vec![a_monitor]), device(B, vec![b_monitor])];

            // Place B against A's right edge, sliding down by part of A's height.
            let a_logical = (width_a / scale_a, height_a / scale_a);
            let b_y = offset * a_logical.1;
            let mut layout = LayoutDoc::default();
            layout.apply_edit(A, &devices, vec![
                placement(A, "a", 0.0, 0.0),
                placement(B, "b", a_logical.0, b_y),
            ]);
            let topology = Topology::build(&devices, &layout);
            let from = topology.device_screens(A).next().unwrap().clone();
            let to = topology.device_screens(B).next().unwrap().clone();

            let pixel = Point::new(width_a - 1.0, (row * height_a).floor());
            let logical_y = pixel.y / scale_a;
            let shared = logical_y >= b_y && logical_y < b_y + height_b / scale_b;
            let crossing = topology.crossing(&from, Side::Right, pixel);
            prop_assert_eq!(crossing.is_some(), shared);

            if let Some(crossing) = crossing {
                prop_assert_eq!(crossing.device, B);
                prop_assert!(to.monitor.bounds.contains(crossing.entry));
                prop_assert_eq!(crossing.entry.x, 0.0);

                let back = topology.crossing(&to, Side::Left, crossing.entry).unwrap();
                prop_assert_eq!(back.device, A);
                prop_assert_eq!(back.entry.x, width_a - 1.0);
                prop_assert!((back.entry.y - pixel.y).abs() <= scale_a.max(scale_b) + 1.0);
            }
        }
    }
}
