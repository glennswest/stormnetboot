# CLAUDE.md — stormnetboot

Componentized boot for the Storm platform: USB-stick boot agent → root over
NVMe/TCP from a stormblock appliance → zeroboot flow-over to local disk. Same
service handles upgrades and recovery. See `README.md` for the full
architecture; this file tracks state and the work plan.

**Read this before believing anything below about PXE.** The PXE chain was
retired 2026-09-02. Phase 10 (v0.4.0) already replaced the first hop with
direct UKI boot media; the remaining PXE code and prose are obsolete and
being removed. A previous session read the old framing at the top of these
files and confidently described a design that had been dead for a day —
which is what this note exists to prevent.

## Version

`0.4.0`, set once in the workspace `Cargo.toml` (`workspace.package.version`);
all three crates inherit it with `version.workspace = true`, so there is one
place to change.

## The current architecture (2026-09-02)

- **Boot is a USB stick, not a network.** A small all-Rust agent (~30k) on a
  USB stick, built on a **BIOS NVMe-over-TCP extension**. The stick is boot
  device #1, the local disk #2. The agent checks whether a full update is
  needed; **if not it falls through to the local disk**. If so it attaches a
  **CoW clone of an ISO on `forge.g16.lo`** over NVMe/TCP and boots it. It is
  an ISO, so the node assimilates itself.
- **`forge.g16.lo`** (stood up 2026-09-02, high-speed net) is the build
  destination and the source of those ISO clones.
- **What goes away:** PXE, TFTP, DHCP boot options, the HTTP first hop, and
  **microdns**. What is left is **sbregistry + a USB stick, over TCP**.
- **A data partition survives the reinstall**, so a returning node may already
  know what it is. Two exits from assimilation: fresh metal (SNO-shaped,
  identity assigned later) and returning metal (resumes as itself). This also
  decides whether a bulk cold boot of a cluster comes back as that cluster or
  as N unidentified nodes. The v0.2.0 data-slab guard already refuses to
  format it — the protection exists, the read does not.
- **Two update methods, and they are different machines:**
  1. **Total rewrite (mostly)** — BMC power-cycles the metal, it comes up on
     the USB agent and reinstalls. Wipe / install / bulk upgrade; can be done
     to every node in a cluster at once.
  2. **Upgrade in place** — the OpenShift upgrade: rolling restarts on masters
     then nodes, on a **live system**. No cold boot. A/B rollover belongs here.
- **BMC is still required, and is not optional**: power control triggers
  method 1, and it is how **install progress** is watched. Greenfield Rust —
  no Rust IPMI/Redfish in-tree; the Go bmh-operator serves until core cutover.

### What is reusable, and what is dead

| In-tree | Verdict |
|---|---|
| `stormnetboot-server/src/ipxe.rs`, `/boot.ipxe`, kernel+initramfs HTTP serving | **dead** — the first hop it serves no longer exists |
| hosts file / microdns / DHCP framing in the docs | **dead** |
| `boothost.rs` + `kube_store.rs` (CRD, status write-back, conditions) | **reuse** — this is the install-progress surface the BMC half needs |
| `stormnetboot-agent` (phase reporting, flow-over log follow, inventory) | **reuse** — progress from a live node, unchanged |
| `state.rs` (bounded fleet phase tracking, eviction, counts) | **reuse** |
| `components.rs` (stormview feed), `metrics.rs` | **reuse** |
| `pallet.rs`, `stormsig.rs`, `claims.rs` (sbregistry, signatures, CoW claims) | **reuse** — the ISO clone comes the same way |
| `mac.rs` | **reuse** — host matching |
| `stormnetboot-init` + `media.rs` + `build-boot-media.sh` (v0.4.0) | **keep and extend** — this is the first half of the new flow |

### Gaps between v0.4.0 and the flow above

1. **No fall-through.** v0.4.0's media always attaches its root; the agent
   should check, and boot the local disk when no update is needed.
2. **Wrong source.** It attaches sbregistry goldens, not a CoW ISO clone from
   `forge.g16.lo`.
3. **Not A/B.** Self-refresh rewrites the one ESP in place; A/B rollover is
   method 2's machinery and does not exist.
4. **Data partition not read.** Nothing consumes surviving identity, so every
   node still comes up SNO-shaped.
5. **No BMC at all.** Power control and progress interaction are unwritten.

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
- [x] Phase 9 — BootHost CRD as the host record source (2026-09-01): kube-rs
      watch feeding the cluster layer of the host store, with the file store
      underneath as the bootstrap layer (consulted only where no BootHost
      covers the MAC, so deleting one falls back rather than to nothing);
      level-triggered status write-back with Available/Progressing/Degraded,
      observedGeneration and transition times that move only when the status
      does, capped at 200 patches a pass; `spec.online: false` parks a machine
      without deleting its identity; hardware inventory posted by the running
      node into `status.hardware`; ServiceAccount + ClusterRole (read
      BootHosts, write only their status); `--print-crd` generates the CRD
      from the types the server serves. Behind the default `kubernetes`
      feature, so a bootstrap or air-gapped build carries no API client.
- [x] Phase 10 — direct boot media (2026-09-01): a bootable
      USB/SSD image that replaces the whole PXE first hop. GPT + one ESP
      carrying a UKI (stub + baked cmdline + kernel + initramfs) at
      `/EFI/BOOT/BOOTX64.EFI`, the removable-media path firmware boots with no
      NVRAM entry. No DHCP options, no TFTP, no HTTP: the cmdline is baked, and
      the only transport is `nvme-tcp://` to a stormblock on pve.
      Self-refresh follows the platform rule — **not** a poll-and-apply
      updater. At boot, after the root is attached, the media's embedded pallet
      digest is compared with the digest the attached root declares; if they
      differ the ESP is rewritten from a UKI carried *in the golden*, so the
      next boot runs it. Level-triggered, idempotent, digest-pinned, and it
      never fails the boot — a media refresh that cannot complete logs and
      continues. Built and verified on dev: GPT + 512 MiB ESP, UKI sections
      confirmed above the image base, kernel and initramfs byte-identical to
      their sources, cmdline round-tripping out of `.cmdline`. Two real bugs
      fell out — the initramfs module guard matched only underscored
      filenames so `nvme-tcp.ko.xz` failed every build, and UKI section VMAs
      derived from the stub's file size land below its non-zero `ImageBase`,
      which objcopy warns about and then exits 0 on.
- [ ] Phase 11 — the USB boot agent's decision. Fall through to the local disk
      when no update is needed (v0.4.0 always attaches), and read the
      surviving data partition so a returning node comes back as itself
      instead of SNO-shaped.
- [ ] Phase 12 — source the root from a CoW ISO clone on `forge.g16.lo`
      rather than an sbregistry golden. `claims.rs` already speaks the claim
      protocol; the artifact changes, the transport does not.
- [ ] Phase 13 — BMC: power control (what triggers a total rewrite) and
      install-progress interaction. Greenfield Rust IPMI/Redfish. The
      progress half reuses `boothost.rs`, `kube_store.rs`,
      `stormnetboot-agent` and `state.rs` — see the reuse table above.
- [ ] Phase 14 — A/B rollover for upgrade-in-place (method 2). Distinct from
      the media self-refresh, which rewrites one ESP for method 1.
- [ ] Remove the dead PXE surface: `ipxe.rs`, the `/boot.ipxe` route, the
      kernel/initramfs HTTP serving, and the PXE/microdns prose in README.md.
      Left in place today only because the rewrite is still settling.
- [ ] Next — publish a real boot pallet and run a machine through the whole
      chain on the appliance VM, against a live rustkube apiserver. Nothing in
      phase 9 has met a real apiserver yet: no cluster was reachable from the
      build box, so the watch and the status writer are covered by unit tests
      and a loopback smoke test only.

## Session log

- 2026-08-31 — repo created; ecosystem survey (stormblock, sbregistry,
  stormcos, stormuefi, pxemanager, ipxe, zeroboot); architecture written.
