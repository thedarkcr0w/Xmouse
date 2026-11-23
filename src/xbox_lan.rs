//! Finding the Xbox on the home network, for a direct LAN connection.
//!
//! The xHome service only hands us the console's Teredo (IPv6 tunnel) candidates. Their
//! embedded IPv4 address is the router's *public* address, so on a router without NAT
//! loopback ("hairpinning") the PC can't reach the console that way even though both sit on the
//! same LAN. Knowing the console's private address lets us offer it as an extra ICE candidate,
//! and the connection then goes straight across the LAN.
//!
//! The address comes from the setting (typed in by the user) or from the console's local
//! discovery protocol (SmartGlass, UDP 5050): we broadcast a discovery request and every
//! Xbox on the network answers with its name.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

/// UDP port the console's discovery service listens on.
const DISCOVERY_PORT: u16 = 5050;
/// Port the console's remote play (WebRTC) socket uses; the same one it puts in its candidates.
pub const STREAM_PORT: u16 = 9002;

#[derive(Clone, Debug, PartialEq)]
pub struct LanConsole {
    pub name: String,
    pub ip: Ipv4Addr,
}

/// SmartGlass discovery request: header (type 0xDD00, payload length, version 0) and payload
/// (flags 0, client type 3 = Windows desktop, protocol versions 0..=2). All big-endian.
fn discovery_request() -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&0u32.to_be_bytes()); // flags
    payload.extend_from_slice(&3u16.to_be_bytes()); // client type
    payload.extend_from_slice(&0u16.to_be_bytes()); // min version
    payload.extend_from_slice(&2u16.to_be_bytes()); // max version
    let mut pkt = Vec::new();
    pkt.extend_from_slice(&0xDD00u16.to_be_bytes());
    pkt.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    pkt.extend_from_slice(&0u16.to_be_bytes());
    pkt.extend_from_slice(&payload);
    pkt
}

/// The console name from a discovery response (type 0xDD01), if the packet is one.
fn parse_response(b: &[u8]) -> Option<String> {
    if b.len() < 6 || u16::from_be_bytes([b[0], b[1]]) != 0xDD01 {
        return None;
    }
    // header (6) + flags (4) + client type (2), then the name: u16 length, bytes, NUL.
    let p = 6 + 4 + 2;
    let len = u16::from_be_bytes([*b.get(p)?, *b.get(p + 1)?]) as usize;
    let name = b.get(p + 2..p + 2 + len)?;
    Some(String::from_utf8_lossy(name).trim().to_string())
}

/// Broadcast a discovery request and collect the consoles that answer within `wait`.
/// Blocking: call it from `spawn_blocking`.
pub fn discover(wait: Duration) -> Vec<LanConsole> {
    match discover_inner(wait) {
        Ok(found) => found,
        Err(e) => {
            log::warn!("LAN discovery failed: {e}");
            Vec::new()
        }
    }
}

fn discover_inner(wait: Duration) -> std::io::Result<Vec<LanConsole>> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    sock.set_broadcast(true)?;
    sock.set_read_timeout(Some(Duration::from_millis(200)))?;
    let req = discovery_request();
    let targets: [SocketAddr; 2] = [
        (Ipv4Addr::BROADCAST, DISCOVERY_PORT).into(),
        (Ipv4Addr::new(239, 255, 255, 250), DISCOVERY_PORT).into(),
    ];
    let mut found: Vec<LanConsole> = Vec::new();
    let start = Instant::now();
    let mut next_send = start;
    let mut buf = [0u8; 2048];
    while start.elapsed() < wait {
        // A few repeats: UDP broadcasts get lost on busy Wi-Fi.
        if Instant::now() >= next_send {
            for t in &targets {
                if let Err(e) = sock.send_to(&req, t) {
                    log::debug!("LAN discovery: send to {t} failed: {e}");
                }
            }
            next_send = Instant::now() + Duration::from_millis(500);
        }
        match sock.recv_from(&mut buf) {
            Ok((n, SocketAddr::V4(from))) => {
                if let Some(name) = parse_response(&buf[..n]) {
                    if !found.iter().any(|c| c.ip == *from.ip()) {
                        log::info!("LAN discovery: \"{name}\" at {}", from.ip());
                        found.push(LanConsole {
                            name,
                            ip: *from.ip(),
                        });
                    }
                }
            }
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => log::debug!("LAN discovery: receive failed: {e}"),
        }
    }
    Ok(found)
}

/// Pick the console to use: the one with the same name as in the xHome list, or the only one.
pub fn pick(found: &[LanConsole], name: &str) -> Option<Ipv4Addr> {
    found
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(name.trim()))
        .or(if found.len() == 1 { found.first() } else { None })
        .map(|c| c.ip)
}

/// A host ICE candidate for the console's LAN address, in the console's own format.
/// High priority so the LAN pair wins over the Teredo-derived ones when both work.
pub fn lan_candidate(ip: Ipv4Addr) -> String {
    format!("a=candidate:20 1 UDP 2130706431 {ip} {STREAM_PORT} typ host ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_layout() {
        assert_eq!(
            discovery_request(),
            [0xDD, 0x00, 0, 10, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 2]
        );
    }

    #[test]
    fn parses_response_name() {
        let mut b = vec![0xDD, 0x01, 0, 0, 0, 2];
        b.extend_from_slice(&[0, 0, 0, 2]); // flags
        b.extend_from_slice(&[0, 1]); // client type
        b.extend_from_slice(&[0, 4]);
        b.extend_from_slice(b"Xbox\0");
        assert_eq!(parse_response(&b).as_deref(), Some("Xbox"));
        assert_eq!(parse_response(&[0xDD, 0x00, 0, 0, 0, 0]), None);
        assert_eq!(parse_response(&[0xDD, 0x01, 0, 0]), None);
    }

    #[test]
    fn picks_by_name_or_only_one() {
        let a = LanConsole {
            name: "Salon".into(),
            ip: Ipv4Addr::new(192, 168, 1, 10),
        };
        let b = LanConsole {
            name: "Pokoj".into(),
            ip: Ipv4Addr::new(192, 168, 1, 11),
        };
        assert_eq!(pick(&[a.clone(), b.clone()], "pokoj"), Some(b.ip));
        assert_eq!(pick(&[a.clone(), b], "Other"), None);
        assert_eq!(pick(&[a.clone()], "Other"), Some(a.ip));
        assert_eq!(pick(&[], "Salon"), None);
    }
}
