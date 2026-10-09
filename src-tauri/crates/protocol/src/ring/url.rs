//! Relay URLs: `wss://host[:port][/path]`, or `ws://` to a loopback host. Deliberately
//! narrower than WHATWG URLs, so that every URL accepted here normalizes to the same origin in
//! Rust and in the Worker's `new URL()`: ASCII only, no userinfo, query, fragment, percent
//! escapes or dot segments, and an IPv4 host only as a plain dotted quad.

use super::RingId;
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};

/// The Hosted Relay, preset when a Ring is created and the default of `xshelld pair`. A
/// placeholder until the domain is confirmed before release.
pub const HOSTED_RELAY_URL: &str = "wss://relay.xshell.app";

/// A Roster's `relayUrl` is at most this long.
pub const MAX_RELAY_URL_LEN: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlError(pub &'static str);

impl fmt::Display for UrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bad relay url: {}", self.0)
    }
}

impl std::error::Error for UrlError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayUrl {
    /// TLS (`wss`) or plain (`ws`, loopback only).
    pub tls: bool,
    /// Lowercase; an IPv6 host in its canonical form, without brackets.
    pub host: String,
    pub port: u16,
    /// Empty, or starting with `/` and without a trailing `/`.
    pub path: String,
}

impl RelayUrl {
    pub fn parse(s: &str) -> Result<Self, UrlError> {
        if s.len() > MAX_RELAY_URL_LEN {
            return Err(UrlError("longer than 512 bytes"));
        }
        if !s.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(UrlError("not printable ASCII"));
        }
        let (scheme, rest) = s.split_once("://").ok_or(UrlError("no scheme"))?;
        let tls = match scheme.to_ascii_lowercase().as_str() {
            "wss" => true,
            "ws" => false,
            _ => return Err(UrlError("scheme is not wss or ws")),
        };
        if rest.contains(['?', '#']) {
            return Err(UrlError("query or fragment"));
        }
        if rest.contains(['\\', '%', '@']) {
            return Err(UrlError("backslash, percent escape or userinfo"));
        }
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        let (host, port) = parse_authority(authority)?;
        let port = port.unwrap_or(if tls { 443 } else { 80 });
        if !tls && !is_loopback(&host) {
            return Err(UrlError("ws:// is only allowed to a loopback host"));
        }
        let path = path.trim_end_matches('/');
        for seg in path.split('/').skip(1) {
            if seg == "." || seg == ".." {
                return Err(UrlError("dot segment in path"));
            }
            if !seg.bytes().all(is_path_byte) {
                return Err(UrlError("bad character in path"));
            }
        }
        if path.contains("//") {
            return Err(UrlError("empty path segment"));
        }
        Ok(RelayUrl {
            tls,
            host,
            port,
            path: path.to_string(),
        })
    }

    fn default_port(&self) -> u16 {
        if self.tls {
            443
        } else {
            80
        }
    }

    /// `scheme://host[:port]`, lowercase, default port dropped, IPv6 in brackets: what a device
    /// signs into its challenge answer and what the Relay compares against.
    pub fn origin(&self) -> String {
        let scheme = if self.tls { "wss" } else { "ws" };
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == self.default_port() {
            format!("{scheme}://{host}")
        } else {
            format!("{scheme}://{host}:{}", self.port)
        }
    }

    /// The WebSocket endpoint of one Ring on this Relay.
    pub fn ring_endpoint(&self, ring: &RingId) -> String {
        format!("{}{}/v1/ring/{}", self.origin(), self.path, ring)
    }

    /// The pairing pipe's WebSocket endpoint for `slot` on this Relay (section 16).
    pub fn pair_endpoint(&self, slot: &str) -> String {
        format!("{}{}/v1/pair/{}", self.origin(), self.path, slot)
    }

    pub fn is_loopback(&self) -> bool {
        is_loopback(&self.host)
    }
}

/// Normalizes a Relay URL to the origin used in the auth message.
pub fn normalize_origin(relay_url: &str) -> Result<String, UrlError> {
    Ok(RelayUrl::parse(relay_url)?.origin())
}

fn is_path_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:".contains(&b)
}

fn parse_authority(a: &str) -> Result<(String, Option<u16>), UrlError> {
    let (host, port) = if let Some(rest) = a.strip_prefix('[') {
        let (v6, after) = rest
            .split_once(']')
            .ok_or(UrlError("unclosed IPv6 bracket"))?;
        let ip: Ipv6Addr = v6.parse().map_err(|_| UrlError("bad IPv6 address"))?;
        // Rust prints IPv4-mapped addresses dotted, WHATWG URL in hex: refuse them.
        if ip.to_string().contains('.') {
            return Err(UrlError("IPv6 address with an embedded IPv4 address"));
        }
        let port = match after {
            "" => None,
            p => Some(
                p.strip_prefix(':')
                    .ok_or(UrlError("junk after IPv6 host"))?,
            ),
        };
        (ip.to_string(), port)
    } else {
        let (host, port) = match a.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (a, None),
        };
        (parse_name(host)?, port)
    };
    let port = match port {
        None => None,
        Some(p) => {
            if p.is_empty() || p.len() > 5 || !p.bytes().all(|b| b.is_ascii_digit()) {
                return Err(UrlError("bad port"));
            }
            if p.starts_with('0') {
                return Err(UrlError("port with a leading zero"));
            }
            match p.parse::<u16>() {
                Ok(n) if n > 0 => Some(n),
                _ => return Err(UrlError("bad port")),
            }
        }
    };
    Ok((host, port))
}

fn parse_name(host: &str) -> Result<String, UrlError> {
    if host.is_empty() || host.len() > 253 {
        return Err(UrlError("bad host length"));
    }
    let host = host.to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    for l in &labels {
        let ok = !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
        if !ok {
            return Err(UrlError("bad host name"));
        }
    }
    // WHATWG URL reads a host whose last label is numeric (or hex) as an IPv4 address in one
    // of several spellings; accept only the dotted quad, which both sides print the same.
    let last = labels[labels.len() - 1];
    if last.bytes().all(|b| b.is_ascii_digit()) || last.starts_with("0x") {
        let ip: Ipv4Addr = host.parse().map_err(|_| UrlError("bad IPv4 address"))?;
        if ip.to_string() != host {
            return Err(UrlError("non-canonical IPv4 address"));
        }
    }
    Ok(host)
}

fn is_loopback(host: &str) -> bool {
    if host == "localhost" {
        return true;
    }
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return ip.is_loopback();
    }
    if let Ok(ip) = host.parse::<Ipv6Addr>() {
        return ip.is_loopback();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_normalize() {
        for (url, origin) in [
            ("wss://relay.example.com", "wss://relay.example.com"),
            ("WSS://Relay.Example.COM/", "wss://relay.example.com"),
            (
                "wss://relay.example.com:443/x/y/",
                "wss://relay.example.com",
            ),
            (
                "wss://relay.example.com:8443",
                "wss://relay.example.com:8443",
            ),
            ("ws://127.0.0.1:80", "ws://127.0.0.1"),
            ("ws://127.0.0.1:8787", "ws://127.0.0.1:8787"),
            ("ws://localhost:1", "ws://localhost:1"),
            ("ws://[::1]:9000", "ws://[::1]:9000"),
            ("ws://[0:0:0:0:0:0:0:1]", "ws://[::1]"),
            ("wss://[2001:DB8::1]", "wss://[2001:db8::1]"),
            ("wss://10.0.0.1", "wss://10.0.0.1"),
        ] {
            assert_eq!(normalize_origin(url).as_deref(), Ok(origin), "{url}");
        }
    }

    #[test]
    fn refuses_what_could_normalize_differently() {
        for url in [
            "https://relay.example.com",
            "relay.example.com",
            "ws://relay.example.com",
            "ws://10.0.0.1",
            "wss://relay.example.com?x=1",
            "wss://relay.example.com/#f",
            "wss://user@relay.example.com",
            "wss://relay.example.com/%2e%2e",
            "wss://relay.example.com/a/../b",
            "wss://relay.example.com/a//b",
            "wss://relay.example.com:0",
            "wss://relay.example.com:0443",
            "wss://relay.example.com:65536",
            "wss://relay.example.com:",
            "wss://127.1",
            "wss://0x7f.0.0.1",
            "wss://1.2.3.04",
            "wss://-bad.example.com",
            "wss://bad..example.com",
            "wss://relay.example.com.",
            "wss://réla y.example",
            "wss://[::1",
            "wss://[::ffff:1.2.3.4]",
            "wss://[::1]x",
            "wss://",
            "wss://a\\b",
        ] {
            assert!(RelayUrl::parse(url).is_err(), "{url} parsed");
        }
        assert!(RelayUrl::parse(&format!("wss://a.example/{}", "x".repeat(600))).is_err());
    }

    #[test]
    fn ring_endpoint_appends_to_the_path() {
        let ring = RingId::parse("abcdefghijklmnopqrstuvwxyz").unwrap();
        let u = RelayUrl::parse("wss://r.example.com:8443/base/").unwrap();
        assert_eq!(
            u.ring_endpoint(&ring),
            "wss://r.example.com:8443/base/v1/ring/abcdefghijklmnopqrstuvwxyz"
        );
    }
}
