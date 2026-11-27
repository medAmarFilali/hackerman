//! Handing an app off to another computer: what its window has open, and opening here what
//! another computer handed off (ADR 1).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use hk_core::handoff::{BROWSERS, Found, Opening, places_to_look};
use windows::Win32::Foundation::{HLOCAL, HWND, LocalFree};
use windows::Win32::System::Com::{
    CLSCTX_ALL, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
    CoTaskMemFree, IPersistFile, STGM_READ,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationValuePattern, TreeScope_Descendants,
    UIA_ClassNamePropertyId, UIA_ValuePatternId,
};
use windows::Win32::UI::Shell::{
    AssocIsDangerous, CommandLineToArgvW, FOLDERID_Documents, FOLDERID_Profile, FOLDERID_Recent,
    IShellLinkW, IShellWindows, IWebBrowserApp, KF_FLAG_DEFAULT, PathCreateFromUrlW,
    SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, SHGetKnownFolderPath,
    ShellExecuteExW, ShellLink, ShellWindows,
};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowTextW, SW_SHOWNORMAL};
use windows::core::{BSTR, GUID, HSTRING, Interface, PCWSTR};

use super::{app_windows, dev};

pub(super) fn what_is_open(app: &str, monitor: Option<&str>) -> Result<Found, String> {
    let window =
        app_windows::front_window(app, monitor).ok_or_else(|| format!("{app} has no window"))?;
    // SAFETY: once per thread; after that it only says it's done already.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let title = title_of(window);
    match app {
        browser if BROWSERS.contains(&browser) => {
            Opening::page(&address_of(window)?, browser).map(Found::Open)
        }
        "code" => {
            let storage = std::env::var_os("APPDATA")
                .map(|folder| PathBuf::from(folder).join(r"Code\User\globalStorage\storage.json"));
            let text = storage
                .and_then(|file| std::fs::read_to_string(file).ok())
                .unwrap_or_default();
            let folders = vs_code_folders(&text);
            let folder = folder_in_title(&title, &folders)
                .ok_or_else(|| format!("couldn't tell which folder \"{title}\" is"))?;
            Ok(Found::Open(Opening::Project {
                folder: folder.display().to_string(),
            }))
        }
        "explorer" => {
            let folder = explorer_folder(window).ok_or("File Explorer isn't showing a folder")?;
            Ok(Found::Open(Opening::Folder {
                folder: folder.display().to_string(),
            }))
        }
        _ => {
            let program = format!("{app}.exe");
            let on_command_lines = dev::processes::snapshot()
                .into_iter()
                .filter(|process| process.name.eq_ignore_ascii_case(&program))
                .filter_map(|process| process.command_line)
                // The first is the program itself.
                .flat_map(|line| arguments_of(&line).into_iter().skip(1));
            let candidates = on_command_lines
                .map(PathBuf::from)
                .chain(recent_files(&title));
            file_in_title(&title, candidates)
                .map(Found::File)
                .ok_or_else(|| format!("couldn't tell which file \"{title}\" is"))
        }
    }
}

/// Opens what another computer handed off. It decides nothing about how: see ADR 1.
pub(super) fn open(opening: &Opening) -> Result<(), String> {
    if !opening.is_safe() {
        return Err("that can't be opened".into());
    }
    let quoted = |text: &str| format!("\"{text}\"");
    match opening {
        Opening::Page { address, .. } => {
            let started = opening.browser().is_some_and(|browser| {
                shell_open(&format!("{browser}.exe"), Some(&quoted(address))).is_ok()
            });
            if started {
                Ok(())
            } else {
                shell_open(address, None)
            }
        }
        Opening::Project { folder } => shell_open("Code.exe", Some(&quoted(&folder_here(folder)?)))
            .map_err(|_| "VS Code isn't installed here".to_string()),
        Opening::Folder { folder } => shell_open(&folder_here(folder)?, None),
    }
}

/// Opens a handed-off file with its usual program, unless Windows would run it as a program.
pub(super) fn open_file(file: &Path) -> Result<(), String> {
    let kind = file
        .extension()
        .map(|extension| format!(".{}", extension.to_string_lossy()));
    // SAFETY: only reads the string. Windows' own list: .exe, .bat, .ps1, .lnk and the rest.
    if kind.is_some_and(|kind| unsafe { AssocIsDangerous(&HSTRING::from(kind)) }.as_bool()) {
        return Err(format!(
            "{} was saved but not opened, as it's a program",
            file.display()
        ));
    }
    shell_open(&file.display().to_string(), None)
}

pub(super) fn handoff_folder() -> Option<PathBuf> {
    Some(known_folder(&FOLDERID_Documents)?.join("Hackerman"))
}

fn folder_here(folder: &str) -> Result<String, String> {
    let home = known_folder(&FOLDERID_Profile).unwrap_or_default();
    places_to_look(Path::new(folder), &home)
        .into_iter()
        .find(|place| place.is_dir())
        .map(|place| place.display().to_string())
        .ok_or_else(|| format!("{folder} isn't on this PC"))
}

fn shell_open(file: &str, parameters: Option<&str>) -> Result<(), String> {
    let file = HSTRING::from(file);
    let parameters = parameters.map(HSTRING::from);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        // No error boxes: whoever handed it off is told instead.
        fMask: SEE_MASK_FLAG_NO_UI | SEE_MASK_NOASYNC,
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: parameters
            .as_ref()
            .map_or(PCWSTR::null(), |text| PCWSTR(text.as_ptr())),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: the strings outlive the call.
    unsafe { ShellExecuteExW(&raw mut info) }.map_err(|error| error.message())
}

fn title_of(window: HWND) -> String {
    let mut text = [0u16; 512];
    // SAFETY: Windows writes at most the buffer's length.
    let length = unsafe { GetWindowTextW(window, &mut text) };
    String::from_utf16_lossy(&text[..length.max(0) as usize])
}

/// What a Chromium browser's address bar says.
fn address_of(window: HWND) -> Result<String, String> {
    let failed =
        |error: windows::core::Error| format!("couldn't read the address: {}", error.message());
    // SAFETY: COM objects are released when dropped. A window that has gone since is an error.
    unsafe {
        let automation: IUIAutomation =
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).map_err(failed)?;
        let browser = automation.ElementFromHandle(window).map_err(failed)?;
        let class = VARIANT::from(BSTR::from("OmniboxViewViews"));
        let condition = automation
            .CreatePropertyCondition(UIA_ClassNamePropertyId, &class)
            .map_err(failed)?;
        let bar = browser
            .FindFirst(TreeScope_Descendants, &condition)
            .map_err(|_| "couldn't find its address bar".to_string())?;
        let value: IUIAutomationValuePattern = bar
            .GetCurrentPatternAs(UIA_ValuePatternId)
            .map_err(failed)?;
        Ok(value.CurrentValue().map_err(failed)?.to_string())
    }
}

/// The folders open in VS Code's windows, from its `storage.json`.
fn vs_code_folders(storage: &str) -> Vec<PathBuf> {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(storage) else {
        return Vec::new();
    };
    let state = &json["windowsState"];
    std::iter::once(&state["lastActiveWindow"])
        .chain(state["openedWindows"].as_array().into_iter().flatten())
        .filter_map(|window| window["folder"].as_str())
        .filter_map(path_of_url)
        .collect()
}

/// Which folder a VS Code title like "main.rs - hackerman - Visual Studio Code" is for.
fn folder_in_title<'a>(title: &str, folders: &'a [PathBuf]) -> Option<&'a Path> {
    let shown = title
        .rsplit_once(" - Visual Studio Code")
        .map_or(title, |(shown, _)| shown);
    let shown = shown.trim_start_matches('●').trim();
    folders
        .iter()
        .filter(|folder| {
            folder
                .file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| {
                    shown.eq_ignore_ascii_case(name)
                        || shown
                            .to_lowercase()
                            .ends_with(&format!(" - {}", name.to_lowercase()))
                })
        })
        .max_by_key(|folder| folder.as_os_str().len())
        .map(PathBuf::as_path)
}

/// The folder a File Explorer window shows. With tabs, the first tab's.
fn explorer_folder(window: HWND) -> Option<PathBuf> {
    // SAFETY: COM objects are released when dropped.
    unsafe {
        let windows: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_ALL).ok()?;
        (0..windows.Count().ok()?).find_map(|index| {
            let browser: IWebBrowserApp = windows.Item(&VARIANT::from(index)).ok()?.cast().ok()?;
            (browser.HWND().ok()?.0 == window.0 as isize).then_some(())?;
            path_of_url(&browser.LocationURL().ok()?.to_string())
        })
    }
}

/// `file:///c%3A/Users/one` as `c:\Users\one`. Other kinds of address give nothing.
fn path_of_url(url: &str) -> Option<PathBuf> {
    // VS Code writes the drive's colon as %3A, which Windows then takes for part of a name.
    let url = url.replace("%3A", ":").replace("%3a", ":");
    let mut path = [0u16; 32768];
    let mut length = path.len() as u32;
    // SAFETY: Windows writes at most `length` characters, and says how many.
    unsafe {
        PathCreateFromUrlW(
            &HSTRING::from(url),
            windows::core::PWSTR(path.as_mut_ptr()),
            &raw mut length,
            0,
        )
        .ok()?;
    }
    Some(PathBuf::from(String::from_utf16_lossy(
        &path[..length as usize],
    )))
}

/// A command line split up the way programs see it.
fn arguments_of(line: &str) -> Vec<String> {
    let line = HSTRING::from(line);
    let mut count = 0;
    // SAFETY: Windows gives back `count` strings in one allocation, freed once read.
    unsafe {
        let arguments = CommandLineToArgvW(&line, &raw mut count);
        if arguments.is_null() {
            return Vec::new();
        }
        let read = (0..count.max(0) as usize)
            .filter_map(|at| (*arguments.add(at)).to_string().ok())
            .collect();
        let _ = LocalFree(Some(HLOCAL(arguments.cast())));
        read
    }
}

/// The files Windows lists as opened lately whose names are in this title.
fn recent_files(title: &str) -> Vec<PathBuf> {
    let Some(folder) = known_folder(&FOLDERID_Recent) else {
        return Vec::new();
    };
    let title = title.to_lowercase();
    let mut links: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            // A shortcut is named after its file: "report.docx.lnk".
            let name = entry.file_name().to_string_lossy().to_lowercase();
            name.strip_suffix(".lnk")
                .is_some_and(|named| named.len() >= 3 && title.contains(named))
        })
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .collect();
    links.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    links
        .into_iter()
        .filter_map(|(_, link)| target_of(&link))
        .collect()
}

fn target_of(link: &Path) -> Option<PathBuf> {
    // SAFETY: COM objects are released when dropped, and Windows writes at most the buffer.
    unsafe {
        let shortcut: IShellLinkW =
            CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        shortcut
            .cast::<IPersistFile>()
            .ok()?
            .Load(&HSTRING::from(link.as_os_str()), STGM_READ)
            .ok()?;
        let mut path = [0u16; 32768];
        shortcut.GetPath(&mut path, std::ptr::null_mut(), 0).ok()?;
        let length = path.iter().position(|&c| c == 0)?;
        (length > 0).then(|| PathBuf::from(String::from_utf16_lossy(&path[..length])))
    }
}

/// Of these files, in order, the first that exists and whose name is in the window's title.
fn file_in_title(title: &str, candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    let title = title.to_lowercase();
    let named = |name: Option<&OsStr>| {
        name.and_then(OsStr::to_str)
            .is_some_and(|name| name.chars().count() >= 3 && title.contains(&name.to_lowercase()))
    };
    candidates
        .into_iter()
        .find(|path| (named(path.file_name()) || named(path.file_stem())) && path.is_file())
}

fn known_folder(id: &GUID) -> Option<PathBuf> {
    // SAFETY: Windows allocates the path for the caller to free.
    unsafe {
        let path = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let text = path.to_string().ok();
        CoTaskMemFree(Some(path.0.cast()));
        text.map(PathBuf::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vs_code_title_is_matched_to_its_folder() {
        let storage = r#"{"windowsState": {
            "lastActiveWindow": {"folder": "file:///c%3A/Users/one/Documents/code/sawt-rnd"},
            "openedWindows": [
                {"folder": "file:///c%3A/Users/one/Documents/code/hackerman"},
                {"folder": "vscode-remote://ssh-remote%2Bbox/home/x"},
                {"workspaceIdentifier": {"id": "1"}}
            ]}}"#;
        let folders = vs_code_folders(storage);
        assert_eq!(
            folders,
            [
                PathBuf::from(r"c:\Users\one\Documents\code\sawt-rnd"),
                PathBuf::from(r"c:\Users\one\Documents\code\hackerman"),
            ]
        );
        let found = |title| folder_in_title(title, &folders).map(Path::to_path_buf);
        assert_eq!(
            found("● engine.rs - hackerman - Visual Studio Code"),
            Some(PathBuf::from(r"c:\Users\one\Documents\code\hackerman"))
        );
        assert_eq!(
            found("sawt-rnd - Visual Studio Code"),
            Some(PathBuf::from(r"c:\Users\one\Documents\code\sawt-rnd"))
        );
        assert_eq!(found("notes - other - Visual Studio Code"), None);
        assert!(vs_code_folders("not json").is_empty());
    }

    #[test]
    fn a_command_line_splits_as_programs_see_it() {
        assert_eq!(
            arguments_of(r#""C:\Program Files\App\app.exe" /n "C:\My Files\report.docx" x"#),
            [
                r"C:\Program Files\App\app.exe",
                "/n",
                r"C:\My Files\report.docx",
                "x"
            ]
        );
    }

    #[test]
    fn the_file_named_in_the_title_is_the_one_open() {
        let folder = std::env::temp_dir().join(format!("hackerman-handoff-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let report = folder.join("report.docx");
        let notes = folder.join("notes.txt");
        std::fs::write(&report, "r").unwrap();
        std::fs::write(&notes, "n").unwrap();
        let candidates = || vec![folder.join("gone.txt"), notes.clone(), report.clone()];

        assert_eq!(
            file_in_title("report - Word", candidates()),
            Some(report.clone())
        );
        assert_eq!(
            file_in_title("*notes.txt - Notepad", candidates()),
            Some(notes.clone())
        );
        assert_eq!(
            file_in_title("gone.txt - Notepad", candidates()),
            None,
            "it must exist"
        );
        assert_eq!(file_in_title("Spotify Premium", candidates()), None);
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn programs_are_saved_but_never_opened() {
        for name in ["setup.exe", "run.bat", "script.ps1", "a.cmd", "x.lnk"] {
            let error = open_file(Path::new(name)).unwrap_err();
            assert!(error.contains("as it's a program"), "{name}: {error}");
        }
    }

    /// Only looks. Run with `--nocapture` and `--ignored` with Edge or Chrome open to see what
    /// would be handed off.
    #[test]
    #[ignore = "reads the browsers and windows open on this PC"]
    fn what_is_open_here() {
        for app in ["msedge", "chrome", "code", "explorer", "winword", "notepad"] {
            println!("{app}: {:?}", what_is_open(app, None));
        }
    }
}
