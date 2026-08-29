//! Encrypted links between Hackerman devices, and how they find each other.
//!
//! Devices talk over QUIC, which encrypts everything with TLS 1.3. Each device
//! has its own self-signed certificate, and devices trust each other by
//! certificate [`Fingerprint`] rather than through a certificate authority.
//! They learn each other's fingerprints by pairing with a short code.

#![forbid(unsafe_code)]

mod discovery;
mod identity;
mod link;
mod pairing;
mod tls;

pub use discovery::{Discovery, DiscoveryEvent, FoundDevice, local_addresses};
pub use hk_core::device::Fingerprint;
pub use identity::{Identity, fingerprint_of};
pub use link::{
    Accepted, BlobChannel, DataReader, DataWriter, Endpoint, IncomingConnection, IncomingRequest,
    Link, LinkReceiver, LinkSender, MAX_BLOB_LEN,
};
pub use pairing::{PairingChannel, generate_code};
pub use tls::TrustStore;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("generating the device certificate failed: {0}")]
    Certificate(#[from] rcgen::Error),
    #[error("TLS setup failed: {0}")]
    Tls(#[from] rustls::Error),
    #[error("QUIC setup failed: {0}")]
    QuicSetup(#[from] quinn::crypto::rustls::NoInitialCipherSuite),
    #[error("connecting failed: {0}")]
    Connect(#[from] quinn::ConnectError),
    #[error("connection lost: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("sending failed: {0}")]
    Write(#[from] quinn::WriteError),
    #[error("receiving failed: {0}")]
    Read(#[from] quinn::ReadExactError),
    #[error(transparent)]
    Codec(#[from] hk_proto::CodecError),
    #[error("device discovery failed: {0}")]
    Discovery(#[from] mdns_sd::Error),
    #[error("peer sent a {0}-byte message, more than allowed")]
    MessageTooLarge(usize),
    #[error("peer presented no certificate")]
    NoPeerCertificate,
    #[error("device {0} isn't paired with this one")]
    NotPaired(Fingerprint),
    #[error("reached device {found} instead of the expected one")]
    WrongDevice { found: Fingerprint },
    #[error("no known address for this device")]
    NoAddress,
    #[error("peer asked for an unknown protocol")]
    UnknownProtocol,
    #[error("large data transfer failed: {0}")]
    Blob(String),
    #[error("the pairing code didn't match")]
    WrongCode,
}
