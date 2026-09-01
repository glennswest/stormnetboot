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

use std::process::ExitCode;

use crate::{cmdline::BootParams, report::Reporter};

fn main() -> ExitCode {
    // Log to the console the way an initramfs must: no subscriber machinery,
    // no files, just stdout, which is the kernel console at this point.
    let cmdline = match steps::read_cmdline() {
        Ok(raw) => raw,
        Err(err) => {
            eprintln!("stormnetboot-init: cannot read /proc/cmdline: {err}");
            return ExitCode::FAILURE;
        }
    };

    let params = BootParams::parse(&cmdline);
    let reporter = Reporter::new(params.report_url.clone(), params.mac.clone());

    match steps::run(&params, &reporter) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("stormnetboot-init: {err:#}");
            reporter.failed(&format!("{err:#}"));
            // Do not exit: PID 1 exiting panics the kernel and the operator
            // loses the message they need. Hand them a shell instead.
            steps::emergency_shell();
            ExitCode::FAILURE
        }
    }
}
