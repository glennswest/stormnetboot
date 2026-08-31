# stormnetboot

Componentized network boot for the Storm platform. A machine PXE-boots a tiny
payload (iPXE → kernel + ~5 MB initramfs), brings its root up over **NVMe/TCP**
from a stormblock appliance, and then — while running — **zeroboots** (flows
over) the assets onto its local system disk. The same service drives upgrades
and bare-metal recovery. It replaces the monolithic install ISO / disk image
with a boot payload measured in megabytes.

stormnetboot is part of the PXE-chain rewrite: the legacy chain (pxemanager, a
monolithic Go binary on RouterOS) is replaced by storm-native components that
**stormcos itself hosts**. The boot server is not a special appliance — it is a
component running on a stormcos node, projecting signed pallets over HTTP/TFTP.

## Where it sits

| Project | Role in this design |
|---|---|
| `stormblock` | The engine. NVMe-oF/TCP target (`:4420`), its own NVMe/TCP *initiator* (`nvme-tcp://` device URIs), ublk local export, pallets API (`:9090`), `boot-local --local-disk` flow-over ("zeroboot"). Explicitly not responsible for PXE. |
| `stormblock-registry` (sbregistry) | Source of truth for content: OCI pallet/stack artifacts, signed boot pallets (kernel + initramfs + cmdline), goldens → per-host CoW clones with NVMe/TCP attach info (`/v1/clones/claim`). |
| `stormcos` | The OS being booted. Kernel already validated for `NVME_TCP`, `BLK_DEV_UBLK`, `EROFS_FS`. Its architecture doc names the netboot leg as the open question — stormnetboot is that leg. |
| `stormuefi` + `pallet-format` | The local boot chain the assimilated node ends up on (ESP → pallet select with A/B fallback). |
| `microdns` | DHCP. Per-reservation `next_server`, `boot_file`, `boot_file_efi`, `ipxe_boot_url` already exist — DHCP is **not** stormnetboot's job. |
| `ipxe` (fork) | Provides `ipxe.efi`, `snponly.efi`, `undionly.kpxe` with the HTTP read-ahead patches. stormnetboot serves these for chainload. |
| `pxemanager` | The legacy Go monolith this rewrite retires. |
| `stormupgrade` | The fleet upgrade operator on stormcos. Uses this project as its recovery path and the same pallet channels for content. |
| `rustkube` | The orchestrator (with rustkube-node). Schedules the boot-chain components; mkube is not used anywhere, including Rose. |
| `stormconsole` / `stormview` | The StormCOS console. stormnetboot ships a stormview component feed so netboot state shows up as its own console panel. |

stormblock documented a `serve-boot` HTTP surface (`docs/stormblock-ipxe-boot.md`)
but never implemented it, and its own guidance says PXE is not the engine's job.
stormnetboot implements that surface as its own component.

## Boot flow

```
PXE ROM ──TFTP──▶ iPXE (snponly.efi / undionly.kpxe)
   │
   ├─HTTP─▶ GET /boot.ipxe?mac=...      per-host script from stormnetboot-server
   ├─HTTP─▶ GET /boot/vmlinuz            ┐ projected from the ACTIVE signed
   ├─HTTP─▶ GET /boot/initramfs.img      ┘ boot pallet — never baked in
   │
   ▼
/init (stormnetboot-init, static musl)
   ├─ bring up NIC, parse rd.stormblock.* cmdline
   ├─ start stormblock with the claimed CoW clone as an
   │    nvme-tcp://appliance:4420/<nqn>?nsid=N backing device
   ├─ export root as /dev/ublkb0
   ▼
switch_root → stormcos runs with root on the appliance
   │
   ▼  (background, node fully operational)
zeroboot flow-over: stormblock boot-local --local-disk
   ├─ format system drive as bootable stormblock volume
   ├─ RAID-1 against the network leg, rebuild extent-by-extent
   ├─ publish boot/kernel/system pallets to local GPT, stormuefi → ESP
   └─ break mirror, drop the network source
   ▼
next boot: stormuefi from local disk — no network dependency
```

Target: ~25 s power-on to login on the network leg (per the timing table in
`stormblock/docs/stormblock-ipxe-boot.md`), with assimilation converging in the
background after that.

## Components (Rust workspace)

1. **`stormnetboot-server`** — the boot asset service, hosted on stormcos
   (container under stormpump, scheduled by rustkube):
   - TFTP for firmware chainload only (`undionly.kpxe`, `ipxe.efi`,
     `snponly.efi`); everything after that is HTTP. UEFI HTTP boot skips TFTP
     entirely where firmware supports it.
   - HTTP: `/boot/vmlinuz`, `/boot/initramfs.img` (streamed out of the active
     signed boot pallet), `/boot.ipxe?mac=...`, `/boot/` listing, `/health`,
     `/metrics`.
   - Host resolution: MAC/serial → node record → claim a CoW clone from
     sbregistry (`/v1/clones/claim`) → render the `rd.stormblock.*` cmdline
     with the returned NVMe/TCP attach info. Claims happen server-side at
     script-render time so the initramfs stays dumb.
2. **`stormnetboot-init`** — the initramfs `/init` (static musl). Extends
   stormblock's `scripts/build-stormblock-initramfs.sh` flow: NIC up, cmdline
   parse, stormblock up with the remote volume, `/dev/ublkb0`, `switch_root`.
3. **Assimilation sequencing** — after `switch_root`, drive the existing engine
   flow-over (`boot-local --local-disk`), pallet publish to local GPT, stormuefi
   into the ESP, mirror break. The engine does the work; stormnetboot sequences
   it and reports status.

## Transport: NVMe/TCP via stormblock's own initiator

Root always arrives as `/dev/ublkb0`, with **stormblock in the datapath as the
NVMe/TCP initiator** (`nvme-tcp://` backing device) rather than the kernel
initiator + nvme-cli:

- Uniform datapath: remote, mirrored, and local root are all ublk — neither
  `switch_root` nor the flow-over cares which phase the node is in.
- Flow-over *requires* stormblock owning the volume (it is a RAID-1 leg swap).
- One static binary in the initramfs; no nvme-cli, no `/dev/nvme-fabrics`
  choreography.

Kernel `nvme_tcp` stays in the initramfs as a debug/fallback path.

## Storage tiering

Two classes of stormblock server participate:

- **Bulk appliance** (e.g. the 240 TB unit): holds goldens, serves the per-host
  CoW root clones over NVMe/TCP, and is the source the flow-over drains from.
- **Rose / RouterOS stormblock** (storage-limited): can host the *boot chain*
  at the network edge — iPXE binaries, boot pallet projection — because that
  footprint is megabytes, not terabytes. It never holds goldens or clones.

The tiering falls out of the design: stormnetboot-server is tiny and stateless
(it projects pallets it fetches by digest), so it runs anywhere, while the data
plane stays on the appliance with the capacity.

## Upgrades and recovery — the same service

- **Upgrade in place**: a running node pulls new pallets from the same
  sbregistry, publishes them to the local GPT, activates with `tries_left`,
  reboots through stormuefi. Rollback is a GPT attribute write, never a data
  write. Fleet-level policy (channels, waves, health gates) is `stormupgrade`,
  the operator project designed alongside this one.
- **Recovery**: a node that cannot boot locally re-PXEs and re-assimilates.
  The netboot path *is* the reinstall path — there is no separate installer,
  which keeps stormcos's "no installer" stance intact.

## Console integration

stormconsole is pluggable: every domain contributes its own components through
the stormview contract, aggregated at `/api/v1/components` (+
`/ws/components`). `stormnetboot-server` publishes a stormview feed so the
console gets a netboot panel with no console-side code:

- Hosts currently netbooting, with phase — chainload → kernel fetch →
  NVMe/TCP attach → `switch_root` → assimilating → local.
- Assimilation progress per host (flow-over extents migrated, mirror state).
- The active signed boot pallet (version, digest, signature state) and which
  hosts booted from which pallet version.
- Clone claims outstanding against sbregistry, and TFTP/HTTP fetch activity.

Live updates ride the same feed, so a rack of machines PXE-booting is
watchable from the fleet view in real time.

## Open questions

- Boot pallet source of truth: sbregistry OCI artifact (recommended) with the
  engine's on-drive pallet as local cache, or engine-first.
- Host identity: MAC only, or SMBIOS serial/UUID via iPXE variables (needed
  for multi-NIC hosts).
- Whether `stormblock` grows a `boot-nvme` orchestrator mirroring
  `boot_iscsi.rs`, or `BootLocal` learns an `nvme-tcp://` slab source — engine
  work, tracked as stormblock issues, not patched here.
- Host records as a rustkube resource vs microdns-only.

## Building

Per the cross-project rules: **all builds run on `root@dev.g8.lo`**, with
`CARGO_TARGET_DIR=/build/cargo/stormnetboot`. Editing on the Mac is fine;
trusting a Mac build is not.
