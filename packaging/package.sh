#!/usr/bin/env bash
# Pack release archives into out/ after `cargo build --release`.
#
#   bash packaging/package.sh Linux   aarch64-unknown-linux-gnu
#   bash packaging/package.sh macOS   x86_64-apple-darwin
#   bash packaging/package.sh Windows
#
# The second argument is the extra target built next to the native one.
set -euo pipefail

os="$1"
extra="${2:-}"
mkdir -p out stage

# pack <archive name> <folder with the binaries> <tar|zip>
pack() {
    local name="$1" bindir="$2" format="$3"
    local dir="stage/$name"
    rm -rf "$dir"
    mkdir -p "$dir"
    for f in hotbar hotbarw hotbar.exe hotbarw.exe; do
        if [ -f "$bindir/$f" ]; then
            cp "$bindir/$f" "$dir/"
        fi
    done
    cp README.md LICENSE packaging/config.toml "$dir/"
    if [ "$format" = zip ]; then
        (cd stage && 7z a -tzip "../out/$name.zip" "$name" > /dev/null)
    else
        tar -C stage -czf "out/$name.tar.gz" "$name"
    fi
}

case "$os" in
    Linux)
        pack hotbar-linux-x86_64 target/release tar
        if [ -n "$extra" ]; then
            pack hotbar-linux-arm64 "target/$extra/release" tar
        fi
        ;;
    macOS)
        # One binary that runs natively on Apple Silicon and Intel Macs.
        mkdir -p target/universal
        for b in hotbar hotbarw; do
            lipo -create -output "target/universal/$b" "target/release/$b" "target/$extra/release/$b"
        done
        pack hotbar-macos-universal target/universal tar
        ;;
    Windows)
        pack hotbar-windows-x86_64 target/release zip
        ;;
    *)
        echo "unknown os: $os" >&2
        exit 1
        ;;
esac

ls -l out
