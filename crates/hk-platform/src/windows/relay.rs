//! Lets a helper running as the system do this computer's input work for the agent: the input
//! hooks, hiding the cursor, and replaying input from other PCs.
//!
//! Windows keeps a program from sending input to programs running as administrator, and from
//! hearing about input while one of those is in front, unless it has as much permission itself.
//! The agent runs as the user, so the lock-screen service also starts this helper on the user's
//! desktop while they're using the computer (see `crates/hk-win-service`), and the agent hands
//! its input work to it while it's there. Everything else stays with the agent, and the agent
//! does all of it itself whenever there's no helper, as it did before there was one.
//!
//! They talk over a pipe that only the system and the user who set the service up can open.
//! Every key press and mouse movement on the computer waits for the agent to say whether to keep
//! it from Windows, so the helper only waits a moment for that. An agent that doesn't answer in
//! time is let go of, and goes back to its own hooks.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use hk_core::geometry::Point;
use hk_core::input::InputEvent;
use hk_core::switcher::LocalEvent;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_IO_PENDING, ERROR_NO_DATA, ERROR_PIPE_CONNECTED, ERROR_SUCCESS, HANDLE,
    HLOCAL, LocalFree, WAIT_OBJECT_0,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
    SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ,
    FILE_SHARE_MODE, FILE_WRITE_DATA, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile,
    SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, WriteFile,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
    WaitForSingleObject,
};
use windows::core::{HSTRING, Owned, PCWSTR};

use super::{hook, session};
use crate::{Error, InputHandler};

/// Where the helper waits for the agent.
pub(super) const CHANNEL: &str = r"\\.\pipe\hackerman-input";
/// Who the helper's end of the pipe can belong to: the system, or administrators. Anyone else
/// with a pipe of that name would be pretending, and could make up input for other PCs.
pub(super) const HELPER_ACCOUNTS: [&str; 2] = ["S-1-5-18", "S-1-5-32-544"];
/// How often the agent looks for the helper.
pub(super) const LOOK_EVERY: Duration = Duration::from_secs(2);

/// Changes whenever what's said below does. The helper is only brought up to date when an
/// administrator agrees to it, so for a while it can be older than the agent.
const VERSION: u32 = 1;
/// How long every input event on the computer can wait for the agent to answer.
const ANSWER_WITHIN: Duration = Duration::from_millis(100);
const HELLO_WITHIN: Duration = Duration::from_secs(5);
/// Anything taking this long to send means the other side has stopped reading.
const SEND_WITHIN: Duration = Duration::from_millis(500);
/// How often a wait with no time limit checks whether the pipe has been given up on.
const CHECK_CLOSED_EVERY: Duration = Duration::from_millis(200);
/// Far larger than any message.
const MAX_MESSAGE: usize = 1024;
const BUFFER: u32 = 64 * 1024;

#[derive(Debug, PartialEq, Serialize, Deserialize)]
enum ToHelper {
    Hello {
        version: u32,
    },
    /// Start or stop the input hooks.
    Capture(bool),
    /// Whether to keep an event from Windows.
    Verdict {
        id: u64,
        swallow: bool,
    },
    Inject(InputEvent),
    Park(Point),
    Restore(Point),
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
enum FromHelper {
    Hello {
        version: u32,
    },
    /// The hooks are in, so the agent's own can go.
    Capturing,
    /// Wants a verdict.
    Event {
        id: u64,
        event: LocalEvent,
    },
    /// The agent took too long with a verdict, so the hooks are letting everything through
    /// without asking, until it asks for them again.
    Released,
}

/// What either end does with input on this computer: the real thing, or a stand-in for tests.
pub(super) trait Machine: Clone + Send + Sync + 'static {
    type Hooks: Send + 'static;
    /// Starts the input hooks, until what this returns is dropped.
    fn hook(&self, handler: InputHandler) -> Result<Self::Hooks, Error>;
    /// Replays input from another PC. It mustn't wait long.
    fn replay(&self, event: &InputEvent);
    /// Moves the cursor out of the way and hides it, while input goes to another PC.
    fn park(&self, at: Point);
    fn restore(&self, to: Point);
    /// Stops hiding the cursor, wherever it is, as the agent's own hooks are taking over.
    fn unhide(&self);
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Nothing kept behind these can be left half changed by a panic.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// With Windows' own error number, rather than the wider kind the `windows` crate gives.
fn io_error(error: &windows::core::Error) -> io::Error {
    let code = error.code().0 as u32;
    if code >> 16 == 0x8007 {
        io::Error::from_raw_os_error((code & 0xFFFF) as i32)
    } else {
        io::Error::other(error.message())
    }
}

/// Either end of the pipe. It's opened for overlapped use, which lets one thread wait to read
/// while others write.
struct Pipe {
    handle: HANDLE,
    /// Set once either side gives up on it, which makes anything waiting on it stop.
    closed: AtomicBool,
    /// The same, for good: the helper's end is used again for the next agent otherwise.
    finished: AtomicBool,
    sending: Mutex<()>,
}

// SAFETY: a handle can be used from any thread, and overlapped reads and writes on it don't
// wait for each other.
unsafe impl Send for Pipe {}
unsafe impl Sync for Pipe {}

impl Drop for Pipe {
    fn drop(&mut self) {
        // SAFETY: the handle is this pipe's, and nothing uses it after this.
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

impl Pipe {
    fn new(handle: HANDLE) -> Self {
        Self {
            handle,
            closed: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            sending: Mutex::new(()),
        }
    }

    /// The helper's end, which only the system and this account can open, to read and write.
    /// Programs kept in a sandbox can't open it. There's only ever one pipe of this name, and
    /// this makes it first, so nothing else can wait for agents under it while this is here.
    fn create(name: &str, account: &str) -> io::Result<Self> {
        let rules = format!("D:P(A;;GA;;;SY)(A;;0x12019b;;;{account})S:(ML;;NW;;;ME)");
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: Windows allocates the descriptor, which is kept for the call that copies it
        // and then freed. The attributes point to it for that call only.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &HSTRING::from(rules),
                SDDL_REVISION_1,
                &raw mut descriptor,
                None,
            )
            .map_err(|error| io_error(&error))?;
            let attributes = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor.0,
                bInheritHandle: false.into(),
            };
            // Only one, made by this program: one already there is someone else's.
            let handle = CreateNamedPipeW(
                &HSTRING::from(name),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                BUFFER,
                BUFFER,
                0,
                Some(&raw const attributes),
            );
            let error = io::Error::last_os_error();
            LocalFree(Some(HLOCAL(descriptor.0)));
            if handle.is_invalid() {
                return Err(error);
            }
            Ok(Self::new(handle))
        }
    }

    /// The agent's end.
    fn open(name: &str) -> io::Result<Self> {
        // SAFETY: a plain call, whose handle is owned from here. Asking for identification
        // only means the helper can't act as this user, which it has no need to.
        let handle = unsafe {
            CreateFileW(
                &HSTRING::from(name),
                (FILE_GENERIC_READ | FILE_WRITE_DATA).0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                None,
            )
        }
        .map_err(|error| io_error(&error))?;
        Ok(Self::new(handle))
    }

    /// The account the pipe belongs to, which is whoever made it.
    fn owner(&self) -> Option<String> {
        let mut owner = PSID::default();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: Windows allocates the descriptor that `owner` points into, which is freed
        // once the owner has been copied out as text.
        unsafe {
            let found = GetSecurityInfo(
                self.handle,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                Some(&raw mut owner),
                None,
                None,
                None,
                Some(&raw mut descriptor),
            );
            if found != ERROR_SUCCESS {
                return None;
            }
            let text = session::sid_text(owner);
            LocalFree(Some(HLOCAL(descriptor.0)));
            text
        }
    }

    /// Waits for an agent, as long as it takes.
    fn accept(&self) -> io::Result<()> {
        loop {
            // SAFETY: `transfer` keeps what it's given until Windows has finished with it.
            let connected = self.transfer(None, |overlapped| unsafe {
                ConnectNamedPipe(self.handle, Some(overlapped))
            });
            match connected {
                // One got here before the call.
                Err(error) if error.raw_os_error() == Some(ERROR_PIPE_CONNECTED.0 as i32) => {
                    return Ok(());
                }
                // One came and went before the call.
                Err(error) if error.raw_os_error() == Some(ERROR_NO_DATA.0 as i32) => {
                    // SAFETY: the handle is valid for as long as this exists.
                    let _ = unsafe { DisconnectNamedPipe(self.handle) };
                }
                other => return other.map(|_| ()),
            }
        }
    }

    /// Lets the agent go, so that the next one can connect.
    fn disconnect(&self) {
        // SAFETY: the handle is valid for as long as this exists.
        let _ = unsafe { DisconnectNamedPipe(self.handle) };
        self.closed.store(false, Ordering::SeqCst);
    }

    /// Gives up on it. Anything waiting on it stops, and nothing more goes through.
    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        // SAFETY: the handle is valid for as long as this exists.
        let _ = unsafe { CancelIoEx(self.handle, None) };
    }

    #[cfg(test)]
    fn finish(&self) {
        self.finished.store(true, Ordering::SeqCst);
        self.close();
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst) || self.finished.load(Ordering::SeqCst)
    }

    fn send<T: Serialize>(&self, message: &T, within: Duration) -> io::Result<()> {
        let body = postcard::to_allocvec(message).map_err(io::Error::other)?;
        let mut frame = Vec::with_capacity(2 + body.len());
        frame.extend_from_slice(&(body.len() as u16).to_le_bytes());
        frame.extend_from_slice(&body);
        let _one_at_a_time = lock(&self.sending);
        let sent = self.write_all(&frame, within);
        if sent.is_err() {
            self.close();
        }
        sent
    }

    /// The next message, waiting at most `within` for it, if that's given. `None` is one from a
    /// newer version that this one can't make sense of.
    fn receive<T: DeserializeOwned>(&self, within: Option<Duration>) -> io::Result<Option<T>> {
        let received = self.read_message(within);
        if received.is_err() {
            self.close();
        }
        Ok(postcard::from_bytes(&received?).ok())
    }

    fn read_message(&self, within: Option<Duration>) -> io::Result<Vec<u8>> {
        let mut length = [0; 2];
        self.read_exact(&mut length, within)?;
        let length = usize::from(u16::from_le_bytes(length));
        if length > MAX_MESSAGE {
            return Err(io::Error::other("a message too large to be one"));
        }
        let mut body = vec![0; length];
        self.read_exact(&mut body, within)?;
        Ok(body)
    }

    fn read_exact(&self, buffer: &mut [u8], within: Option<Duration>) -> io::Result<()> {
        let mut filled = 0;
        while filled < buffer.len() {
            let rest = &mut buffer[filled..];
            // SAFETY: the buffer outlives the read, which `transfer` waits for.
            let read = self.transfer(within, |overlapped| unsafe {
                ReadFile(self.handle, Some(rest), None, Some(overlapped))
            })?;
            if read == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            filled += read;
        }
        Ok(())
    }

    fn write_all(&self, bytes: &[u8], within: Duration) -> io::Result<()> {
        let mut sent = 0;
        while sent < bytes.len() {
            let rest = &bytes[sent..];
            // SAFETY: the bytes outlive the write, which `transfer` waits for.
            let written = self.transfer(Some(within), |overlapped| unsafe {
                WriteFile(self.handle, Some(rest), None, Some(overlapped))
            })?;
            if written == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            sent += written;
        }
        Ok(())
    }

    /// Starts one read, write or wait for an agent, and waits for it to finish. With a time
    /// limit, it's called off after that long. It's called off as soon as the pipe is closed.
    fn transfer(
        &self,
        within: Option<Duration>,
        start: impl FnOnce(*mut OVERLAPPED) -> windows::core::Result<()>,
    ) -> io::Result<usize> {
        if self.is_closed() {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        // SAFETY: the event is owned here and outlives the operation, and so does `overlapped`:
        // every way out of here waits for Windows to be finished with both.
        unsafe {
            let event = CreateEventW(None, true, false, PCWSTR::null())
                .map_err(|error| io_error(&error))?;
            let event = Owned::new(event);
            let mut overlapped = OVERLAPPED {
                hEvent: *event,
                ..Default::default()
            };
            if let Err(error) = start(&raw mut overlapped)
                && error.code() != ERROR_IO_PENDING.to_hresult()
            {
                return Err(io_error(&error));
            }
            let started = Instant::now();
            loop {
                let left = within.map_or(CHECK_CLOSED_EVERY, |within| {
                    within.saturating_sub(started.elapsed())
                });
                let wait = left.min(CHECK_CLOSED_EVERY).as_millis() as u32;
                if WaitForSingleObject(*event, wait) == WAIT_OBJECT_0 {
                    break;
                }
                let out_of_time = within.is_some_and(|within| started.elapsed() >= within);
                if out_of_time || self.is_closed() {
                    let _ = CancelIoEx(self.handle, Some(&raw const overlapped));
                    let mut ignored = 0;
                    let _ = GetOverlappedResult(
                        self.handle,
                        &raw const overlapped,
                        &raw mut ignored,
                        true,
                    );
                    return Err(if out_of_time {
                        io::ErrorKind::TimedOut
                    } else {
                        io::ErrorKind::BrokenPipe
                    }
                    .into());
                }
            }
            let mut moved = 0;
            GetOverlappedResult(self.handle, &raw const overlapped, &raw mut moved, false)
                .map_err(|error| io_error(&error))?;
            Ok(moved as usize)
        }
    }
}

/// The helper's end: waits for the agent, one at a time, and does what it asks.
pub(super) struct Server {
    pipe: Arc<Pipe>,
}

impl Server {
    /// Fails if something already has the name, which may be a helper already running.
    pub(super) fn create(channel: &str, account: &str) -> io::Result<Self> {
        Ok(Self {
            pipe: Arc::new(Pipe::create(channel, account)?),
        })
    }

    /// Goes on until there's no way to go on, or, in tests, until [`Server::stop`].
    pub(super) fn run(&self, machine: &impl Machine) -> io::Result<()> {
        loop {
            let accepted = self.pipe.accept();
            if self.pipe.finished.load(Ordering::SeqCst) {
                return Ok(());
            }
            accepted?;
            if let Err(error) = converse(&self.pipe, machine) {
                tracing::info!(%error, "the agent has gone");
            }
            self.pipe.disconnect();
        }
    }

    #[cfg(test)]
    fn stop(&self) {
        self.pipe.finish();
    }
}

/// The hooks running for the agent, and where its verdicts go.
struct Hooking<H> {
    // Goes first, so that the hooks stop waiting for verdicts before they're removed.
    verdicts: Sender<(u64, bool)>,
    /// Whether they've stopped asking, because the agent took too long.
    released: Arc<AtomicBool>,
    _hooks: H,
}

fn converse<M: Machine>(pipe: &Arc<Pipe>, machine: &M) -> io::Result<()> {
    let hello = pipe.receive(Some(HELLO_WITHIN))?;
    pipe.send(&FromHelper::Hello { version: VERSION }, SEND_WITHIN)?;
    if hello != Some(ToHelper::Hello { version: VERSION }) {
        // Letting it go straight away would throw away what was just sent, before it could be
        // read. The agent goes once it knows, and nothing more it says is done.
        while pipe.receive::<ToHelper>(Some(HELLO_WITHIN)).is_ok() {}
        return Err(io::Error::other(
            "an agent from another version of Hackerman",
        ));
    }
    tracing::info!("the agent is here");

    let mut hooking: Option<Hooking<M::Hooks>> = None;
    loop {
        let Some(message) = pipe.receive(None)? else {
            continue;
        };
        match message {
            ToHelper::Capture(true) => match &hooking {
                // They're asking again.
                Some(hooking) => {
                    hooking.released.store(false, Ordering::SeqCst);
                    pipe.send(&FromHelper::Capturing, SEND_WITHIN)?;
                }
                None => {
                    let (verdicts, verdicts_rx) = mpsc::channel();
                    let released = Arc::new(AtomicBool::new(false));
                    let asking =
                        ask_the_agent(pipe.clone(), verdicts_rx, released.clone(), machine.clone());
                    match machine.hook(asking) {
                        Ok(hooks) => {
                            hooking = Some(Hooking {
                                verdicts,
                                released,
                                _hooks: hooks,
                            });
                            pipe.send(&FromHelper::Capturing, SEND_WITHIN)?;
                        }
                        // The agent keeps its own.
                        Err(error) => tracing::warn!(%error, "can't start the input hooks"),
                    }
                }
            },
            ToHelper::Capture(false) => hooking = None,
            ToHelper::Verdict { id, swallow } => {
                if let Some(hooking) = &hooking {
                    let _ = hooking.verdicts.send((id, swallow));
                }
            }
            ToHelper::Inject(event) => machine.replay(&event),
            ToHelper::Park(at) => machine.park(at),
            ToHelper::Restore(to) => machine.restore(to),
            ToHelper::Hello { .. } => {}
        }
    }
}

/// What the helper's hooks do with each event: ask the agent, and wait a moment for its verdict.
///
/// An agent that takes too long gets its hooks back, rather than being let go of: letting go
/// would throw away what it has sent and not been read yet, like a key being let go.
fn ask_the_agent<M: Machine>(
    pipe: Arc<Pipe>,
    verdicts: Receiver<(u64, bool)>,
    released: Arc<AtomicBool>,
    machine: M,
) -> InputHandler {
    let mut last = 0;
    Box::new(move |event| {
        if released.load(Ordering::SeqCst) {
            return false;
        }
        last += 1;
        if pipe
            .send(&FromHelper::Event { id: last, event }, ANSWER_WITHIN)
            .is_err()
        {
            return false;
        }
        let deadline = Instant::now() + ANSWER_WITHIN;
        loop {
            match verdicts.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok((id, swallow)) if id == last => return swallow,
                // Too late, for one before.
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {
                    released.store(true, Ordering::SeqCst);
                    // The agent hides it with its own hooks, if it should be.
                    machine.unhide();
                    hook::report(|| {
                        tracing::warn!(
                            "the agent took too long to say what to do with input, so it's \
                             doing that itself again for now"
                        );
                    });
                    // Small enough to fit, unless the agent has stopped reading altogether.
                    if pipe.send(&FromHelper::Released, ANSWER_WITHIN).is_err() {
                        pipe.close();
                    }
                    return false;
                }
                // The hooks are on their way out.
                Err(RecvTimeoutError::Disconnected) => return false,
            }
        }
    })
}

/// The agent's end. It does all its input work itself until the helper is there, and again
/// if the helper goes.
pub(super) struct Relay<M: Machine> {
    machine: M,
    channel: String,
    owners: Vec<String>,
    /// How long after taking its hooks back to ask for the helper's again.
    ask_again_after: Duration,
    /// To ask from a thread of its own.
    this: Weak<Self>,
    link: Mutex<Option<Arc<Pipe>>>,
    capturing: Mutex<Option<Arc<Capturing<M::Hooks>>>>,
    /// Where the cursor is parked, while it is.
    parked: Mutex<Option<Point>>,
    /// Held while capturing starts or stops, and while the helper comes or goes, so that one
    /// of those can't happen halfway through another. Never needed by anything an input
    /// event does, since those can be waited on while it's held.
    changing: Mutex<()>,
}

struct Capturing<H> {
    handler: Arc<Mutex<InputHandler>>,
    /// Whether the helper's hooks are doing it, rather than this program's.
    by_helper: Arc<AtomicBool>,
    local: Mutex<Option<H>>,
}

/// What this program's own hooks do: the same as the helper's, unless the helper's are doing it.
fn local_handler<H>(capturing: &Capturing<H>) -> InputHandler {
    let (handler, by_helper) = (capturing.handler.clone(), capturing.by_helper.clone());
    Box::new(move |event| {
        if by_helper.load(Ordering::SeqCst) {
            return false;
        }
        let mut handler = lock(&handler);
        handler(event)
    })
}

enum NoHelper {
    NotThere,
    /// Whoever has the pipe isn't the system.
    SomeoneElse(Option<String>),
    OtherVersion(u32),
}

impl<M: Machine> Relay<M> {
    /// Starts looking for the helper, every so often.
    pub(super) fn start(
        machine: M,
        channel: &str,
        owners: Vec<String>,
        look_every: Duration,
    ) -> Arc<Self> {
        let relay = Arc::new_cyclic(|this| Self {
            machine,
            channel: channel.to_string(),
            owners,
            ask_again_after: look_every * 5,
            this: this.clone(),
            link: Mutex::new(None),
            capturing: Mutex::new(None),
            parked: Mutex::new(None),
            changing: Mutex::new(()),
        });
        let weak = Arc::downgrade(&relay);
        let started = std::thread::Builder::new()
            .name("input-relay".into())
            .spawn(move || keep_in_touch(&weak, look_every));
        if let Err(error) = started {
            tracing::error!(%error, "can't start the thread that looks for the input helper");
        }
        relay
    }

    fn link(&self) -> Option<Arc<Pipe>> {
        lock(&self.link).clone()
    }

    fn capturing(&self) -> Option<Arc<Capturing<M::Hooks>>> {
        lock(&self.capturing).clone()
    }

    /// The helper, if it's the one with the hooks, which is also the one hiding the cursor.
    fn hooked_by_helper(&self) -> Option<Arc<Pipe>> {
        let by_helper = self
            .capturing()
            .is_some_and(|capturing| capturing.by_helper.load(Ordering::SeqCst));
        by_helper.then(|| self.link()).flatten()
    }

    /// Replays input from another PC, through the helper while it's there.
    pub(super) fn replay(&self, event: &InputEvent) {
        if let Some(pipe) = self.link()
            && pipe.send(&ToHelper::Inject(*event), SEND_WITHIN).is_ok()
        {
            return;
        }
        self.machine.replay(event);
    }

    pub(super) fn park(&self, at: Point) {
        *lock(&self.parked) = Some(at);
        if let Some(pipe) = self.hooked_by_helper()
            && pipe.send(&ToHelper::Park(at), SEND_WITHIN).is_ok()
        {
            return;
        }
        self.machine.park(at);
    }

    pub(super) fn restore(&self, to: Point) {
        *lock(&self.parked) = None;
        if let Some(pipe) = self.hooked_by_helper()
            && pipe.send(&ToHelper::Restore(to), SEND_WITHIN).is_ok()
        {
            return;
        }
        self.machine.restore(to);
    }

    /// Captures input, with the helper's hooks while it's there and this program's otherwise.
    /// This program's own come first, and go once the helper says it has its own in.
    pub(super) fn capture(
        self: &Arc<Self>,
        handler: InputHandler,
    ) -> Result<CaptureGuard<M>, Error> {
        let _changing = lock(&self.changing);
        let capturing = Arc::new(Capturing {
            handler: Arc::new(Mutex::new(handler)),
            by_helper: Arc::default(),
            local: Mutex::new(None),
        });
        let link = self.link();
        match self.machine.hook(local_handler(&capturing)) {
            Ok(hooks) => *lock(&capturing.local) = Some(hooks),
            Err(error) if link.is_none() => return Err(error),
            Err(error) => tracing::warn!(%error, "can't start this program's own input hooks"),
        }
        *lock(&self.capturing) = Some(capturing.clone());
        if let Some(pipe) = link {
            let _ = pipe.send(&ToHelper::Capture(true), SEND_WITHIN);
        }
        Ok(CaptureGuard {
            relay: self.clone(),
            capturing,
        })
    }

    /// Tries to reach the helper.
    fn meet(&self) -> Result<Arc<Pipe>, NoHelper> {
        let pipe = Pipe::open(&self.channel).map_err(|_| NoHelper::NotThere)?;
        let owner = pipe.owner();
        if !owner
            .as_ref()
            .is_some_and(|owner| self.owners.contains(owner))
        {
            return Err(NoHelper::SomeoneElse(owner));
        }
        pipe.send(&ToHelper::Hello { version: VERSION }, SEND_WITHIN)
            .map_err(|_| NoHelper::NotThere)?;
        match pipe.receive(Some(HELLO_WITHIN)) {
            Ok(Some(FromHelper::Hello { version: VERSION })) => Ok(Arc::new(pipe)),
            Ok(Some(FromHelper::Hello { version })) => Err(NoHelper::OtherVersion(version)),
            _ => Err(NoHelper::NotThere),
        }
    }

    /// Hands input work to the helper, until it goes.
    fn work_with(&self, pipe: &Arc<Pipe>) {
        {
            let _changing = lock(&self.changing);
            *lock(&self.link) = Some(pipe.clone());
            if self.capturing().is_some() {
                let _ = pipe.send(&ToHelper::Capture(true), SEND_WITHIN);
            }
        }
        tracing::info!(
            "the system's input helper is here, so programs running as administrator can get \
             input from other PCs, and input can go to other PCs while one of them is in front"
        );
        let ended = self.listen(pipe);
        pipe.close();
        {
            let _changing = lock(&self.changing);
            *lock(&self.link) = None;
            self.hook_here_again();
        }
        match ended {
            Err(error) => tracing::warn!(%error, "the system's input helper has gone"),
            Ok(()) => tracing::info!("the system's input helper has gone"),
        }
    }

    fn listen(&self, pipe: &Arc<Pipe>) -> io::Result<()> {
        loop {
            match pipe.receive(None)? {
                Some(FromHelper::Event { id, event }) => {
                    let swallow = self.capturing().is_some_and(|capturing| {
                        // The helper's hooks are in, which Windows asks before this program's
                        // own. Those mustn't deal with the same event a second time.
                        capturing.by_helper.store(true, Ordering::SeqCst);
                        let mut handler = lock(&capturing.handler);
                        handler(event)
                    });
                    pipe.send(&ToHelper::Verdict { id, swallow }, SEND_WITHIN)?;
                }
                Some(FromHelper::Capturing) => self.leave_hooks_to_helper(pipe),
                Some(FromHelper::Released) => self.take_hooks_back(pipe),
                Some(FromHelper::Hello { .. }) | None => {}
            }
        }
    }

    /// The helper's hooks have stopped asking, because this program took too long to answer.
    /// Its own hooks take over, and it asks for the helper's again in a while.
    fn take_hooks_back(&self, pipe: &Arc<Pipe>) {
        {
            let _changing = lock(&self.changing);
            self.hook_here_again();
        }
        tracing::warn!(
            "this program took too long to say what to do with input, so its own hooks are doing \
             that for now"
        );
        let (this, pipe, after) = (self.this.clone(), pipe.clone(), self.ask_again_after);
        let started = std::thread::Builder::new()
            .name("input-relay-again".into())
            .spawn(move || {
                std::thread::sleep(after);
                if let Some(relay) = this.upgrade() {
                    relay.ask_again(&pipe);
                }
            });
        if let Err(error) = started {
            tracing::warn!(%error, "can't ask for the helper's hooks again later");
        }
    }

    fn ask_again(&self, pipe: &Arc<Pipe>) {
        let _changing = lock(&self.changing);
        let still_there = self.link().is_some_and(|link| Arc::ptr_eq(&link, pipe));
        let hooking_here = self
            .capturing()
            .is_some_and(|capturing| !capturing.by_helper.load(Ordering::SeqCst));
        if still_there && hooking_here {
            let _ = pipe.send(&ToHelper::Capture(true), SEND_WITHIN);
        }
    }

    fn leave_hooks_to_helper(&self, pipe: &Pipe) {
        let _changing = lock(&self.changing);
        let Some(capturing) = self.capturing() else {
            return;
        };
        capturing.by_helper.store(true, Ordering::SeqCst);
        // This program's cursor-hiding window goes with its hooks.
        let local = lock(&capturing.local).take();
        drop(local);
        if let Some(at) = *lock(&self.parked) {
            let _ = pipe.send(&ToHelper::Park(at), SEND_WITHIN);
        }
    }

    /// Takes the hooks back from the helper. The cursor it was hiding is showing now.
    fn hook_here_again(&self) {
        let Some(capturing) = self.capturing() else {
            return;
        };
        capturing.by_helper.store(false, Ordering::SeqCst);
        let hooked_here = lock(&capturing.local).is_some();
        if !hooked_here {
            match self.machine.hook(local_handler(&capturing)) {
                Ok(hooks) => *lock(&capturing.local) = Some(hooks),
                Err(error) => tracing::error!(%error, "can't capture input"),
            }
        }
        if let Some(at) = *lock(&self.parked) {
            self.machine.park(at);
        }
    }
}

fn keep_in_touch<M: Machine>(relay: &Weak<Relay<M>>, look_every: Duration) {
    // Every key press and mouse movement on the computer waits for this thread's verdicts.
    // SAFETY: this only concerns this thread, and a failure leaves it as it is.
    let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) };
    let mut reported = false;
    loop {
        let Some(relay) = relay.upgrade() else {
            return;
        };
        // One that can't be used isn't worth asking again so soon.
        let wait = match relay.meet() {
            Ok(pipe) => {
                relay.work_with(&pipe);
                reported = false;
                look_every
            }
            Err(NoHelper::NotThere) => look_every,
            Err(NoHelper::SomeoneElse(owner)) => {
                if !std::mem::replace(&mut reported, true) {
                    tracing::warn!(
                        owner = owner.as_deref().unwrap_or("nobody"),
                        "something other than the system is pretending to be the input helper"
                    );
                }
                look_every * 15
            }
            Err(NoHelper::OtherVersion(version)) => {
                if !std::mem::replace(&mut reported, true) {
                    tracing::warn!(
                        version,
                        "the system's input helper is from another version of Hackerman, so \
                         it isn't used. Updating it in Settings fixes that"
                    );
                }
                look_every * 15
            }
        };
        drop(relay);
        std::thread::sleep(wait);
    }
}

/// Keeps capture going, until it's dropped.
pub(super) struct CaptureGuard<M: Machine> {
    relay: Arc<Relay<M>>,
    capturing: Arc<Capturing<M::Hooks>>,
}

impl<M: Machine> Drop for CaptureGuard<M> {
    fn drop(&mut self) {
        // Waits for an event already being dealt with, and leaves nothing to deal with any
        // that come after: one could start crossing to another PC with no hooks left to do it.
        *lock(&self.capturing.handler) = Box::new(|_| false);
        let _changing = lock(&self.relay.changing);
        {
            let mut current = lock(&self.relay.capturing);
            if current
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &self.capturing))
            {
                *current = None;
            }
        }
        if let Some(pipe) = self.relay.link() {
            let _ = pipe.send(&ToHelper::Capture(false), SEND_WITHIN);
        }
        let local = lock(&self.capturing.local).take();
        drop(local);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use hk_core::input::{KeyCode, MouseButton};
    use windows::Win32::Security::TOKEN_QUERY;
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    use super::*;

    /// Stands in for the real input work, and says what it was asked to do.
    #[derive(Default, Clone)]
    struct Pretend {
        handler: Arc<Mutex<Option<InputHandler>>>,
        hooked: Arc<AtomicUsize>,
        replayed: Arc<Mutex<Vec<InputEvent>>>,
        cursor: Arc<Mutex<Vec<(&'static str, Point)>>>,
    }

    struct PretendHooks(Arc<Mutex<Option<InputHandler>>>);

    impl Drop for PretendHooks {
        fn drop(&mut self) {
            lock(&self.0).take();
        }
    }

    impl Machine for Pretend {
        type Hooks = PretendHooks;

        fn hook(&self, handler: InputHandler) -> Result<PretendHooks, Error> {
            *lock(&self.handler) = Some(handler);
            self.hooked.fetch_add(1, Ordering::SeqCst);
            Ok(PretendHooks(self.handler.clone()))
        }

        fn replay(&self, event: &InputEvent) {
            lock(&self.replayed).push(*event);
        }

        fn park(&self, at: Point) {
            lock(&self.cursor).push(("park", at));
        }

        fn restore(&self, to: Point) {
            lock(&self.cursor).push(("restore", to));
        }

        fn unhide(&self) {
            lock(&self.cursor).push(("unhide", Point::default()));
        }
    }

    impl Pretend {
        /// Someone using the keyboard. `None` with no hooks to hear it.
        fn press(&self, code: KeyCode) -> Option<bool> {
            let mut handler = lock(&self.handler);
            let handler = handler.as_mut()?;
            Some(handler(LocalEvent::Key {
                code,
                pressed: true,
            }))
        }

        fn has_hooks(&self) -> bool {
            lock(&self.handler).is_some()
        }

        fn replayed(&self) -> Vec<InputEvent> {
            lock(&self.replayed).clone()
        }

        fn cursor(&self) -> Vec<(&'static str, Point)> {
            lock(&self.cursor).clone()
        }
    }

    const CLICK: InputEvent = InputEvent::Button {
        button: MouseButton::Left,
        pressed: true,
    };

    fn me() -> String {
        let mut token = HANDLE::default();
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) }.unwrap();
        let token = unsafe { Owned::new(token) };
        session::account_of(*token).unwrap()
    }

    fn channel(name: &str) -> String {
        format!(
            r"\\.\pipe\hackerman-relay-test-{name}-{}",
            std::process::id()
        )
    }

    impl Pipe {
        /// Opens the agent's end, once the helper is ready for another agent.
        fn open_soon(channel: &str) -> Self {
            let started = Instant::now();
            loop {
                match Pipe::open(channel) {
                    Ok(pipe) => return pipe,
                    Err(error) => assert!(
                        started.elapsed() < Duration::from_secs(10),
                        "can't open it: {error}"
                    ),
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    fn eventually(what: &str, check: impl Fn() -> bool) {
        let started = Instant::now();
        while !check() {
            assert!(started.elapsed() < Duration::from_secs(10), "never: {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// A helper running on a thread of its own, which stops when this is dropped.
    struct Helper {
        server: Arc<Server>,
        machine: Pretend,
        thread: Option<std::thread::JoinHandle<io::Result<()>>>,
    }

    impl Helper {
        fn start(channel: &str) -> Self {
            let server = Arc::new(Server::create(channel, &me()).unwrap());
            let machine = Pretend::default();
            let thread = {
                let (server, machine) = (server.clone(), machine.clone());
                std::thread::spawn(move || server.run(&machine))
            };
            Self {
                server,
                machine,
                thread: Some(thread),
            }
        }
    }

    impl Drop for Helper {
        fn drop(&mut self) {
            self.server.stop();
            if let Some(thread) = self.thread.take()
                && !std::thread::panicking()
            {
                thread.join().unwrap().unwrap();
            }
        }
    }

    fn agent(channel: &str, owners: Vec<String>) -> (Arc<Relay<Pretend>>, Pretend) {
        let machine = Pretend::default();
        let relay = Relay::start(machine.clone(), channel, owners, Duration::from_millis(50));
        (relay, machine)
    }

    /// Swallows key A, as if the cursor were on another PC, and lets everything else through.
    fn handler(seen: &Arc<AtomicUsize>) -> InputHandler {
        let seen = seen.clone();
        Box::new(move |event| {
            seen.fetch_add(1, Ordering::SeqCst);
            matches!(
                event,
                LocalEvent::Key {
                    code: KeyCode::KeyA,
                    ..
                }
            )
        })
    }

    #[test]
    fn with_no_helper_the_agent_does_it_all_itself() {
        let (relay, here) = agent(&channel("alone"), vec![me()]);
        let seen = Arc::new(AtomicUsize::new(0));
        let capture = relay.capture(handler(&seen)).unwrap();

        assert_eq!(here.press(KeyCode::KeyA), Some(true));
        assert_eq!(here.press(KeyCode::KeyB), Some(false));
        relay.replay(&CLICK);
        relay.park(Point::new(1.0, 2.0));
        relay.restore(Point::new(3.0, 4.0));

        assert_eq!(here.replayed(), [CLICK]);
        assert_eq!(
            here.cursor(),
            [
                ("park", Point::new(1.0, 2.0)),
                ("restore", Point::new(3.0, 4.0))
            ]
        );
        drop(capture);
        assert!(!here.has_hooks(), "the hooks go with the capture");
    }

    #[test]
    fn with_the_helper_there_it_does_the_input_work() {
        let channel = channel("helper");
        let helper = Helper::start(&channel);
        let (relay, here) = agent(&channel, vec![me()]);
        let seen = Arc::new(AtomicUsize::new(0));
        let capture = relay.capture(handler(&seen)).unwrap();

        eventually("the helper's hooks take over", || {
            helper.machine.has_hooks() && !here.has_hooks()
        });
        // The agent's switcher still decides.
        assert_eq!(helper.machine.press(KeyCode::KeyA), Some(true));
        assert_eq!(helper.machine.press(KeyCode::KeyB), Some(false));
        assert_eq!(seen.load(Ordering::SeqCst), 2);

        relay.replay(&CLICK);
        relay.park(Point::new(5.0, 6.0));
        eventually("input and the cursor reach the helper", || {
            helper.machine.replayed() == [CLICK] && helper.machine.cursor().len() == 1
        });
        assert_eq!(helper.machine.cursor(), [("park", Point::new(5.0, 6.0))]);
        assert!(here.replayed().is_empty() && here.cursor().is_empty());

        drop(capture);
        eventually("the helper's hooks go with the capture", || {
            !helper.machine.has_hooks()
        });
        assert!(!here.has_hooks());
    }

    #[test]
    fn a_helper_that_comes_later_takes_over_and_the_parked_cursor_stays_hidden() {
        let channel = channel("later");
        let (relay, here) = agent(&channel, vec![me()]);
        let seen = Arc::new(AtomicUsize::new(0));
        let _capture = relay.capture(handler(&seen)).unwrap();
        relay.park(Point::new(7.0, 8.0));
        assert!(here.has_hooks());

        let helper = Helper::start(&channel);
        eventually("the helper's hooks take over", || {
            helper.machine.has_hooks() && !here.has_hooks()
        });
        eventually("the helper hides the cursor", || {
            helper.machine.cursor() == [("park", Point::new(7.0, 8.0))]
        });
        assert_eq!(helper.machine.press(KeyCode::KeyA), Some(true));
    }

    #[test]
    fn when_the_helper_goes_the_agent_takes_its_hooks_back() {
        let channel = channel("goes");
        let helper = Helper::start(&channel);
        let (relay, here) = agent(&channel, vec![me()]);
        let seen = Arc::new(AtomicUsize::new(0));
        let _capture = relay.capture(handler(&seen)).unwrap();
        eventually("the helper's hooks take over", || !here.has_hooks());
        relay.park(Point::new(9.0, 9.0));
        eventually("the helper hides the cursor", || {
            !helper.machine.cursor().is_empty()
        });

        drop(helper);
        eventually("the agent's own hooks come back", || here.has_hooks());
        assert_eq!(here.press(KeyCode::KeyA), Some(true));
        // The helper's cursor-hiding window went with it.
        assert_eq!(here.cursor(), [("park", Point::new(9.0, 9.0))]);
        relay.replay(&CLICK);
        assert_eq!(here.replayed(), [CLICK]);
    }

    /// Every input event on the computer waits for the agent, so it can't take its time.
    #[test]
    fn an_agent_that_takes_too_long_gets_its_hooks_back_and_loses_nothing_it_sent() {
        let channel = channel("slow");
        let helper = Helper::start(&channel);
        let (relay, here) = agent(&channel, vec![me()]);
        let slow = Arc::new(AtomicBool::new(true));
        let _capture = {
            let slow = slow.clone();
            relay
                .capture(Box::new(move |_| {
                    if slow.load(Ordering::SeqCst) {
                        std::thread::sleep(ANSWER_WITHIN * 3);
                    }
                    true
                }))
                .unwrap()
        };
        eventually("the helper's hooks take over", || !here.has_hooks());
        relay.park(Point::new(4.0, 4.0));

        let started = Instant::now();
        assert_eq!(
            helper.machine.press(KeyCode::KeyA),
            Some(false),
            "let through"
        );
        // Well within Windows' own limit, which is about a second at most.
        assert!(
            started.elapsed() < Duration::from_millis(600),
            "{:?}",
            started.elapsed()
        );
        eventually("the agent's own hooks come back", || here.has_hooks());
        assert_eq!(
            here.cursor(),
            [("park", Point::new(4.0, 4.0))],
            "hidden by the agent now"
        );
        assert_eq!(
            helper.machine.cursor(),
            [("park", Point::new(4.0, 4.0)), ("unhide", Point::default())]
        );
        let started = Instant::now();
        assert_eq!(helper.machine.press(KeyCode::KeyB), Some(false));
        assert!(started.elapsed() < ANSWER_WITHIN, "the helper stops asking");

        // Still connected, so a key let go of or a click doesn't go missing.
        relay.replay(&CLICK);
        eventually("the click arrives", || helper.machine.replayed() == [CLICK]);
        assert!(here.replayed().is_empty());

        // It's welcome back once it keeps up.
        slow.store(false, Ordering::SeqCst);
        eventually("the helper's hooks take over again", || {
            helper.machine.has_hooks() && !here.has_hooks()
        });
        assert_eq!(helper.machine.press(KeyCode::KeyA), Some(true));
    }

    #[test]
    fn a_pipe_that_isnt_the_systems_is_never_used() {
        let channel = channel("pretender");
        let helper = Helper::start(&channel);
        // The real agent only accepts the system's: this helper is the test's own account.
        let (relay, here) = agent(&channel, HELPER_ACCOUNTS.map(String::from).to_vec());
        let seen = Arc::new(AtomicUsize::new(0));
        let _capture = relay.capture(handler(&seen)).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        relay.replay(&CLICK);
        assert!(here.has_hooks());
        assert!(!helper.machine.has_hooks());
        assert_eq!(here.replayed(), [CLICK]);
        assert!(helper.machine.replayed().is_empty());
    }

    #[test]
    fn an_agent_from_another_version_is_turned_away() {
        let channel = channel("version");
        let helper = Helper::start(&channel);
        let pipe = Pipe::open(&channel).unwrap();
        pipe.send(
            &ToHelper::Hello {
                version: VERSION + 1,
            },
            SEND_WITHIN,
        )
        .unwrap();
        // Told which version the helper is, so it can say so.
        assert_eq!(
            pipe.receive(Some(HELLO_WITHIN)).unwrap(),
            Some(FromHelper::Hello { version: VERSION })
        );
        pipe.send(&ToHelper::Capture(true), SEND_WITHIN).unwrap();
        pipe.send(&ToHelper::Inject(CLICK), SEND_WITHIN).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(!helper.machine.has_hooks());
        assert!(helper.machine.replayed().is_empty());
        drop(pipe);

        // The next one can come in.
        let pipe = Pipe::open_soon(&channel);
        pipe.send(&ToHelper::Hello { version: VERSION }, SEND_WITHIN)
            .unwrap();
        assert_eq!(
            pipe.receive(Some(HELLO_WITHIN)).unwrap(),
            Some(FromHelper::Hello { version: VERSION })
        );
    }

    /// A program of the user's can't add a pipe of its own under the helper's name, to have the
    /// next agent that comes along talk to it instead.
    #[test]
    fn nothing_else_can_wait_for_agents_under_the_helpers_name() {
        use windows::Win32::System::Pipes::PIPE_UNLIMITED_INSTANCES;

        let channel = channel("squat");
        let _helper = Helper::start(&channel);
        let another = unsafe {
            CreateNamedPipeW(
                &HSTRING::from(channel.as_str()),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                BUFFER,
                BUFFER,
                0,
                None,
            )
        };
        let refused = another.is_invalid();
        if !refused {
            unsafe { CloseHandle(another) }.unwrap();
        }
        assert!(refused, "{}", io::Error::last_os_error());
        assert!(Pipe::open(&channel).is_ok(), "the agent still gets in");
    }

    #[test]
    fn only_one_helper_can_have_the_name() {
        let channel = channel("one");
        let _helper = Helper::start(&channel);
        assert!(Server::create(&channel, &me()).is_err());
    }

    #[test]
    fn a_message_this_version_doesnt_know_is_skipped() {
        let channel = channel("unknown");
        let helper = Helper::start(&channel);
        let pipe = Pipe::open(&channel).unwrap();
        pipe.send(&ToHelper::Hello { version: VERSION }, SEND_WITHIN)
            .unwrap();
        pipe.receive::<FromHelper>(Some(HELLO_WITHIN)).unwrap();
        // As if a newer agent had added a kind of message at the end.
        #[derive(Serialize)]
        enum Newer {
            _A,
            _B,
            _C,
            _D,
            _E,
            _F,
            Unknown(u32),
        }
        pipe.send(&Newer::Unknown(7), SEND_WITHIN).unwrap();
        pipe.send(&ToHelper::Inject(CLICK), SEND_WITHIN).unwrap();
        eventually("what comes after is still done", || {
            helper.machine.replayed() == [CLICK]
        });
    }
}
