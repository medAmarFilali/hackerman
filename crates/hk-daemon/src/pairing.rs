//! The two sides of pairing, each running in its own task.
//!
//! The device the user starts pairing from is the initiator. It waits for the
//! user to type the code shown on the other device, the responder.

use std::net::SocketAddr;
use std::time::Duration;

use hk_core::device::Fingerprint;
use hk_ipc::PairingStage;
use hk_net::{Endpoint, PairingChannel};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::engine::Event;

/// How long to wait for the user to type the code.
const CODE_TIMEOUT: Duration = Duration::from_secs(180);

/// The outcome of a pairing, sent once by either side.
pub struct Paired {
    pub fingerprint: Fingerprint,
    pub name: String,
    pub address: SocketAddr,
}

pub async fn initiate(
    endpoint: Endpoint,
    addresses: Vec<SocketAddr>,
    my_name: String,
    code: oneshot::Receiver<String>,
    events: UnboundedSender<Event>,
) {
    let result = async {
        let mut channel = match endpoint.connect_any_for_pairing(&addresses).await {
            Ok(channel) => channel,
            Err(error) => {
                tracing::info!(%error, "pairing: can't reach device");
                return Err(
                    "Couldn't reach that device. Is Hackerman open on it, and is the firewall \
                     allowing it?"
                        .to_string(),
                );
            }
        };
        let peer_name = channel.exchange_names(&my_name).await.map_err(|error| {
            tracing::info!(%error, "pairing: device refused");
            "The other device can't pair right now. Make sure Hackerman is open on it, then try \
             again."
                .to_string()
        })?;
        let _ = events.send(Event::PairingStage {
            peer_name: Some(peer_name.clone()),
            stage: PairingStage::EnterCode,
        });

        let code = tokio::time::timeout(CODE_TIMEOUT, code)
            .await
            .map_err(|_| "No code was entered in time.".to_string())?
            .map_err(|_| "Pairing was cancelled.".to_string())?;
        let _ = events.send(Event::PairingStage {
            peer_name: Some(peer_name.clone()),
            stage: PairingStage::Verifying,
        });

        let fingerprint = channel.peer();
        let address = channel.remote_address();
        confirm(channel.confirm_as_initiator(&code)).await?;
        Ok(Paired {
            fingerprint,
            name: peer_name,
            address,
        })
    }
    .await;
    let _ = events.send(Event::PairingFinished {
        result,
        responder: false,
    });
}

pub async fn respond(
    mut channel: PairingChannel,
    my_name: String,
    code: String,
    headless: bool,
    events: UnboundedSender<Event>,
) {
    let result = async {
        let peer_name = channel
            .exchange_names(&my_name)
            .await
            .map_err(|_| "The other device stopped pairing.".to_string())?;
        if headless {
            tracing::info!(%code, peer = %peer_name, "pairing code");
        }
        let _ = events.send(Event::PairingStage {
            peer_name: Some(peer_name.clone()),
            stage: PairingStage::ShowCode { code: code.clone() },
        });

        let fingerprint = channel.peer();
        let address = channel.remote_address();
        tokio::time::timeout(CODE_TIMEOUT, confirm(channel.confirm_as_responder(&code)))
            .await
            .map_err(|_| "No code was entered in time.".to_string())??;
        Ok(Paired {
            fingerprint,
            name: peer_name,
            address,
        })
    }
    .await;
    let _ = events.send(Event::PairingFinished {
        result,
        responder: true,
    });
}

async fn confirm(exchange: impl Future<Output = Result<(), hk_net::Error>>) -> Result<(), String> {
    exchange.await.map_err(|error| match error {
        hk_net::Error::WrongCode => "That code didn't match.".to_string(),
        other => {
            tracing::info!(error = %other, "pairing: connection failed");
            "The connection to the other device was lost.".to_string()
        }
    })
}
