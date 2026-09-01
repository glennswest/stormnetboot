# Changelog

## [Unreleased]

### 2026-09-01
- **docs:** Capacity section with measured numbers (194-227 hosts/s, 58 MB RSS at 500 concurrent; wire-bound at ~76 hosts/s on 10 GbE) and an explicit "how it wires into stormcos" table separating what is built from what is planned.
- **feat:** `stormnetboot-server` v0.1.0 — Rust/axum boot asset service:
  per-host rendered `/boot.ipxe`, asset serving under `/boot/` with Range
  support, `/boot.json` listing, `/health`, `/readyz` gated on the kernel and
  initramfs actually being present, Prometheus `/metrics`, and graceful
  SIGTERM shutdown for running under stormpump.
- **fix:** Corrected an iPXE render test that asserted the wrong cmdline
  ordering (extra cmdline appends after `root=`, not after the portal).
- **docs:** Image loadouts: `min` (boot+kernel, multi-stage from an
  appliance) and `max` (full stack, `--offline`, one-and-done) as named
  stacks; ISO / raw / qcow2 from one spec; max is what bootstraps the first
  appliance and covers air-gapped sites.

### 2026-08-31
- **docs:** Initial architecture: componentized PXE chain hosted on stormcos;
  tiny kernel/initramfs boot payload; root over NVMe/TCP from a stormblock
  appliance via the engine's own initiator + ublk; zeroboot flow-over to local
  disk; same service for upgrades and recovery; storage tiering between bulk
  appliance and Rose edge.
- **docs:** Project CLAUDE.md with work plan (phases 0–7) and context notes.
- **docs:** Correct orchestrator: rustkube (+ rustkube-node) everywhere,
  never mkube — including Rose. All mkube references translated.
- **docs:** Re-survey against current platform: mkube formally retired
  2026-08-27 for rustkube + fastetcd + stormboot; add pxe-operator as the
  control-plane half of the PXE rewrite (scope split is a phase-3 open
  question); note digest-only pallet serving per the no-auto-update rule.
- **docs:** Console integration: stormview component feed from
  stormnetboot-server (netboot phases, assimilation progress, boot pallet
  versions, clone claims) rendered as a stormconsole panel; new phase 8.
- **docs:** OpenShift/Metal3 alignment section (BMH resource → ForcePXE →
  this chain; golden ≈ image, flow-over ≈ deploy, assimilation-complete ≈
  boot-complete); operator split clarified against the live Go bmh-operator
  and the deferred bmh-operator-rs stub; appliance VM detail (impulse1 data
  volume, hosts IPMI/BMH layer and updates).
- **docs:** Design principles: Kubernetes/OpenShift look and feel (spec/
  status, standard conditions, printer columns, Events, level-triggered
  idempotent reconcile, console plugin panel) and data-center constant-usage
  rules (no stop-the-world, background yields to foreground, resumable and
  idempotent, bounded for long uptime, observable by default).
- **docs:** HTTP-first boot: UEFI HTTP Boot and BMC virtual-media ISO as the
  normal paths, HTTP-served iPXE for chainload, TFTP demoted to a
  last-resort responder for legacy PXE ROMs. The appliance/storage cluster
  serves the HTTPS itself from the nodes holding the goldens — no separate
  web tier. Noted Redfish VirtualMedia as greenfield work.
- **docs:** Locality section: appliances across racks/rows/floors/sites,
  serving degrades rack → row → floor → site, reusing the existing
  stormdrive/stormblock failure-domain labels; policy deliberately TBD and
  configured via stormconsole.
- **docs:** Appliances run as an independent cluster; boot/asset services
  become ordinary Kubernetes Services (LB/failover/DNS from rustkube).
  Boot-storm section: many networks, any node can serve, HTTP ISO boot,
  load-balanced stateless boot tier, replicated goldens, and only the
  first few-MB hop as a file transfer — the rest demand-paged over NVMe/TCP.
- **docs:** Drop the inspection cycle: no agent ramdisk — the netboot boots
  full stormcos and inspection is a hardware report from the running node.
  New "Fleet birth and day 2" section: N nodes (3 → 10 000) as thin CoW
  clones of one golden; day-2 cluster join = boot.d profile on identified
  nodes, never a reprovision. Boot-storm pacing added to open questions.
- **docs:** Hosting decision: Rose ruled out (capacity + security posture);
  interim appliance is a stormblock VM on pve.g8.lo; stormbastion is the
  target host for the boot chain. Storage-tiering section replaced by
  Hosting.
- **docs:** Cross-link stormupgrade as the fleet upgrade policy layer.
- **chore:** Repository created.
