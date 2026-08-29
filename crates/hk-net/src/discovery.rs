//! Finding other Hackerman devices on the local network with mDNS.
//!
//! Each device announces itself with its fingerprint, name and port, and
//! listens for the announcements of others, so nobody has to type an IP
//! address. An announcement proves nothing: only pairing and the certificate
//! check establish who a device is.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use hk_core::device::Fingerprint;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

use crate::Error;

const SERVICE_TYPE: &str = "_hackerman._udp.local.";
const FINGERPRINT_PROPERTY: &str = "fp";
const NAME_PROPERTY: &str = "name";
const MODEL_PROPERTY: &str = "model";

#[derive(Debug, Clone, PartialEq)]
pub struct FoundDevice {
    pub fingerprint: Fingerprint,
    pub name: String,
    /// The make and model, like "Dell Latitude 7420", if the device says.
    pub model: Option<String>,
    /// Best first.
    pub addresses: Vec<SocketAddr>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DiscoveryEvent {
    Found(FoundDevice),
    Lost(Fingerprint),
}

/// Announces this device and watches for others until dropped.
pub struct Discovery {
    daemon: ServiceDaemon,
    me: Fingerprint,
    model: Option<String>,
    port: u16,
}

impl Discovery {
    /// `on_event` runs on a background task for every change.
    pub fn start(
        me: Fingerprint,
        name: &str,
        model: Option<String>,
        port: u16,
        on_event: impl Fn(DiscoveryEvent) + Send + 'static,
    ) -> Result<Self, Error> {
        let daemon = ServiceDaemon::new()?;
        let discovery = Self {
            daemon,
            me,
            model,
            port,
        };
        discovery.announce(name)?;

        let events = discovery.daemon.browse(SERVICE_TYPE)?;
        tokio::spawn(async move {
            let mut seen: HashMap<String, Fingerprint> = HashMap::new();
            while let Ok(event) = events.recv_async().await {
                match event {
                    ServiceEvent::ServiceResolved(service) => {
                        let Some(fingerprint) = service
                            .get_property_val_str(FINGERPRINT_PROPERTY)
                            .and_then(|text| text.parse().ok())
                        else {
                            continue;
                        };
                        if fingerprint == me {
                            continue;
                        }
                        seen.insert(service.get_fullname().to_string(), fingerprint);
                        let name = service
                            .get_property_val_str(NAME_PROPERTY)
                            .unwrap_or("Unnamed device")
                            .to_string();
                        let mut addresses: Vec<Ipv4Addr> =
                            service.get_addresses_v4().into_iter().collect();
                        addresses.sort_by_key(|address| address_rank(*address));
                        on_event(DiscoveryEvent::Found(FoundDevice {
                            fingerprint,
                            name,
                            model: service
                                .get_property_val_str(MODEL_PROPERTY)
                                .map(ToString::to_string),
                            addresses: addresses
                                .into_iter()
                                .map(|address| SocketAddr::new(address.into(), service.get_port()))
                                .collect(),
                        }));
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => {
                        if let Some(fingerprint) = seen.remove(&fullname) {
                            on_event(DiscoveryEvent::Lost(fingerprint));
                        }
                    }
                    _ => {}
                }
            }
        });
        Ok(discovery)
    }

    /// Announces this device, or updates the announcement after a rename.
    pub fn announce(&self, name: &str) -> Result<(), Error> {
        let fingerprint = self.me.to_string();
        // The fingerprint keeps instance names unique even if two computers share a name.
        let instance = &fingerprint[..16];
        let mut properties = vec![
            (FINGERPRINT_PROPERTY, fingerprint.as_str()),
            (NAME_PROPERTY, name),
        ];
        if let Some(model) = &self.model {
            properties.push((MODEL_PROPERTY, model.as_str()));
        }
        let service = ServiceInfo::new(
            SERVICE_TYPE,
            instance,
            &format!("{instance}.local."),
            "",
            self.port,
            &properties[..],
        )?
        .enable_addr_auto();
        self.daemon.register(service)?;
        Ok(())
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}

/// This computer's IPv4 addresses, the one most likely to be its real LAN
/// address first.
pub fn local_addresses() -> Vec<Ipv4Addr> {
    let mut addresses: Vec<(u8, Ipv4Addr)> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|interface| !interface.is_loopback())
        .filter_map(|interface| match interface.ip() {
            IpAddr::V4(address) => {
                let virtual_adapter = ["vEthernet", "VMware", "VirtualBox", "WSL", "Hyper-V"]
                    .iter()
                    .any(|name| interface.name.contains(name));
                let rank = address_rank(address) + if virtual_adapter { 10 } else { 0 };
                Some((rank, address))
            }
            IpAddr::V6(_) => None,
        })
        .collect();
    addresses.sort();
    addresses.dedup();
    addresses.into_iter().map(|(_, address)| address).collect()
}

/// Lower is better. Home and office networks nearly always use these private
/// ranges, while link-local addresses mean a network adapter got no address.
fn address_rank(address: Ipv4Addr) -> u8 {
    match address.octets() {
        [192, 168, ..] => 0,
        [10, ..] => 1,
        [172, second, ..] if (16..32).contains(&second) => 2,
        [169, 254, ..] => 4,
        _ => 3,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::mpsc;

    use super::*;

    #[test]
    fn address_ranking_prefers_home_networks() {
        let mut addresses: Vec<Ipv4Addr> = [
            "169.254.0.1",
            "8.8.8.8",
            "10.0.0.2",
            "172.20.1.1",
            "192.168.1.5",
        ]
        .iter()
        .map(|text| text.parse().unwrap())
        .collect();
        addresses.sort_by_key(|address| address_rank(*address));
        assert_eq!(addresses[0], "192.168.1.5".parse::<Ipv4Addr>().unwrap());
        assert_eq!(addresses[1], "10.0.0.2".parse::<Ipv4Addr>().unwrap());
        assert_eq!(addresses[4], "169.254.0.1".parse::<Ipv4Addr>().unwrap());
    }

    /// Uses the real network, so it's run on demand: `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "needs a real network that allows multicast"]
    async fn two_devices_find_each_other() {
        let a = Fingerprint::from_bytes([0xaa; 32]);
        let b = Fingerprint::from_bytes([0xbb; 32]);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let tx_b = tx.clone();
        let _first = Discovery::start(a, "first", Some("Maker One".into()), 24901, move |event| {
            let _ = tx.send((a, event));
        })
        .unwrap();
        let _second = Discovery::start(b, "second", None, 24902, move |event| {
            let _ = tx_b.send((b, event));
        })
        .unwrap();

        let (mut a_found_b, mut b_found_a) = (false, false);
        let wait = async {
            while let Some((observer, event)) = rx.recv().await {
                if let DiscoveryEvent::Found(found) = event {
                    if observer == a && found.fingerprint == b && found.name == "second" {
                        assert_eq!(found.model, None);
                        a_found_b = true;
                    }
                    if observer == b && found.fingerprint == a && found.name == "first" {
                        assert_eq!(found.model.as_deref(), Some("Maker One"));
                        b_found_a = true;
                    }
                }
                if a_found_b && b_found_a {
                    break;
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(15), wait)
            .await
            .expect("devices didn't find each other in time");
    }
}
