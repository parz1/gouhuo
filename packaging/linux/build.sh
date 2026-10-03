#!/usr/bin/env bash
# Build the single-file Linux server without a runtime libc dependency.
set -euo pipefail
cd "$(dirname "$0")/../.."

version=$(awk '/^\[/{p=($0=="[workspace.package]")} p&&/^version *=/{gsub(/"/,"",$3);print $3;exit}' Cargo.toml)
if [[ -z "$version" ]]; then
  echo "Cargo.toml has no workspace version" >&2
  exit 1
fi
if [[ -n "${GITHUB_REF_NAME:-}" && "${GITHUB_REF_TYPE:-}" == tag && "${GITHUB_REF_NAME#v}" != "$version" ]]; then
  echo "Tag does not match Cargo.toml: $GITHUB_REF_NAME / $version" >&2
  exit 1
fi

target=x86_64-unknown-linux-musl
# Rust supplies the musl CRT. Use the system driver for static PIE linking;
# Debian's musl-gcc wrapper can inject an interpreter even with crt-static.
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=cc
export CC_x86_64_unknown_linux_musl=musl-gcc
export RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=+crt-static -C link-self-contained=yes"
cargo build --locked --profile dist --target "$target" -p server --bin gouhuo-server "$@"
binary="${CARGO_TARGET_DIR:-target}/$target/dist/gouhuo-server"
program_headers=$(readelf -l "$binary")
dynamic_section=$(readelf -d "$binary")
if grep -q 'INTERP' <<< "$program_headers"; then
  echo "Linux server unexpectedly requires a dynamic loader" >&2
  exit 1
fi
if grep -q 'NEEDED' <<< "$dynamic_section"; then
  echo "Linux server unexpectedly requires shared libraries" >&2
  exit 1
fi

suffix=""
for arg in "$@"; do
  if [[ "$arg" == --no-default-features ]]; then suffix="-no-web"; fi
done
name="gouhuo-server-$version$suffix-linux-x64-musl"
stage="target/linux-package/$name"
mkdir -p "$stage" target/installer
cp "$binary" "$stage/gouhuo-server"
chmod 755 "$stage/gouhuo-server"
cp LICENSES/MPL-2.0.txt "$stage/LICENSE"
cp packaging/linux/README.md "$stage/README.md"
tar -czf "target/installer/$name.tar.gz" -C "$stage" gouhuo-server LICENSE README.md
(cd target/installer && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
echo "Static Linux server: target/installer/$name.tar.gz"
