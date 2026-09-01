#!/bin/bash
# Build the netboot initramfs: stormnetboot-init as /init, the stormblock
# engine, busybox, real kmod, and the storage + network modules needed to reach
# a root that is on the other side of a NIC.
#
# Runs ON the build box (dev.g8.lo), never on a workstation — the modules come
# from the target kernel's tree and a macOS build has none of this.
#
#   ./scripts/build-netboot-initramfs.sh [init-binary] [stormblock-binary] [kver] [output]
#
# Output goes to /build/images by default. NEVER write it into /tmp: /tmp on
# dev is a tmpfs sized at half of RAM, and an image written there is RAM that
# is never given back.
set -euo pipefail

INIT_BIN="${1:-target/x86_64-unknown-linux-musl/release/stormnetboot-init}"
STORMBLOCK_BIN="${2:-/build/assets/stormblock}"
KVER="${3:-$(uname -r)}"
OUTPUT="${4:-/build/images/stormnetboot-initramfs-${KVER}.img}"
MODROOT="${MODROOT:-/lib/modules}"

say() { printf '==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

[[ -x "$INIT_BIN" ]]       || die "no init binary at $INIT_BIN (cargo build --release --target x86_64-unknown-linux-musl)"
[[ -x "$STORMBLOCK_BIN" ]] || die "no stormblock binary at $STORMBLOCK_BIN"
[[ -d "$MODROOT/$KVER" ]]  || die "no module tree at $MODROOT/$KVER"

# A dynamically linked init cannot run in an initramfs that has no loader.
# Checking here turns a kernel panic into a build error.
if file "$INIT_BIN" | grep -q "dynamically linked"; then
    die "$INIT_BIN is dynamically linked; build it against x86_64-unknown-linux-musl"
fi

ROOT="$(mktemp -d)"
cleanup() { rm -rf "$ROOT"; }
trap cleanup EXIT

say "staging in $ROOT"
mkdir -p "$ROOT"/{bin,sbin,usr/sbin,usr/share/udhcpc,lib/modules,dev,proc,sys,run,etc,tmp,var,sysroot}

install -m 0755 "$INIT_BIN" "$ROOT/init"
install -m 0755 "$STORMBLOCK_BIN" "$ROOT/usr/sbin/stormblock"

# busybox provides mount, ip, sh and the rest. Every applet is symlinked so a
# path lookup finds it whatever name it is called by.
BUSYBOX="$(command -v busybox)" || die "busybox not installed on the build host"
install -m 0755 "$BUSYBOX" "$ROOT/bin/busybox"
for applet in $("$BUSYBOX" --list); do
    ln -sf /bin/busybox "$ROOT/bin/$applet"
done
ln -sf /bin/busybox "$ROOT/sbin/switch_root"
ln -sf /bin/busybox "$ROOT/sbin/udhcpc"

# Real kmod, not busybox's modprobe: the busybox applet does not read
# modules.dep the same way and silently fails to pull dependencies.
KMOD="$(command -v modprobe)" || die "kmod not installed on the build host"
install -m 0755 "$KMOD" "$ROOT/usr/sbin/modprobe"
install -m 0755 "$(command -v depmod)" "$ROOT/usr/sbin/depmod"
mkdir -p "$ROOT/lib64"
for lib in $(ldd "$KMOD" | awk '{print $3}' | grep -v '^$'); do
    install -m 0755 "$lib" "$ROOT/lib64/" 2>/dev/null || true
done
LOADER="$(ldd "$KMOD" | grep 'ld-linux' | awk '{print $1}')"
[[ -n "$LOADER" ]] && install -m 0755 "$LOADER" "$ROOT/lib64/" 2>/dev/null || true

say "copying modules for $KVER"
# nvme + nvme_tcp are what makes a remote root possible at all; ublk is what
# turns it into a block device; net drivers are what makes any of it reachable.
SUBTREES="kernel/drivers/nvme kernel/drivers/block kernel/drivers/net kernel/drivers/scsi kernel/drivers/ata kernel/drivers/virtio kernel/fs kernel/lib kernel/crypto kernel/net"
for sub in $SUBTREES; do
    src="$MODROOT/$KVER/$sub"
    [[ -d "$src" ]] || continue
    mkdir -p "$ROOT/lib/modules/$KVER/$(dirname "$sub")"
    cp -a "$src" "$ROOT/lib/modules/$KVER/$(dirname "$sub")/"
done
for meta in modules.order modules.builtin modules.builtin.modinfo; do
    [[ -f "$MODROOT/$KVER/$meta" ]] && cp -a "$MODROOT/$KVER/$meta" "$ROOT/lib/modules/$KVER/"
done

# depmod against the staged tree, so modprobe can resolve dependencies inside
# the initramfs rather than against the build host's kernel.
depmod -b "$ROOT" "$KVER" || die "depmod failed; the module set is incomplete"

# Confirm the two modules without which this initramfs cannot do its job.
for required in nvme_tcp ublk_drv; do
    if ! grep -q "/${required}\.ko" "$ROOT/lib/modules/$KVER/modules.dep"; then
        die "$required is not in the staged module set; a netboot root is impossible without it"
    fi
done

cat > "$ROOT/usr/share/udhcpc/default.script" <<'SCRIPT'
#!/bin/sh
# Minimal DHCP lease applier. Only the bound/renew case matters here: the
# initramfs needs an address long enough to reach the portal.
case "$1" in
    bound|renew)
        ip addr add "$ip/$mask" dev "$interface" 2>/dev/null
        ip link set "$interface" up
        [ -n "$router" ] && ip route add default via "${router%% *}" dev "$interface" 2>/dev/null
        : > /etc/resolv.conf
        for dns in $dns; do echo "nameserver $dns" >> /etc/resolv.conf; done
        ;;
esac
exit 0
SCRIPT
chmod 0755 "$ROOT/usr/share/udhcpc/default.script"

echo 'ublk[bc].* 0:0 0660' > "$ROOT/etc/mdev.conf"

mkdir -p "$(dirname "$OUTPUT")"
say "packing $OUTPUT"
(cd "$ROOT" && find . | cpio -o -H newc --quiet) | zstd -19 -T0 -q -f -o "$OUTPUT"

say "built $(du -h "$OUTPUT" | cut -f1) at $OUTPUT"
say "publish it as the initramfs member of a boot pallet:"
cat <<EOF

  sbregistry pallet publish stormcos/boot:\$VERSION \\
      --member kernel=/boot/vmlinuz-$KVER \\
      --member initramfs=$OUTPUT
EOF
