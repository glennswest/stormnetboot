# Changelog

## [Unreleased]

### 2026-08-31
- **docs:** Initial architecture: componentized PXE chain hosted on stormcos;
  tiny kernel/initramfs boot payload; root over NVMe/TCP from a stormblock
  appliance via the engine's own initiator + ublk; zeroboot flow-over to local
  disk; same service for upgrades and recovery; storage tiering between bulk
  appliance and Rose edge.
- **docs:** Project CLAUDE.md with work plan (phases 0–7) and context notes.
- **docs:** Cross-link stormupgrade as the fleet upgrade policy layer.
- **chore:** Repository created.
