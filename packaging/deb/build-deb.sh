#!/usr/bin/env bash
# Build a .deb for Debian/Ubuntu/Linux Mint.
#
# The binary is compiled inside an Ubuntu 22.04 container, so it needs
# glibc >= 2.35 and runs on Ubuntu 22.04+, Mint 21+ and Debian 12+.
#
#   packaging/deb/build-deb.sh            # -> target/deb/layoutpilot_<ver>_amd64.deb
#   BASE_IMAGE=ubuntu:24.04 packaging/deb/build-deb.sh
set -euo pipefail

repo=$(cd "$(dirname "$0")/../.." && pwd)
image=${BASE_IMAGE:-ubuntu:22.04}
toolchain=$(sed -n 's/^channel = "\(.*\)"/\1/p' "$repo/rust-toolchain.toml")
version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' "$repo/Cargo.toml")
out="$repo/target/deb"
build_target="$repo/target/deb-build"
mkdir -p "$out" "$build_target" "${CARGO_HOME:-$HOME/.cargo}/registry"

echo ">> building layoutpilot $version with Rust $toolchain in $image"
docker run --rm \
    -v "$repo:/src:ro" \
    -v "$build_target:/target" \
    -v "${CARGO_HOME:-$HOME/.cargo}/registry:/usr/local/cargo/registry" \
    -e CARGO_TARGET_DIR=/target \
    -e CARGO_RESOLVER_INCOMPATIBLE_RUST_VERSIONS=fallback \
    -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
    -w /src \
    "$image" bash -euc "
        export DEBIAN_FRONTEND=noninteractive RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo
        apt-get update -qq
        apt-get install -y -qq --no-install-recommends ca-certificates curl gcc libc6-dev >/dev/null
        curl -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain $toolchain
        /usr/local/cargo/bin/cargo build --release --locked -p layoutpilot-daemon
        chown -R \$HOST_UID:\$HOST_GID /target /usr/local/cargo/registry
    "

binary="$build_target/release/layoutpilot"
glibc=$(objdump -T "$binary" | grep -oE 'GLIBC_[0-9.]+' | sort -V | tail -1)
echo ">> binary needs $glibc"

root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
install -Dm755 "$binary" "$root/usr/bin/layoutpilot"
install -Dm644 "$repo/packaging/deb/layoutpilot.desktop" "$root/etc/xdg/autostart/layoutpilot.desktop"
sed 's|^ExecStart=.*|ExecStart=/usr/bin/layoutpilot run|' "$repo/packaging/layoutpilot.service" \
    | install -Dm644 /dev/stdin "$root/usr/lib/systemd/user/layoutpilot.service"
doc="$root/usr/share/doc/layoutpilot"
install -Dm644 "$repo/packaging/deb/README.Debian" "$doc/README.Debian"
install -Dm644 "$repo/config.example.toml" "$doc/config.example.toml"
install -Dm644 "$repo/packaging/60-layoutpilot.rules" "$doc/60-layoutpilot.rules"
cat "$repo/LICENSE-MIT" > "$doc/copyright"

size=$(du -sk "$root" | cut -f1)
install -d "$root/DEBIAN"
cat > "$root/DEBIAN/control" <<EOF
Package: layoutpilot
Version: $version
Architecture: amd64
Maintainer: NMitin <nmitin@gmail.com>
Installed-Size: $size
Depends: libc6 (>= 2.35)
Suggests: wl-clipboard
Section: utils
Priority: optional
Homepage: https://github.com/nmitin/key-switcher
Description: keyboard layout auto-corrector (ru/us)
 Fixes words typed in the wrong keyboard layout (ghbdtn -> привет) as you
 type, with double Shift to fix the last word or a selected fragment.
 Works on X11 (XRecord/XTest) and Wayland (evdev/uinput).
EOF

deb="$out/layoutpilot_${version}_amd64.deb"
dpkg-deb --root-owner-group --build "$root" "$deb" >/dev/null
echo ">> $deb"
