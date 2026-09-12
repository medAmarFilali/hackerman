//! The whole way across: files copied as File Explorer copies them, two agents over a real
//! encrypted connection, and Windows' own code pasting at the far end. Only the keyboard and
//! mouse are simulated.
//!
//! These only run on demand, in a window station of their own, because they change the
//! clipboard and Windows may show a progress window: see `scripts/run-isolated.ps1`.
//!
//! Both agents share that one clipboard here, which real computers wouldn't. That doesn't
//! get in the way, because what one writes (a list of paths) and what the other writes
//! (virtual files) are different, and the first has already read its list by then.

#![cfg(windows)]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hk_core::clipboard::{ClipboardData, SharedFile};
use hk_core::geometry::Point;
use hk_core::input::InputEvent;
use hk_core::layout::{Monitor, Placement};
use hk_daemon::{Options, run};
use hk_ipc::{DeviceStatus, IpcConnection, Request, UiState};
use hk_net::{Fingerprint, Identity};
use hk_platform::fake::FakePlatform;
use hk_platform::{
    Capture, Error, FileSource, InputHandler, NativePlatform, Platform, data_object, native,
};
use windows::Win32::Foundation::POINTL;
use windows::Win32::System::Com::IDataObject;
use windows::Win32::System::Ole::{DROPEFFECT_COPY, IDropTarget, OleGetClipboard, OleInitialize};
use windows::Win32::System::SystemServices::MK_LBUTTON;
use windows::Win32::UI::Shell::{BHID_SFUIObject, IShellItem, SHCreateItemFromParsingName};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
};
use windows::core::HSTRING;

const TIMEOUT: Duration = Duration::from_secs(60);
const BIG_FILE_SIZE: u32 = 30_000_000;

/// A simulated keyboard, mouse and screen, with the real clipboard.
#[derive(Clone)]
struct RealClipboard {
    input: FakePlatform,
    real: NativePlatform,
}

impl Platform for RealClipboard {
    fn monitors(&self) -> Vec<Monitor> {
        self.input.monitors()
    }
    fn model(&self) -> Option<String> {
        self.input.model()
    }
    fn desktop_folder(&self) -> Option<PathBuf> {
        self.input.desktop_folder()
    }
    fn park_point(&self) -> Point {
        self.input.park_point()
    }
    fn park_cursor(&self, at: Point) {
        self.input.park_cursor(at);
    }
    fn restore_cursor(&self, to: Point) {
        self.input.restore_cursor(to);
    }
    fn inject(&self, event: &InputEvent) {
        self.input.inject(event);
    }
    fn start_capture(&self, handler: InputHandler) -> Result<Capture, Error> {
        self.input.start_capture(handler)
    }
    fn fullscreen_app_in_front(&self) -> bool {
        self.input.fullscreen_app_in_front()
    }
    fn watch_clipboard(&self, on_change: Box<dyn FnMut() + Send>) -> Result<Capture, Error> {
        self.real.watch_clipboard(on_change)
    }
    fn read_clipboard(&self) -> Result<ClipboardData, Error> {
        self.real.read_clipboard()
    }
    fn write_clipboard(&self, data: &ClipboardData) -> Result<(), Error> {
        self.real.write_clipboard(data)
    }
    fn clipboard_files(&self) -> Result<Vec<PathBuf>, Error> {
        self.real.clipboard_files()
    }
    fn write_clipboard_files(
        &self,
        files: Vec<SharedFile>,
        source: Arc<dyn FileSource>,
    ) -> Result<(), Error> {
        self.real.write_clipboard_files(files, source)
    }
}

struct Agent {
    input: FakePlatform,
    app: IpcConnection,
    /// The newest state seen. An agent only reports changes, so what's being waited for
    /// may already have happened.
    state: Option<UiState>,
}

impl Agent {
    async fn wait_for(&mut self, what: &str, check: impl Fn(&UiState) -> bool) {
        let wait = async {
            while !self.state.as_ref().is_some_and(&check) {
                self.state = Some(self.app.recv().await.unwrap().unwrap());
            }
        };
        tokio::time::timeout(TIMEOUT, wait)
            .await
            .unwrap_or_else(|_| panic!("never saw: {what}"))
    }

    fn move_mouse(&self, x: f64, y: f64) -> bool {
        self.input.move_cursor(Point::new(x, y))
    }
}

/// Starts an agent with the real clipboard, already paired with the other.
async fn start(root: &Path, name: &str, port: u16, other: (Fingerprint, u16)) -> Agent {
    let input = FakePlatform::with_screens(1);
    let platform = RealClipboard {
        input: input.clone(),
        real: native().unwrap(),
    };
    start_with(root, name, port, other, input, platform).await
}

/// Starts an agent already paired with the other, at the address given.
async fn start_with(
    root: &Path,
    name: &str,
    port: u16,
    other: (Fingerprint, u16),
    input: FakePlatform,
    platform: impl Platform,
) -> Agent {
    let dir = root.join(name);
    std::fs::write(dir.join("hackerman.toml"), format!("name = \"{name}\"\n")).unwrap();
    let (other_id, other_port) = other;
    std::fs::write(
        dir.join("devices.json"),
        format!(
            r#"[{{"fingerprint":"{other_id}","name":"other","addresses":["127.0.0.1:{other_port}"]}}]"#
        ),
    )
    .unwrap();

    let channel = format!(
        r"\\.\pipe\hackerman-real-clipboard-{name}-{}",
        std::process::id()
    );
    let options = Options {
        config_dir: dir,
        port: Some(port),
        channel: channel.clone(),
        capture: true,
        discovery: false,
        headless_pairing: false,
    };
    tokio::spawn(run(options, platform));

    let started = Instant::now();
    let app = loop {
        if let Ok(app) = hk_ipc::connect(&channel).await {
            break app;
        }
        assert!(started.elapsed() < TIMEOUT, "{name}'s agent never answered");
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    Agent {
        input,
        app,
        state: None,
    }
}

fn identity_in(root: &Path, name: &str) -> Fingerprint {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    Identity::load_or_create(&dir.join("identity"))
        .unwrap()
        .fingerprint()
}

/// Waits for the two to connect, then places them next to each other.
async fn side_by_side(a: &mut Agent, a_id: Fingerprint, b: &mut Agent, b_id: Fingerprint) {
    let connected = |other: Fingerprint| {
        move |state: &UiState| {
            state.devices.iter().any(|device| {
                device.fingerprint == other && device.status == DeviceStatus::Connected
            })
        }
    };
    a.wait_for("connected", connected(b_id)).await;
    b.wait_for("connected", connected(a_id)).await;

    let place = |device, x| Placement {
        device,
        monitor: "fake-0".into(),
        x,
        y: 0.0,
        width: None,
    };
    a.app
        .send(&Request::SetPlacements {
            placements: vec![place(a_id, 0.0), place(b_id, 1920.0)],
        })
        .await
        .unwrap();
    a.wait_for("placed", |state| {
        state
            .screens
            .iter()
            .any(|screen| screen.device == b_id && screen.x == 1920.0)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
}

struct Copying {
    root: PathBuf,
    into: PathBuf,
    /// What's selected in File Explorer when copying.
    selected: [PathBuf; 2],
    /// Each file, and where it should end up.
    sources: Vec<(PathBuf, PathBuf)>,
}

/// Folders, unusual names and enough bytes to take a moment, in a temporary folder.
fn files_to_copy(name: &str) -> Copying {
    let root = std::env::temp_dir().join(name);
    let _ = std::fs::remove_dir_all(&root);
    let from = root.join("copied from here");
    let into = root.join("pasted to here");
    std::fs::create_dir_all(from.join("Dossier été/sous dossier")).unwrap();
    std::fs::create_dir_all(&into).unwrap();
    let big: Vec<u8> = (0..BIG_FILE_SIZE)
        .map(|at| ((at / 7) % 251) as u8)
        .collect();
    std::fs::write(from.join("Dossier été/sous dossier/grand.bin"), &big).unwrap();
    std::fs::write(from.join("Dossier été/petit ✓.txt"), "petit").unwrap();
    std::fs::write(from.join("seul.txt"), "seul").unwrap();

    Copying {
        selected: [from.join("Dossier été"), from.join("seul.txt")],
        sources: vec![
            (
                from.join("Dossier été/sous dossier/grand.bin"),
                into.join("Dossier été/sous dossier/grand.bin"),
            ),
            (
                from.join("Dossier été/petit ✓.txt"),
                into.join("Dossier été/petit ✓.txt"),
            ),
            (from.join("seul.txt"), into.join("seul.txt")),
        ],
        root,
        into,
    }
}

/// Pastes what's on the clipboard into a folder.
fn paste_clipboard_into(into: &Path, is_done: &dyn Fn() -> bool) {
    unsafe { OleInitialize(None) }.unwrap();
    let object = unsafe { OleGetClipboard() }.unwrap();
    paste_with_windows(&object, into, is_done);
}

/// Pastes into a folder as File Explorer does: by dropping onto the folder, with Windows'
/// code doing the copying.
fn paste_with_windows(object: &IDataObject, into: &Path, is_done: &dyn Fn() -> bool) {
    unsafe {
        let folder: IShellItem =
            SHCreateItemFromParsingName(&HSTRING::from(into.as_os_str()), None).unwrap();
        let target: IDropTarget = folder.BindToHandler(None, &BHID_SFUIObject).unwrap();
        let mut effect = DROPEFFECT_COPY;
        let at = POINTL { x: 0, y: 0 };
        target
            .DragEnter(object, MK_LBUTTON, at, &raw mut effect)
            .unwrap();
        assert_eq!(effect, DROPEFFECT_COPY, "the folder doesn't take these");
        target
            .Drop(object, MK_LBUTTON, at, &raw mut effect)
            .unwrap();

        // That code may call back to this thread from another one.
        let give_up_at = Instant::now() + TIMEOUT;
        let mut message = MSG::default();
        while !is_done() {
            assert!(Instant::now() < give_up_at, "the paste never finished");
            while PeekMessageW(&raw mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&raw const message);
                DispatchMessageW(&raw const message);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn all_pasted(sources: &[(PathBuf, PathBuf)]) -> bool {
    sources.iter().all(|(from, to)| {
        let (from, to) = (std::fs::metadata(from), std::fs::metadata(to));
        matches!((from, to), (Ok(from), Ok(to)) if from.len() == to.len())
    })
}

/// Pastes on a thread the async runtime knows nothing about, as Windows' threads are, then
/// checks that every file arrived whole.
async fn paste_and_check(
    copying: Copying,
    paste: impl FnOnce(&Path, &dyn Fn() -> bool) + Send + 'static,
) {
    let (into, check) = (copying.into.clone(), copying.sources.clone());
    let pasting = std::thread::spawn(move || paste(&into, &|| all_pasted(&check)));
    while !pasting.is_finished() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    pasting.join().unwrap();

    for (from, to) in &copying.sources {
        assert!(
            std::fs::read(from).unwrap() == std::fs::read(to).unwrap(),
            "{}",
            to.display()
        );
    }
    println!("pasted {BIG_FILE_SIZE} bytes across");
    let _ = std::fs::remove_dir_all(&copying.root);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "replaces what's on the real clipboard: see scripts/run-isolated.ps1"]
async fn files_copied_on_one_computer_paste_on_another() {
    let copying = files_to_copy("hackerman-real-clipboard");
    let root = &copying.root;
    let (a_id, b_id) = (identity_in(root, "a"), identity_in(root, "b"));
    let (a_port, b_port) = (24891, 24892);
    let mut a = start(root, "a", a_port, (b_id, b_port)).await;
    let mut b = start(root, "b", b_port, (a_id, a_port)).await;
    side_by_side(&mut a, a_id, &mut b, b_id).await;

    // Copied the way File Explorer does it, while using this computer.
    assert!(!a.move_mouse(100.0, 100.0));
    let mut clipboard = arboard::Clipboard::new().unwrap();
    clipboard.clear().unwrap();
    clipboard.set().file_list(&copying.selected).unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The cursor arrives at the other computer, and with it the list of files.
    assert!(a.move_mouse(1919.0, 500.0));
    tokio::time::sleep(Duration::from_millis(1500)).await;

    paste_and_check(copying, paste_clipboard_into).await;
}

/// Reads through what an agent put on its simulated clipboard.
struct OnClipboard(FakePlatform);

impl FileSource for OnClipboard {
    fn open(&self, index: usize, offset: u64) -> Box<dyn Read + Send> {
        self.0
            .read_pasted_file(index, offset)
            .expect("no files to paste")
    }
}

/// The same with a simulated clipboard, handing the files straight to Windows' paste code.
/// Another program can hold the clipboard of that window station, and this still shows
/// whether pasting works when one does: see `scripts/run-isolated.ps1`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "may show a window: see scripts/run-isolated.ps1"]
async fn files_copied_on_one_computer_are_pasted_by_windows_on_another() {
    let copying = files_to_copy("hackerman-real-paste");
    let root = &copying.root;
    let (a_id, b_id) = (identity_in(root, "c"), identity_in(root, "d"));
    let (a_port, b_port) = (24893, 24894);
    let (a_input, b_input) = (FakePlatform::with_screens(1), FakePlatform::with_screens(1));
    let mut a = start_with(root, "c", a_port, (b_id, b_port), a_input.clone(), a_input).await;
    let mut b = start_with(root, "d", b_port, (a_id, a_port), b_input.clone(), b_input).await;
    side_by_side(&mut a, a_id, &mut b, b_id).await;

    assert!(!a.move_mouse(100.0, 100.0));
    a.input.copy_files(copying.selected.to_vec());
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The cursor arrives at the other computer, and with it the list of files.
    assert!(a.move_mouse(1919.0, 500.0));
    let started = Instant::now();
    while b.input.files_to_paste().is_empty() {
        assert!(started.elapsed() < TIMEOUT, "the list of files never came");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let files = b.input.files_to_paste();
    let source: Arc<dyn FileSource> = Arc::new(OnClipboard(b.input.clone()));
    paste_and_check(copying, move |into, is_done| {
        unsafe { OleInitialize(None) }.unwrap();
        paste_with_windows(&data_object(files, source), into, is_done);
    })
    .await;
}
