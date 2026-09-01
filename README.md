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
| `ipxe` (fork) | Optional chainload stage for machines whose firmware can't HTTP-boot on its own; the fork's HTTP read-ahead patches matter there. Served over HTTP like everything else — never TFTP. |
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
firmware ── UEFI HTTP Boot ─┐
BMC ────── virtual media ───┤  (legacy PXE ROM → TFTP: last resort only)
iPXE chainload (HTTP) ──────┤
                            │
                            ▼
      HTTPS Service on the appliance/storage cluster
   ├─▶ GET /boot.ipxe?mac=...       per-host script from stormnetboot-server
   ├─▶ GET /boot/vmlinuz             ┐ projected from the ACTIVE signed
   ├─▶ GET /boot/initramfs.img       ┘ boot pallet — never baked in
   └─▶ GET /boot/stormcos.iso        same content as an ISO, for virtual media
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

## Design principles

**Kubernetes/OpenShift look and feel.** An operator who knows OpenShift
should already know how to drive this. Concretely: every resource splits
`spec` (desired) from `status` (observed) and carries standard
`status.conditions` — `Available` / `Progressing` / `Degraded` with
`reason`, `message`, `lastTransitionTime`, and `observedGeneration`.
Reconciliation is level-triggered and idempotent, never a one-shot
imperative command. Resources declare `additionalPrinterColumns` so
`oc get bmh` is readable at a glance, and state transitions emit Events
(rustkube serves events.k8s.io/v1, and honours printer columns and CRD
status subresources). The console side follows the same rule: stormconsole
is already patterned on the OpenShift console, so netboot state arrives as a
plugin panel rather than a bespoke UI.

**Built for a data center that never stops.** Provisioning infrastructure is
not a maintenance-window tool — it runs continuously while the fleet it
serves is in production:

- **Nothing stops the world.** The boot tier is HA by construction
  (independent appliance cluster, stateless servers behind Services,
  replicated goldens), and upgrading the boot tier itself is a rolling
  operation like any other workload.
- **Background work yields to foreground work.** Assimilation is the
  model: flow-over migrates one extent per lock cycle precisely so root I/O
  keeps flowing while it runs. Anything long-running here inherits that
  rule — throttled, pre-emptible, never starving the node's real job.
- **Failure is routine, not exceptional.** Drives fail, nodes die, BMCs
  hang, a segment drops. Every operation is resumable and idempotent: an
  interrupted flow-over continues, a half-finished claim is reclaimed, a
  node that vanished mid-provision is retried or escalated.
- **Bounded for long uptime.** No unbounded queues, logs, or caches;
  backpressure when a thousand machines boot at once instead of collapse.
- **Observable by default.** Prometheus `/metrics` (as stormblock and
  sbregistry already expose), conditions that mean something to an alert
  rule, and events that explain transitions after the fact.

## Components (Rust workspace)

1. **`stormnetboot-server`** — the boot asset service, hosted on stormcos
   (container under stormpump, scheduled by rustkube):
   - **HTTP first; TFTP is a last resort.** The payload is small enough that
     firmware can fetch it directly: UEFI HTTP Boot where the firmware does
     it, BMC virtual media (an HTTP ISO over Redfish/IPMI) where it doesn't,
     and HTTP-served iPXE to chainload in between. A minimal TFTP responder
     exists only for machines that can do nothing else — legacy PXE ROMs —
     and nothing in the design may assume it.
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

**The storage cluster serves the HTTPS itself.** There is no separate web
tier: the boot endpoint is a Service on the appliance cluster, backed by
stormnetboot-server running on the same nodes that hold the goldens and
pallets. The bytes are already local to the server projecting them, so
serving is a read from local storage rather than a fetch across a tier, and
each appliance member that holds a replica can serve it. Load balancing,
TLS termination and locality all land on the same endpoints the platform
already gives the cluster.

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

### Locality

Appliances spread across racks, rows, floors and sites, so "where do I boot
from" has a good answer instead of one central answer. Serving prefers the
nearest replica and **degrades gracefully**: rack-local if there is one, else
row, else floor, else site. A boot storm inside one rack is served inside
that rack.

This does not need a new vocabulary. The failure-domain labels already exist
and are already split: stormdrive resolves physical location (enclosure/bay
via SES, PCIe slot, SAS address) and owns `hba`/`shelf`/`bay`, while
stormblock owns node-and-above — `site`, `building`, `room`, `row`, `rack`,
`node`, `cluster` — across one hierarchy, `site ⊃ building ⊃ floor/room ⊃
row ⊃ rack ⊃ node ⊃ hba ⊃ shelf ⊃ bay`. Drives are registered with location
labels already, so placement policy is a consumer of facts the platform
collects, not a new inventory to maintain.

**Status: direction, not policy.** How many replicas per domain, what
steers a booting host to a near one, and how aggressive the fallback should
be are all open — and are the kind of thing to learn by running it across
real racks and sites rather than deciding on paper. stormconsole is the
configuration surface for it: the console already renders stormdrive and
stormblock, so placement policy is a panel there, not a config file this
project invents.

### Boot storms don't happen

Several properties stack up so that scale never funnels through one server:

- **Thousands means many networks.** Fleets that size are spread across
  segments, each with its own DHCP scope and boot tier — microdns already
  runs per network. The unit of boot load is the segment, not the fleet.
- **Any node can serve.** Every assimilated stormcos node runs stormblock
  and can carry the boot tier — the server is a stateless pallet projection,
  so promoting a node to boot-server is a boot.d `start` line. The install
  capacity grows with the installed fleet.
- **It is HTTP, so it load-balances.** The same image machinery emits ISOs
  (`stormblock image build --format iso`), and the BMC can attach one over
  virtual media from a load-balanced HTTPS endpoint. The normal path has no
  TFTP in it, so it is not stuck being per-segment; the last-resort TFTP
  responder is the only piece that ever needs to be segment-local.
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
- Replica placement policy: how many golden replicas per failure domain, who
  decides, and how a booting host is steered to a near one (DNS, Service
  topology hints, or an explicit portal in the rendered cmdline). See
  "Locality" — the direction is settled, the policy is not.
- Exact split with `pxe-operator` and `bmh-operator-rs`: today the Go
  bmh-operator does PXE serving and IPMI in one process; the target split is
  boot resources + DHCP (pxe-operator), BMC/power (bmh-operator-rs, deferred
  until core cutover — and Rust IPMI is greenfield), assets (this project).
  Whether clone claims happen operator-side or server-side needs settling
  before phase 3.

## Capacity

The server hands each booting host **one script plus ~16 MB** (a ~12 MB
kernel and a ~4.4 MB initramfs) and is then out of the picture: everything
after that arrives over NVMe/TCP, and the host never talks to the boot server
again. It is a boot-time file server, not a provisioning engine and not a
data path.

Measured on dev (8 cores, loopback, v0.1.0 — so this measures the server, not
a network):

| Load | Time | Rate |
|---|---|---|
| 1 host | 21 ms | — |
| 100 concurrent | 0.52 s | 194 hosts/s, 3.2 GB/s |
| 500 concurrent | 2.20 s | 227 hosts/s, 3.7 GB/s |

RSS stayed at 58 MB serving 500 concurrent hosts, and throughput rose rather
than collapsed as concurrency grew. Every host pulls the *same* two files, so
they are served from page cache — roughly 16 MB of hot cache serves the whole
fleet, which is why memory stays flat.

**The wire is the bottleneck, not the server.** At 16.4 MB per host: ~7-8
hosts/s on 1 GbE, ~76 hosts/s on 10 GbE. A 42-node rack is a few seconds of
wire time; 1000 hosts on 10 GbE is well under a minute. And because capacity
is added by adding serving nodes (stateless, behind a Service, any
assimilated node can serve), the answer to "more hosts" is never a bigger
boot server.

## How it wires into stormcos

Two directions, which are easy to conflate — it is stormcos serving stormcos:

**It runs on stormcos.** On an appliance node it is an ordinary container in a
boot.d unit (`20-netboot`), started by stormpump as PID 1 with its rootfs a
golden clone, and exposed to the network as a rustkube Service. Nothing about
it is special-cased: it is deployed, updated and rolled the same way any
workload is, and `--base-url` is the Service name so clients never learn a
node address. Config is all `STORMNETBOOT_*` environment variables precisely
so a boot.d spec can set it without a config file.

**It serves stormcos.** What it hands out is the kernel and initramfs from
the active signed boot pallet; the machine that pulls them comes up as full
stormcos with root over NVMe/TCP, then flow-over assimilates it to local
disk.

Wiring status, honestly — today only the first row is built:

| Integration | Mechanism | State |
|---|---|---|
| Serve assets | directory on disk | **done** (v0.1.0) |
| Assets from pallets | sbregistry `:5100`, verify STORMSIG, serve by digest | phase 2 |
| Per-host identity | MAC → host record → `/v1/clones/claim` → `rd.stormblock.*` | phase 3 |
| The booted node | `stormnetboot-init` → stormblock `nvme-tcp://` → `/dev/ublkb0` → `switch_root` | phase 4 |
| Hosting on stormcos | boot.d unit + rustkube Service | phase 7 |

## Status

**v0.2.0 — all phases implemented, not yet run against real hardware.** Three
binaries, 83 tests:

| Crate | What it is | Size |
|---|---|---|
| `stormnetboot-server` | The boot service: pallet projection, claims, both HTTP surfaces | 9.1 MB |
| `stormnetboot-init` | Initramfs PID 1: NVMe/TCP root → `switch_root` | 439 KB |
| `stormnetboot-agent` | Reports running/assimilating/local, plus inventory | 439 KB |

```bash
stormnetboot-server \
  --listen 0.0.0.0:8080 --mgmt-listen 0.0.0.0:9096 \
  --asset-dir /var/lib/stormnetboot/assets \
  --base-url https://boot.storm.lo \
  --registry http://sbregistry:5100 \
  --pallet-repo stormcos/boot --pallet-ref 10.20 \
  --trusted-key <ed25519-public-key-hex> \
  --golden stormcos --claim \
  --hosts-file /var/lib/stormnetboot/hosts.json \
  --local-disk /dev/sda
```

Every flag has a `STORMNETBOOT_*` environment variable, so the same binary runs
from a shell, a boot.d spec, or a container with no config file.

Four refusals are deliberate, and each has a test:

- `/readyz` returns 503 listing what is missing until the kernel and initramfs
  are actually present. A boot server that answers but cannot deliver a kernel
  is worse than one that is plainly down.
- Starting with `--registry` and no `--trusted-key` is refused outright. What
  this server hands out is executed as the kernel of every machine that asks,
  so serving unsigned content takes an explicit `--allow-unsigned`.
- With no portal resolved, the rendered script carries no `rd.stormblock.*` and
  says so. A node that stops in the initramfs is recoverable; one that attaches
  the wrong volume is not.
- `--unknown-hosts deny` refuses a machine with no record, for sites where the
  boot network is not trusted to contain only machines that belong there.

The next step is a real one rather than more code: publish a boot pallet and
run a machine through the whole chain on the appliance VM. Host records also
still come from a file — the `BootHost` CRD is shipped in `deploy/manifests`
and wiring it up via kube-rs is the first follow-up.

## Building

Per the cross-project rules: **all builds run on `root@dev.g8.lo`**, with
`CARGO_TARGET_DIR=/build/cargo/stormnetboot`. Editing on the Mac is fine;
trusting a Mac build is not.

```bash
ssh root@dev.g8.lo
cd /root/src/stormnetboot && git pull
export CARGO_TARGET_DIR=/build/cargo/stormnetboot
cargo build --release && cargo test --release
```
