# CLAUDE.md — stormnetboot

Componentized network boot for the Storm platform: PXE → tiny kernel/initramfs
→ root over NVMe/TCP from a stormblock appliance → zeroboot flow-over to local
disk. Same service handles upgrades and recovery. See `README.md` for the full
architecture; this file tracks state and the work plan.

## Version

`0.2.0`, set once in the workspace `Cargo.toml` (`workspace.package.version`);
all three crates inherit it with `version.workspace = true`, so there is one
place to change.

## Context that is easy to lose

- "zeroboot" here is Storm vocabulary for stormblock's flow-over/assimilation
  (`stormblock boot-local --local-disk`), **not** the Firecracker fork engine
  in `~/zeroboot` (that repo is a concept donor only).
- stormblock documents a `serve-boot` HTTP surface in
  `stormblock/docs/stormblock-ipxe-boot.md` but never implemented it, and its
  CLAUDE.md says PXE is not the engine's job. stormnetboot implements that
  surface as its own component. When it lands, file a stormblock issue to
  update that doc to point here (cross-project rule: issues, not drive-by
  fixes).
- sbregistry (`stormblock-registry`, port `:5100`) already serves per-host CoW
  clones with NVMe/TCP attach info (`/v1/clones/claim`) and signed boot pallet
  artifacts; its spec phase 5 anticipated per-PXE-host install media.
- DHCP belongs to microdns — reservations already carry `next_server`,
  `boot_file`, `boot_file_efi`, `ipxe_boot_url`.
- Transport decision: stormblock's own NVMe/TCP initiator (`nvme-tcp://`
  backing device) + ublk export, not kernel nvme_tcp + nvme-cli. Flow-over
  needs stormblock in the datapath; uniform `/dev/ublkb0` root in every phase.
- **Appliances are their own independent cluster**, separate from the
  workload clusters they provision; boot/asset services are ordinary
  Kubernetes Services there (load balancing, failover, DNS from the
  platform, not from this code), and the provisioner never depends on the
  cluster it provisions.
- **HTTP first; TFTP is a last resort.** The payload is small enough for
  firmware to fetch over HTTP directly: UEFI HTTP Boot, a BMC-attached HTTP
  ISO over Redfish/IPMI virtual media, or HTTP-served iPXE chainload. Keep a
  minimal TFTP responder for legacy PXE ROMs only — never assume it, never
  put it on the normal path (TFTP is UDP, per-segment, unbalanceable). Note:
  Redfish VirtualMedia is greenfield — the Go bmh-operator does media by
  iSCSI sanboot and has no virtual-media support.
- **The storage/appliance cluster serves the HTTPS itself** — no separate
  web tier. stormnetboot-server runs on the appliance nodes that already
  hold the goldens/pallets, so serving is a local read and every member with
  a replica can serve.
- Locality (direction, policy TBD — expect to learn it by running it):
  appliances spread across racks/rows/floors/sites; serving prefers the
  nearest golden replica and degrades rack → row → floor → site. Reuse the
  existing failure-domain labels — stormdrive owns `hba`/`shelf`/`bay` and
  physical resolution (SES/PCIe/SAS), stormblock owns `site`/`building`/
  `room`/`row`/`rack`/`node`/`cluster`; drives already register with
  location labels. stormconsole is the configuration surface — don't invent
  a config file for placement.
- Scale story: thousands of nodes span many networks (per-network microdns);
  any assimilated node can serve the boot tier (stateless, a boot.d `start`
  line); HTTP ISO boot is a second front door; goldens replicate as
  stormblock volumes so serving capacity is added by adding replicas. Only
  the first hop (iPXE/kernel/initramfs, a few MB) is a file transfer —
  everything else is demand-paged blocks over NVMe/TCP.
- Hosting (decided 2026-08-31): interim appliance is a **storage appliance VM
  on `pve.g8.lo`** — data volume on the spinning `impulse1` store — hosting
  the engine, sbregistry, this boot chain, the new IPMI/BMH layer, and
  serving updates. Target host is `stormbastion` (same roles as a stormcos
  profile). **Rose is out** — insufficient capacity and, decisively, not a
  secure-environment fit. The 240 TB unit takes the bulk role when it powers
  up. A public version of the appliance is the long-term goal.
- Provisioning aligns with OpenShift/Metal3 (BMH resource → ForcePXE →
  power-cycle → this chain), with one deliberate divergence: **no agent
  ramdisk, no inspection boot** — the netboot boots full stormcos, and
  inspection is a hardware-inventory report from the running OS on first
  boot. golden ≈ image, flow-over ≈ deploy, assimilation-complete ≈
  boot-complete → persistent local boot. No Rust IPMI/Redfish exists
  in-tree yet; bmh-operator-rs is a deferred stub — the Go bmh-operator
  keeps serving until core cutover.
- Fleet model: N nodes (3 → 10 000) each claim a thin CoW clone of one
  golden (metadata cost, not copies), assimilate in the background, and end
  day 1 as identified standalone stormcos nodes. Day 2 cluster join = apply
  a boot.d profile from the day-1 identity; never a reprovision.
- Engine gaps go to stormblock as GitHub issues (e.g. `boot-nvme` orchestrator
  or an `nvme-tcp://` slab source for `BootLocal`).
- **Orchestrator is rustkube (+ rustkube-node) over fastetcd, never mkube —
  even on Rose.** mkube formally retired 2026-08-27 (`mkube/CLAUDE.md:190`;
  governing spec `rustkube/enhancements/rose-node-and-mkube-migration.md`).
  Older sibling docs that say "mkube" are legacy vocabulary; translate, don't
  follow.
- `pxe-operator` (sibling repo, skeleton) is the control-plane half of the
  same PXE rewrite — rustkube host/boot resources + microdns programming.
  Reconcile scope with it before phase 3 (clone claims: operator-side vs
  server-side).
- Platform update philosophy: no poll-and-apply updaters; images become
  sealed goldens in sbregistry and land as CoW clone swaps. stormnetboot's
  boot-pallet projection must follow the same rule — serve by digest from the
  active pallet, never "latest".
- **Kubernetes/OpenShift look and feel is a requirement**: spec/status split,
  standard conditions (Available/Progressing/Degraded) with reason/message/
  lastTransitionTime/observedGeneration, printer columns, Events on
  transitions, level-triggered idempotent reconcile. An OpenShift operator
  should recognize everything.
- **Designed for a data center that never stops**: HA boot tier (no
  stop-the-world), background work yields to foreground (flow-over's
  one-extent-per-lock-cycle is the model), every operation resumable and
  idempotent because failure is routine, bounded queues/logs for long
  uptime, backpressure instead of collapse under a boot surge, Prometheus
  metrics + meaningful conditions for alerting.
- Console integration goes through the stormview contract: stormconsole
  aggregates every domain's components at `/api/v1/components` +
  `/ws/components`; stormnetboot-server publishes its own feed.

## Build & deploy

- Build on `root@dev.g8.lo` only; `CARGO_TARGET_DIR=/build/cargo/stormnetboot`.
- Container images: podman, `scratch` base, pushed to the local registry
  (sbregistry).
- Never write disk images to `/tmp` on dev — use `/build/images`.

## Work plan

- [x] Phase 0 — design: README architecture, this work plan, changelog (2026-08-31)
- [x] Phase 1 — `stormnetboot-server` skeleton (2026-09-01): axum HTTP
      serving boot assets from a directory (ServeDir, so Range works — HTTP
      boot needs it), templated per-host `/boot.ipxe`, `/boot.json` listing,
      `/health`, `/readyz` (fails while kernel/initramfs are missing),
      `/metrics`, SIGTERM graceful shutdown. Built and tested on dev; smoke
      test covers all routes, 206 ranges, and 404 accounting. No TFTP —
      HTTP-first by decision; a last-resort responder can come later if real
      legacy hardware demands it.
- [x] Phase 2 — pallet projection (2026-09-01): sbregistry OCI client fetches
      the boot pallet by digest, verifies STORMSIG (Ed25519, trusted-key list,
      subject binding checked before the maths) and materialises members into
      the asset cache. Inline members read from the spec, not a blob. Refuses
      to serve unsigned unless `--allow-unsigned`.
- [x] Phase 3 — host resolution + claims (2026-09-01): MAC normalisation
      across every spelling, host records from JSON (rustkube BootHost CRD
      shipped in deploy/manifests as the intended source), per-host CoW clone
      claimed from sbregistry and cached so firmware retries do not leak
      clones, `rd.stormblock.*` rendered from the claim.
- [x] Phase 4 — `stormnetboot-init` (2026-09-01): initramfs PID 1. Parses the
      cmdline contract, loads nvme_tcp/ublk_drv, brings up the NIC, hands
      stormblock an `nvme-tcp://` slab, waits for `/dev/ublkb0`, mounts,
      writes identity, `switch_root`. Drops to a shell rather than panicking.
- [x] Phase 5 — assimilation reporting (2026-09-01): `stormnetboot-agent`
      follows the engine's output and turns flow-over lines into phase
      reports. Flow-over has **no status API or status file** — log lines are
      the only signal, including the abort after 16 extent failures that would
      otherwise leave a node "assimilating" forever.
- [x] Phase 6 — recovery path (2026-09-01): a node that cannot boot locally
      re-PXEs and re-assimilates; there is no separate installer. Rollout is a
      pallet digest change picked up by the refresh loop. Fleet-level policy
      belongs to stormupgrade, not here.
- [x] Phase 7 — stormcos hosting (2026-09-01): scratch Containerfile, boot.d
      unit `20-netboot`, BootHost CRD, DaemonSet + Service manifests,
      initramfs build script.
- [x] Phase 8 — console integration (2026-09-01): stormview component feed at
      `/api/v1/components` + `/ws/components` on the management port.
- [ ] Next — wire host records to the BootHost CRD via kube-rs (the file store
      is the bootstrap path); publish a real boot pallet and run a machine
      through the whole chain on the appliance VM.

## Session log

- 2026-08-31 — repo created; ecosystem survey (stormblock, sbregistry,
  stormcos, stormuefi, pxemanager, ipxe, zeroboot); architecture written.
