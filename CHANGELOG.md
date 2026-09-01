# Changelog

## [Unreleased]

## [v0.3.0] — 2026-09-01

### Added
- **`BootHost` resources are the host record source.** A kube-rs watch keeps
  the cluster layer of the host store current, replacing the whole set on
  every relist so a delete missed during a disconnect heals itself. The JSON
  hosts file stays underneath as the bootstrap layer, consulted only for MACs
  no `BootHost` covers — which is what lets one appliance boot the machines
  that will become its own cluster and then keep serving from that cluster,
  with no cutover and no file quietly overriding a `kubectl` edit. Deleting a
  `BootHost` falls back to the file rather than to nothing.
- **Status write-back**, level-triggered: phase, pallet version, the
  sbregistry claim the host is booting from, reported hardware, and the
  standard `Available` / `Progressing` / `Degraded` conditions with reasons,
  messages, `observedGeneration`, and `lastTransitionTime` that moves only
  when the status moves. Passes are capped at 200 patches so ten thousand
  machines racked at once converge over a few seconds instead of hitting the
  apiserver in one burst, and nothing about it is on the boot path: an
  unreachable cluster costs the fleet its `kubectl get bh` view and nothing
  else.
- **`--print-crd`** emits the CRD from the Rust types the server actually
  serves, so an installed schema cannot disagree with the code answering to
  it. The `bootMACAddress` pattern is now in the schema as well as the parser:
  a MAC the apiserver accepted and the boot server cannot match is a machine
  that silently never boots.
- **`spec.online: false` parks a machine** without deleting the identity it
  has been given — refused with a 403 and an iPXE comment naming the host. A
  machine pulled for repair has to come back as itself.
- **Hardware inventory into `status.hardware`.** `stormnetboot-agent` posts
  structured inventory to a new `/api/v1/inventory`, and the boot state and
  console feed carry it. This is the whole of "inspection": a running node
  reading its own `/proc` and `/sys`, not an agent ramdisk and a second boot.
- `GET /api/v1/hosts` on the management surface lists the resolved records and
  the count in each layer; `stormnetboot_host_records{source=...}` and
  `stormnetboot_boothost_synced` expose the same thing to Prometheus, so a
  cluster layer that empties out while the file layer still answers is
  visible rather than silent. New counters for status writes, status write
  failures and watch errors.
- ServiceAccount, ClusterRole and ClusterRoleBinding in `deploy/manifests`.
  The server may read every `BootHost` and write only their status: which
  machines exist and what they should be is an operator's statement, and a
  boot server able to rewrite that could re-identify a machine mid-install.

### Changed
- The Kubernetes client sits behind a default-on `kubernetes` cargo feature.
  Built without it there is no API client in the binary at all, which is what
  a first bootstrap or an air-gapped site actually wants.
- `Phase` grew a `slug()`, and the console feed, the metric labels and the
  `BootHost` status now share it. One host in one phase must not read three
  ways depending on where an operator looked.

## [v0.2.0] — 2026-09-01

- **feat:** Multi-slab boot. `rd.stormblock.slab=` now takes a comma-separated
  list (mapping to the engine's already-repeatable `--slab`), and
  `rd.stormblock.data-slab=` names the slab holding node identity and
  per-service data. The init **refuses to start a flow-over whose
  `--local-disk` target is the same physical device as the data slab** —
  formatting it would re-mint the node CA and the ServiceAccount signing key,
  invalidating every token in the cluster, silently and in the background.
  Engine-side fixes filed as glennswest/stormblock#88 and
  glennswest/stormpump#12.
- **fix:** `device_base` no longer strips an NVMe namespace digit, which had
  reduced `/dev/nvme0n1` to `/dev/nvme0n` and would have let the data-slab
  guard pass on the very device it was meant to protect.
- **feat:** Pallet projection. `stormnetboot-server` fetches the boot pallet
  from sbregistry by digest, verifies its STORMSIG signature (Ed25519, trusted
  key list, subject binding checked before the signature maths so a valid
  signature over a different artifact cannot be replayed), and materialises
  members into the asset cache. Inline members are read from the pallet spec
  rather than fetched as blobs, which they are not. Serving an unsigned pallet
  requires `--allow-unsigned`.
- **feat:** Host identity and per-host claims. MAC normalisation across every
  spelling (colons, dashes, dotted quads, bare hex, PXE `BOOTIF`); host records
  from JSON with a `BootHost` CRD shipped as the intended source; a per-host
  CoW clone claimed from sbregistry and cached so firmware retries reuse a
  claim instead of leaking clones.
- **feat:** `stormnetboot-init` — initramfs PID 1. Parses the cmdline contract,
  loads `nvme_tcp`/`ublk_drv`, brings up the NIC, hands stormblock an
  `nvme-tcp://` slab, waits for `/dev/ublkb0`, mounts root, writes the pinned
  identity, and `switch_root`s. Drops to a shell rather than panicking the
  kernel, and reports each step to the boot server.
- **feat:** `stormnetboot-agent` — reports the phases nothing else can see.
  Follows the engine's output to turn flow-over into `assimilating` and
  `local`, because flow-over has no status API or status file. Reports
  hardware inventory from the running node, which is what removes the need for
  an inspection boot.
- **feat:** Two HTTP surfaces. The firmware-facing boot surface (`:8080`) and
  the management surface (`:9096`, console feed, metrics, host admin) are
  separate listeners on separate ports, verified to 404 each other's routes.
- **feat:** stormview component feed at `/api/v1/components` and
  `/ws/components`, with health rolled up onto a `system` component.
- **feat:** Deployment: scratch Containerfile, `20-netboot` boot.d unit,
  `BootHost` CRD with printer columns and conditions, DaemonSet + Service
  manifests, and an initramfs build script that refuses to produce an image
  missing `nvme_tcp` or `ublk_drv`.
- **docs:** Capacity section with measured numbers and a wiring table
  separating what is built from what is planned.

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
