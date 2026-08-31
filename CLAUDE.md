# CLAUDE.md — stormnetboot

Componentized network boot for the Storm platform: PXE → tiny kernel/initramfs
→ root over NVMe/TCP from a stormblock appliance → zeroboot flow-over to local
disk. Same service handles upgrades and recovery. See `README.md` for the full
architecture; this file tracks state and the work plan.

## Version

No code yet — semver starts at `0.1.0` with the first crate. All version
locations (workspace `Cargo.toml`, member crates) must match once they exist.

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
- Hosting (decided 2026-08-31): interim appliance is a **storage appliance VM
  on `pve.g8.lo`** — data volume on the spinning `impulse1` store — hosting
  the engine, sbregistry, this boot chain, the new IPMI/BMH layer, and
  serving updates. Target host is `stormbastion` (same roles as a stormcos
  profile). **Rose is out** — insufficient capacity and, decisively, not a
  secure-environment fit. The 240 TB unit takes the bulk role when it powers
  up. A public version of the appliance is the long-term goal.
- Provisioning aligns with OpenShift/Metal3 (BMH resource → ForcePXE →
  power-cycle → this chain); initramfs ≈ ironic-python-agent, golden ≈
  image, flow-over ≈ deploy, assimilation-complete ≈ boot-complete →
  persistent local boot. No Rust IPMI/Redfish exists in-tree yet;
  bmh-operator-rs is a deferred stub — the Go bmh-operator keeps serving
  until core cutover.
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
- [ ] Phase 1 — `stormnetboot-server` skeleton: axum HTTP serving static iPXE
      binaries + templated `boot.ipxe`; minimal TFTP for firmware chainload;
      `/health`, `/metrics`
- [ ] Phase 2 — pallet projection: fetch the active signed boot pallet from
      sbregistry (engine pallet as cache), stream `/boot/vmlinuz` and
      `/boot/initramfs.img` out of it, verify STORMSIG before serving
- [ ] Phase 3 — host resolution: MAC/serial → node record, claim CoW clone via
      sbregistry `/v1/clones/claim`, render `rd.stormblock.*` cmdline
- [ ] Phase 4 — `stormnetboot-init`: initramfs `/init` bringing root up via
      stormblock NVMe/TCP initiator → `/dev/ublkb0` → `switch_root`; file
      stormblock issues for any engine gaps found
- [ ] Phase 5 — assimilation sequencing: drive `boot-local --local-disk`
      flow-over, pallet publish to local GPT, stormuefi to ESP, mirror break,
      status reporting
- [ ] Phase 6 — upgrade path: pull-publish-activate-reboot loop with
      `tries_left`/rollback; prove netboot-as-recovery on a wiped node
- [ ] Phase 7 — stormcos hosting: container build, rustkube/stormpump
      manifests, deployment onto the PVE appliance VM, then stormbastion
- [ ] Phase 8 — console integration: stormview component feed from
      stormnetboot-server (netboot phases, assimilation progress, boot pallet
      versions, clone claims) aggregated by stormconsole

## Session log

- 2026-08-31 — repo created; ecosystem survey (stormblock, sbregistry,
  stormcos, stormuefi, pxemanager, ipxe, zeroboot); architecture written.
