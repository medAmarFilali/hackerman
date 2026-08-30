//! Translation between Windows scan codes and W3C key codes.
//!
//! Scan codes identify physical key positions, independent of the keyboard
//! layout, which is exactly what W3C codes describe. Extended keys (sent with
//! an 0xE0 prefix) are written here as 0xE0xx.

use hk_core::input::KeyCode;
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_NUMLOCK, VK_PAUSE};

/// How to replay a key with `SendInput`.
pub(super) struct KeyStroke {
    /// Virtual-key code, or 0 to send `scan` as a physical scan code.
    pub vk: u16,
    pub scan: u16,
    pub extended: bool,
}

pub(super) fn from_windows(vk: u32, scan: u32, extended: bool) -> KeyCode {
    // Pause and NumLock share scan code 0x45 and Windows reports their
    // extended flag inconsistently, but their virtual-key codes are reliable.
    if vk == u32::from(VK_PAUSE.0) {
        return KeyCode::Pause;
    }
    if vk == u32::from(VK_NUMLOCK.0) {
        return KeyCode::NumLock;
    }
    let scan = (scan & 0xFF) as u16 | if extended { 0xE000 } else { 0 };
    SCAN_CODES
        .iter()
        .find(|(candidate, _)| *candidate == scan)
        .map_or(KeyCode::Unidentified, |&(_, code)| code)
}

pub(super) fn to_windows(code: KeyCode) -> Option<KeyStroke> {
    match code {
        KeyCode::Pause => Some(KeyStroke {
            vk: VK_PAUSE.0,
            scan: 0,
            extended: false,
        }),
        KeyCode::NumLock => Some(KeyStroke {
            vk: VK_NUMLOCK.0,
            scan: 0x45,
            extended: true,
        }),
        _ => SCAN_CODES
            .iter()
            .find(|(_, candidate)| *candidate == code)
            .map(|&(scan, _)| KeyStroke {
                vk: 0,
                scan: scan & 0xFF,
                extended: scan & 0xE000 != 0,
            }),
    }
}

/// From the "Windows" column of the W3C UI Events KeyboardEvent code table.
const SCAN_CODES: &[(u16, KeyCode)] = &[
    (0x0001, KeyCode::Escape),
    (0x0002, KeyCode::Digit1),
    (0x0003, KeyCode::Digit2),
    (0x0004, KeyCode::Digit3),
    (0x0005, KeyCode::Digit4),
    (0x0006, KeyCode::Digit5),
    (0x0007, KeyCode::Digit6),
    (0x0008, KeyCode::Digit7),
    (0x0009, KeyCode::Digit8),
    (0x000A, KeyCode::Digit9),
    (0x000B, KeyCode::Digit0),
    (0x000C, KeyCode::Minus),
    (0x000D, KeyCode::Equal),
    (0x000E, KeyCode::Backspace),
    (0x000F, KeyCode::Tab),
    (0x0010, KeyCode::KeyQ),
    (0x0011, KeyCode::KeyW),
    (0x0012, KeyCode::KeyE),
    (0x0013, KeyCode::KeyR),
    (0x0014, KeyCode::KeyT),
    (0x0015, KeyCode::KeyY),
    (0x0016, KeyCode::KeyU),
    (0x0017, KeyCode::KeyI),
    (0x0018, KeyCode::KeyO),
    (0x0019, KeyCode::KeyP),
    (0x001A, KeyCode::BracketLeft),
    (0x001B, KeyCode::BracketRight),
    (0x001C, KeyCode::Enter),
    (0x001D, KeyCode::ControlLeft),
    (0x001E, KeyCode::KeyA),
    (0x001F, KeyCode::KeyS),
    (0x0020, KeyCode::KeyD),
    (0x0021, KeyCode::KeyF),
    (0x0022, KeyCode::KeyG),
    (0x0023, KeyCode::KeyH),
    (0x0024, KeyCode::KeyJ),
    (0x0025, KeyCode::KeyK),
    (0x0026, KeyCode::KeyL),
    (0x0027, KeyCode::Semicolon),
    (0x0028, KeyCode::Quote),
    (0x0029, KeyCode::Backquote),
    (0x002A, KeyCode::ShiftLeft),
    (0x002B, KeyCode::Backslash),
    (0x002C, KeyCode::KeyZ),
    (0x002D, KeyCode::KeyX),
    (0x002E, KeyCode::KeyC),
    (0x002F, KeyCode::KeyV),
    (0x0030, KeyCode::KeyB),
    (0x0031, KeyCode::KeyN),
    (0x0032, KeyCode::KeyM),
    (0x0033, KeyCode::Comma),
    (0x0034, KeyCode::Period),
    (0x0035, KeyCode::Slash),
    (0x0036, KeyCode::ShiftRight),
    (0x0037, KeyCode::NumpadMultiply),
    (0x0038, KeyCode::AltLeft),
    (0x0039, KeyCode::Space),
    (0x003A, KeyCode::CapsLock),
    (0x003B, KeyCode::F1),
    (0x003C, KeyCode::F2),
    (0x003D, KeyCode::F3),
    (0x003E, KeyCode::F4),
    (0x003F, KeyCode::F5),
    (0x0040, KeyCode::F6),
    (0x0041, KeyCode::F7),
    (0x0042, KeyCode::F8),
    (0x0043, KeyCode::F9),
    (0x0044, KeyCode::F10),
    (0x0046, KeyCode::ScrollLock),
    (0x0047, KeyCode::Numpad7),
    (0x0048, KeyCode::Numpad8),
    (0x0049, KeyCode::Numpad9),
    (0x004A, KeyCode::NumpadSubtract),
    (0x004B, KeyCode::Numpad4),
    (0x004C, KeyCode::Numpad5),
    (0x004D, KeyCode::Numpad6),
    (0x004E, KeyCode::NumpadAdd),
    (0x004F, KeyCode::Numpad1),
    (0x0050, KeyCode::Numpad2),
    (0x0051, KeyCode::Numpad3),
    (0x0052, KeyCode::Numpad0),
    (0x0053, KeyCode::NumpadDecimal),
    (0x0056, KeyCode::IntlBackslash),
    (0x0057, KeyCode::F11),
    (0x0058, KeyCode::F12),
    (0x0059, KeyCode::NumpadEqual),
    (0x0064, KeyCode::F13),
    (0x0065, KeyCode::F14),
    (0x0066, KeyCode::F15),
    (0x0067, KeyCode::F16),
    (0x0068, KeyCode::F17),
    (0x0069, KeyCode::F18),
    (0x006A, KeyCode::F19),
    (0x006B, KeyCode::F20),
    (0x006C, KeyCode::F21),
    (0x006D, KeyCode::F22),
    (0x006E, KeyCode::F23),
    (0x0070, KeyCode::KanaMode),
    (0x0071, KeyCode::Lang2),
    (0x0072, KeyCode::Lang1),
    (0x0073, KeyCode::IntlRo),
    (0x0076, KeyCode::F24),
    (0x0079, KeyCode::Convert),
    (0x007B, KeyCode::NonConvert),
    (0x007D, KeyCode::IntlYen),
    (0x007E, KeyCode::NumpadComma),
    (0xE010, KeyCode::MediaTrackPrevious),
    (0xE019, KeyCode::MediaTrackNext),
    (0xE01C, KeyCode::NumpadEnter),
    (0xE01D, KeyCode::ControlRight),
    (0xE020, KeyCode::AudioVolumeMute),
    (0xE021, KeyCode::LaunchApp2),
    (0xE022, KeyCode::MediaPlayPause),
    (0xE024, KeyCode::MediaStop),
    (0xE02E, KeyCode::AudioVolumeDown),
    (0xE030, KeyCode::AudioVolumeUp),
    (0xE032, KeyCode::BrowserHome),
    (0xE035, KeyCode::NumpadDivide),
    (0xE037, KeyCode::PrintScreen),
    (0xE038, KeyCode::AltRight),
    (0xE047, KeyCode::Home),
    (0xE048, KeyCode::ArrowUp),
    (0xE049, KeyCode::PageUp),
    (0xE04B, KeyCode::ArrowLeft),
    (0xE04D, KeyCode::ArrowRight),
    (0xE04F, KeyCode::End),
    (0xE050, KeyCode::ArrowDown),
    (0xE051, KeyCode::PageDown),
    (0xE052, KeyCode::Insert),
    (0xE053, KeyCode::Delete),
    (0xE05B, KeyCode::MetaLeft),
    (0xE05C, KeyCode::MetaRight),
    (0xE05D, KeyCode::ContextMenu),
    (0xE05E, KeyCode::Power),
    (0xE05F, KeyCode::Sleep),
    (0xE063, KeyCode::WakeUp),
    (0xE065, KeyCode::BrowserSearch),
    (0xE066, KeyCode::BrowserFavorites),
    (0xE067, KeyCode::BrowserRefresh),
    (0xE068, KeyCode::BrowserStop),
    (0xE069, KeyCode::BrowserForward),
    (0xE06A, KeyCode::BrowserBack),
    (0xE06B, KeyCode::LaunchApp1),
    (0xE06C, KeyCode::LaunchMail),
    (0xE06D, KeyCode::MediaSelect),
];

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn table_has_no_duplicates() {
        let scans: HashSet<_> = SCAN_CODES.iter().map(|(scan, _)| scan).collect();
        let codes: HashSet<_> = SCAN_CODES.iter().map(|(_, code)| code).collect();
        assert_eq!(scans.len(), SCAN_CODES.len());
        assert_eq!(codes.len(), SCAN_CODES.len());
    }

    #[test]
    fn every_key_round_trips() {
        for &(scan, code) in SCAN_CODES {
            let stroke = to_windows(code).unwrap();
            let back = from_windows(0, u32::from(stroke.scan), stroke.extended);
            assert_eq!(back, code, "scan code {scan:#06x}");
        }
    }

    #[test]
    fn extended_flag_tells_keys_apart() {
        assert_eq!(from_windows(0, 0x1C, false), KeyCode::Enter);
        assert_eq!(from_windows(0, 0x1C, true), KeyCode::NumpadEnter);
        assert_eq!(from_windows(0, 0x47, false), KeyCode::Numpad7);
        assert_eq!(from_windows(0, 0x47, true), KeyCode::Home);
    }

    #[test]
    fn pause_and_num_lock_use_virtual_keys() {
        assert_eq!(
            from_windows(u32::from(VK_PAUSE.0), 0x45, false),
            KeyCode::Pause
        );
        assert_eq!(
            from_windows(u32::from(VK_NUMLOCK.0), 0x45, true),
            KeyCode::NumLock
        );
        assert_eq!(to_windows(KeyCode::Pause).unwrap().vk, VK_PAUSE.0);
    }

    #[test]
    fn unknown_scan_codes_are_unidentified() {
        assert_eq!(from_windows(0, 0x7F, true), KeyCode::Unidentified);
    }
}
