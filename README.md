# stormnetboot

> **The PXE chain is retired (2026-09-02).** A machine boots a small all-Rust
> agent from a **USB stick** — first boot device, local disk second — over a
> BIOS NVMe-over-TCP extension. The agent asks whether a full update is needed;
> if not it **falls through to the local disk**, and if so it attaches a **CoW
> clone of an ISO on `forge.g16.lo` over NVMe/TCP** and boots that. Because it
> is an ISO the node assimilates itself. No PXE, no TFTP, no DHCP boot options,
> no HTTP first hop, and no microdns: **sbregistry and a USB stick, over TCP.**
> v0.4.0 built the first half of this (direct UKI boot media, `nvme-tcp://` the
> only transport). Sections below still describing the PXE first hop, iPXE
> chainloading, TFTP or DHCP boot options describe the retired design and are
> being rewritten; `crates/stormnetboot-server/src/ipxe.rs` and the
> `/boot.ipxe` route are obsolete and still in-tree.
>
> **Two update methods, deliberately different machines.** (1) *Total rewrite* —
> BMC power-cycles the metal, it comes up on the USB agent and reinstalls; this
> is wipe, install and bulk upgrade, and it can be done to every node in a
> cluster at once. (2) *Upgrade in place* — the OpenShift upgrade: rolling
> restarts on masters then nodes, on a live system, no cold boot. A/B rollover
> belongs to (2).
>
> **BMC is still required**: power control is what triggers method (1), and it
> is also how install progress is watched. That half reuses this project's
> progress machinery — `BootHost` status, the agent's phase reporting, the
> console feed.

Componentized network boot for the Storm platform. A machine boots a tiny
payload, brings its root up over **NVMe/TCP** from a stormblock appliance, and
then — while running — **zeroboots** (flows over) the assets onto its local
system disk. The same service drives upgrades and bare-metal recovery. It
replaces the monolithic install ISO / disk image with a boot payload measured
in megabytes.

## Where it sits

| Project | Role in this design |
|---|---|
| [`stormbootx`](https://github.com/glennswest/stormbootx) | **The USB boot agent.** A 45 KB UEFI application: service tag from SMBIOS, `nvme-tcp://` attach over `EFI_TCP4`, published as `EFI_BLOCK_IO_PROTOCOL`. Its own repo — it is `no_std`, edition 2021 and targets `x86_64-unknown-uefi`, so it never fitted this workspace. |
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

### Direct boot media — no PXE, no HTTP

The whole first hop above exists to hand a machine a kernel, an initramfs and a
command line. Local media can carry all three, which removes DHCP options,
TFTP, HTTP and the boot server from the boot path entirely and leaves
`nvme-tcp://` as the only transport:

```
USB/SSD ─ GPT ─ ESP ─ /EFI/BOOT/BOOTX64.EFI   ← UKI: stub + cmdline + kernel + initramfs
   │                                            (removable-media path: no NVRAM entry needed)
   ▼
/init  ── attaches nvme-tcp:// straight from the baked cmdline ──▶ switch_root
   │
   ▼  root mounted, before the handover
media refresh: compare the ESP stamp against the digest THIS golden declares
   ├─ same        → nothing to do
   ├─ different   → copy the golden's usr/lib/stormcos/boot-media/boot.efi
   │                 over the ESP, then write the stamp
   └─ no stamp    → refuse: that ESP is not ours
```

Built by `scripts/build-boot-media.sh`. See
[Self-refreshing media](#self-refreshing-media) for why this is not an
auto-updater.

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

### Self-refreshing media

Boot media that can only be updated by carrying a USB stick to the machine
becomes the one hand-managed thing in an otherwise hands-off platform. So the
media updates itself — but deliberately **not** the way an auto-updater would,
because registry-poll auto-update was retired here as an incident failure
class. The differences are the whole point:

| Auto-updater (retired) | This |
|---|---|
| Polls on a timer, in the background | Runs once per boot, in the foreground |
| Asks "is there something newer?" | Asks "does the ESP match the digest **this golden** declares?" |
| Resolves a moving tag | Compares a pinned digest |
| Fetches from a registry over HTTP | Reads a file from the already-attached root |
| Can change a running machine | Only ever affects the *next* boot |

The golden carries `usr/lib/stormcos/boot-media/{boot.efi,media.conf}` — a
finished UKI and the digest that belongs with it. After the root is mounted and
before `switch_root`, `stormnetboot-init` compares that digest with the stamp
on the ESP and rewrites the ESP if they differ. Because the payload travels
inside the golden, an update arrives over `nvme-tcp://` like everything else;
there is no second source of truth and nothing new to reach.

Three properties make it safe to run on every boot:

- **It cannot fail a boot.** Every error path logs and continues. A running
  machine is worth more than current boot media.
- **An interruption retries rather than lies.** The UKI is written under a
  temporary name and renamed into place; the stamp is written *last*. A crash
  between the two leaves a working image with a stale stamp, so the next boot
  simply repeats the copy.
- **It will not touch an ESP that is not ours.** The stamp doubles as proof of
  ownership, so a mistyped `rd.stormnetboot.media=` pointing at a vendor
  recovery partition or the machine's real bootloader is skipped, not
  overwritten.

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

Wiring status, honestly — everything below is built; none of it has met real
hardware or a live apiserver yet:

| Integration | Mechanism | State |
|---|---|---|
| Serve assets | directory on disk | **done** (v0.1.0) |
| Assets from pallets | sbregistry `:5100`, verify STORMSIG, serve by digest | **done** (v0.2.0) |
| Per-host identity | MAC → host record → `/v1/clones/claim` → `rd.stormblock.*` | **done** (v0.2.0) |
| The booted node | `stormnetboot-init` → stormblock `nvme-tcp://` → `/dev/ublkb0` → `switch_root` | **done** (v0.2.0) |
| Hosting on stormcos | boot.d unit + rustkube Service | **done** (v0.2.0) |
| Host records | `BootHost` watch over kube-rs, hosts file underneath | **done** (v0.3.0) |
| Reporting back | `BootHost` status: phase, claim, conditions, hardware | **done** (v0.3.0) |

## Where host records come from

Two layers, and the order matters. A `BootHost` resource in the cluster is the
source of truth; the JSON hosts file is the bootstrap layer beneath it,
consulted only for MACs no `BootHost` covers.

That order is what lets one appliance boot the machines that will *become* its
cluster and then keep serving from that cluster — no cutover, no second code
path, and no file quietly overriding what an operator changed with `kubectl`.
Deleting a `BootHost` falls back to the file rather than to nothing, which is
the behaviour a bootstrap needs and the one a fleet expects.

```bash
kubectl apply -f deploy/manifests/20-boothost-crd.yaml     # or:
stormnetboot-server --print-crd | kubectl apply -f -       # generated from the types
```

`--print-crd` emits the CRD from the Rust types the server actually serves, so
the schema an operator installs cannot disagree with the code that answers to
it. The YAML in `deploy/manifests` is the same contract, with the comments.

```yaml
apiVersion: netboot.storm.io/v1alpha1
kind: BootHost
metadata:
  name: node7
  namespace: storm-system
spec:
  bootMACAddress: "aa:bb:cc:dd:ee:01"
  role: worker
  online: true
```

The server reads them and writes back what happened, on the status
subresource — phase, pallet version, the sbregistry clone the host is booting
from, the hardware the node reported about itself, and the standard
`Available` / `Progressing` / `Degraded` conditions with reasons, messages,
`observedGeneration`, and transition times that move only when the status
moves.

```
$ kubectl get boothosts -A
NAMESPACE      NAME    MAC                 HOSTNAME  ROLE    PHASE          PALLET  AGE
storm-system   node7   aa:bb:cc:dd:ee:01   node7     worker  assimilating   10.20   14m
```

The write-back is level-triggered and capped at 200 patches a pass: ten
thousand machines racked at once converge over a few seconds rather than
arriving at the apiserver as one burst. None of it is on the boot path — an
unreachable cluster costs the fleet its `kubectl get bh` view and nothing
else, because the records are already in memory and the file is still
underneath.

`spec.online: false` parks a machine without deleting the identity it has been
given: the boot server refuses it with a 403 and an iPXE comment saying why. A
host pulled for repair has to come back as itself, so the record outlives the
outage.

**Inventory instead of inspection.** There is no agent ramdisk and no second
reboot. The node is already running the OS it will keep, so `stormnetboot-agent`
reads its own `/proc` and `/sys` and POSTs the result to `/api/v1/inventory`,
which lands in `status.hardware`. Ironic's inspection boot, replaced by a file
read.

## Status

**v0.3.0 — all phases implemented, not yet run against real hardware or a live
apiserver.** Three binaries, 115 tests:

| Crate | What it is | Size |
|---|---|---|
| `stormnetboot-server` | The boot service: pallet projection, claims, `BootHost` records and status, both HTTP surfaces | 9.1 MB |
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
  --kube --kube-namespace storm-system \
  --hosts-file /var/lib/stormnetboot/hosts.json \
  --local-disk /dev/sda
```

Built without the `kubernetes` feature there is no API client in the binary at
all, and `--hosts-file` is the whole story — which is what an air-gapped site
or a first bootstrap actually wants.

Every flag has a `STORMNETBOOT_*` environment variable, so the same binary runs
from a shell, a boot.d spec, or a container with no config file.

Five refusals are deliberate, and each has a test:

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
- A host whose record says `online: false` is refused by name. Parking a
  machine must not mean deleting the identity it has to come back as.

The next step is a real one rather than more code: publish a boot pallet and
run a machine through the whole chain on the appliance VM, against a live
rustkube apiserver.

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

### Direct boot media

The initramfs first, then the media that carries it. Both write to
`/build/images` — never `/tmp`, which on dev is a tmpfs sized at half of RAM,
so a disk image written there is memory that is never given back.

```bash
cargo build --release --target x86_64-unknown-linux-musl -p stormnetboot-init

./scripts/build-netboot-initramfs.sh \
    /build/cargo/stormnetboot/x86_64-unknown-linux-musl/release/stormnetboot-init \
    /build/cargo/stormblock/release/stormblock

./scripts/build-boot-media.sh \
    --portal 192.168.8.129 --port 4420 \
    --nqn nqn.2026-09.lo.g16:storage1-root \
    --hostname storage1 --role storage \
    --volume stormcos --media-dev /dev/sda1
```

`--portal`, `--port` and `--nqn` are required and have no defaults —
particularly `--port`, because the iSCSI path defaults it to 3260 and this is
not that path, so a guess attaches nothing or the wrong thing.

The build emits three files:

| File | Purpose |
|---|---|
| `stormnetboot-boot-<host>.img` | write to the stick: `dd if=… of=/dev/sdX bs=4M conv=fsync` |
| `stormnetboot-boot-<host>.efi` | the UKI — publish into the golden as `boot.efi` |
| `stormnetboot-boot-<host>.media.conf` | its digest — publish alongside as `media.conf` |

Publishing the last two into `usr/lib/stormcos/boot-media/` is what lets the
stick refresh itself on later boots. Skip that and the media still boots, it
just stays on the kernel it shipped with.

`--media-dev` is the ESP *as the booted machine names it* (default
`/dev/sda1`); a USB stick is usually `sda` on a server whose internal drives
are NVMe. Getting it wrong is safe — the refresh refuses any ESP that does not
already carry our stamp.
