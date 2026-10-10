//! Playing and pausing other programs.
//!
//! Most players tell Windows what they're doing, and Windows lets other programs control them:
//! that's what the buttons in the volume flyout use, and it reaches the right app wherever it
//! is. Some don't, like the usual Stremio app. Those have to be sent the keys they'd get from
//! their own window having the focus, and the way that works depends on the player.

use hk_core::apps::{MediaCommand, MediaState};
use hk_core::input::{InputEvent, KeyCode};
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as MediaSession,
    GlobalSystemMediaTransportControlsSessionManager as SessionManager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as PlaybackStatus,
};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::UI::Input::KeyboardAndMouse::{MAPVK_VK_TO_VSC, MapVirtualKeyW};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, PostMessageW, WM_APPCOMMAND, WM_KEYDOWN, WM_KEYUP,
};

use super::apps::{self, AppWindow};
use super::{inject, keymap};
use crate::Error;

/// How far back and forward to go in something with no previous or next, like a film.
const SEEK_TICKS: i64 = 10 * 10_000_000;

/// From Windows' headers. These go in the top half of what a window is sent.
const APPCOMMAND_MEDIA_NEXTTRACK: u32 = 11;
const APPCOMMAND_MEDIA_PREVIOUSTRACK: u32 = 12;
const APPCOMMAND_MEDIA_PLAY_PAUSE: u32 = 14;

/// What Chromium-based apps take keys in, when it isn't their main window.
const WEB_CONTENT_WINDOW: &str = "Chrome_RenderWidgetHostHWND";

/// Players that get buttons even if they don't tell Windows anything, and what to call them.
/// Stremio has gone by different program names.
const KNOWN_PLAYERS: [(&str, &str); 4] = [
    ("spotify", "Spotify"),
    ("stremio", "Stremio"),
    ("stremio-shell-ng", "Stremio"),
    ("stremio-community-v5", "Stremio"),
];

pub(super) fn is_known_player(app: &str) -> bool {
    KNOWN_PLAYERS.iter().any(|(id, _)| *id == app)
}

pub(super) fn name_of_known_player(app: &str) -> Option<&'static str> {
    KNOWN_PLAYERS
        .iter()
        .find_map(|(id, name)| (*id == app).then_some(*name))
}

/// Whether an app is controlled with its own keyboard shortcuts.
fn uses_keys(app: &str) -> bool {
    app.starts_with("stremio")
}

/// The name for something playing with no window to go by. `source` is how it's known to
/// Windows, which is a long code for many Store apps, and a random-looking one for Firefox.
pub(super) fn name_of_player(app: &str, source: &str) -> String {
    if let Some(name) = name_of_known_player(app) {
        return name.to_string();
    }
    let looks_like_a_name = !source.contains(['!', '_'])
        && !source.is_empty()
        && !source.chars().all(|letter| letter.is_ascii_hexdigit());
    if looks_like_a_name {
        source.trim_end_matches(".exe").to_string()
    } else {
        "Media player".to_string()
    }
}

/// Something that can be played and paused, as Windows knows it.
pub(super) struct Session {
    /// The same as the app's id in the list of apps, where the two can be matched up.
    pub(super) app: String,
    pub(super) source: String,
    pub(super) state: MediaState,
}

/// Windows knows apps by a name that isn't their program's: "Spotify.exe" for one copy of
/// Spotify, and "SpotifyAB.SpotifyMusic_zpdnekdrzrea0!Spotify" for the one from the Store.
fn app_from_source(source: &str) -> String {
    let name = source
        .rsplit(['!', '\\'])
        .next()
        .unwrap_or(source)
        .to_lowercase();
    match name.strip_suffix(".exe") {
        Some(without) => without.to_string(),
        None => name,
    }
}

fn manager() -> windows::core::Result<SessionManager> {
    // Windows' newer APIs need this on any thread using them. It's fine if it was done before.
    // SAFETY: nothing is passed. These threads have no other use for COM to clash with.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    SessionManager::RequestAsync()?.join()
}

fn all_sessions() -> windows::core::Result<Vec<MediaSession>> {
    Ok(manager()?.GetSessions()?.into_iter().collect())
}

/// What's playing or could be, as far as players tell Windows.
pub(super) fn sessions() -> Vec<Session> {
    let sessions = match all_sessions() {
        Ok(sessions) => sessions,
        Err(error) => {
            tracing::debug!(%error, "can't ask Windows what's playing");
            return Vec::new();
        }
    };
    sessions
        .iter()
        .filter_map(|session| describe(session).ok())
        .collect()
}

fn describe(session: &MediaSession) -> windows::core::Result<Session> {
    let source = session.SourceAppUserModelId()?.to_string();
    let info = session.GetPlaybackInfo()?;
    let controls = info.Controls()?;
    let seeks = controls.IsPlaybackPositionEnabled()?;
    Ok(Session {
        app: app_from_source(&source),
        source,
        state: MediaState {
            playing: Some(info.PlaybackStatus()? == PlaybackStatus::Playing),
            can_go_back: controls.IsPreviousEnabled()? || controls.IsRewindEnabled()? || seeks,
            can_go_forward: controls.IsNextEnabled()? || controls.IsFastForwardEnabled()? || seeks,
        },
    })
}

pub(super) fn command(app: &str, command: MediaCommand) -> Result<(), Error> {
    let failed = |error: windows::core::Error| Error::Media(error.message());
    if let Some(session) = session_of(app).map_err(failed)? {
        let accepted = send_to_session(&session, command).map_err(failed)?;
        return if accepted {
            Ok(())
        } else {
            Err(Error::Media(format!("{app} said no")))
        };
    }
    let Some(window) = apps::app_windows()
        .into_iter()
        .find(|window| window.app == app)
    else {
        return Err(Error::Media(format!("{app} isn't open")));
    };
    if uses_keys(app) {
        send_keys(&window, command)
    } else {
        send_app_command(&window, command)
    }
}

/// The session for an app. If it has several, the one playing is the one meant.
fn session_of(app: &str) -> windows::core::Result<Option<MediaSession>> {
    let sessions = all_sessions()?;
    let mine = sessions.into_iter().filter(|session| {
        session
            .SourceAppUserModelId()
            .is_ok_and(|source| app_from_source(&source.to_string()) == app)
    });
    let mut first = None;
    for session in mine {
        let playing = session
            .GetPlaybackInfo()
            .and_then(|info| info.PlaybackStatus())
            .is_ok_and(|status| status == PlaybackStatus::Playing);
        if playing {
            return Ok(Some(session));
        }
        first.get_or_insert(session);
    }
    Ok(first)
}

fn send_to_session(session: &MediaSession, command: MediaCommand) -> windows::core::Result<bool> {
    let controls = session.GetPlaybackInfo()?.Controls()?;
    match command {
        MediaCommand::PlayPause => session.TryTogglePlayPauseAsync()?.join(),
        MediaCommand::Forward if controls.IsNextEnabled()? => session.TrySkipNextAsync()?.join(),
        MediaCommand::Forward if controls.IsFastForwardEnabled()? => {
            session.TryFastForwardAsync()?.join()
        }
        MediaCommand::Forward => seek(session, SEEK_TICKS),
        MediaCommand::Back if controls.IsPreviousEnabled()? => {
            session.TrySkipPreviousAsync()?.join()
        }
        MediaCommand::Back if controls.IsRewindEnabled()? => session.TryRewindAsync()?.join(),
        MediaCommand::Back => seek(session, -SEEK_TICKS),
    }
}

fn seek(session: &MediaSession, by_ticks: i64) -> windows::core::Result<bool> {
    let position = session.GetTimelineProperties()?.Position()?.Duration;
    session
        .TryChangePlaybackPositionAsync(position.saturating_add(by_ticks).max(0))?
        .join()
}

/// What Windows sends a window for the media keys on a keyboard. The app decides what to do
/// with it, and doesn't say whether it did anything.
fn send_app_command(window: &AppWindow, command: MediaCommand) -> Result<(), Error> {
    let code = match command {
        MediaCommand::PlayPause => APPCOMMAND_MEDIA_PLAY_PAUSE,
        MediaCommand::Back => APPCOMMAND_MEDIA_PREVIOUSTRACK,
        MediaCommand::Forward => APPCOMMAND_MEDIA_NEXTTRACK,
    };
    // SAFETY: posting takes plain values, and doesn't wait for the app.
    unsafe {
        PostMessageW(
            Some(window.window),
            WM_APPCOMMAND,
            WPARAM(window.window.0 as usize),
            LPARAM(((code << 16) as usize) as isize),
        )
    }
    .map_err(|error| Error::Media(error.message()))
}

/// Stremio's own shortcuts, from its player. Space works on letting go of it, so it needs
/// both halves of a key press.
fn key_for(command: MediaCommand) -> KeyCode {
    match command {
        MediaCommand::PlayPause => KeyCode::Space,
        MediaCommand::Back => KeyCode::ArrowLeft,
        MediaCommand::Forward => KeyCode::ArrowRight,
    }
}

/// With the player in front, this is a real key press. Otherwise it goes to the player's own
/// window, so that nobody's typing lands in the wrong place, and nothing jumps to the front.
fn send_keys(window: &AppWindow, command: MediaCommand) -> Result<(), Error> {
    let code = key_for(command);
    // SAFETY: only reads which window is in front.
    if unsafe { GetForegroundWindow() } == window.window {
        for pressed in [true, false] {
            inject::inject(&InputEvent::Key { code, pressed });
        }
        return Ok(());
    }
    let Some(stroke) = keymap::to_windows(code) else {
        return Err(Error::Media(format!("{code} can't be sent")));
    };
    let target = web_content_of(window.window).unwrap_or(window.window);
    // These keys all have a known virtual key, which is what makes them work without a layout.
    let vk = u32::from(virtual_key(code));
    // SAFETY: looks up a number.
    let scan = unsafe { MapVirtualKeyW(vk, MAPVK_VK_TO_VSC) };
    let extended = stroke.extended;
    for (message, lparam) in [
        (WM_KEYDOWN, key_lparam(scan, extended, false)),
        (WM_KEYUP, key_lparam(scan, extended, true)),
    ] {
        // SAFETY: posting takes plain values, and doesn't wait for the app.
        unsafe { PostMessageW(Some(target), message, WPARAM(vk as usize), LPARAM(lparam)) }
            .map_err(|error| Error::Media(error.message()))?;
    }
    Ok(())
}

fn virtual_key(code: KeyCode) -> u16 {
    match code {
        KeyCode::Space => 0x20,
        KeyCode::ArrowLeft => 0x25,
        KeyCode::ArrowRight => 0x27,
        _ => 0,
    }
}

/// What a window is told about a key, packed into one number the way Windows does it.
fn key_lparam(scan: u32, extended: bool, releasing: bool) -> isize {
    let mut value = 1 | (scan << 16);
    if extended {
        value |= 1 << 24;
    }
    if releasing {
        value |= (1 << 30) | (1 << 31);
    }
    value as i32 as isize
}

fn web_content_of(window: HWND) -> Option<HWND> {
    apps::child_windows(window)
        .into_iter()
        .find(|&child| apps::class_of(child) == WEB_CONTENT_WINDOW)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pauses and restarts a real player, so it only runs on demand, with something playing
    /// that's fine to interrupt, and nothing else playing under that app's name:
    /// `HK_TEST_MEDIA_APP=msedge cargo test -p hk-platform a_real_player -- --ignored --nocapture`
    #[test]
    #[ignore = "controls whatever is playing"]
    fn a_real_player_can_be_paused_and_started_again() {
        let app = std::env::var("HK_TEST_MEDIA_APP").expect("which app to try it on");
        let state = || {
            sessions()
                .into_iter()
                .find(|session| session.app == app)
                .map(|session| session.state)
        };
        let wait_until = |wanted: bool| {
            for _ in 0..100 {
                if state().is_some_and(|state| state.playing == Some(wanted)) {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            false
        };

        assert!(wait_until(true), "{app} isn't playing: {:?}", state());
        println!("playing: {:?}", state());
        command(&app, MediaCommand::PlayPause).unwrap();
        assert!(wait_until(false), "it should have paused: {:?}", state());
        println!("paused: {:?}", state());
        command(&app, MediaCommand::PlayPause).unwrap();
        assert!(wait_until(true), "it should have started: {:?}", state());
        println!("playing again: {:?}", state());

        for (name, skip) in [
            ("back", MediaCommand::Back),
            ("forward", MediaCommand::Forward),
        ] {
            println!("{name}: {:?}", command(&app, skip));
        }
    }

    #[test]
    fn apps_are_matched_however_windows_knows_them() {
        for (source, app) in [
            ("Spotify.exe", "spotify"),
            ("SpotifyAB.SpotifyMusic_zpdnekdrzrea0!Spotify", "spotify"),
            ("MSEdge", "msedge"),
            ("Chrome", "chrome"),
            ("vlc.EXE", "vlc"),
            (
                r"{1AC14E77-02E7}\WindowsPowerShell\v1.0\powershell.exe",
                "powershell",
            ),
        ] {
            assert_eq!(app_from_source(source), app, "{source}");
        }
    }

    #[test]
    fn a_player_with_no_window_gets_a_sensible_name() {
        assert_eq!(name_of_player("spotify", "Spotify.exe"), "Spotify");
        assert_eq!(name_of_player("vlc", "vlc.exe"), "vlc");
        assert_eq!(name_of_player("chrome", "Chrome"), "Chrome");
        assert_eq!(
            name_of_player("x", "Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic"),
            "Media player"
        );
        assert_eq!(name_of_player("x", "308046B0AF4A39CB"), "Media player");
        assert_eq!(name_of_player("x", ""), "Media player");
    }

    #[test]
    fn only_stremio_gets_keys_sent() {
        assert!(uses_keys("stremio"));
        assert!(uses_keys("stremio-shell-ng"));
        assert!(!uses_keys("spotify"));
    }

    #[test]
    fn known_players_have_their_proper_names() {
        assert!(is_known_player("spotify"));
        assert_eq!(name_of_known_player("stremio-shell-ng"), Some("Stremio"));
        assert!(!is_known_player("notepad"));
    }

    /// Every key sent this way must have a virtual key, or it silently does nothing.
    #[test]
    fn each_command_has_a_key_that_can_be_sent() {
        for command in [
            MediaCommand::PlayPause,
            MediaCommand::Back,
            MediaCommand::Forward,
        ] {
            let code = key_for(command);
            assert_ne!(virtual_key(code), 0, "{code}");
            assert!(keymap::to_windows(code).is_some(), "{code}");
        }
    }

    #[test]
    fn a_key_is_described_to_a_window_the_way_windows_does_it() {
        // One press of Space, which has scan code 0x39.
        assert_eq!(key_lparam(0x39, false, false), 0x0039_0001);
        // Letting go sets the top two bits.
        assert_eq!(key_lparam(0x39, false, true) as u32, 0xC039_0001);
        // The arrow keys are in the extended part of the keyboard.
        assert_eq!(key_lparam(0x4B, true, false), 0x014B_0001);
    }
}
