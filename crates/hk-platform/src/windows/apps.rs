//! Which apps have windows on which monitors, what they're called, and what they look like.
//!
//! Nothing here reads a window's title, only whether it has one: titles often say what
//! someone is working on, and this list goes to other computers.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use hk_core::apps::{MediaState, RunningApp};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, TRUE, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, GetDIBits, GetObjectW, HBITMAP, HGDIOBJ, MONITOR_DEFAULTTONEAREST,
    MonitorFromWindow,
};
use windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Shell::ExtractIconExW;
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, EnumChildWindows, EnumWindows, GCLP_HICON, GW_OWNER, GWL_EXSTYLE,
    GetClassLongPtrW, GetClassNameW, GetIconInfo, GetWindow, GetWindowLongPtrW,
    GetWindowTextLengthW, GetWindowThreadProcessId, HICON, ICON_BIG, ICONINFO, IsIconic,
    IsWindowVisible, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_GETICON, WS_EX_TOOLWINDOW,
};
use windows::core::{BOOL, HSTRING, PCWSTR, PWSTR, w};

use super::{displays, media};

/// Parts of Windows itself that have windows, but aren't apps someone opened. File Explorer
/// is an app, but only its folder windows: `explorer` also draws the taskbar and desktop.
const NOT_APPS: [&str; 9] = [
    "applicationframehost",
    "hackerman",
    "lockapp",
    "searchapp",
    "searchhost",
    "shellexperiencehost",
    "shellhost",
    "startmenuexperiencehost",
    "textinputhost",
];
const NOT_APP_WINDOWS: [&str; 5] = [
    "Progman",
    "Shell_SecondaryTrayWnd",
    "Shell_TrayWnd",
    "WorkerW",
    "Windows.UI.Core.CoreWindow",
];

/// What hosts the windows of Microsoft Store apps, with the real app in a child window.
const STORE_APP_HOST: &str = "applicationframehost";

/// What to remember about an app between one look and the next: reading a file's details
/// every couple of seconds would be a waste.
struct Known {
    path: PathBuf,
    name: String,
    /// One of its windows, stored as a number since handles can't move between threads. It
    /// may have closed since, which only means it gives no icon.
    window: isize,
}

/// How long to wait for an app to say what its icon is.
const ICON_ANSWER_MS: u32 = 100;

static KNOWN: Mutex<Option<HashMap<String, Known>>> = Mutex::new(None);

fn known() -> MutexGuard<'static, Option<HashMap<String, Known>>> {
    KNOWN.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One top-level window that counts as an app, frontmost first.
pub(super) struct AppWindow {
    pub(super) window: HWND,
    pub(super) app: String,
    pub(super) path: PathBuf,
}

pub(super) fn app_windows() -> Vec<AppWindow> {
    top_level_windows()
        .into_iter()
        .filter(|&window| is_app_window(window))
        .filter_map(|window| {
            let path = program_of(window)?;
            let app = id_from_path(&path)?;
            (!NOT_APPS.contains(&app.as_str())).then_some(AppWindow { window, app, path })
        })
        .collect()
}

pub(super) fn running() -> Vec<RunningApp> {
    let monitors = displays::ids_by_handle();
    let mut apps: Vec<RunningApp> = Vec::new();
    for found in app_windows() {
        // SAFETY: a window that has gone since is reported as the nearest monitor or none.
        let handle = unsafe { MonitorFromWindow(found.window, MONITOR_DEFAULTTONEAREST) };
        let monitor = monitors.get(&(handle.0 as isize)).cloned();
        // SAFETY: only reads a flag.
        let minimized = unsafe { IsIconic(found.window) }.as_bool();
        match apps.iter_mut().find(|app| app.id == found.app) {
            Some(app) => {
                app.minimized &= minimized;
                if let Some(monitor) = monitor
                    && !app.monitors.contains(&monitor)
                {
                    app.monitors.push(monitor);
                }
            }
            None => apps.push(RunningApp {
                name: name_of(&found.app, &found.path, found.window),
                id: found.app,
                monitors: monitor.into_iter().collect(),
                minimized,
                media: None,
            }),
        }
    }
    add_media(&mut apps, media::sessions());
    apps
}

/// Says what can be played and paused. Something playing with no window, from the tray,
/// comes after the apps with windows.
fn add_media(apps: &mut Vec<RunningApp>, sessions: Vec<media::Session>) {
    for app in apps.iter_mut() {
        if media::is_known_player(&app.id) {
            app.media = Some(MediaState {
                playing: None,
                can_go_back: true,
                can_go_forward: true,
            });
        }
    }
    for session in sessions {
        match apps.iter_mut().find(|app| app.id == session.app) {
            // An app can have several, such as a browser with two tabs. What's playing
            // is what the buttons are for.
            Some(RunningApp {
                media: Some(state), ..
            }) if state.playing == Some(true) => {}
            Some(app) => app.media = Some(session.state),
            None => apps.push(RunningApp {
                name: media::name_of_player(&session.app, &session.source),
                id: session.app,
                monitors: Vec::new(),
                minimized: false,
                media: Some(session.state),
            }),
        }
    }
}

/// What identifies an app: the name of its program, without the rest.
fn id_from_path(path: &Path) -> Option<String> {
    Some(path.file_stem()?.to_str()?.to_lowercase())
}

fn top_level_windows() -> Vec<HWND> {
    unsafe extern "system" fn collect(window: HWND, windows: LPARAM) -> BOOL {
        // SAFETY: `windows` is the pointer to the vector passed below, which outlives
        // EnumWindows, and this is only called during it.
        unsafe { &mut *(windows.0 as *mut Vec<HWND>) }.push(window);
        TRUE
    }

    let mut windows: Vec<HWND> = Vec::new();
    let pointer = LPARAM(&raw mut windows as isize);
    // Windows goes through them from the front to the back.
    // SAFETY: the callback only uses the pointer as described there.
    let _ = unsafe { EnumWindows(Some(collect), pointer) };
    windows
}

fn is_app_window(window: HWND) -> bool {
    // SAFETY: these only read about a window. One that has gone since gives a harmless answer.
    unsafe {
        if !IsWindowVisible(window).as_bool()
            || GetWindow(window, GW_OWNER).is_ok_and(|owner| !owner.is_invalid())
            || GetWindowLongPtrW(window, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0
            || GetWindowTextLengthW(window) == 0
            || is_cloaked(window)
        {
            return false;
        }
    }
    !NOT_APP_WINDOWS.contains(&class_of(window).as_str())
}

/// Windows hides some windows without saying they're not visible: those on another virtual
/// desktop, and suspended Store apps.
fn is_cloaked(window: HWND) -> bool {
    let mut cloaked = 0u32;
    // SAFETY: the size given is that of the value it fills in.
    unsafe {
        DwmGetWindowAttribute(
            window,
            DWMWA_CLOAKED,
            (&raw mut cloaked).cast(),
            size_of::<u32>() as u32,
        )
    }
    .is_ok()
        && cloaked != 0
}

pub(super) fn class_of(window: HWND) -> String {
    let mut name = [0u16; 128];
    // SAFETY: the slice gives its own length.
    let length = unsafe { GetClassNameW(window, &mut name) };
    String::from_utf16_lossy(&name[..usize::try_from(length).unwrap_or(0)])
}

fn process_of(window: HWND) -> u32 {
    let mut process = 0;
    // SAFETY: the number is a plain value for Windows to fill in.
    unsafe { GetWindowThreadProcessId(window, Some(&raw mut process)) };
    process
}

/// The program a window belongs to. For a Store app, that's the one in its host's window.
fn program_of(window: HWND) -> Option<PathBuf> {
    let host = process_of(window);
    let path = program_of_process(host)?;
    if id_from_path(&path).as_deref() != Some(STORE_APP_HOST) {
        return Some(path);
    }
    child_windows(window)
        .into_iter()
        .map(process_of)
        .find(|&process| process != host)
        .and_then(program_of_process)
}

pub(super) fn child_windows(parent: HWND) -> Vec<HWND> {
    unsafe extern "system" fn collect(window: HWND, windows: LPARAM) -> BOOL {
        // SAFETY: as in `top_level_windows`.
        unsafe { &mut *(windows.0 as *mut Vec<HWND>) }.push(window);
        TRUE
    }

    let mut windows: Vec<HWND> = Vec::new();
    let pointer = LPARAM(&raw mut windows as isize);
    // SAFETY: as in `top_level_windows`.
    let _ = unsafe { EnumChildWindows(Some(parent), Some(collect), pointer) };
    windows
}

fn program_of_process(process: u32) -> Option<PathBuf> {
    // SAFETY: the handle is checked and closed, and the buffer's size goes with it.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process).ok()?;
        let mut path = vec![0u16; 1024];
        let mut length = path.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &raw mut length,
        );
        let _ = CloseHandle(handle);
        result.ok()?;
        Some(PathBuf::from(String::from_utf16_lossy(
            &path[..length as usize],
        )))
    }
}

fn name_of(app: &str, path: &Path, window: HWND) -> String {
    let mut known = known();
    let known = known.get_or_insert_default();
    if let Some(found) = known.get_mut(app) {
        found.window = window.0 as isize;
        return found.name.clone();
    }
    let name = media::name_of_known_player(app)
        .map(ToString::to_string)
        .or_else(|| description_of(path))
        .map_or_else(|| capitalized(app), |name| tidy_name(&name));
    known.insert(
        app.to_string(),
        Known {
            path: path.to_path_buf(),
            name: name.clone(),
            window: window.0 as isize,
        },
    );
    name
}

/// Some Store apps describe themselves by the name of their program, like "WhatsApp.Root".
fn tidy_name(name: &str) -> String {
    name.strip_suffix(".Root").unwrap_or(name).to_string()
}

fn capitalized(text: &str) -> String {
    let mut letters = text.chars();
    match letters.next() {
        Some(first) => first.to_uppercase().chain(letters).collect(),
        None => String::new(),
    }
}

/// The name a program gives itself, which is what Task Manager shows. It's in the language
/// its file was made for, and some programs leave it empty.
fn description_of(path: &Path) -> Option<String> {
    let path = HSTRING::from(path.as_os_str());
    // SAFETY: the buffer has the size Windows asked for, and the values read from it stay
    // inside it, at the lengths Windows reports.
    unsafe {
        let size = GetFileVersionInfoSizeW(&path, None);
        if size == 0 {
            return None;
        }
        let mut info = vec![0u8; size as usize];
        GetFileVersionInfoW(&path, None, size, info.as_mut_ptr().cast()).ok()?;

        // Which languages it has, as pairs of a language and a code page.
        let mut pairs = std::ptr::null_mut();
        let mut pairs_size = 0;
        VerQueryValueW(
            info.as_ptr().cast(),
            w!("\\VarFileInfo\\Translation"),
            &raw mut pairs,
            &raw mut pairs_size,
        )
        .ok()
        .ok()?;
        if pairs_size < 4 {
            return None;
        }
        let pair = std::slice::from_raw_parts(pairs.cast::<u16>().cast_const(), 2);
        let query = HSTRING::from(format!(
            "\\StringFileInfo\\{:04x}{:04x}\\FileDescription",
            pair[0], pair[1]
        ));

        let mut text = std::ptr::null_mut();
        let mut length = 0;
        VerQueryValueW(info.as_ptr().cast(), &query, &raw mut text, &raw mut length)
            .ok()
            .ok()?;
        let text = std::slice::from_raw_parts(text.cast::<u16>().cast_const(), length as usize);
        let end = text
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(text.len());
        let name = String::from_utf16_lossy(&text[..end]);
        let name = name.trim();
        (!name.is_empty()).then(|| name.to_string())
    }
}

/// A picture of an app seen by [`running`], as a PNG.
pub(super) fn icon(app: &str) -> Option<Vec<u8>> {
    let (path, window) = {
        let known = known();
        let found = known.as_ref()?.get(app)?;
        (found.path.clone(), HWND(found.window as *mut _))
    };
    icon_of_program(&path).or_else(|| icon_of_window(window))
}

fn as_png(icon: HICON) -> Option<Vec<u8>> {
    let (width, height, rgba) = pixels_of(icon)?;
    encode_png(width, height, &rgba)
}

fn icon_of_program(path: &Path) -> Option<Vec<u8>> {
    let wide: Vec<u16> = OsStr::new(path).encode_wide().chain([0]).collect();
    let mut large = HICON::default();
    // SAFETY: the path ends in a zero, and one icon is asked for, into one place.
    let found = unsafe { ExtractIconExW(PCWSTR(wide.as_ptr()), 0, Some(&raw mut large), None, 1) };
    if found == 0 || found == u32::MAX || large.is_invalid() {
        return None;
    }
    let png = as_png(large);
    // SAFETY: this icon was made for this program, and nothing else has it.
    let _ = unsafe { DestroyIcon(large) };
    png
}

/// Store apps often keep theirs out of the program itself, and give it to their windows. This
/// icon belongs to the window, so it isn't destroyed here.
fn icon_of_window(window: HWND) -> Option<Vec<u8>> {
    let mut answer = 0usize;
    // SAFETY: only asks for a number, and gives up rather than wait on a program that's stuck.
    let answered = unsafe {
        SendMessageTimeoutW(
            window,
            WM_GETICON,
            WPARAM(ICON_BIG as usize),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            ICON_ANSWER_MS,
            Some(&raw mut answer),
        )
    };
    if answered.0 != 0 && answer != 0 {
        return as_png(HICON(answer as *mut _));
    }
    // SAFETY: only reads a value belonging to the window's class.
    let from_class = unsafe { GetClassLongPtrW(window, GCLP_HICON) };
    (from_class != 0)
        .then(|| as_png(HICON(from_class as *mut _)))
        .flatten()
}

/// The icon's size, and its pixels from the top, four bytes each.
fn pixels_of(icon: HICON) -> Option<(u32, u32, Vec<u8>)> {
    let mut info = ICONINFO::default();
    // SAFETY: fills in a plain structure. The two bitmaps it makes are deleted below.
    unsafe { GetIconInfo(icon, &raw mut info) }.ok()?;
    let result = read_icon_bitmaps(info.hbmColor, info.hbmMask);
    // SAFETY: as above, and nothing else has them.
    unsafe {
        let _ = DeleteObject(HGDIOBJ(info.hbmColor.0));
        let _ = DeleteObject(HGDIOBJ(info.hbmMask.0));
    }
    result
}

fn read_icon_bitmaps(color: HBITMAP, mask: HBITMAP) -> Option<(u32, u32, Vec<u8>)> {
    let mut size = BITMAP::default();
    // SAFETY: the size given is that of the structure it fills in.
    let filled = unsafe {
        GetObjectW(
            HGDIOBJ(color.0),
            size_of::<BITMAP>() as i32,
            Some((&raw mut size).cast()),
        )
    };
    let (width, height) = (size.bmWidth, size.bmHeight);
    if filled == 0 || !(1..=256).contains(&width) || !(1..=256).contains(&height) {
        return None;
    }
    let mut colors = read_bitmap(color, width, height)?;
    // Old icons have no transparency of their own, and say what's see-through in a mask.
    if colors.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 0) {
        let mask = read_bitmap(mask, width, height)?;
        let masks = mask.as_chunks::<4>().0;
        for (pixel, masked) in colors.as_chunks_mut::<4>().0.iter_mut().zip(masks) {
            pixel[3] = if masked[0] == 0 { 255 } else { 0 };
        }
    }
    for pixel in colors.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2); // Windows has them as blue, green, red.
    }
    Some((width as u32, height as u32, colors))
}

fn read_bitmap(bitmap: HBITMAP, width: i32, height: i32) -> Option<Vec<u8>> {
    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    let mut format = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height, // Negative is from the top.
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: the buffer is the size this format and these rows need.
    let read = unsafe {
        let context = CreateCompatibleDC(None);
        let read = GetDIBits(
            context,
            bitmap,
            0,
            height as u32,
            Some(pixels.as_mut_ptr().cast()),
            &raw mut format,
            DIB_RGB_COLORS,
        );
        let _ = DeleteDC(context);
        read
    };
    (read == height).then_some(pixels)
}

fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    let mut png = Vec::new();
    let mut encoder = png::Encoder::new(&mut png, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().ok()?;
    writer.write_image_data(rgba).ok()?;
    writer.finish().ok()?;
    Some(png)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str) -> RunningApp {
        RunningApp {
            id: id.into(),
            name: id.into(),
            monitors: vec!["m".into()],
            minimized: false,
            media: None,
        }
    }

    fn session(app: &str, playing: bool) -> media::Session {
        media::Session {
            app: app.into(),
            source: app.into(),
            state: MediaState {
                playing: Some(playing),
                can_go_back: false,
                can_go_forward: true,
            },
        }
    }

    #[test]
    fn programs_are_known_by_their_file_names() {
        let path = Path::new(r"C:\Program Files\WindowsApps\SpotifyAB\Spotify.exe");
        assert_eq!(id_from_path(path).as_deref(), Some("spotify"));
        assert_eq!(capitalized("stremio"), "Stremio");
        assert_eq!(tidy_name("WhatsApp.Root"), "WhatsApp");
        assert_eq!(tidy_name("Microsoft Edge"), "Microsoft Edge");
    }

    #[test]
    fn something_playing_is_added_to_its_app() {
        let mut apps = vec![app("notepad"), app("spotify")];
        add_media(&mut apps, vec![session("spotify", true)]);
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].media, None);
        assert_eq!(apps[1].media.unwrap().playing, Some(true));
        assert_eq!(apps[1].monitors, ["m"], "it stays on its screen");
    }

    #[test]
    fn something_playing_from_the_tray_is_added_as_an_app_with_no_screen() {
        let mut apps = vec![app("notepad")];
        add_media(&mut apps, vec![session("spotify", false)]);
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[1].id, "spotify");
        assert_eq!(apps[1].name, "Spotify");
        assert!(apps[1].monitors.is_empty());
    }

    #[test]
    fn with_two_tabs_the_buttons_are_for_the_one_playing() {
        for sessions in [
            vec![session("msedge", true), session("msedge", false)],
            vec![session("msedge", false), session("msedge", true)],
        ] {
            let mut apps = vec![app("msedge")];
            add_media(&mut apps, sessions);
            assert_eq!(apps[0].media.unwrap().playing, Some(true));
        }
    }

    /// Stremio doesn't report what it's doing, so it can't say whether it's playing.
    #[test]
    fn a_known_player_that_reports_nothing_still_gets_buttons() {
        let mut apps = vec![app("stremio")];
        add_media(&mut apps, vec![]);
        assert_eq!(
            apps[0].media,
            Some(MediaState {
                playing: None,
                can_go_back: true,
                can_go_forward: true,
            })
        );
    }

    #[test]
    fn a_known_player_that_does_report_says_so() {
        let mut apps = vec![app("spotify")];
        add_media(&mut apps, vec![session("spotify", false)]);
        assert_eq!(apps[0].media.unwrap().playing, Some(false));
        assert!(!apps[0].media.unwrap().can_go_back);
    }

    #[test]
    fn a_small_picture_becomes_a_png() {
        let pixels = [255, 0, 0, 255, 0, 255, 0, 128];
        let png = encode_png(2, 1, &pixels).unwrap();
        assert_eq!(&png[..4], b"\x89PNG");
    }
}
