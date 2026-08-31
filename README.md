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
| `rustkube` | The orchestrator (with rustkube-node), over fastetcd. Schedules the boot-chain components; mkube is retired (2026-08-27) and appears nowhere. |
| `pxe-operator` | The control-plane side of the PXE rewrite (rustkube watcher, skeleton today): reconciles host/boot resources, programs microdns `next_server`/boot files. stormnetboot-server is the data plane it points firmware at. |
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

## Hosting

The appliances form their **own independent cluster** — separate from every
workload cluster they provision. That separation is what makes the boot tier
ordinary: inside the appliance cluster, the boot and asset services are
plain Kubernetes Services (ClusterIP/LoadBalancer, EndpointSlices, DNS,
Gateway/Route where wanted — all of which rustkube already serves), so
load balancing, failover, and rollout of the boot tier are solved by the
platform instead of by bespoke code here. It also means the thing that
provisions a cluster never depends on that cluster being up.

- **Appliance cluster (interim: one member)**: a stormblock VM on
  `pve.g8.lo` — goldens, per-host CoW root clones over NVMe/TCP, the source
  the flow-over drains from — until the 240 TB unit comes up. Adding members
  later is the same operation as adding any node, and the services in front
  of them do not change.
- **`stormbastion` (target)**: a hardened bastion + asset host running
  stormcos, consolidating stormblock, sbregistry, stormnetboot-server, and the
  upgrade content source behind one audited front. See the stormbastion repo.
- **Rose is explicitly out.** A RouterOS container host has neither the
  capacity nor the security posture this chain needs; nothing in the boot path
  may depend on it.

stormnetboot-server stays tiny and stateless (it projects pallets it fetches
by digest), so it colocates with either host without coupling to it.

## Upgrades and recovery — the same service

- **Upgrade in place**: a running node pulls new pallets from the same
  sbregistry, publishes them to the local GPT, activates with `tries_left`,
  reboots through stormuefi. Rollback is a GPT attribute write, never a data
  write. Fleet-level policy (channels, waves, health gates) is `stormupgrade`,
  the operator project designed alongside this one.
- **Recovery**: a node that cannot boot locally re-PXEs and re-assimilates.
  The netboot path *is* the reinstall path — there is no separate installer,
  which keeps stormcos's "no installer" stance intact.

## OpenShift/Metal3 alignment

Provisioning semantics track OpenShift's Metal3/BMO model so the concepts
map: a BareMetalHost-shaped rustkube resource (BMC + credential Secret, boot
MAC, `online`, image) is the trigger; the operator sets ForcePXE over
IPMI/Redfish and power-cycles; the host lands on this boot chain.

The deliberate divergence: **no agent ramdisk, no inspection boot.** Ironic
boots ironic-python-agent to inspect, then boots again to deploy. Here the
netbooted OS *is* full stormcos on a network root — the deploy artifact and
the running system are the same thing — and zeroboot flow-over is the deploy
step, running while the node does real work. Inspection is a report from
running stormcos: first boot posts hardware inventory to the host's
BMH-shaped resource, so the inspecting phase costs no extra reboot cycle.
The Go bmh-operator's `POST /boot-complete` → set-boot-disk flow keeps its
storm equivalent: **assimilation-complete is the boot-complete signal**, at
which point the host flips to persistent local boot.

## Fleet birth and day 2

The single-node path is the fleet path. N servers — 3, 6, 10, 10 000 —
power on, PXE, and each claims its own thin CoW clone of the one stormcos
golden: appliance-side cost per node is metadata, not a copy. Flow-over then
drains each root to local disk in the background at whatever pace the
appliance can serve. Day 1 ends with N *identified*, standalone stormcos
nodes booting locally — identity (MAC/serial → name → role) is pinned in
the host records at PXE time.

Day 2 is cluster join, and it is deliberately not an install step: a node's
role is its boot.d `start` lines, so joining is applying a profile — start
rustkube-node everywhere, plus the control-plane units on the chosen
masters — driven from the identity established on day 1. Promotion later is
more `start` lines, never a reprovision.

### Boot storms don't happen

Several properties stack up so that scale never funnels through one server:

- **Thousands means many networks.** Fleets that size are spread across
  segments, each with its own DHCP scope and boot tier — microdns already
  runs per network. The unit of boot load is the segment, not the fleet.
- **Any node can serve.** Every assimilated stormcos node runs stormblock
  and can carry the boot tier — the server is a stateless pallet projection,
  so promoting a node to boot-server is a boot.d `start` line. The install
  capacity grows with the installed fleet.
- **HTTP ISO boot is a second front door.** The same image machinery emits
  ISOs (`stormblock image build --format iso`), so hosts can HTTP-boot an
  ISO instead of the PXE/TFTP path — useful for firmware without clean PXE,
  for BMC virtual-media, and as another way to spread load.
- **The boot tier load-balances.** Stateless servers behind one boot URL
  across a cluster of serving nodes; per-host state (claims, identity) lives
  in rustkube/sbregistry, not in the server. In the appliance cluster this
  is just a Service with several endpoints.
- **Only the first hop is a file transfer.** What firmware pulls over
  TFTP/HTTP is a few megabytes — iPXE, kernel, initramfs — and that is the
  whole PXE payload. Everything after it arrives over NVMe/TCP as blocks,
  demand-paged into a running system rather than downloaded up front, so the
  bytes that would be a "storm" in an image-push model never traverse the
  boot path at all.
- **Assets replicate.** Goldens are stormblock volumes, so they mirror
  across appliance-cluster members (and toward a segment) the same way any
  volume does. A node attaches to a replica near it; adding a replica adds
  serving capacity without changing what any client is configured to do.

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
- Flow-over pacing when many nodes on one segment assimilate from the same
  appliance at once: appliance-side throttle vs plan-side waves.
- Boot-serving promotion: what marks a node as a boot server (a boot.d
  profile bit, a rustkube resource, or automatic per-segment election).
- Replica placement policy: how many golden replicas per segment, who
  decides, and how a booting host is steered to a near one (DNS, Service
  topology hints, or an explicit portal in the rendered cmdline).
- Exact split with `pxe-operator` and `bmh-operator-rs`: today the Go
  bmh-operator does PXE serving and IPMI in one process; the target split is
  boot resources + DHCP (pxe-operator), BMC/power (bmh-operator-rs, deferred
  until core cutover — and Rust IPMI is greenfield), assets (this project).
  Whether clone claims happen operator-side or server-side needs settling
  before phase 3.

## Building

Per the cross-project rules: **all builds run on `root@dev.g8.lo`**, with
`CARGO_TARGET_DIR=/build/cargo/stormnetboot`. Editing on the Mac is fine;
trusting a Mac build is not.
