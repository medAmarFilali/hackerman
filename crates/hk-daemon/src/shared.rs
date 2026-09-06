//! State used on the input hook's thread as well as the agent's other tasks.
//!
//! Every keystroke and mouse movement on the computer waits for the hook, so
//! nothing here may block: locks are held only briefly, and messages go into
//! unbounded queues.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, Instant};

use hk_core::device::Fingerprint;
use hk_core::switcher::{Action, LocalEvent, Switcher};
use hk_platform::Platform;
use hk_proto::Message;
use tokio::sync::mpsc::UnboundedSender;

use crate::engine::Event;

/// How recently this device's own keyboard or mouse must have been used for it to
/// count as the one being used.
const IN_USE_WINDOW: Duration = Duration::from_secs(10);

const NEVER: u64 = u64::MAX;

#[derive(Clone)]
pub struct Shared<P> {
    switcher: Arc<Mutex<Switcher>>,
    peers: Arc<RwLock<HashMap<Fingerprint, UnboundedSender<Message>>>>,
    pub platform: P,
    events: UnboundedSender<Event>,
    started: Instant,
    /// When input last stayed on this machine, in milliseconds since `started`.
    last_local_input: Arc<AtomicU64>,
    /// Whether to tell the main loop about the next input that stays on this machine.
    report_next_input: Arc<AtomicBool>,
    /// Whether to replay input from other devices. Sessions check this themselves,
    /// so input stops at once rather than after a trip through the main loop.
    accepts_input: Arc<AtomicBool>,
}

impl<P: Platform> Shared<P> {
    pub fn new(switcher: Switcher, platform: P, events: UnboundedSender<Event>) -> Self {
        Self {
            accepts_input: Arc::new(AtomicBool::new(true)),
            switcher: Arc::new(Mutex::new(switcher)),
            peers: Arc::default(),
            platform,
            events,
            started: Instant::now(),
            last_local_input: Arc::new(AtomicU64::new(NEVER)),
            report_next_input: Arc::default(),
        }
    }

    /// Called for every local input event. Returns whether to hide the event
    /// from this machine.
    pub fn on_local_input(&self, event: LocalEvent) -> bool {
        let now = self.started.elapsed();
        let outcome = self.switcher().handle_local(event, now);
        if !outcome.swallow {
            self.last_local_input
                .store(now.as_millis() as u64, Ordering::Relaxed);
            if self.report_next_input.swap(false, Ordering::Relaxed) {
                let _ = self.events.send(Event::LocalInput);
            }
        }
        self.apply(outcome.actions);
        outcome.swallow
    }

    /// Whether someone has just been using this machine's own keyboard or mouse.
    pub fn in_use_locally(&self) -> bool {
        let last = self.last_local_input.load(Ordering::Relaxed);
        last != NEVER
            && (self.started.elapsed().as_millis() as u64).saturating_sub(last)
                <= IN_USE_WINDOW.as_millis() as u64
    }

    pub fn report_next_local_input(&self) {
        self.report_next_input.store(true, Ordering::Relaxed);
    }

    pub fn set_accepts_input(&self, accepts: bool) {
        self.accepts_input.store(accepts, Ordering::Relaxed);
    }

    /// What a session checks before replaying input from its peer.
    pub fn accepts_input(&self) -> Arc<AtomicBool> {
        self.accepts_input.clone()
    }

    pub fn switcher(&self) -> MutexGuard<'_, Switcher> {
        // The switcher has no invariants a panic could break halfway, so a
        // poisoned lock is still safe to use.
        self.switcher.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Carries out the switcher's decisions. Must be called without holding its
    /// lock: moving the cursor can trigger the input hook, which takes the lock.
    pub fn apply(&self, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Enter(device) => {
                    self.send(device, Message::Enter);
                    let _ = self.events.send(Event::ControlChanged);
                }
                Action::Leave(device) => {
                    self.send(device, Message::Leave);
                    let _ = self.events.send(Event::ControlChanged);
                }
                Action::Forward(device, event) => self.send(device, Message::Input(event)),
                Action::ParkCursor(at) => self.platform.park_cursor(at),
                Action::RestoreCursor(to) => self.platform.restore_cursor(to),
                Action::StateChanged => {
                    let _ = self.events.send(Event::ControlChanged);
                }
            }
        }
    }

    fn send(&self, device: Fingerprint, message: Message) {
        let peers = self.peers.read().unwrap_or_else(PoisonError::into_inner);
        if let Some(peer) = peers.get(&device) {
            // Fails only if the session just ended, which is about to be noticed.
            let _ = peer.send(message);
        }
    }

    pub fn add_peer(&self, device: Fingerprint, sender: UnboundedSender<Message>) {
        self.peers
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(device, sender);
    }

    pub fn remove_peer(&self, device: Fingerprint) {
        self.peers
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&device);
    }
}
