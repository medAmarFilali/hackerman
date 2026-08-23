use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hk_core::device::Fingerprint;
use hk_proto::Message;
use quinn::crypto::rustls::{HandshakeData, QuicClientConfig, QuicServerConfig};
use rustls::pki_types::CertificateDer;
use tokio::task::JoinSet;

use crate::pairing::PairingChannel;
use crate::tls::{self, LINK_PROTOCOL, PAIRING_PROTOCOL, TrustStore};
use crate::{Error, Identity, fingerprint_of};

/// Any single message larger than this is treated as a protocol violation.
const MAX_MESSAGE_LEN: usize = 256 * 1024;

/// The largest piece of data that can go over a [`BlobChannel`].
pub const MAX_BLOB_LEN: usize = 256 * 1024 * 1024;

/// Below the default of 0 used by the stream carrying input.
const BLOB_PRIORITY: i32 = -1;

/// How much of one stream can be on its way at once, which limits its speed to this
/// much per round trip. 8 MB is 400 MB/s even on Wi-Fi with 20 ms round trips.
const STREAM_WINDOW: u32 = 8 * 1024 * 1024;

/// Told to the other side when a file can't be read to its end, so that a stream cut
/// short isn't mistaken for a file that really is that short.
const ABORTED: u32 = 1;

/// Certificates are checked by fingerprint, not by name, so this is arbitrary.
const SERVER_NAME: &str = "hackerman";

/// QUIC connection close code for a device that isn't paired.
const CLOSE_NOT_PAIRED: u32 = 1;

/// A local QUIC endpoint that can both accept and make connections. Clones
/// share the same socket.
#[derive(Clone)]
pub struct Endpoint {
    endpoint: quinn::Endpoint,
    me: Fingerprint,
    trust: TrustStore,
    link_client: quinn::ClientConfig,
    pairing_client: quinn::ClientConfig,
}

impl Endpoint {
    /// Listens on `listen`. Only devices in `trust` can open a normal link,
    /// and changes to it apply straight away.
    pub fn bind(identity: &Identity, listen: SocketAddr, trust: TrustStore) -> Result<Self, Error> {
        let transport = Arc::new(transport_config());

        let server_crypto = QuicServerConfig::try_from(tls::server_config(identity)?)?;
        let mut server = quinn::ServerConfig::with_crypto(Arc::new(server_crypto));
        server.transport_config(transport.clone());

        let client = |crypto: rustls::ClientConfig| -> Result<quinn::ClientConfig, Error> {
            let crypto = QuicClientConfig::try_from(crypto)?;
            let mut client = quinn::ClientConfig::new(Arc::new(crypto));
            client.transport_config(transport.clone());
            Ok(client)
        };
        let link_client = client(tls::link_client_config(identity, trust.clone())?)?;
        let pairing_client = client(tls::pairing_client_config(identity)?)?;

        Ok(Self {
            endpoint: quinn::Endpoint::server(server, listen)?,
            me: identity.fingerprint(),
            trust,
            link_client,
            pairing_client,
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr, Error> {
        Ok(self.endpoint.local_addr()?)
    }

    /// Closes every connection, telling the other side so it notices at once
    /// rather than waiting for a timeout.
    pub async fn shutdown(&self) {
        self.endpoint.close(0u32.into(), b"shutting down");
        let _ = tokio::time::timeout(Duration::from_millis(500), self.endpoint.wait_idle()).await;
    }

    /// Connects to `expected` at `address`.
    ///
    /// The connecting side must send the first message: the stream only
    /// becomes visible to the other side once data arrives on it.
    pub async fn connect(&self, address: SocketAddr, expected: Fingerprint) -> Result<Link, Error> {
        let connection = self
            .endpoint
            .connect_with(self.link_client.clone(), address, SERVER_NAME)?
            .await?;
        let peer = peer_fingerprint(&connection)?;
        if peer != expected {
            return Err(Error::WrongDevice { found: peer });
        }
        let (send, recv) = connection.open_bi().await?;
        Ok(Link::new(connection, send, recv, peer))
    }

    /// Tries all of `addresses` at once and returns the first that works. A
    /// computer often has several, and some belong to virtual network adapters
    /// that lead nowhere.
    pub async fn connect_any(
        &self,
        addresses: &[SocketAddr],
        expected: Fingerprint,
    ) -> Result<Link, Error> {
        let mut attempts = JoinSet::new();
        for &address in addresses {
            let endpoint = self.clone();
            attempts.spawn(async move { endpoint.connect(address, expected).await });
        }
        let mut last_error = Error::NoAddress;
        while let Some(attempt) = attempts.join_next().await {
            match attempt {
                Ok(Ok(link)) => return Ok(link),
                Ok(Err(error)) => last_error = error,
                Err(_) => {} // The attempt's task panicked or was cancelled.
            }
        }
        Err(last_error)
    }

    /// Connects to whichever device is at `address` in order to pair with it.
    pub async fn connect_for_pairing(&self, address: SocketAddr) -> Result<PairingChannel, Error> {
        let connection = self
            .endpoint
            .connect_with(self.pairing_client.clone(), address, SERVER_NAME)?
            .await?;
        let peer = peer_fingerprint(&connection)?;
        let (send, recv) = connection.open_bi().await?;
        Ok(PairingChannel::new(connection, send, recv, self.me, peer))
    }

    /// Like [`Self::connect_any`], but in order to pair.
    pub async fn connect_any_for_pairing(
        &self,
        addresses: &[SocketAddr],
    ) -> Result<PairingChannel, Error> {
        let mut attempts = JoinSet::new();
        for &address in addresses {
            let endpoint = self.clone();
            attempts.spawn(async move { endpoint.connect_for_pairing(address).await });
        }
        let mut last_error = Error::NoAddress;
        while let Some(attempt) = attempts.join_next().await {
            match attempt {
                Ok(Ok(channel)) => return Ok(channel),
                Ok(Err(error)) => last_error = error,
                Err(_) => {} // The attempt's task panicked or was cancelled.
            }
        }
        Err(last_error)
    }

    /// Waits for the next device to connect. Returns `None` once the endpoint
    /// is closed.
    pub async fn next_incoming(&self) -> Option<IncomingConnection> {
        Some(IncomingConnection {
            incoming: self.endpoint.accept().await?,
            me: self.me,
            trust: self.trust.clone(),
        })
    }
}

/// A connection attempt that hasn't been checked yet.
pub struct IncomingConnection {
    incoming: quinn::Incoming,
    me: Fingerprint,
    trust: TrustStore,
}

/// What an incoming connection turned out to be.
pub enum Accepted {
    Link(Link),
    Pairing(PairingChannel),
}

impl IncomingConnection {
    /// Completes the handshake and checks who is connecting and why. Run this
    /// in its own task: it waits on the remote device.
    pub async fn establish(self) -> Result<Accepted, Error> {
        let connection = self.incoming.await?;
        let peer = peer_fingerprint(&connection)?;
        let protocol = connection
            .handshake_data()
            .and_then(|data| data.downcast::<HandshakeData>().ok())
            .and_then(|data| data.protocol);

        match protocol.as_deref() {
            Some(LINK_PROTOCOL) => {
                if !self.trust.contains(peer) {
                    connection.close(CLOSE_NOT_PAIRED.into(), b"not paired");
                    return Err(Error::NotPaired(peer));
                }
                let (send, recv) = connection.accept_bi().await?;
                Ok(Accepted::Link(Link::new(connection, send, recv, peer)))
            }
            Some(PAIRING_PROTOCOL) => {
                let (send, recv) = connection.accept_bi().await?;
                Ok(Accepted::Pairing(PairingChannel::new(
                    connection, send, recv, self.me, peer,
                )))
            }
            _ => Err(Error::UnknownProtocol),
        }
    }
}

fn transport_config() -> quinn::TransportConfig {
    let mut transport = quinn::TransportConfig::default();
    // Frequent keep-alives with a short idle timeout make a dead link show up
    // within a second, so a controller never keeps swallowing input for a
    // peer that is gone.
    transport.keep_alive_interval(Some(Duration::from_millis(250)));
    transport.max_idle_timeout(Some(quinn::VarInt::from_u32(1_000).into()));
    transport.stream_receive_window(quinn::VarInt::from_u32(STREAM_WINDOW));
    transport.send_window(u64::from(STREAM_WINDOW) * 4);
    transport
}

/// An authenticated, encrypted message channel to one peer.
pub struct Link {
    peer: Fingerprint,
    remote_address: SocketAddr,
    blobs: BlobChannel,
    sender: LinkSender,
    receiver: LinkReceiver,
}

impl Link {
    fn new(
        connection: quinn::Connection,
        send: quinn::SendStream,
        recv: quinn::RecvStream,
        peer: Fingerprint,
    ) -> Self {
        Self {
            peer,
            remote_address: connection.remote_address(),
            blobs: BlobChannel {
                connection: connection.clone(),
            },
            sender: LinkSender {
                stream: send,
                _connection: connection,
            },
            receiver: LinkReceiver { stream: recv },
        }
    }

    pub fn peer(&self) -> Fingerprint {
        self.peer
    }

    pub fn remote_address(&self) -> SocketAddr {
        self.remote_address
    }

    /// For large data going to or from the peer. See [`BlobChannel`].
    pub fn blobs(&self) -> BlobChannel {
        self.blobs.clone()
    }

    /// Splits the link so sending and receiving can happen independently.
    pub fn split(self) -> (LinkSender, LinkReceiver) {
        (self.sender, self.receiver)
    }
}

/// Sends large pieces of data, each on a QUIC stream of its own and at low
/// priority. Input messages keep overtaking them, so copying a huge image doesn't
/// make the mouse lag.
#[derive(Clone)]
pub struct BlobChannel {
    connection: quinn::Connection,
}

impl BlobChannel {
    /// Returns once the other side has received all of `data`.
    pub async fn send(&self, data: &[u8]) -> Result<(), Error> {
        let mut stream = self.connection.open_uni().await?;
        stream.set_priority(BLOB_PRIORITY).map_err(blob_error)?;
        stream.write_all(data).await?;
        stream.finish().map_err(blob_error)?;
        stream.stopped().await.map_err(blob_error)?;
        Ok(())
    }

    /// The next piece of data from the other side. An error for one piece doesn't
    /// mean the connection has gone: that shows up on the main channel.
    pub async fn receive(&self) -> Result<Vec<u8>, Error> {
        let mut stream = self.connection.accept_uni().await?;
        stream.read_to_end(MAX_BLOB_LEN).await.map_err(blob_error)
    }

    /// Asks the other side for some data of any size, which arrives at low priority
    /// as it's read. Dropping the [`DataReader`] tells the other side to stop.
    ///
    /// Only for a peer that said it can answer: an older one never looks at this.
    pub async fn request(&self, request: &[u8]) -> Result<DataReader, Error> {
        let (mut send, recv) = self.connection.open_bi().await?;
        write_frame(&mut send, request).await?;
        send.finish().map_err(blob_error)?;
        Ok(DataReader { stream: recv })
    }

    /// Waits for the other side's next [`Self::request`]. An error means the connection
    /// has gone. Run [`IncomingRequest::read`] on its own task, as it waits on the peer.
    pub async fn next_request(&self) -> Result<IncomingRequest, Error> {
        let (send, recv) = self.connection.accept_bi().await?;
        Ok(IncomingRequest { send, recv })
    }
}

/// A request for data that hasn't been read yet.
pub struct IncomingRequest {
    send: quinn::SendStream,
    recv: quinn::RecvStream,
}

impl IncomingRequest {
    /// What's being asked for, and where to send it.
    pub async fn read(mut self) -> Result<(Vec<u8>, DataWriter), Error> {
        let request = read_frame(&mut self.recv).await?;
        self.send.set_priority(BLOB_PRIORITY).map_err(blob_error)?;
        Ok((request, DataWriter { stream: self.send }))
    }
}

/// Where the answer to a request goes.
pub struct DataWriter {
    stream: quinn::SendStream,
}

impl DataWriter {
    /// A short message ahead of the data, for [`DataReader::read_message`].
    pub async fn write_message(&mut self, message: &[u8]) -> Result<(), Error> {
        write_frame(&mut self.stream, message).await
    }

    /// Waits while the other side is behind. Fails if it has stopped wanting this.
    pub async fn write(&mut self, data: &[u8]) -> Result<(), Error> {
        self.stream.write_all(data).await.map_err(blob_error)
    }

    /// Everything has been written.
    pub fn finish(mut self) -> Result<(), Error> {
        self.stream.finish().map_err(blob_error)
    }

    /// Ends this in a way the other side sees as a failure.
    pub fn abort(mut self) {
        let _ = self.stream.reset(ABORTED.into());
    }
}

/// The answer to a request, as it arrives.
pub struct DataReader {
    stream: quinn::RecvStream,
}

impl DataReader {
    /// The short message from [`DataWriter::write_message`].
    pub async fn read_message(&mut self) -> Result<Vec<u8>, Error> {
        read_frame(&mut self.stream).await
    }

    /// Reads as much as has arrived, up to the size of `buffer`. Returns how much that
    /// was, which is zero only at the end. An error means it was cut short.
    pub async fn read(&mut self, buffer: &mut [u8]) -> Result<usize, Error> {
        match self.stream.read(buffer).await {
            Ok(Some(count)) => Ok(count),
            Ok(None) => Ok(0),
            Err(error) => Err(blob_error(error)),
        }
    }
}

fn blob_error(error: impl std::fmt::Display) -> Error {
    Error::Blob(error.to_string())
}

pub struct LinkSender {
    stream: quinn::SendStream,
    // Keeps the connection open for as long as the link is in use.
    _connection: quinn::Connection,
}

impl LinkSender {
    pub async fn send(&mut self, message: &Message) -> Result<(), Error> {
        write_frame(&mut self.stream, &hk_proto::encode(message)?).await
    }
}

pub struct LinkReceiver {
    stream: quinn::RecvStream,
}

impl LinkReceiver {
    /// The next message. Not safe to cancel: dropping this partway through a
    /// message loses the part already read, and the stream can't recover.
    ///
    /// One this version can't read is skipped: it may well be from a newer version,
    /// and the frame around it says where the next one starts.
    pub async fn recv(&mut self) -> Result<Message, Error> {
        loop {
            let frame = read_frame(&mut self.stream).await?;
            match hk_proto::decode(&frame) {
                Ok(message) => return Ok(message),
                Err(error) => tracing::debug!(%error, "skipped a message this version can't read"),
            }
        }
    }
}

/// Writes `body` preceded by its length, so the receiver knows where it ends.
pub(crate) async fn write_frame(stream: &mut quinn::SendStream, body: &[u8]) -> Result<(), Error> {
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(body);
    stream.write_all(&frame).await?;
    Ok(())
}

pub(crate) async fn read_frame(stream: &mut quinn::RecvStream) -> Result<Vec<u8>, Error> {
    let mut length = [0; 4];
    stream.read_exact(&mut length).await?;
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_MESSAGE_LEN {
        return Err(Error::MessageTooLarge(length));
    }
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await?;
    Ok(body)
}

fn peer_fingerprint(connection: &quinn::Connection) -> Result<Fingerprint, Error> {
    let certificates = connection
        .peer_identity()
        .and_then(|identity| identity.downcast::<Vec<CertificateDer<'static>>>().ok())
        .ok_or(Error::NoPeerCertificate)?;
    let certificate = certificates.first().ok_or(Error::NoPeerCertificate)?;
    Ok(fingerprint_of(certificate))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loopback() -> SocketAddr {
        "127.0.0.1:0".parse().unwrap()
    }

    fn accepted_link(accepted: Accepted) -> Link {
        match accepted {
            Accepted::Link(link) => link,
            Accepted::Pairing(_) => panic!("expected a normal link"),
        }
    }

    #[tokio::test]
    async fn paired_devices_exchange_messages() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let server = Endpoint::bind(&a, loopback(), TrustStore::new([b.fingerprint()])).unwrap();
        let client = Endpoint::bind(&b, loopback(), TrustStore::new([a.fingerprint()])).unwrap();
        let address = server.local_addr().unwrap();

        let server_side = async {
            let incoming = server.next_incoming().await.unwrap();
            let link = accepted_link(incoming.establish().await.unwrap());
            assert_eq!(link.peer(), b.fingerprint());
            let (mut tx, mut rx) = link.split();
            assert_eq!(rx.recv().await.unwrap(), Message::Enter);
            tx.send(&Message::Leave).await.unwrap();
            // Returned so the link stays open until the client has read the reply.
            (tx, rx)
        };
        let client_side = async {
            let link = client.connect(address, a.fingerprint()).await.unwrap();
            assert_eq!(link.peer(), a.fingerprint());
            let (mut tx, mut rx) = link.split();
            tx.send(&Message::Enter).await.unwrap();
            assert_eq!(rx.recv().await.unwrap(), Message::Leave);
            (tx, rx)
        };
        tokio::join!(server_side, client_side);
    }

    /// A blob far larger than any message, sent while messages keep flowing.
    #[tokio::test]
    async fn large_data_travels_alongside_messages() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let server = Endpoint::bind(&a, loopback(), TrustStore::new([b.fingerprint()])).unwrap();
        let client = Endpoint::bind(&b, loopback(), TrustStore::new([a.fingerprint()])).unwrap();
        let address = server.local_addr().unwrap();
        let blob: Vec<u8> = (0..8_000_000u32).map(|i| (i % 251) as u8).collect();

        let server_side = async {
            let incoming = server.next_incoming().await.unwrap();
            let link = accepted_link(incoming.establish().await.unwrap());
            let blobs = link.blobs();
            let (tx, mut rx) = link.split();
            let messages = async {
                for _ in 0..100 {
                    assert_eq!(rx.recv().await.unwrap(), Message::Enter);
                }
            };
            let (received, ()) = tokio::join!(blobs.receive(), messages);
            (received.unwrap(), tx, rx)
        };
        let client_side = async {
            let link = client.connect(address, a.fingerprint()).await.unwrap();
            let blobs = link.blobs();
            let (mut tx, rx) = link.split();
            // The stream only appears to the other side once something is sent on it.
            tx.send(&Message::Enter).await.unwrap();
            let messages = async {
                for _ in 1..100 {
                    tx.send(&Message::Enter).await.unwrap();
                    tokio::task::yield_now().await;
                }
            };
            let (sent, ()) = tokio::join!(blobs.send(&blob), messages);
            sent.unwrap();
            (tx, rx)
        };
        let ((received, _a, _b), _keep_open) = tokio::join!(server_side, client_side);
        assert_eq!(received.len(), blob.len());
        assert_eq!(received, blob);
    }

    /// A newer peer might send anything, and the link has to survive it.
    #[tokio::test]
    async fn a_message_this_version_cannot_read_is_skipped() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let server = Endpoint::bind(&a, loopback(), TrustStore::new([b.fingerprint()])).unwrap();
        let client = Endpoint::bind(&b, loopback(), TrustStore::new([a.fingerprint()])).unwrap();
        let address = server.local_addr().unwrap();

        let server_side = async {
            let incoming = server.next_incoming().await.unwrap();
            let link = accepted_link(incoming.establish().await.unwrap());
            let (tx, mut rx) = link.split();
            assert_eq!(rx.recv().await.unwrap(), Message::Enter);
            assert_eq!(rx.recv().await.unwrap(), Message::Leave);
            (tx, rx)
        };
        let client_side = async {
            let link = client.connect(address, a.fingerprint()).await.unwrap();
            let (mut tx, rx) = link.split();
            tx.send(&Message::Enter).await.unwrap();
            // A variant far past any that exist.
            write_frame(&mut tx.stream, &[200, 1, 2, 3]).await.unwrap();
            tx.send(&Message::Leave).await.unwrap();
            (tx, rx)
        };
        tokio::join!(server_side, client_side);
    }

    /// Far larger than a clipboard's limit, in pieces, with input still getting through.
    #[tokio::test]
    async fn requested_data_of_any_size_arrives_alongside_messages() {
        const SIZE: usize = 300 * 1024 * 1024;
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let server = Endpoint::bind(&a, loopback(), TrustStore::new([b.fingerprint()])).unwrap();
        let client = Endpoint::bind(&b, loopback(), TrustStore::new([a.fingerprint()])).unwrap();
        let address = server.local_addr().unwrap();

        let server_side = async {
            let incoming = server.next_incoming().await.unwrap();
            let link = accepted_link(incoming.establish().await.unwrap());
            let blobs = link.blobs();
            let (tx, mut rx) = link.split();
            let answer = async {
                let request = blobs.next_request().await.unwrap();
                let (asked, mut writer) = request.read().await.unwrap();
                assert_eq!(asked, b"the big one");
                writer.write_message(b"coming").await.unwrap();
                let piece = vec![7; 1024 * 1024];
                for _ in 0..SIZE / piece.len() {
                    writer.write(&piece).await.unwrap();
                }
                writer.finish().unwrap();
            };
            let messages = async {
                assert_eq!(rx.recv().await.unwrap(), Message::Enter);
                for _ in 0..200 {
                    assert_eq!(rx.recv().await.unwrap(), Message::Leave);
                }
            };
            tokio::join!(answer, messages);
            (tx, rx)
        };
        let client_side = async {
            let link = client.connect(address, a.fingerprint()).await.unwrap();
            let blobs = link.blobs();
            let (mut tx, rx) = link.split();
            tx.send(&Message::Enter).await.unwrap();
            let mut received = 0;
            let read = async {
                let mut reader = blobs.request(b"the big one").await.unwrap();
                assert_eq!(reader.read_message().await.unwrap(), b"coming");
                let mut buffer = vec![0; 256 * 1024];
                loop {
                    let count = reader.read(&mut buffer).await.unwrap();
                    if count == 0 {
                        break;
                    }
                    assert!(buffer[..count].iter().all(|&byte| byte == 7));
                    received += count;
                }
            };
            let messages = async {
                for _ in 0..200 {
                    tx.send(&Message::Leave).await.unwrap();
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            };
            tokio::join!(read, messages);
            assert_eq!(received, SIZE);
            (tx, rx)
        };
        tokio::join!(server_side, client_side);
    }

    #[tokio::test]
    async fn stopping_a_read_tells_the_other_side_and_failure_is_not_the_end() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let server = Endpoint::bind(&a, loopback(), TrustStore::new([b.fingerprint()])).unwrap();
        let client = Endpoint::bind(&b, loopback(), TrustStore::new([a.fingerprint()])).unwrap();
        let address = server.local_addr().unwrap();

        let server_side = async {
            let incoming = server.next_incoming().await.unwrap();
            let link = accepted_link(incoming.establish().await.unwrap());
            let blobs = link.blobs();
            let (tx, mut rx) = link.split();
            assert_eq!(rx.recv().await.unwrap(), Message::Enter);

            // The reader walks away, and writing notices rather than going on forever.
            let (_, mut writer) = blobs.next_request().await.unwrap().read().await.unwrap();
            let piece = vec![1; 1024 * 1024];
            let mut stopped = false;
            for _ in 0..1000 {
                if writer.write(&piece).await.is_err() {
                    stopped = true;
                    break;
                }
            }
            assert!(stopped);

            // A failure partway is reported as one.
            let (_, mut writer) = blobs.next_request().await.unwrap().read().await.unwrap();
            writer.write(b"part of it").await.unwrap();
            writer.abort();
            assert_eq!(rx.recv().await.unwrap(), Message::Leave);
            (tx, rx)
        };
        let client_side = async {
            let link = client.connect(address, a.fingerprint()).await.unwrap();
            let blobs = link.blobs();
            let (mut tx, rx) = link.split();
            tx.send(&Message::Enter).await.unwrap();

            let mut reader = blobs.request(b"walk away").await.unwrap();
            let mut buffer = [0; 16];
            reader.read(&mut buffer).await.unwrap();
            drop(reader);

            let mut reader = blobs.request(b"fail").await.unwrap();
            let mut failed = false;
            loop {
                match reader.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(_) => {
                        failed = true;
                        break;
                    }
                }
            }
            assert!(failed, "looked like the end of a short file");
            tx.send(&Message::Leave).await.unwrap();
            (tx, rx)
        };
        tokio::join!(server_side, client_side);
    }

    #[tokio::test]
    async fn unknown_device_is_rejected() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let stranger = Identity::generate().unwrap();
        let server = Endpoint::bind(&a, loopback(), TrustStore::new([b.fingerprint()])).unwrap();
        let client =
            Endpoint::bind(&stranger, loopback(), TrustStore::new([a.fingerprint()])).unwrap();
        let address = server.local_addr().unwrap();

        let server_side = async {
            let incoming = server.next_incoming().await.unwrap();
            let result = incoming.establish().await;
            assert!(
                matches!(result, Err(Error::NotPaired(found)) if found == stranger.fingerprint())
            );
        };
        let client_side = async {
            // The stranger may finish its half of the handshake before the
            // server rejects it, but it can never get a message through.
            if let Ok(link) = client.connect(address, a.fingerprint()).await {
                let (mut tx, mut rx) = link.split();
                let _ = tx.send(&Message::Enter).await;
                assert!(rx.recv().await.is_err());
            }
        };
        tokio::join!(server_side, client_side);
    }

    #[tokio::test]
    async fn device_refuses_an_untrusted_server() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let impostor = Identity::generate().unwrap();
        // The impostor would accept b, but b only trusts a.
        let server =
            Endpoint::bind(&impostor, loopback(), TrustStore::new([b.fingerprint()])).unwrap();
        let client = Endpoint::bind(&b, loopback(), TrustStore::new([a.fingerprint()])).unwrap();
        let address = server.local_addr().unwrap();

        let server_side = async {
            let _ = server.next_incoming().await.unwrap().establish().await;
        };
        let (_, connected) = tokio::join!(server_side, client.connect(address, a.fingerprint()));
        assert!(connected.is_err());
    }

    #[tokio::test]
    async fn trusted_device_at_the_wrong_address_is_refused() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let c = Identity::generate().unwrap();
        // b trusts both, but is looking for c specifically.
        let trust = TrustStore::new([a.fingerprint(), c.fingerprint()]);
        let server = Endpoint::bind(&a, loopback(), TrustStore::new([b.fingerprint()])).unwrap();
        let client = Endpoint::bind(&b, loopback(), trust).unwrap();
        let address = server.local_addr().unwrap();

        let server_side = async {
            // Ends when the client hangs up, having not opened a stream.
            let _ = server.next_incoming().await.unwrap().establish().await;
        };
        let (_, connected) = tokio::join!(server_side, client.connect(address, c.fingerprint()));
        assert!(matches!(connected, Err(Error::WrongDevice { .. })));
    }

    #[tokio::test]
    async fn connect_any_finds_the_address_that_works() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let server = Endpoint::bind(&a, loopback(), TrustStore::new([b.fingerprint()])).unwrap();
        let client = Endpoint::bind(&b, loopback(), TrustStore::new([a.fingerprint()])).unwrap();
        let dead_end: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let addresses = [dead_end, server.local_addr().unwrap()];

        let server_side = async {
            accepted_link(
                server
                    .next_incoming()
                    .await
                    .unwrap()
                    .establish()
                    .await
                    .unwrap(),
            )
        };
        let client_side = async {
            let link = client
                .connect_any(&addresses, a.fingerprint())
                .await
                .unwrap();
            let (mut tx, rx) = link.split();
            tx.send(&Message::Enter).await.unwrap();
            (tx, rx)
        };
        let (link, _keep_open) = tokio::join!(server_side, client_side);
        assert_eq!(link.peer(), b.fingerprint());
    }

    #[tokio::test]
    async fn connect_any_with_no_addresses_reports_that() {
        let a = Identity::generate().unwrap();
        let client = Endpoint::bind(&a, loopback(), TrustStore::default()).unwrap();
        let result = client.connect_any(&[], a.fingerprint()).await;
        assert!(matches!(result, Err(Error::NoAddress)));
    }

    #[tokio::test]
    async fn pairing_makes_a_device_trusted_immediately() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let a_trust = TrustStore::default();
        let b_trust = TrustStore::default();
        let server = Endpoint::bind(&a, loopback(), a_trust.clone()).unwrap();
        let client = Endpoint::bind(&b, loopback(), b_trust.clone()).unwrap();
        let address = server.local_addr().unwrap();

        // Not paired yet: neither side lets a normal connection through.
        let (accepted, connected) = tokio::join!(
            async { server.next_incoming().await.unwrap().establish().await },
            client.connect(address, a.fingerprint()),
        );
        assert!(accepted.is_err());
        assert!(connected.is_err());

        a_trust.add(b.fingerprint());
        b_trust.add(a.fingerprint());
        let server_side = async {
            accepted_link(
                server
                    .next_incoming()
                    .await
                    .unwrap()
                    .establish()
                    .await
                    .unwrap(),
            )
        };
        let client_side = async {
            let link = client.connect(address, a.fingerprint()).await.unwrap();
            let (mut tx, rx) = link.split();
            tx.send(&Message::Enter).await.unwrap();
            (tx, rx)
        };
        let (link, _keep_open) = tokio::join!(server_side, client_side);
        assert_eq!(link.peer(), b.fingerprint());
    }
}
