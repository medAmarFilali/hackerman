//! One running connection to a peer.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Result, bail};
use hk_core::device::Fingerprint;
use hk_core::input::Held;
use hk_net::{BlobChannel, Link, LinkReceiver, LinkSender};
use hk_platform::Platform;
use hk_proto::{Message, PROTOCOL_VERSION};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::engine::Event;
use crate::files::{self, OfferedFiles};

/// Runs until the connection drops or `outgoing` is closed. `outgoing` must
/// begin with this device's `Hello`.
pub async fn run<P: Platform>(
    link: Link,
    id: u64,
    platform: P,
    accepts_input: Arc<AtomicBool>,
    offered_files: OfferedFiles,
    events: UnboundedSender<Event>,
    outgoing: UnboundedReceiver<Message>,
) {
    let peer = link.peer();
    let blobs = link.blobs();
    let (sender, receiver) = link.split();
    let mut held = Held::default();

    // Sending and receiving are separate loops raced as a whole, not a single
    // loop choosing between "next to send" and "next received" each time round.
    // Such a loop would abandon a half-read message whenever there was something
    // to send, because receiving isn't safe to cancel partway.
    let result = tokio::select! {
        result = send_loop(sender, outgoing) => result,
        result = receive_loop(id, peer, receiver, &platform, &accepts_input, &events, &mut held) => result,
        result = receive_blobs(peer, blobs.clone(), &events) => result,
        result = files::serve(peer, blobs, offered_files, events.clone()) => result,
    };
    if let Err(error) = result {
        tracing::info!(%peer, %error, "connection ended");
    }

    // Whatever ended the session, don't leave the peer's keys stuck down here.
    for event in held.release_all() {
        platform.inject(&event);
    }
    let _ = events.send(Event::SessionDown { id, peer });
}

async fn send_loop(mut sender: LinkSender, mut outgoing: UnboundedReceiver<Message>) -> Result<()> {
    while let Some(message) = outgoing.recv().await {
        sender.send(&message).await?;
    }
    Ok(())
}

async fn receive_loop<P: Platform>(
    id: u64,
    peer: Fingerprint,
    mut receiver: LinkReceiver,
    platform: &P,
    accepts_input: &AtomicBool,
    events: &UnboundedSender<Event>,
    held: &mut Held,
) -> Result<()> {
    loop {
        match receiver.recv().await? {
            Message::Hello {
                protocol_version,
                device_name,
                monitors,
                layout,
                model,
                enabled,
                features,
            } => {
                if protocol_version != PROTOCOL_VERSION {
                    let _ = events.send(Event::PeerIncompatible { peer });
                    // Hanging up at once could lose this device's own introduction
                    // on its way out, and the other side needs it to report the
                    // mismatch too.
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    bail!(
                        "peer speaks protocol version {protocol_version}, \
                         this device speaks {PROTOCOL_VERSION}"
                    );
                }
                let _ = events.send(Event::SessionUp {
                    id,
                    peer,
                    name: device_name,
                    monitors,
                    layout,
                    model,
                    enabled,
                    features,
                });
            }
            Message::Input(event) => {
                // The peer may not have heard yet that this device is turned off.
                // What it already holds down is released when it leaves.
                if accepts_input.load(Ordering::Relaxed) {
                    held.track(&event);
                    platform.inject(&event);
                }
            }
            Message::Enter => {
                let _ = events.send(Event::ControlledBy { peer, active: true });
            }
            Message::Leave => {
                for event in held.release_all() {
                    platform.inject(&event);
                }
                let _ = events.send(Event::ControlledBy {
                    peer,
                    active: false,
                });
            }
            Message::Monitors(monitors) => {
                let _ = events.send(Event::PeerMonitors { peer, monitors });
            }
            Message::Layout(layout) => {
                let _ = events.send(Event::PeerLayout { layout });
            }
            Message::Renamed(name) => {
                let _ = events.send(Event::PeerRenamed { peer, name });
            }
            Message::ClipboardOffer(version) => {
                let _ = events.send(Event::ClipboardOffered { peer, version });
            }
            Message::ClipboardRequest(version) => {
                let _ = events.send(Event::ClipboardRequested { peer, version });
            }
            Message::Enabled(enabled) => {
                let _ = events.send(Event::PeerEnabled { peer, enabled });
            }
            Message::Transfer(id, update) => {
                let _ = events.send(Event::TransferUpdated { peer, id, update });
            }
            Message::TransferCancel(id) => {
                let _ = events.send(Event::TransferCancelled { peer, id });
            }
            Message::Apps(apps) => {
                let _ = events.send(Event::PeerApps { peer, apps });
            }
            Message::AppIcon { app, png } => {
                let _ = events.send(Event::PeerAppIcon { peer, app, png });
            }
            Message::Media { app, command } => {
                let _ = events.send(Event::MediaRequested { peer, app, command });
            }
            Message::AppAction { app, action } => {
                let _ = events.send(Event::AppActionRequested { peer, app, action });
            }
            Message::MonitorIdentities(identities) => {
                let _ = events.send(Event::PeerMonitorIdentities { peer, identities });
            }
            Message::Showing(showing) => {
                let _ = events.send(Event::PeerShowing { showing });
            }
        }
    }
}

async fn receive_blobs(
    peer: Fingerprint,
    blobs: BlobChannel,
    events: &UnboundedSender<Event>,
) -> Result<()> {
    loop {
        match blobs.receive().await {
            Ok(bytes) => {
                let _ = events.send(Event::ClipboardArrived { peer, bytes });
            }
            Err(hk_net::Error::Blob(error)) => {
                // One failed transfer doesn't affect anything else on the connection.
                tracing::warn!(%peer, %error, "a transfer failed");
            }
            Err(error) => return Err(error.into()),
        }
    }
}
