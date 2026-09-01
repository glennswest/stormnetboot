//! Reading flow-over progress out of the engine's output.
//!
//! `boot-local --local-disk` has no status API and writes no status file — the
//! only evidence that assimilation is happening, or has finished, is a handful
//! of lines on the engine's stdout. So the agent reads them.
//!
//! This is a genuine coupling to another project's log text, which is normally
//! a bad idea. It is the honest option here: the alternative is reporting
//! nothing at all for the phase that takes longest. The parser is deliberately
//! permissive about everything except the words it keys on, and an unmatched
//! line is simply not an event.

/// What a line of engine output told us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Flow-over began; the node is assimilating onto a local disk.
    Started { disk: String },
    /// Flow-over finished. `moved` extents are now local.
    Complete { moved: u64, failed: u64 },
    /// Migration gave up. The engine stops after repeated extent failures.
    Aborted,
    /// A single extent failed; not fatal on its own.
    ExtentFailed { detail: String },
}

/// Classify one line of engine output.
pub fn parse_line(line: &str) -> Option<Event> {
    let line = line.trim();

    // "Flow-over: migrating to local slab {id} on {disk} in background"
    if let Some(rest) = line.strip_prefix("Flow-over: migrating to local slab") {
        let disk = rest
            .split(" on ")
            .nth(1)
            .and_then(|s| s.split(" in background").next())
            .unwrap_or("")
            .trim()
            .to_owned();
        return Some(Event::Started { disk });
    }

    // "Flow-over complete: {moved} extent(s) now on local disk"
    if line.starts_with("Flow-over complete:") {
        let moved = first_number(line).unwrap_or(0);
        return Some(Event::Complete { moved, failed: 0 });
    }

    // "flow-over complete: {moved} extent(s) migrated, {failed} failed"
    if line.starts_with("flow-over complete:") {
        let numbers = all_numbers(line);
        return Some(Event::Complete {
            moved: numbers.first().copied().unwrap_or(0),
            failed: numbers.get(1).copied().unwrap_or(0),
        });
    }

    if line.starts_with("flow-over: aborting") {
        return Some(Event::Aborted);
    }

    if let Some(detail) = line.strip_prefix("flow-over: extent ") {
        return Some(Event::ExtentFailed {
            detail: detail.trim().to_owned(),
        });
    }

    None
}

fn all_numbers(line: &str) -> Vec<u64> {
    line.split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect()
}

fn first_number(line: &str) -> Option<u64> {
    all_numbers(line).into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_the_start_line_and_names_the_disk() {
        let event = parse_line("Flow-over: migrating to local slab 7 on /dev/sda in background");
        assert_eq!(
            event,
            Some(Event::Started {
                disk: "/dev/sda".into()
            })
        );
    }

    #[test]
    fn recognises_both_completion_spellings() {
        assert_eq!(
            parse_line("Flow-over complete: 1024 extent(s) now on local disk"),
            Some(Event::Complete {
                moved: 1024,
                failed: 0
            })
        );
        assert_eq!(
            parse_line("flow-over complete: 1024 extent(s) migrated, 3 failed"),
            Some(Event::Complete {
                moved: 1024,
                failed: 3
            })
        );
    }

    #[test]
    fn recognises_the_abort_that_would_otherwise_look_like_success() {
        // The engine gives up after repeated extent failures and says so only
        // in a log line. Missing this leaves a node reported as assimilating
        // forever.
        assert_eq!(
            parse_line("flow-over: aborting after repeated failures"),
            Some(Event::Aborted)
        );
    }

    #[test]
    fn extent_failures_are_events_but_not_terminal() {
        assert_eq!(
            parse_line("flow-over: extent Some(3)/12: device busy"),
            Some(Event::ExtentFailed {
                detail: "Some(3)/12: device busy".into()
            })
        );
    }

    #[test]
    fn ordinary_engine_chatter_is_not_an_event() {
        for line in [
            "",
            "stormblock 12.4.0 starting",
            "ublk: exported /dev/ublkb0",
            "INFO serving volume stormpump",
        ] {
            assert_eq!(parse_line(line), None, "line: {line:?}");
        }
    }

    #[test]
    fn leading_whitespace_and_timestamps_do_not_hide_a_completion() {
        assert_eq!(
            parse_line("   Flow-over complete: 5 extent(s) now on local disk  "),
            Some(Event::Complete {
                moved: 5,
                failed: 0
            })
        );
    }
}
