//! Progress reporting from inside the initramfs.
//!
//! Deliberately the crudest HTTP client that can work: a hand-written POST over
//! a raw TCP socket, no TLS, no async runtime, no dependency that has to be in
//! the initramfs. Reporting is best-effort by design — a boot must never fail
//! because the thing watching it was unreachable.

use std::{
    io::Write as _,
    net::{TcpStream, ToSocketAddrs as _},
    time::Duration,
};

const TIMEOUT: Duration = Duration::from_secs(3);

pub struct Reporter {
    /// Base URL of the boot server, e.g. `http://boot.storm.lo:8080`.
    url: Option<String>,
    mac: Option<String>,
}

impl Reporter {
    pub fn new(url: Option<String>, mac: Option<String>) -> Self {
        Self { url, mac }
    }

    pub fn phase(&self, phase: &str, detail: Option<&str>) {
        if let Err(err) = self.try_post(phase, detail) {
            // Printed, never fatal.
            eprintln!("stormnetboot-init: report {phase} failed: {err}");
        }
    }

    pub fn failed(&self, detail: &str) {
        self.phase("failed", Some(detail));
    }

    fn try_post(&self, phase: &str, detail: Option<&str>) -> std::io::Result<()> {
        let (Some(url), Some(mac)) = (&self.url, &self.mac) else {
            return Ok(());
        };

        let (host_port, host_header) = split_url(url)?;
        let body = build_body(mac, phase, detail);

        let addr = host_port
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| std::io::Error::other(format!("cannot resolve {host_port}")))?;

        let mut stream = TcpStream::connect_timeout(&addr, TIMEOUT)?;
        stream.set_write_timeout(Some(TIMEOUT))?;
        stream.set_read_timeout(Some(TIMEOUT))?;

        write!(
            stream,
            "POST /api/v1/report HTTP/1.1\r\n\
             Host: {host_header}\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        )?;
        stream.flush()?;
        Ok(())
    }
}

/// Split `http://host:port/...` into the connect target and the Host header.
fn split_url(url: &str) -> std::io::Result<(String, String)> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| std::io::Error::other(format!("only http:// is supported here: {url}")))?;
    let authority = rest.split('/').next().unwrap_or(rest);
    if authority.is_empty() {
        return Err(std::io::Error::other(format!("no host in {url}")));
    }

    let connect = if authority.contains(':') {
        authority.to_owned()
    } else {
        format!("{authority}:80")
    };
    Ok((connect, authority.to_owned()))
}

/// Minimal JSON, escaped by hand — pulling serde into the initramfs to emit
/// three fields is not a trade worth making.
fn build_body(mac: &str, phase: &str, detail: Option<&str>) -> String {
    let mut body = format!(
        "{{\"mac\":\"{}\",\"phase\":\"{}\"",
        escape(mac),
        escape(phase)
    );
    if let Some(detail) = detail {
        body.push_str(&format!(",\"detail\":\"{}\"", escape(detail)));
    }
    body.push('}');
    body
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_json_the_server_accepts() {
        let body = build_body("aa:bb:cc:dd:ee:ff", "running", None);
        assert_eq!(body, r#"{"mac":"aa:bb:cc:dd:ee:ff","phase":"running"}"#);

        let body = build_body("aa:bb:cc:dd:ee:ff", "failed", Some("attach timed out"));
        assert_eq!(
            body,
            r#"{"mac":"aa:bb:cc:dd:ee:ff","phase":"failed","detail":"attach timed out"}"#
        );
    }

    #[test]
    fn escapes_detail_so_a_kernel_message_cannot_break_the_json() {
        let body = build_body("m", "failed", Some("said \"no\"\nand\\stopped"));
        assert!(body.contains(r#"said \"no\"\nand\\stopped"#));
        // Must still be one line of valid-looking JSON.
        assert!(!body[1..].contains('\n'));
    }

    #[test]
    fn splits_urls_with_and_without_ports() {
        assert_eq!(
            split_url("http://boot.storm.lo:8080").unwrap(),
            ("boot.storm.lo:8080".into(), "boot.storm.lo:8080".into())
        );
        assert_eq!(
            split_url("http://boot.storm.lo/x").unwrap(),
            ("boot.storm.lo:80".into(), "boot.storm.lo".into())
        );
    }

    #[test]
    fn rejects_schemes_it_cannot_speak() {
        assert!(split_url("https://boot.storm.lo").is_err());
        assert!(split_url("boot.storm.lo:8080").is_err());
    }

    #[test]
    fn a_reporter_with_no_url_is_silently_inert() {
        let r = Reporter::new(None, Some("aa:bb:cc:dd:ee:ff".into()));
        // Must not panic or block; there is nothing to report to.
        r.phase("running", None);
        assert!(r.try_post("running", None).is_ok());
    }
}
