//! stormnetboot-init — PID 1 inside the netboot initramfs.
//!
//! Its whole job is to turn a kernel command line into a running stormcos on a
//! network root: bring up the interface, hand stormblock an `nvme-tcp://` slab,
//! wait for `/dev/ublkb0`, mount it and `switch_root`. The engine process
//! survives the switch and keeps serving the ublk device for the life of the
//! boot.
//!
//! Two rules shape everything here. **Never guess**: a machine that stops in
//! the initramfs with a clear message is recoverable, one that attaches the
//! wrong volume is not. And **always report**: nothing else can see a machine
//! between power-on and cluster join, so each step is posted back to the boot
//! server before it is attempted.

mod cmdline;
mod media;
mod report;
mod steps;

use crate::{cmdline::BootParams, report::Reporter};

/// PID 1 has exactly one way to finish: `switch_root`. Every other path holds
/// the console.
///
/// Returning from here — with any code — panics the kernel and replaces the
/// console with a panic screen, which erases the diagnostic that would say
/// why. `-> !` makes that a compile error rather than a discipline.
fn main() -> ! {
    // Log to the console the way an initramfs must: no subscriber machinery,
    // no files, just stdout, which is the kernel console at this point.
    let cmdline = match steps::read_cmdline() {
        Ok(raw) => raw,
        Err(err) => {
            // Not fatal in the "give up" sense: a shell can read the command
            // line by hand, and someone standing at the console can do more
            // with a prompt than with a panic.
            eprintln!("stormnetboot-init: cannot read /proc/cmdline: {err}");
            steps::emergency_shell();
        }
    };

    let params = BootParams::parse(&cmdline);
    let reporter = Reporter::new(params.report_url.clone(), params.mac.clone());

    match steps::run(&params, &reporter) {
        // `run` ends in switch_root, which never returns. Reaching here means
        // it finished without handing over, which is itself a failure.
        Ok(()) => {
            eprintln!("stormnetboot-init: boot finished without switch_root; nothing left to do");
            steps::emergency_shell();
        }
        Err(err) => {
            eprintln!("stormnetboot-init: {err:#}");
            // What it was working from, because the next person reading this
            // console has nothing else: the panic screen used to take even
            // this away.
            eprintln!("stormnetboot-init: cmdline was: {cmdline}");
            reporter.failed(&format!("{err:#}"));
            steps::emergency_shell();
        }
    }
}
