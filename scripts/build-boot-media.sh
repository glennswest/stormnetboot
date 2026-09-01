#!/bin/bash
# Build bootable USB/SSD media that brings a machine up on an NVMe/TCP root
# with no PXE, no DHCP options, no TFTP and no HTTP anywhere in the path.
#
# The output is a GPT image with one ESP holding a UKI at the removable-media
# path /EFI/BOOT/BOOTX64.EFI — the file UEFI firmware boots with no NVRAM
# entry, which is what lets one stick boot a machine it has never seen. The
# kernel command line is baked into the UKI, so nothing has to be handed to the
# machine at boot: it powers on, attaches nvme-tcp://, and switch_roots.
#
#   dd if=<output> of=/dev/sdX bs=4M conv=fsync status=progress
#
# It also emits the UKI on its own plus a media.conf stamp. Publish those two
# into the golden as usr/lib/stormcos/boot-media/{boot.efi,media.conf} and
# stormnetboot-init will refresh the stick in place on the next boot — the
# digest comparison in crates/stormnetboot-init/src/media.rs.
#
# Runs ON the build box (dev.g8.lo). Output goes to /build/images. NEVER write
# a disk image into /tmp: /tmp on dev is a tmpfs sized at half of RAM, and a
# sparse image written there is RAM that is never given back.
set -euo pipefail

say()  { printf '==> %s\n' "$*"; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }

# --- what the machine needs to find its root -------------------------------
PORTAL=""             # stormblock portal address (host or IP, never host:port)
PORT=""               # per-export, so there is deliberately no default
NQN=""                # subsystem NQN to attach
NSID="1"
VOLUME=""             # stormblock.volume — note: no rd. prefix, that is the
                      # engine's own spelling and "fixing" it breaks the boot
HOSTNAME_=""
ROLE=""
LOCAL_DISK=""         # set to flow over onto local disk in the background
DATA_SLAB=""
EXTRA=""

# --- where the media is, and what goes on it -------------------------------
# The ESP as the *booted machine* will name it. A USB stick is usually /dev/sda
# on a server whose internal drives are NVMe. Getting this wrong is safe: the
# refresh refuses to write an ESP that does not already carry our stamp.
MEDIA_DEV="/dev/sda1"
KVER="$(uname -r)"
KERNEL=""
INITRAMFS=""
STUB="/usr/lib/systemd/boot/efi/linuxx64.efi.stub"
SIZE_MIB="512"
OUTDIR="/build/images"
OUTPUT=""

usage() {
    sed -n '2,20p' "$0" | sed 's/^# \?//'
    cat <<'USAGE'

Required:
  --portal ADDR         NVMe/TCP portal (host or IP)
  --port N              portal port
  --nqn NQN             subsystem NQN

Optional:
  --nsid N              namespace (default 1)
  --volume NAME         stormblock.volume within the slab
  --hostname NAME       identity pinned into the media
  --role NAME           storm.role
  --local-disk DEV      flow over onto this disk in the background
  --data-slab DEV       slab holding node identity; never formatted
  --media-dev DEV       the ESP as the booted machine sees it (default /dev/sda1)
  --kernel PATH         vmlinuz (default /lib/modules/KVER/vmlinuz or /boot)
  --initramfs PATH      initramfs image (default /build/images/stormnetboot-initramfs-KVER.img)
  --kver VER            kernel version (default: running kernel)
  --stub PATH           EFI stub (default systemd's linuxx64.efi.stub)
  --size MIB            ESP size in MiB (default 512)
  --output PATH         image path (default /build/images/stormnetboot-boot-<host>.img)
  --extra "a=b c=d"     extra kernel command line tokens
USAGE
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --portal)     PORTAL="$2"; shift 2 ;;
        --port)       PORT="$2"; shift 2 ;;
        --nqn)        NQN="$2"; shift 2 ;;
        --nsid)       NSID="$2"; shift 2 ;;
        --volume)     VOLUME="$2"; shift 2 ;;
        --hostname)   HOSTNAME_="$2"; shift 2 ;;
        --role)       ROLE="$2"; shift 2 ;;
        --local-disk) LOCAL_DISK="$2"; shift 2 ;;
        --data-slab)  DATA_SLAB="$2"; shift 2 ;;
        --media-dev)  MEDIA_DEV="$2"; shift 2 ;;
        --kernel)     KERNEL="$2"; shift 2 ;;
        --initramfs)  INITRAMFS="$2"; shift 2 ;;
        --kver)       KVER="$2"; shift 2 ;;
        --stub)       STUB="$2"; shift 2 ;;
        --size)       SIZE_MIB="$2"; shift 2 ;;
        --output)     OUTPUT="$2"; shift 2 ;;
        --extra)      EXTRA="$2"; shift 2 ;;
        -h|--help)    usage; exit 0 ;;
        *)            die "unknown argument: $1 (--help for usage)" ;;
    esac
done

# The three that have no safe default. port especially: the iSCSI path defaults
# it to 3260 and this is not that path, so a guess attaches nothing or the
# wrong thing.
[[ -n "$PORTAL" ]] || { usage >&2; die "--portal is required"; }
[[ -n "$PORT"   ]] || die "--port is required (per-export; there is no safe default)"
[[ -n "$NQN"    ]] || die "--nqn is required"

KERNEL="${KERNEL:-$(ls -1 "/lib/modules/$KVER/vmlinuz" "/boot/vmlinuz-$KVER" 2>/dev/null | head -1 || true)}"
INITRAMFS="${INITRAMFS:-/build/images/stormnetboot-initramfs-${KVER}.img}"
OUTPUT="${OUTPUT:-$OUTDIR/stormnetboot-boot-${HOSTNAME_:-node}.img}"

[[ -n "$KERNEL" && -f "$KERNEL" ]] || die "no kernel for $KVER (--kernel)"
[[ -f "$INITRAMFS" ]] || die "no initramfs at $INITRAMFS — run build-netboot-initramfs.sh first (--initramfs)"
[[ -f "$STUB"      ]] || die "no EFI stub at $STUB (dnf install systemd-boot-unsigned, or --stub)"

for tool in objcopy mkfs.fat mmd mcopy sfdisk sha256sum; do
    command -v "$tool" >/dev/null || die "$tool not installed on the build host"
done

case "$OUTPUT" in
    /tmp/*) die "refusing to write a disk image into /tmp (tmpfs = RAM); use $OUTDIR" ;;
esac

WORK="$(mktemp -d)"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

# --- the command line ------------------------------------------------------
# Everything the machine would otherwise have been told by DHCP and iPXE.
CMDLINE="rd.stormblock.portal=$PORTAL rd.stormblock.port=$PORT rd.stormblock.nqn=$NQN rd.stormblock.nsid=$NSID"
[[ -n "$VOLUME"     ]] && CMDLINE="$CMDLINE stormblock.volume=$VOLUME"
[[ -n "$LOCAL_DISK" ]] && CMDLINE="$CMDLINE rd.stormblock.local-disk=$LOCAL_DISK"
[[ -n "$DATA_SLAB"  ]] && CMDLINE="$CMDLINE rd.stormblock.data-slab=$DATA_SLAB"
[[ -n "$HOSTNAME_"  ]] && CMDLINE="$CMDLINE storm.hostname=$HOSTNAME_"
[[ -n "$ROLE"       ]] && CMDLINE="$CMDLINE storm.role=$ROLE"
CMDLINE="$CMDLINE rd.stormnetboot.media=$MEDIA_DEV"
[[ -n "$EXTRA"      ]] && CMDLINE="$CMDLINE $EXTRA"

printf '%s\n' "$CMDLINE" > "$WORK/cmdline.txt"
say "cmdline: $CMDLINE"

cat > "$WORK/os-release" <<EOF
ID=stormcos
NAME="stormcos netboot media"
PRETTY_NAME="stormcos direct NVMe/TCP boot (${HOSTNAME_:-node})"
VERSION_ID=$KVER
EOF

# --- assemble the UKI ------------------------------------------------------
# Section addresses are computed from the actual file sizes rather than the
# usual hardcoded constants: a kernel larger than the gap between two fixed
# VMAs overlaps the next section, and the failure is a machine that resets
# instantly with nothing on the console.
align() { local v=$1 a=$2; echo $(( (v + a - 1) / a * a )); }
stub_end=$(( $(stat -c%s "$STUB") ))
osrel_vma=$(align $(( stub_end + 0x1000 )) 0x1000)
cmdline_vma=$(align $(( osrel_vma + $(stat -c%s "$WORK/os-release") + 0x1000 )) 0x1000)
linux_vma=$(align $(( cmdline_vma + $(stat -c%s "$WORK/cmdline.txt") + 0x1000 )) 0x200000)
initrd_vma=$(align $(( linux_vma + $(stat -c%s "$KERNEL") + 0x1000 )) 0x200000)

say "building UKI (linux @ $(printf 0x%x $linux_vma), initrd @ $(printf 0x%x $initrd_vma))"
objcopy \
    --add-section .osrel="$WORK/os-release"     --change-section-vma .osrel="$osrel_vma" \
    --add-section .cmdline="$WORK/cmdline.txt"  --change-section-vma .cmdline="$cmdline_vma" \
    --add-section .linux="$KERNEL"              --change-section-vma .linux="$linux_vma" \
    --add-section .initrd="$INITRAMFS"          --change-section-vma .initrd="$initrd_vma" \
    "$STUB" "$WORK/boot.efi"

DIGEST="sha256:$(sha256sum "$WORK/boot.efi" | cut -d' ' -f1)"
say "UKI digest $DIGEST ($(du -h "$WORK/boot.efi" | cut -f1))"

# The stamp is both the version marker and the proof of ownership: the refresh
# refuses to write any ESP that does not already carry one.
cat > "$WORK/media.conf" <<EOF
# stormnetboot boot media. The digest is of the UKI in EFI/BOOT/BOOTX64.EFI.
pallet=$DIGEST
kver=$KVER
built=$(date -u +%Y-%m-%dT%H:%M:%SZ)
EOF

# --- ESP, then the GPT around it -------------------------------------------
# mtools writes the FAT image as a plain file, so none of this needs a loop
# device, a mount, or privileges.
ESP="$WORK/esp.img"
truncate -s "${SIZE_MIB}M" "$ESP"
mkfs.fat -F 32 -n STORMBOOT "$ESP" >/dev/null

mmd   -i "$ESP" ::/EFI ::/EFI/BOOT ::/stormnetboot
mcopy -i "$ESP" "$WORK/boot.efi"   ::/EFI/BOOT/BOOTX64.EFI
mcopy -i "$ESP" "$WORK/media.conf" ::/stormnetboot/media.conf

ALIGN_MIB=1
mkdir -p "$(dirname "$OUTPUT")"
rm -f "$OUTPUT"
truncate -s "$(( (SIZE_MIB + ALIGN_MIB + 1) ))M" "$OUTPUT"

sfdisk --quiet --label gpt "$OUTPUT" <<EOF
start=$(( ALIGN_MIB * 1024 * 2 )), size=$(( SIZE_MIB * 1024 * 2 )), type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, name="STORMBOOT"
EOF

dd if="$ESP" of="$OUTPUT" bs=1M seek="$ALIGN_MIB" conv=notrunc status=none

# Keep the UKI and stamp beside the image: these two are what gets published
# into the golden so the stick can refresh itself.
install -m 0644 "$WORK/boot.efi"   "${OUTPUT%.img}.efi"
install -m 0644 "$WORK/media.conf" "${OUTPUT%.img}.media.conf"

say "built $(du -h "$OUTPUT" | cut -f1) at $OUTPUT"
cat <<EOF

  Write it:
    dd if=$OUTPUT of=/dev/sdX bs=4M conv=fsync status=progress

  Let it update itself — publish these into the golden as
  usr/lib/stormcos/boot-media/ and every later boot rewrites the stick:
    ${OUTPUT%.img}.efi         -> boot.efi
    ${OUTPUT%.img}.media.conf  -> media.conf

  The machine will look for its ESP at $MEDIA_DEV. If that is wrong the
  refresh skips with a message and the boot is unaffected.
EOF
