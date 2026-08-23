//! Pairing two devices with a short code.
//!
//! One device shows a code and the user types it on the other. That code is
//! far too short to be an encryption key, and someone on the network could try
//! to guess it from a recording of the traffic. SPAKE2 avoids both problems:
//! the two sides turn the code into a strong shared key without ever sending it
//! or anything that could test a guess offline. An attacker gets one guess per
//! attempt, and each attempt gets a new code.
//!
//! Both devices' certificate fingerprints go into the calculation. If someone
//! sits between them, each side sees a different peer certificate, the keys
//! come out different, and the final check fails. Once it succeeds, each device
//! knows the fingerprint it saw belongs to the device showing or typing the
//! code.

use std::net::SocketAddr;
use std::time::Duration;

use hk_core::device::Fingerprint;
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use spake2::{Ed25519Group, Identity, Password, Spake2};

use crate::Error;
use crate::link::{read_frame, write_frame};

const INITIATOR_LABEL: &[u8] = b"hackerman pairing: initiator";
const RESPONDER_LABEL: &[u8] = b"hackerman pairing: responder";

/// A new random 6-digit pairing code.
pub fn generate_code() -> String {
    // Numbers at or above the largest multiple of a million would make low
    // codes slightly more likely, so they're drawn again.
    const LIMIT: u32 = u32::MAX - (u32::MAX % 1_000_000);
    let random = SystemRandom::new();
    loop {
        let mut bytes = [0; 4];
        random
            .fill(&mut bytes)
            .expect("the operating system's random number generator failed");
        let value = u32::from_le_bytes(bytes);
        if value < LIMIT {
            return format!("{:06}", value % 1_000_000);
        }
    }
}

/// A connection that can only be used to pair.
pub struct PairingChannel {
    connection: quinn::Connection,
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    me: Fingerprint,
    peer: Fingerprint,
}

impl PairingChannel {
    pub(crate) fn new(
        connection: quinn::Connection,
        send: quinn::SendStream,
        recv: quinn::RecvStream,
        me: Fingerprint,
        peer: Fingerprint,
    ) -> Self {
        Self {
            connection,
            send,
            recv,
            me,
            peer,
        }
    }

    pub fn peer(&self) -> Fingerprint {
        self.peer
    }

    pub fn remote_address(&self) -> SocketAddr {
        self.connection.remote_address()
    }

    /// Tells the other device this one's name, and returns its name. The names
    /// are only for display until the code has been checked.
    pub async fn exchange_names(&mut self, my_name: &str) -> Result<String, Error> {
        write_frame(&mut self.send, my_name.as_bytes()).await?;
        let name = read_frame(&mut self.recv).await?;
        Ok(String::from_utf8_lossy(&name).into_owned())
    }

    /// Runs the exchange on the device where the user typed `code`.
    pub async fn confirm_as_initiator(mut self, code: &str) -> Result<(), Error> {
        let (state, message) = Spake2::<Ed25519Group>::start_a(
            &Password::new(code.as_bytes()),
            &identity(self.me),
            &identity(self.peer),
        );
        write_frame(&mut self.send, &message).await?;
        let reply = read_frame(&mut self.recv).await?;
        let key = state.finish(&reply).map_err(|_| Error::WrongCode)?;

        write_frame(&mut self.send, &tag(&key, INITIATOR_LABEL)).await?;
        let proof = read_frame(&mut self.recv)
            .await
            .map_err(|error| match error {
                // The other side hangs up without a word if the code was wrong.
                Error::Read(_) => Error::WrongCode,
                other => other,
            })?;
        verify(&key, RESPONDER_LABEL, &proof)?;
        self.connection.close(0u32.into(), b"paired");
        Ok(())
    }

    /// Runs the exchange on the device showing `code`.
    pub async fn confirm_as_responder(mut self, code: &str) -> Result<(), Error> {
        let message = read_frame(&mut self.recv).await?;
        let (state, reply) = Spake2::<Ed25519Group>::start_b(
            &Password::new(code.as_bytes()),
            &identity(self.peer),
            &identity(self.me),
        );
        write_frame(&mut self.send, &reply).await?;
        let key = state.finish(&message).map_err(|_| Error::WrongCode)?;

        let proof = read_frame(&mut self.recv).await?;
        if let Err(error) = verify(&key, INITIATOR_LABEL, &proof) {
            self.connection.close(0u32.into(), b"wrong code");
            return Err(error);
        }
        write_frame(&mut self.send, &tag(&key, RESPONDER_LABEL)).await?;
        // Stay connected until the other side has read that, and hangs up.
        let _ = tokio::time::timeout(Duration::from_secs(3), self.connection.closed()).await;
        Ok(())
    }
}

fn identity(device: Fingerprint) -> Identity {
    Identity::new(device.to_string().as_bytes())
}

fn tag(key: &[u8], label: &[u8]) -> Vec<u8> {
    let key = hmac::Key::new(hmac::HMAC_SHA256, key);
    hmac::sign(&key, label).as_ref().to_vec()
}

fn verify(key: &[u8], label: &[u8], proof: &[u8]) -> Result<(), Error> {
    let key = hmac::Key::new(hmac::HMAC_SHA256, key);
    hmac::verify(&key, label, proof).map_err(|_| Error::WrongCode)
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use super::*;
    use crate::{Accepted, Endpoint, Identity as DeviceIdentity, TrustStore};

    fn endpoint(identity: &DeviceIdentity) -> Endpoint {
        let loopback: SocketAddr = "127.0.0.1:0".parse().unwrap();
        Endpoint::bind(identity, loopback, TrustStore::default()).unwrap()
    }

    /// Runs a pairing where the initiator types `typed` and the responder shows `shown`.
    async fn pair(typed: &str, shown: &str) -> (Result<(), Error>, Result<(), Error>) {
        let a = DeviceIdentity::generate().unwrap();
        let b = DeviceIdentity::generate().unwrap();
        let (initiator, responder) = (endpoint(&a), endpoint(&b));
        let address = responder.local_addr().unwrap();
        let (a_fingerprint, b_fingerprint) = (a.fingerprint(), b.fingerprint());

        let initiator_side = async {
            let mut channel = initiator.connect_for_pairing(address).await.unwrap();
            assert_eq!(channel.peer(), b_fingerprint);
            assert_eq!(channel.exchange_names("laptop").await.unwrap(), "desktop");
            channel.confirm_as_initiator(typed).await
        };
        let responder_side = async {
            let incoming = responder.next_incoming().await.unwrap();
            let Accepted::Pairing(mut channel) = incoming.establish().await.unwrap() else {
                panic!("expected a pairing connection");
            };
            assert_eq!(channel.peer(), a_fingerprint);
            assert_eq!(channel.exchange_names("desktop").await.unwrap(), "laptop");
            channel.confirm_as_responder(shown).await
        };
        tokio::join!(initiator_side, responder_side)
    }

    #[tokio::test]
    async fn matching_codes_pair() {
        let (initiator, responder) = pair("123456", "123456").await;
        initiator.unwrap();
        responder.unwrap();
    }

    #[tokio::test]
    async fn wrong_code_fails_on_both_sides() {
        let (initiator, responder) = pair("000000", "123456").await;
        assert!(matches!(initiator, Err(Error::WrongCode)));
        assert!(matches!(responder, Err(Error::WrongCode)));
    }

    #[test]
    fn codes_are_six_digits() {
        for _ in 0..100 {
            let code = generate_code();
            assert_eq!(code.len(), 6);
            assert!(code.bytes().all(|b| b.is_ascii_digit()));
        }
    }

    #[test]
    fn identities_change_the_outcome() {
        // A person in the middle makes each side see a different certificate.
        let first = Fingerprint::from_bytes([1; 32]);
        let second = Fingerprint::from_bytes([2; 32]);
        let third = Fingerprint::from_bytes([3; 32]);
        let password = Password::new(b"123456");
        let (a, a_message) =
            Spake2::<Ed25519Group>::start_a(&password, &identity(first), &identity(second));
        let (b, b_message) =
            Spake2::<Ed25519Group>::start_b(&password, &identity(first), &identity(third));
        assert_ne!(a.finish(&b_message).unwrap(), b.finish(&a_message).unwrap());
    }
}
