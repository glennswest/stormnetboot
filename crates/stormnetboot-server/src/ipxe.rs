//! iPXE script rendering.
//!
//! The script is rendered per host so the initramfs stays dumb: everything it
//! needs to find its root arrives on the kernel command line.

use crate::config::Config;

/// Kernel command line fragment describing where root comes from.
///
/// With no portal configured we deliberately emit nothing rather than a
/// guess — a node that stops in the initramfs is recoverable, a node that
/// attaches the wrong volume is not.
fn root_cmdline(cfg: &Config) -> String {
    match cfg.portal.as_deref() {
        Some(portal) => format!(
            "rd.stormblock.portal={portal} rd.stormblock.port={} root=/dev/ublkb0",
            cfg.portal_port
        ),
        None => String::new(),
    }
}

/// Render the per-host boot script.
///
/// `mac` is echoed back so a machine can tell from its own console which
/// identity the server matched it to.
pub fn render(cfg: &Config, mac: Option<&str>) -> String {
    let base = cfg.base_url();
    let mut cmdline = root_cmdline(cfg);
    if !cfg.extra_cmdline.is_empty() {
        if !cmdline.is_empty() {
            cmdline.push(' ');
        }
        cmdline.push_str(&cfg.extra_cmdline);
    }

    let mut script = String::with_capacity(512);
    script.push_str("#!ipxe\n\n");
    match mac {
        Some(mac) => script.push_str(&format!("# host {mac}\n")),
        None => script.push_str("# no MAC supplied; serving the default boot\n"),
    }

    if cfg.portal.is_none() {
        script.push_str(
            "# WARNING: no NVMe/TCP portal configured on this server. The kernel\n\
             # will boot but the initramfs has nowhere to attach root from.\n",
        );
    }

    script.push_str(&format!(
        "\nset base {base}\n\
         kernel ${{base}}/boot/vmlinuz initrd=initramfs.img {cmdline}\n\
         initrd ${{base}}/boot/initramfs.img\n\
         boot\n"
    ));
    script
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn cfg(args: &[&str]) -> Config {
        let mut argv = vec!["stormnetboot-server"];
        argv.extend_from_slice(args);
        Config::parse_from(argv)
    }

    #[test]
    fn renders_portal_when_configured() {
        let script = render(&cfg(&["--portal", "10.0.0.5"]), Some("aa:bb:cc:dd:ee:ff"));
        assert!(script.starts_with("#!ipxe"));
        assert!(script.contains("rd.stormblock.portal=10.0.0.5"));
        assert!(script.contains("rd.stormblock.port=4420"));
        assert!(script.contains("root=/dev/ublkb0"));
        assert!(script.contains("# host aa:bb:cc:dd:ee:ff"));
        assert!(!script.contains("WARNING"));
    }

    #[test]
    fn warns_instead_of_guessing_without_a_portal() {
        let script = render(&cfg(&[]), None);
        assert!(script.contains("WARNING"));
        assert!(!script.contains("rd.stormblock.portal"));
    }

    #[test]
    fn base_url_trailing_slash_does_not_double_up() {
        let script = render(&cfg(&["--base-url", "http://boot.lo:8080/"]), None);
        assert!(script.contains("set base http://boot.lo:8080\n"));
    }

    #[test]
    fn extra_cmdline_is_appended() {
        let script = render(&cfg(&["--portal", "10.0.0.5", "--extra-cmdline", "console=ttyS0"]), None);
        assert!(script.contains("rd.stormblock.portal=10.0.0.5 console=ttyS0"));
    }
}
