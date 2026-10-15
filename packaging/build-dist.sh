#!/bin/sh
# Builds the release artifacts for this machine's architecture into DIR:
# a tarball, a .deb, an .rpm and SHA256SUMS. Used by CI and the release
# workflow; requires a prior `cargo build --release`, cargo-deb and
# cargo-generate-rpm.
set -eu

dir=${1:?usage: build-dist.sh DIR}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
arch=$(uname -m)
name="airpipe-$version-$arch-linux"

mkdir -p "$dir"
staging=$(mktemp -d)
trap 'rm -rf "$staging"' EXIT

mkdir "$staging/$name"
cp target/release/airpipe README.md CONTRIBUTING.md LICENSE-MIT LICENSE-APACHE "$staging/$name/"
cp -r packaging "$staging/$name/"
tar -C "$staging" -czf "$dir/$name.tar.gz" "$name"

cargo deb --no-build --output "$dir/"
cargo generate-rpm --output "$dir/"

(cd "$dir" && rm -f SHA256SUMS && sha256sum -- * >SHA256SUMS && cat SHA256SUMS)
