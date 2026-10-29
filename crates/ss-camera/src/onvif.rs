//! ONVIF WS-Discovery (UDP multicast 239.255.255.250:3702).

use std::time::Duration;

use serde::Serialize;
use ss_core::Result;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DiscoveredDevice {
    /// Endpoint reference (urn:uuid:…).
    pub endpoint: String,
    /// Device service URLs (XAddrs).
    pub xaddrs: Vec<String>,
    /// ONVIF scopes (name, hardware, location…).
    pub scopes: Vec<String>,
    /// Convenience: value of the `onvif://www.onvif.org/name/` scope, if any.
    pub name: Option<String>,
}

/// SOAP Probe message for `dn:NetworkVideoTransmitter` with the given MessageID uuid.
pub fn probe_message(message_id: &str) -> String {
    let _ = message_id;
    todo!()
}

/// Parse a ProbeMatches response; tolerant of namespace prefixes.
pub fn parse_probe_matches(xml: &str) -> Vec<DiscoveredDevice> {
    let _ = xml;
    todo!()
}

/// Send a probe and collect unique responses until `timeout`.
pub async fn discover(timeout: Duration) -> Result<Vec<DiscoveredDevice>> {
    let _ = timeout;
    todo!()
}
