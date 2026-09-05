//! The local channel: a named pipe on Windows, a Unix socket elsewhere.

use std::io;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, Lines};

type Reader = Box<dyn AsyncRead + Send + Unpin>;
type Writer = Box<dyn AsyncWrite + Send + Unpin>;

/// One side of an open channel. Each message is one line of JSON.
pub struct IpcConnection {
    lines: Lines<BufReader<Reader>>,
    writer: Writer,
}

impl IpcConnection {
    fn new(stream: impl AsyncRead + AsyncWrite + Send + 'static) -> Self {
        let (reader, writer) = tokio::io::split(stream);
        Self {
            lines: BufReader::new(Box::new(reader) as Reader).lines(),
            writer: Box::new(writer),
        }
    }

    pub async fn send(&mut self, message: &impl Serialize) -> io::Result<()> {
        let mut line = serde_json::to_vec(message)?;
        line.push(b'\n');
        self.writer.write_all(&line).await?;
        self.writer.flush().await
    }

    /// The next message, or `None` once the other side has gone. Safe to use in
    /// `tokio::select!`: a partly received line is kept for the next call.
    pub async fn recv<T: DeserializeOwned>(&mut self) -> io::Result<Option<T>> {
        match self.lines.next_line().await? {
            Some(line) => Ok(Some(serde_json::from_str(&line)?)),
            None => Ok(None),
        }
    }
}

#[cfg(windows)]
pub use self::windows::{IpcListener, connect, default_channel_name};

#[cfg(unix)]
pub use self::unix::{IpcListener, connect, default_channel_name};

#[cfg(windows)]
mod windows {
    use std::io;

    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
    use windows::Win32::Foundation::{ERROR_PIPE_BUSY, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows::core::w;

    use super::IpcConnection;

    /// A pipe name for this user, so different users on one computer don't collide.
    pub fn default_channel_name() -> String {
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "default".into());
        format!(r"\\.\pipe\hackerman-{user}")
    }

    pub async fn connect(name: &str) -> io::Result<IpcConnection> {
        // All instances can be briefly busy while the listener prepares the next one.
        for _ in 0..20 {
            match ClientOptions::new().open(name) {
                Ok(pipe) => return Ok(IpcConnection::new(pipe)),
                Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(error) => return Err(error),
            }
        }
        Err(io::ErrorKind::TimedOut.into())
    }

    /// Windows' default lets any user read a pipe. This allows only the owner
    /// (the user who started the agent) and the operating system.
    struct OwnerOnly(PSECURITY_DESCRIPTOR);

    impl OwnerOnly {
        fn new() -> io::Result<Self> {
            let mut descriptor = PSECURITY_DESCRIPTOR::default();
            // SAFETY: the out parameter points to a valid location, and Drop
            // frees what Windows allocates there.
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    w!("D:P(A;;GA;;;OW)(A;;GA;;;SY)"),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    None,
                )
            }?;
            Ok(Self(descriptor))
        }

        fn create(&self, name: &str, first: bool) -> io::Result<NamedPipeServer> {
            let mut attributes = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: self.0.0,
                bInheritHandle: false.into(),
            };
            // SAFETY: the attributes and the descriptor they point to outlive
            // this call, which copies them.
            unsafe {
                ServerOptions::new()
                    .first_pipe_instance(first)
                    .reject_remote_clients(true)
                    .create_with_security_attributes_raw(name, (&raw mut attributes).cast())
            }
        }
    }

    impl Drop for OwnerOnly {
        fn drop(&mut self) {
            // SAFETY: the descriptor was allocated by Windows with LocalAlloc,
            // and nothing refers to it after this.
            unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
        }
    }

    // SAFETY: the descriptor is only read after creation, and Windows' security
    // descriptors aren't tied to the thread that made them.
    unsafe impl Send for OwnerOnly {}

    /// Waits for the app to connect.
    pub struct IpcListener {
        name: String,
        security: OwnerOnly,
        next: NamedPipeServer,
    }

    impl IpcListener {
        /// Fails if another agent already has this name, which is what keeps
        /// two agents from running at once.
        pub fn bind(name: &str) -> io::Result<Self> {
            let security = OwnerOnly::new()?;
            let next = security.create(name, true)?;
            Ok(Self {
                name: name.to_string(),
                security,
                next,
            })
        }

        pub async fn accept(&mut self) -> io::Result<IpcConnection> {
            self.next.connect().await?;
            let replacement = self.security.create(&self.name, false)?;
            let connected = std::mem::replace(&mut self.next, replacement);
            Ok(IpcConnection::new(connected))
        }
    }
}

#[cfg(unix)]
mod unix {
    use std::io;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use tokio::net::{UnixListener, UnixStream};

    use super::IpcConnection;

    /// A socket path private to this user.
    pub fn default_channel_name() -> String {
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .or_else(|| std::env::var_os("HOME"))
            .map_or_else(std::env::temp_dir, PathBuf::from);
        dir.join(".hackerman.sock").to_string_lossy().into_owned()
    }

    pub async fn connect(name: &str) -> io::Result<IpcConnection> {
        Ok(IpcConnection::new(UnixStream::connect(name).await?))
    }

    pub struct IpcListener(UnixListener);

    impl IpcListener {
        /// Fails if another agent is already listening on this name.
        pub fn bind(name: &str) -> io::Result<Self> {
            if std::os::unix::net::UnixStream::connect(name).is_ok() {
                return Err(io::ErrorKind::AddrInUse.into());
            }
            // Left behind by an agent that didn't get to clean up.
            let _ = std::fs::remove_file(name);
            let listener = UnixListener::bind(name)?;
            std::fs::set_permissions(name, std::fs::Permissions::from_mode(0o600))?;
            Ok(Self(listener))
        }

        pub async fn accept(&mut self) -> io::Result<IpcConnection> {
            let (stream, _) = self.0.accept().await?;
            Ok(IpcConnection::new(stream))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Request;

    fn unique_name(test: &str) -> String {
        let id = std::process::id();
        if cfg!(windows) {
            format!(r"\\.\pipe\hackerman-test-{test}-{id}")
        } else {
            std::env::temp_dir()
                .join(format!("hackerman-test-{test}-{id}.sock"))
                .to_string_lossy()
                .into_owned()
        }
    }

    #[tokio::test]
    async fn messages_travel_both_ways() {
        let name = unique_name("travel");
        let mut listener = IpcListener::bind(&name).unwrap();
        let client = async {
            let mut connection = connect(&name).await.unwrap();
            connection.send(&Request::Shutdown).await.unwrap();
            connection.recv::<String>().await.unwrap()
        };
        let server = async {
            let mut connection = listener.accept().await.unwrap();
            let request: Option<Request> = connection.recv().await.unwrap();
            assert_eq!(request, Some(Request::Shutdown));
            connection.send(&"bye".to_string()).await.unwrap();
            connection
        };
        let (reply, _keep_open) = tokio::join!(client, server);
        assert_eq!(reply.as_deref(), Some("bye"));
    }

    #[tokio::test]
    async fn a_second_agent_cannot_take_the_same_name() {
        let name = unique_name("exclusive");
        let _first = IpcListener::bind(&name).unwrap();
        assert!(IpcListener::bind(&name).is_err());
    }

    #[tokio::test]
    async fn several_apps_can_connect_in_turn() {
        let name = unique_name("several");
        let mut listener = IpcListener::bind(&name).unwrap();
        for _ in 0..3 {
            let (client, server) = tokio::join!(connect(&name), listener.accept());
            drop(client.unwrap());
            drop(server.unwrap());
        }
    }

    #[tokio::test]
    async fn connecting_with_no_agent_fails() {
        assert!(connect(&unique_name("nobody")).await.is_err());
    }
}
