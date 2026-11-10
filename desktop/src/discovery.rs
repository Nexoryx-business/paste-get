// SPDX-License-Identifier: GPL-3.0-or-later

//! Announces the daemon on the local network with mDNS / DNS-SD, so phones can find it without
//! typing an address, and looks there for the computers it joined, which may have moved. See
//! `docs/PROTOCOL.md`, section 2.4.
//!
//! Service type `_pasteget._tcp.local.`, TXT records `v` (protocol version), `id` (device ID)
//! and `name` (device name). Announcing is only a convenience: devices still pin the device ID,
//! so a fake announcement can't make a paired device talk to the wrong machine.

use std::net::IpAddr;

use anyhow::{Context, Result};
use mdns_sd::{IfKind, Receiver, ScopedIp, ServiceDaemon, ServiceEvent, ServiceInfo};
use pasteget_core::identity::DeviceId;
use pasteget_core::proto::PROTOCOL_VERSION;

pub const SERVICE_TYPE: &str = "_pasteget._tcp.local.";

/// A paste-get device announced on the network. Untrusted: its addresses are only tried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Announced {
    pub id: DeviceId,
    /// IPv4 only, like the listener.
    pub ips: Vec<IpAddr>,
    pub port: u16,
}

impl Announced {
    /// What an announcement says, if it names a device and where to reach it.
    pub fn read(
        id: Option<&str>,
        ips: impl IntoIterator<Item = IpAddr>,
        port: u16,
    ) -> Option<Self> {
        let id = id?.parse().ok()?;
        let mut ips: Vec<IpAddr> = ips.into_iter().filter(IpAddr::is_ipv4).collect();
        // They come unordered.
        ips.sort();
        ips.dedup();
        (!ips.is_empty() && port != 0).then_some(Self { id, ips, port })
    }
}

/// The devices announced on the network, as [`Advertiser::browse`] finds them, this one
/// included. Ends when the advertiser is dropped.
pub async fn next_announced(events: &Receiver<ServiceEvent>) -> Option<Announced> {
    loop {
        if let ServiceEvent::ServiceResolved(service) = events.recv_async().await.ok()? {
            let announced = Announced::read(
                service.txt_properties.get_property_val_str("id"),
                service.get_addresses().iter().map(ScopedIp::to_ip_addr),
                service.get_port(),
            );
            if announced.is_some() {
                return announced;
            }
        }
    }
}

/// Stops announcing when dropped.
pub struct Advertiser {
    mdns: ServiceDaemon,
}

impl Advertiser {
    pub fn start(device_id: DeviceId, device_name: &str, port: u16) -> Result<Self> {
        let mdns = ServiceDaemon::new().context("could not start mDNS")?;
        // The listener is IPv4-only for now, so don't announce IPv6 addresses.
        mdns.disable_interface(IfKind::IPv6)
            .context("could not configure mDNS")?;

        // Unique, stable labels that don't clash with the OS's own mDNS host name.
        let label = format!("paste-get-{}", &device_id.to_string()[..12]).to_lowercase();
        let mut properties = vec![
            ("v", PROTOCOL_VERSION.to_string()),
            ("id", device_id.to_string()),
        ];
        if !device_name.is_empty() {
            properties.push(("name", device_name.to_owned()));
        }
        let service = ServiceInfo::new(
            SERVICE_TYPE,
            &label,
            &format!("{label}.local."),
            "",
            port,
            &properties[..],
        )
        .context("invalid mDNS service")?
        .enable_addr_auto();
        mdns.register(service)
            .context("could not announce on the network")?;
        Ok(Self { mdns })
    }

    /// Starts looking for paste-get devices on the network: read them with [`next_announced`].
    pub fn browse(&self) -> Result<Receiver<ServiceEvent>> {
        self.mdns
            .browse(SERVICE_TYPE)
            .context("could not look for devices on the network")
    }
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        let _ = self.mdns.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mdns_sd::ServiceEvent;
    use pasteget_core::identity::Identity;
    use std::time::{Duration, Instant};

    #[test]
    fn announcements_are_read_as_devices_and_addresses() {
        let id = Identity::generate().unwrap().device_id();
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();

        let announced = Announced::read(
            Some(&id.to_string()),
            [
                ip("192.168.1.30"),
                ip("fe80::1"),
                ip("10.0.0.2"),
                ip("10.0.0.2"),
            ],
            47290,
        )
        .unwrap();

        assert_eq!(announced.id, id);
        assert_eq!(announced.ips, [ip("10.0.0.2"), ip("192.168.1.30")]);
        assert_eq!(announced.port, 47290);
        // Not a device, or nowhere to reach it.
        assert_eq!(Announced::read(None, [ip("10.0.0.2")], 47290), None);
        assert_eq!(
            Announced::read(Some("bogus"), [ip("10.0.0.2")], 47290),
            None
        );
        assert_eq!(
            Announced::read(Some(&id.to_string()), [ip("fe80::1")], 47290),
            None
        );
        assert_eq!(
            Announced::read(Some(&id.to_string()), [ip("10.0.0.2")], 0),
            None
        );
    }

    #[tokio::test]
    async fn a_browsing_daemon_reads_the_announcements() {
        let id = Identity::generate().unwrap().device_id();
        let _announcing = Advertiser::start(id, "Bureau (Windows)", 47998).unwrap();
        let browsing = Advertiser::start(
            Identity::generate().unwrap().device_id(),
            "Laptop (Linux)",
            47997,
        )
        .unwrap();
        let events = browsing.browse().unwrap();

        let found = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match next_announced(&events).await {
                    Some(announced) if announced.id == id => return announced,
                    Some(_) => {}
                    None => panic!("browsing stopped"),
                }
            }
        })
        .await
        .expect("the announcement was not seen");

        assert_eq!(found.port, 47998);
        assert!(!found.ips.is_empty());
    }

    #[test]
    fn announcement_is_found_by_a_browser() {
        let id = Identity::generate().unwrap().device_id();
        let _advertiser = Advertiser::start(id, "Test PC (Linux)", 47999).unwrap();

        let browser = ServiceDaemon::new().unwrap();
        let events = browser.browse(SERVICE_TYPE).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut found = None;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match events.recv_timeout(left) {
                Ok(ServiceEvent::ServiceResolved(service))
                    if service.txt_properties.get_property_val_str("id")
                        == Some(id.to_string().as_str()) =>
                {
                    found = Some(service);
                    break;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let _ = browser.shutdown();

        let service = found.expect("the announcement was not seen");
        assert_eq!(service.port, 47999);
        assert_eq!(
            service.txt_properties.get_property_val_str("name"),
            Some("Test PC (Linux)")
        );
        assert_eq!(service.txt_properties.get_property_val_str("v"), Some("0"));
    }

    #[test]
    fn announcement_without_name_omits_name_property() {
        let id = Identity::generate().unwrap().device_id();
        let _advertiser = Advertiser::start(id, "", 47996).unwrap();

        let browser = ServiceDaemon::new().unwrap();
        let events = browser.browse(SERVICE_TYPE).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut found = None;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match events.recv_timeout(left) {
                Ok(ServiceEvent::ServiceResolved(service))
                    if service.txt_properties.get_property_val_str("id")
                        == Some(id.to_string().as_str()) =>
                {
                    found = Some(service);
                    break;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let _ = browser.shutdown();

        let service = found.expect("the announcement was not seen");
        assert_eq!(service.port, 47996);
        assert_eq!(service.txt_properties.get_property_val_str("name"), None);
        assert_eq!(service.txt_properties.get_property_val_str("v"), Some("0"));
    }
}
