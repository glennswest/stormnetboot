# Changelog

## [Unreleased]

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
- **docs:** Hosting decision: Rose ruled out (capacity + security posture);
  interim appliance is a stormblock VM on pve.g8.lo; stormbastion is the
  target host for the boot chain. Storage-tiering section replaced by
  Hosting.
- **docs:** Cross-link stormupgrade as the fleet upgrade policy layer.
- **chore:** Repository created.
