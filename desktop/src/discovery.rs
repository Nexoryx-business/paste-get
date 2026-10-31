// SPDX-License-Identifier: GPL-3.0-or-later

//! Announces the daemon on the local network with mDNS / DNS-SD, so phones can find it without
//! typing an address. See `docs/PROTOCOL.md`, section 2.4.
//!
//! Service type `_pasteget._tcp.local.`, TXT records `v` (protocol version), `id` (device ID)
//! and `name` (device name). Announcing is only a convenience: devices still pin the device ID,
//! so a fake announcement can't make a paired device talk to the wrong machine.

use anyhow::{Context, Result};
use mdns_sd::{IfKind, ServiceDaemon, ServiceInfo};
use pasteget_core::identity::DeviceId;
use pasteget_core::proto::PROTOCOL_VERSION;

pub const SERVICE_TYPE: &str = "_pasteget._tcp.local.";

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
        let properties = [
            ("v", PROTOCOL_VERSION.to_string()),
            ("id", device_id.to_string()),
            ("name", device_name.to_owned()),
        ];
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
}
