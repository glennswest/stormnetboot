//! Claiming an image from sbregistry, keyed on the service tag.
//!
//! HTTP is spoken directly over TCP4 rather than through EFI_HTTP_PROTOCOL.
//! One request is a hundred lines; EFI_HTTP is a whole driver stack that
//! firmware may not carry, and the NVMe path needs TCP4 regardless — so this
//! keeps the extension to exactly one protocol dependency instead of two.
//!
//! The response parser is deliberately not a JSON parser. The claim reply is a
//! small flat object from a service we control, and a boot-critical binary is
//! the wrong place to grow a parser for it.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::tcp4::Tcp4Socket;

/// Everything needed to attach, as sbregistry reports it.
///
/// Taken from the **response**, never assumed from the request: older
/// stormblockmk ignores the requested protocol and exports iSCSI regardless,
/// so a client that assumed its own request had been honoured would attach
/// nothing and blame the network.
#[derive(Debug, Clone)]
pub struct Attach {
    pub address: [u8; 4],
    pub port: u16,
    pub nqn: String,
    pub nsid: u32,
}

pub fn parse_ipv4(s: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut parts = s.trim().split('.');
    for slot in out.iter_mut() {
        *slot = parts.next()?.parse::<u8>().ok()?;
    }
    parts.next().is_none().then_some(out)
}

/// Pull one field out of a flat JSON object.
fn field(body: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let at = body.find(&needle)? + needle.len();
    let rest = body[at..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    if let Some(s) = rest.strip_prefix('"') {
        let end = s.find('"')?;
        Some(s[..end].to_string())
    } else {
        let end = rest.find([',', '}', '\n'])?;
        let v = rest[..end].trim();
        (!v.is_empty()).then(|| v.to_string())
    }
}

fn request(
    server: [u8; 4],
    port: u16,
    host: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> Result<String, String> {
    let mut sock = Tcp4Socket::connect(server, port)?;

    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\n");
    req.push_str("User-Agent: stormbootx\r\nAccept: application/json\r\nConnection: close\r\n");
    if let Some(b) = body {
        req.push_str("Content-Type: application/json\r\n");
        req.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    req.push_str("\r\n");
    if let Some(b) = body {
        req.push_str(b);
    }

    sock.send(req.as_bytes())?;
    let raw = sock.read_to_end(256 * 1024)?;
    String::from_utf8(raw).map_err(|_| "response was not UTF-8".to_string())
}

fn split_response(response: &str) -> Result<(u16, &str), String> {
    let status = response
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or("no HTTP status line")?;
    let body = response.split("\r\n\r\n").nth(1).unwrap_or("");
    Ok((status, body))
}

/// Claim an image for this machine.
///
/// The service tag is the `consumer`, which is exactly what that field is for
/// — sbregistry sets it late precisely so a warm clone can be bound to whoever
/// turns out to need it. It also makes `GET /v1/clones?consumer=<tag>` the
/// answer to "what is this machine booting?".
pub fn claim(
    server: [u8; 4],
    port: u16,
    host: &str,
    golden: &str,
    service_tag: &str,
) -> Result<Attach, String> {
    let body = format!("{{\"golden\":\"{golden}\",\"consumer\":\"{service_tag}\"}}");
    let response = request(server, port, host, "POST", "/v1/clones/claim", Some(&body))?;
    let (status, body) = split_response(&response)?;
    if !(200..300).contains(&status) {
        return Err(format!("claim returned HTTP {status}: {}", body.trim()));
    }
    attach_from(body)
}

/// Look for a clone this machine already holds, so a reboot reattaches the
/// same volume instead of minting another.
pub fn existing(
    server: [u8; 4],
    port: u16,
    host: &str,
    service_tag: &str,
) -> Result<Option<Attach>, String> {
    let path = format!("/v1/clones?consumer={service_tag}");
    let response = request(server, port, host, "GET", &path, None)?;
    let (status, body) = split_response(&response)?;
    if !(200..300).contains(&status) {
        return Err(format!("lookup returned HTTP {status}"));
    }
    if body.trim() == "[]" || body.trim().is_empty() {
        return Ok(None);
    }
    attach_from(body).map(Some)
}

fn attach_from(body: &str) -> Result<Attach, String> {
    let address = field(body, "address")
        .and_then(|a| parse_ipv4(&a))
        .ok_or("no usable \"address\" in the response")?;
    let nqn = field(body, "nqn").ok_or("no \"nqn\" in the response")?;
    let port = field(body, "port")
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(4420);
    let nsid = field(body, "nsid")
        .and_then(|n| n.parse::<u32>().ok())
        .unwrap_or(1);
    Ok(Attach {
        address,
        port,
        nqn,
        nsid,
    })
}

/// Everything after the last `/`, for logging a digest without the noise.
pub fn short(s: &str) -> &str {
    s.rsplit(['/', ':']).next().unwrap_or(s)
}

#[allow(dead_code)]
pub fn to_vec(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}
