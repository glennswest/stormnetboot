#!/bin/bash
# Build the USB boot agent: a GPT image whose ESP holds stormbootx as
# /EFI/BOOT/BOOTX64.EFI, plus the config file that tells it where to attach.
#
# This is the whole first hop. No PXE, no DHCP boot options, no TFTP, no HTTP:
# firmware boots the removable-media path with no NVRAM entry, the agent reads
# the machine's service tag out of SMBIOS, attaches nvme-tcp:// and publishes
# the remote image as EFI_BLOCK_IO_PROTOCOL. There is no kernel on this stick.
#
#   dd if=<output> of=/dev/sdX bs=4M conv=fsync
#
# The target lives in \stormboot\stormboot.conf on the media, not in the
# binary. That is deliberate: over one afternoon the portal moved host and then
# changed NQN, and each compiled-in value meant a machine that could not boot
# until someone rebuilt and rewrote a stick. Editing four lines on the ESP is
# the fix now.
#
# Runs ON the build box (dev.g8.lo). Output goes to /build/images — never
# /tmp, which on dev is a tmpfs sized at half of RAM.
set -euo pipefail

say() { printf '==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

PORTAL="192.168.31.202"                     # forge.g16.lo, eth1 (MTU 9000)
PORT="4420"
NQN="nqn.2026-09.lo.g16:stormcos"
NSID="2"
ESP_MIB="4"
OUTDIR="/build/images"
OUTPUT=""
BIN=""

usage() {
    sed -n '2,17p' "$0" | sed 's/^# \?//'
    cat <<'USAGE'

Options:
  --portal ADDR    NVMe/TCP portal (default 192.168.31.202, forge.g16.lo)
  --port N         portal port (default 4420)
  --nqn NQN        subsystem NQN (default nqn.2026-09.lo.g16:stormcos)
  --nsid N         namespace (default 2)
  --size MIB       ESP size (default 4; FAT16 needs >=4085 clusters)
  --binary PATH    prebuilt stormbootx.efi (default: build it)
  --output PATH    image path (default /build/images/stormbootx.img)
USAGE
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --portal) PORTAL="$2"; shift 2 ;;
        --port)   PORT="$2"; shift 2 ;;
        --nqn)    NQN="$2"; shift 2 ;;
        --nsid)   NSID="$2"; shift 2 ;;
        --size)   ESP_MIB="$2"; shift 2 ;;
        --binary) BIN="$2"; shift 2 ;;
        --output) OUTPUT="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown argument: $1 (--help for usage)" ;;
    esac
done

OUTPUT="${OUTPUT:-$OUTDIR/stormbootx.img}"
case "$OUTPUT" in
    /tmp/*) die "refusing to write a disk image into /tmp (tmpfs = RAM); use $OUTDIR" ;;
esac

for tool in mkfs.fat mmd mcopy sfdisk; do
    command -v "$tool" >/dev/null || die "$tool not installed on the build host"
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

if [[ -z "$BIN" ]]; then
    say "building stormbootx for x86_64-unknown-uefi"
    # Excluded from the workspace: a bare `cargo build` would try to build a
    # no_std UEFI binary for the host and fail unhelpfully.
    ( cd "$ROOT" && cargo build --release --target x86_64-unknown-uefi \
        --manifest-path crates/stormbootx/Cargo.toml )
    BIN="${CARGO_TARGET_DIR:-$ROOT/target}/x86_64-unknown-uefi/release/stormbootx.efi"
fi
[[ -f "$BIN" ]] || die "no stormbootx.efi at $BIN"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

cat > "$WORK/stormboot.conf" <<CONF
# stormbootx — edit this instead of rebuilding the binary.
# Read from \stormboot\stormboot.conf on the media this booted from, found via
# EFI_LOADED_IMAGE_PROTOCOL, so it is exactly the volume that was booted and
# never a guess at which ESP is ours.
portal = $PORTAL
port   = $PORT
nqn    = $NQN
nsid   = $NSID
CONF

# FAT16 with 512-byte clusters: FAT32 needs ~33 MB of filesystem before it has
# enough clusters to be legal, which is eight times the whole image. FAT16 at
# the default 2 KB cluster size is rejected below 8 MB for the same reason, so
# -s 1 is what makes a 4 MB ESP possible.
ESP="$WORK/esp.img"
truncate -s "${ESP_MIB}M" "$ESP"
mkfs.fat -F 16 -s 1 -n STORMBOOTX "$ESP" >/dev/null
mmd   -i "$ESP" ::/EFI ::/EFI/BOOT ::/stormboot
mcopy -i "$ESP" "$BIN" ::/EFI/BOOT/BOOTX64.EFI
mcopy -i "$ESP" "$WORK/stormboot.conf" ::/stormboot/stormboot.conf

mkdir -p "$(dirname "$OUTPUT")"
rm -f "$OUTPUT"
truncate -s "$(( ESP_MIB + 2 ))M" "$OUTPUT"
sfdisk --quiet --label gpt "$OUTPUT" <<EOF
start=2048, size=$(( ESP_MIB * 2048 )), type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, name="STORMBOOTX"
EOF
dd if="$ESP" of="$OUTPUT" bs=1M seek=1 conv=notrunc status=none

say "agent   $(du -h "$BIN" | cut -f1)  $BIN"
say "image   $(du -h "$OUTPUT" | cut -f1)  $OUTPUT"
say "target  nvme-tcp://$PORTAL:$PORT/$NQN?nsid=$NSID"
cat <<EOF

  Write it:
    dd if=$OUTPUT of=/dev/sdX bs=4M conv=fsync

  Retarget it without rebuilding — mount the ESP and edit
  \stormboot\stormboot.conf.
EOF
