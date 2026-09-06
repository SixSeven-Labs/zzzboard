//! Small pure helpers: name validation, timestamps, IP masking, client-IP
//! resolution behind the reverse proxy, JSON string quoting.

use std::net::{IpAddr, Ipv4Addr};

use axum::http::HeaderMap;
use chrono::{SecondsFormat, Utc};

/// Page, heartbeat-namespace and heartbeat-key names all share one grammar.
pub const MAX_NAME_LEN: usize = 128;
pub const NAME_RULE: &str = "[A-Za-z0-9_.-]{1,128}";

pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_NAME_LEN
        && s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// UTC, millisecond precision, RFC 3339: `2026-09-06T07:19:34.123Z`.
/// Lexicographic order == chronological order, which the listings rely on.
pub fn now_ts() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// `10.20.30.40` -> `10.20.30.xxx`. IPv6 keeps the first four hextets:
/// `2001:db8:1:2:xxxx`. IPv4-mapped IPv6 is masked as IPv4.
pub fn mask_ip(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => mask_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => mask_v4(v4),
            None => {
                let s = v6.segments();
                format!("{:x}:{:x}:{:x}:{:x}:xxxx", s[0], s[1], s[2], s[3])
            }
        },
    }
}

fn mask_v4(v4: Ipv4Addr) -> String {
    let o = v4.octets();
    format!("{}.{}.{}.xxx", o[0], o[1], o[2])
}

/// The address we attribute the request to. When the TCP peer is a private or
/// loopback address (the Caddy container, or a local run) we trust the LAST
/// entry of X-Forwarded-For — the one appended by that proxy. A public peer is
/// taken at face value; anything it claims in X-Forwarded-For is ignored.
pub fn client_ip(headers: &HeaderMap, peer: IpAddr) -> IpAddr {
    if !is_private(peer) {
        return peer;
    }
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit(',').next())
        .and_then(|s| s.trim().parse::<IpAddr>().ok())
        .unwrap_or(peer)
}

fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || v6
                    .to_ipv4_mapped()
                    .map(|v4| v4.is_private() || v4.is_loopback())
                    .unwrap_or(false)
        }
    }
}

/// A string as a JSON literal, quotes included. Used wherever free text has to
/// fit on one line (the `_log` page, `/history`).
pub fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn names() {
        assert!(valid_name("a"));
        assert!(valid_name("ZZZ_notes.v2-final"));
        assert!(valid_name(&"x".repeat(128)));
        assert!(!valid_name(""));
        assert!(!valid_name(&"x".repeat(129)));
        assert!(!valid_name("has space"));
        assert!(!valid_name("slash/inside"));
        assert!(!valid_name("ünïcode"));
    }

    #[test]
    fn masking() {
        assert_eq!(mask_ip("10.20.30.40".parse().unwrap()), "10.20.30.xxx");
        assert_eq!(mask_ip("::ffff:1.2.3.4".parse().unwrap()), "1.2.3.xxx");
        assert_eq!(
            mask_ip("2001:db8:1:2:3:4:5:6".parse().unwrap()),
            "2001:db8:1:2:xxxx"
        );
    }

    #[test]
    fn forwarded_for_only_from_private_peer() {
        let mut h = HeaderMap::new();
        h.insert(
            "x-forwarded-for",
            HeaderValue::from_static("9.9.9.9, 203.0.113.7"),
        );
        // proxy on the docker network: last hop wins
        assert_eq!(
            client_ip(&h, "172.18.0.2".parse().unwrap()),
            "203.0.113.7".parse::<IpAddr>().unwrap()
        );
        // public peer: header ignored
        assert_eq!(
            client_ip(&h, "198.51.100.1".parse().unwrap()),
            "198.51.100.1".parse::<IpAddr>().unwrap()
        );
        // garbage header: fall back to the peer
        h.insert("x-forwarded-for", HeaderValue::from_static("not-an-ip"));
        assert_eq!(
            client_ip(&h, "127.0.0.1".parse().unwrap()),
            "127.0.0.1".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn timestamp_shape() {
        let ts = now_ts();
        assert_eq!(ts.len(), 24, "{ts}");
        assert!(ts.ends_with('Z'));
    }
}
