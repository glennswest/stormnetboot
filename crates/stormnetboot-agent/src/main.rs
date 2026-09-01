//! stormnetboot-agent — reports a node's own boot progress after switch_root.
//!
//! Runs on the booted node as an ordinary service. Its job is the phases
//! nothing else can see: the node is up, assimilation is running, assimilation
//! finished and the network source is no longer needed. It also reports the
//! hardware inventory that would otherwise require Ironic's separate
//! inspection boot — here it is just a running machine describing itself.

mod flowover;
mod inventory;

use std::{
    io::{BufRead as _, BufReader},
    time::Duration,
};

use anyhow::Context as _;
use clap::Parser;

use crate::flowover::Event;

#[derive(Debug, Parser)]
#[command(
    name = "stormnetboot-agent",
    version,
    about = "Reports node boot and assimilation progress to stormnetboot-server"
)]
struct Args {
    /// Boot server base URL, e.g. `http://boot.storm.lo:8080`.
    #[arg(long, env = "STORMNETBOOT_REPORT_URL")]
    report_url: String,

    /// This node's MAC, as the boot server knows it.
    ///
    /// Defaults to the MAC on the interface holding the default route, which
    /// is the one that PXE booted.
    #[arg(long, env = "STORMNETBOOT_MAC")]
    mac: Option<String>,

    /// Journal unit or log file carrying the engine's output.
    ///
    /// Flow-over has no status API; its progress exists only as lines here.
    #[arg(long, env = "STORMNETBOOT_ENGINE_LOG")]
    engine_log: Option<String>,

    /// Report inventory and exit, rather than following the log.
    #[arg(long, default_value_t = false)]
    once: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let mac = match &args.mac {
        Some(mac) => mac.clone(),
        None => inventory::primary_mac().context(
            "could not determine this node's MAC; pass --mac so reports can be matched to the host",
        )?,
    };

    let client = Reporter {
        url: args.report_url.trim_end_matches('/').to_owned(),
        mac,
    };

    // The node is running — that is the first thing worth saying, and it
    // carries the inventory that replaces an inspection boot.
    let inv = inventory::collect();
    client.report("running", Some(&inv.summary()));
    println!("stormnetboot-agent: reported running ({})", inv.summary());

    if args.once {
        return Ok(());
    }

    match &args.engine_log {
        Some(path) => follow_engine_log(&client, path)?,
        None => {
            // Nothing to follow: stay alive so the service does not flap, but
            // say plainly that assimilation will not be reported.
            eprintln!(
                "stormnetboot-agent: no --engine-log; assimilation progress will not be reported"
            );
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
    }

    Ok(())
}

/// Follow the engine's output and turn flow-over lines into phase reports.
fn follow_engine_log(client: &Reporter, path: &str) -> anyhow::Result<()> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {path}"))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut extent_failures = 0u32;

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                // End of file: the engine is still running and will write more.
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
            Ok(_) => {}
            Err(err) => return Err(err).context("reading engine log"),
        }

        let Some(event) = flowover::parse_line(&line) else {
            continue;
        };

        match event {
            Event::Started { disk } => {
                client.report("assimilating", Some(&format!("flow-over onto {disk}")));
            }
            Event::Complete { moved, failed } => {
                let detail = format!("{moved} extent(s) local, {failed} failed");
                if failed > 0 {
                    client.report("assimilating", Some(&detail));
                } else {
                    // Assimilation done: this node no longer needs the network
                    // source. That is the boot-complete signal.
                    client.report("local", Some(&detail));
                }
                return Ok(());
            }
            Event::Aborted => {
                client.report("failed", Some("flow-over aborted after repeated failures"));
                return Ok(());
            }
            Event::ExtentFailed { detail } => {
                extent_failures += 1;
                // Individual failures are noise until they accumulate; the
                // engine gives up after 16, so surface the trend before that.
                if extent_failures.is_multiple_of(4) {
                    client.report(
                        "assimilating",
                        Some(&format!("{extent_failures} extent failures: {detail}")),
                    );
                }
            }
        }
    }
}

struct Reporter {
    url: String,
    mac: String,
}

impl Reporter {
    fn report(&self, phase: &str, detail: Option<&str>) {
        if let Err(err) = self.try_report(phase, detail) {
            // Never fatal: a node must not fail because the watcher is down.
            eprintln!("stormnetboot-agent: reporting {phase} failed: {err}");
        }
    }

    fn try_report(&self, phase: &str, detail: Option<&str>) -> anyhow::Result<()> {
        let mut body = serde_json::json!({ "mac": self.mac, "phase": phase });
        if let Some(detail) = detail {
            body["detail"] = serde_json::Value::String(detail.to_owned());
        }

        let resp = ureq_post(&format!("{}/api/v1/report", self.url), &body.to_string())?;
        if !(200..300).contains(&resp) {
            anyhow::bail!("boot server returned {resp}");
        }
        Ok(())
    }
}

/// Minimal HTTP POST.
///
/// The agent ships inside a node image where every dependency is weight, and
/// this is one request to one known endpoint.
fn ureq_post(url: &str, body: &str) -> anyhow::Result<u16> {
    use std::io::{Read as _, Write as _};

    let rest = url
        .strip_prefix("http://")
        .context("only http:// is supported")?;
    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    let connect = if authority.contains(':') {
        authority.to_owned()
    } else {
        format!("{authority}:80")
    };

    let mut stream = std::net::TcpStream::connect(&connect)
        .with_context(|| format!("connecting to {connect}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;

    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()?;

    let mut response = String::new();
    stream.read_to_string(&mut response).ok();
    parse_status(&response).context("no status line in response")
}

fn parse_status(response: &str) -> Option<u16> {
    response
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_status_line() {
        assert_eq!(parse_status("HTTP/1.1 202 Accepted\r\n\r\n"), Some(202));
        assert_eq!(parse_status("HTTP/1.1 503 Service Unavailable"), Some(503));
        assert_eq!(parse_status(""), None);
        assert_eq!(parse_status("garbage"), None);
    }
}
