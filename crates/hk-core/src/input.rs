//! Input events as they travel from the controlling machine to a target.

use std::collections::HashSet;

pub use keyboard_types::Code as KeyCode;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    /// Side button usually bound to "back" (XBUTTON1 on Windows).
    Back,
    /// Side button usually bound to "forward" (XBUTTON2 on Windows).
    Forward,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum InputEvent {
    /// Relative pointer movement in the controller's logical pixels.
    PointerMotion {
        dx: f64,
        dy: f64,
    },
    Button {
        button: MouseButton,
        pressed: bool,
    },
    /// Scroll distance in wheel notches. High-resolution wheels and touchpads
    /// produce fractional values. Positive `dy` is the wheel turned away from
    /// the user, positive `dx` is a tilt to the right (both as on Windows).
    Scroll {
        dx: f64,
        dy: f64,
    },
    /// A physical key, identified by its W3C `code` (its position on the
    /// keyboard, not the character it types) so the target applies its own
    /// keyboard layout.
    Key {
        #[serde(with = "key_code_by_name")]
        code: KeyCode,
        pressed: bool,
    },
    /// Absolute pointer position in the target's own desktop pixels.
    PointerPosition {
        x: f64,
        y: f64,
    },
}

/// Remembers which keys and buttons a peer is holding down on this machine,
/// so they can all be released when control leaves or the connection drops.
/// Without this, a peer that disconnects mid-press leaves a key stuck down.
#[derive(Debug, Default)]
pub struct Held {
    keys: HashSet<KeyCode>,
    buttons: HashSet<MouseButton>,
}

impl Held {
    pub fn track(&mut self, event: &InputEvent) {
        match *event {
            InputEvent::Key {
                code,
                pressed: true,
            } => {
                self.keys.insert(code);
            }
            InputEvent::Key {
                code,
                pressed: false,
            } => {
                self.keys.remove(&code);
            }
            InputEvent::Button {
                button,
                pressed: true,
            } => {
                self.buttons.insert(button);
            }
            InputEvent::Button {
                button,
                pressed: false,
            } => {
                self.buttons.remove(&button);
            }
            _ => {}
        }
    }

    /// Release events for everything still held, after which nothing is held.
    pub fn release_all(&mut self) -> Vec<InputEvent> {
        let keys = self.keys.drain().map(|code| InputEvent::Key {
            code,
            pressed: false,
        });
        let buttons = self.buttons.drain().map(|button| InputEvent::Button {
            button,
            pressed: false,
        });
        keys.chain(buttons).collect()
    }
}

/// Serializes key codes by their W3C name ("KeyA") instead of the derived
/// variant index, so peers built against different keyboard-types versions
/// still agree on which key is which.
mod key_code_by_name {
    use super::KeyCode;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(code: &KeyCode, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(code)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<KeyCode, D::Error> {
        let name = String::deserialize(deserializer)?;
        // A key that only a newer peer knows about should not fail the whole message.
        Ok(name.parse().unwrap_or(KeyCode::Unidentified))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize)]
    struct Named {
        code: &'static str,
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Coded {
        #[serde(with = "key_code_by_name")]
        code: KeyCode,
    }

    #[test]
    fn key_codes_are_encoded_by_name() {
        let by_code = postcard::to_allocvec(&Coded {
            code: KeyCode::KeyA,
        })
        .unwrap();
        let by_name = postcard::to_allocvec(&Named { code: "KeyA" }).unwrap();
        assert_eq!(by_code, by_name);
    }

    #[test]
    fn known_key_codes_round_trip() {
        for code in [
            KeyCode::KeyA,
            KeyCode::Enter,
            KeyCode::MetaLeft,
            KeyCode::F24,
        ] {
            let bytes = postcard::to_allocvec(&Coded { code }).unwrap();
            assert_eq!(
                postcard::from_bytes::<Coded>(&bytes).unwrap(),
                Coded { code }
            );
        }
    }

    #[test]
    fn unknown_key_name_becomes_unidentified() {
        let bytes = postcard::to_allocvec(&Named {
            code: "KeyFromTheFuture",
        })
        .unwrap();
        let decoded: Coded = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.code, KeyCode::Unidentified);
    }

    fn key(code: KeyCode, pressed: bool) -> InputEvent {
        InputEvent::Key { code, pressed }
    }

    #[test]
    fn held_releases_only_what_is_still_down() {
        let mut held = Held::default();
        held.track(&key(KeyCode::ShiftLeft, true));
        held.track(&key(KeyCode::KeyA, true));
        held.track(&key(KeyCode::KeyA, false));
        held.track(&InputEvent::Button {
            button: MouseButton::Left,
            pressed: true,
        });
        held.track(&InputEvent::PointerPosition { x: 1.0, y: 2.0 });

        let mut released = held.release_all();
        released.sort_by_key(|event| matches!(event, InputEvent::Button { .. }));
        assert_eq!(
            released,
            [
                key(KeyCode::ShiftLeft, false),
                InputEvent::Button {
                    button: MouseButton::Left,
                    pressed: false
                },
            ]
        );
        assert!(held.release_all().is_empty());
    }
}
