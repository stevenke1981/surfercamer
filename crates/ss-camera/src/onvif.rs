//! ONVIF WS-Discovery (UDP multicast 239.255.255.250:3702).

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use rand::RngExt;
use serde::Serialize;
use ss_core::Result;
use tokio::net::UdpSocket;
use tokio::time::Instant;

const MULTICAST: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const DISCOVERY_PORT: u16 = 3702;
const SCOPE_NAME_PREFIX: &str = "onvif://www.onvif.org/name/";

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
    let id = if message_id.starts_with("uuid:") {
        message_id.to_string()
    } else {
        format!("uuid:{message_id}")
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope" xmlns:w="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery" xmlns:dn="http://www.onvif.org/ver10/network/wsdl">
<e:Header>
<w:MessageID>{id}</w:MessageID>
<w:To e:mustUnderstand="true">urn:schemas-xmlsoap-org:ws:2005:04:discovery</w:To>
<w:Action e:mustUnderstand="true">http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe</w:Action>
</e:Header>
<e:Body>
<d:Probe>
<d:Types>dn:NetworkVideoTransmitter</d:Types>
</d:Probe>
</e:Body>
</e:Envelope>"#
    )
}

/// Local name of a possibly prefixed element name (`d:ProbeMatch` -> `ProbeMatch`).
fn local_name(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

/// Text between the end of a start tag and the next `<`, trimmed.
fn element_text(after_tag: &str) -> &str {
    after_tag.split('<').next().unwrap_or("").trim()
}

/// Decode `%XX` escapes; invalid escapes are kept literally.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(byte) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Default)]
struct Partial {
    endpoint: String,
    xaddrs: Vec<String>,
    scopes: Vec<String>,
}

impl Partial {
    fn finish(self) -> Option<DiscoveredDevice> {
        if self.endpoint.is_empty() {
            return None;
        }
        let name = self
            .scopes
            .iter()
            .find_map(|s| s.strip_prefix(SCOPE_NAME_PREFIX))
            .map(percent_decode);
        Some(DiscoveredDevice {
            endpoint: self.endpoint,
            xaddrs: self.xaddrs,
            scopes: self.scopes,
            name,
        })
    }
}

/// Parse a ProbeMatches response; tolerant of namespace prefixes.
pub fn parse_probe_matches(xml: &str) -> Vec<DiscoveredDevice> {
    let mut devices = Vec::new();
    let mut current: Option<Partial> = None;
    let mut pos = 0;
    while let Some(lt) = xml[pos..].find('<') {
        let start = pos + lt;
        let Some(gt_rel) = xml[start..].find('>') else {
            break;
        };
        let gt = start + gt_rel;
        let tag = &xml[start + 1..gt];
        let after = gt + 1;
        pos = after;
        if tag.starts_with('?') || tag.starts_with('!') {
            continue;
        }
        if let Some(end_name) = tag.strip_prefix('/') {
            if local_name(end_name.trim()) == "ProbeMatch" {
                if let Some(device) = current.take().and_then(Partial::finish) {
                    devices.push(device);
                }
            }
            continue;
        }
        let self_closing = tag.ends_with('/');
        let name = tag
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or("");
        let local = local_name(name);
        if local == "ProbeMatch" {
            if !self_closing {
                current = Some(Partial::default());
            }
            continue;
        }
        let Some(partial) = current.as_mut() else {
            continue;
        };
        match local {
            "Address" => partial.endpoint = element_text(&xml[after..]).to_string(),
            "XAddrs" => {
                partial.xaddrs = element_text(&xml[after..])
                    .split_whitespace()
                    .map(str::to_string)
                    .collect();
            }
            "Scopes" => {
                partial.scopes = element_text(&xml[after..])
                    .split_whitespace()
                    .map(str::to_string)
                    .collect();
            }
            _ => {}
        }
    }
    devices
}

/// Consecutive receive errors after which discovery gives up.
const MAX_RECV_ERRORS: u32 = 3;
/// Pause between retries after a receive error.
const RECV_ERROR_BACKOFF: Duration = Duration::from_millis(50);

/// Send a probe and collect unique responses until `timeout`.
pub async fn discover(timeout: Duration) -> Result<Vec<DiscoveredDevice>> {
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).await?;
    let id: u128 = rand::rng().random();
    let hex = format!("{id:032x}");
    let uuid = format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    );
    let probe = probe_message(&uuid);
    socket
        .send_to(
            probe.as_bytes(),
            SocketAddr::from(SocketAddrV4::new(MULTICAST, DISCOVERY_PORT)),
        )
        .await?;

    let deadline = Instant::now() + timeout;
    let mut seen = HashSet::new();
    let mut devices = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut consecutive_errors = 0u32;
    loop {
        let received = match tokio::time::timeout_at(deadline, socket.recv_from(&mut buf)).await {
            Err(_) => break,
            Ok(Err(e)) => {
                // A persistently failing socket must not spin: give up after a few errors.
                consecutive_errors += 1;
                tracing::debug!("ONVIF discovery receive error: {e}");
                if consecutive_errors >= MAX_RECV_ERRORS {
                    tracing::warn!(
                        "ONVIF discovery stopped after {consecutive_errors} receive errors"
                    );
                    break;
                }
                tokio::time::sleep(RECV_ERROR_BACKOFF).await;
                continue;
            }
            Ok(Ok((n, _))) => {
                consecutive_errors = 0;
                n
            }
        };
        let text = String::from_utf8_lossy(&buf[..received]);
        for device in parse_probe_matches(&text) {
            if seen.insert(device.endpoint.clone()) {
                devices.push(device);
            }
        }
    }
    Ok(devices)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope" xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery" xmlns:dn="http://www.onvif.org/ver10/network/wsdl">
  <SOAP-ENV:Header>
    <wsa:MessageID>uuid:1111</wsa:MessageID>
    <wsa:RelatesTo>uuid:2222</wsa:RelatesTo>
    <wsa:To>http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</wsa:To>
    <wsa:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/ProbeMatches</wsa:Action>
  </SOAP-ENV:Header>
  <SOAP-ENV:Body>
    <d:ProbeMatches>
      <d:ProbeMatch>
        <wsa:EndpointReference>
          <wsa:Address>urn:uuid:abc-123</wsa:Address>
        </wsa:EndpointReference>
        <d:Types>dn:NetworkVideoTransmitter</d:Types>
        <d:Scopes>onvif://www.onvif.org/type/video_encoder onvif://www.onvif.org/name/Front%20Door onvif://www.onvif.org/hardware/IPC-1234 onvif://www.onvif.org/location/country/de</d:Scopes>
        <d:XAddrs>http://192.168.1.20/onvif/device_service http://[fe80::1]/onvif/device_service</d:XAddrs>
        <d:MetadataVersion>1</d:MetadataVersion>
      </d:ProbeMatch>
      <d:ProbeMatch>
        <wsa:EndpointReference>
          <wsa:Address>urn:uuid:def-456</wsa:Address>
        </wsa:EndpointReference>
        <d:Scopes>onvif://www.onvif.org/name/Garage</d:Scopes>
        <d:XAddrs>http://192.168.1.21/onvif/device_service</d:XAddrs>
      </d:ProbeMatch>
    </d:ProbeMatches>
  </SOAP-ENV:Body>
</SOAP-ENV:Envelope>"#;

    #[test]
    fn probe_message_contains_required_parts() {
        let msg = probe_message("abcd-1234");
        assert!(msg.contains("MessageID"));
        assert!(msg.contains("uuid:abcd-1234"));
        assert!(msg.contains("NetworkVideoTransmitter"));
        assert!(msg.contains("Probe"));
        assert_eq!(probe_message("uuid:x").matches("uuid:x").count(), 1);
    }

    #[test]
    fn parses_realistic_probe_matches() {
        let devices = parse_probe_matches(SAMPLE);
        assert_eq!(devices.len(), 2);
        let first = &devices[0];
        assert_eq!(first.endpoint, "urn:uuid:abc-123");
        assert_eq!(
            first.xaddrs,
            vec![
                "http://192.168.1.20/onvif/device_service",
                "http://[fe80::1]/onvif/device_service"
            ]
        );
        assert_eq!(first.scopes.len(), 4);
        assert_eq!(first.name.as_deref(), Some("Front Door"));
        assert_eq!(devices[1].endpoint, "urn:uuid:def-456");
        assert_eq!(devices[1].name.as_deref(), Some("Garage"));
    }

    #[test]
    fn parses_with_different_prefixes() {
        let xml = r#"<a:Envelope xmlns:a="x"><a:Body><x:ProbeMatches>
            <x:ProbeMatch>
              <y:EndpointReference><y:Address>urn:uuid:zzz</y:Address></y:EndpointReference>
              <x:Scopes>onvif://www.onvif.org/name/Lobby%21</x:Scopes>
              <x:XAddrs>http://10.0.0.5/onvif/device_service</x:XAddrs>
            </x:ProbeMatch>
            <x:ProbeMatch>
              <y:EndpointReference><y:Address>urn:uuid:zzz</y:Address></y:EndpointReference>
              <x:XAddrs>http://10.0.0.5/dup</x:XAddrs>
            </x:ProbeMatch>
          </x:ProbeMatches></a:Body></a:Envelope>"#;
        let devices = parse_probe_matches(xml);
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].name.as_deref(), Some("Lobby!"));
        assert_eq!(
            devices[0].xaddrs,
            vec!["http://10.0.0.5/onvif/device_service"]
        );
        assert_eq!(devices[1].name, None);
    }

    #[test]
    fn ignores_matches_without_endpoint_and_garbage() {
        assert!(parse_probe_matches("not xml at all <<<").is_empty());
        assert!(
            parse_probe_matches("<ProbeMatch><XAddrs>http://a</XAddrs></ProbeMatch>").is_empty()
        );
    }

    #[test]
    fn percent_decode_handles_bad_escapes() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("x%zzy"), "x%zzy");
    }
}
