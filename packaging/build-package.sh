#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
output_path="${1:-}"
root_path="${2:-}"
if [[ -z "$output_path" || "$output_path" != /* || -z "$root_path" || "$root_path" != /* ]]; then
  echo "usage: $0 /absolute/path/to/apollo-updated.tar.gz /absolute/path/to/production-root.json" >&2
  exit 2
fi
if [[ "$(uname -s)" != Linux ]]; then
  echo "runtime packages must be built on Linux" >&2
  exit 2
fi

cargo build --manifest-path "$repo_dir/Cargo.toml" --release \
  --bin apollo-updated --bin apollo-updatectl
if [[ ! -f "$root_path" || -L "$root_path" ]]; then
  echo "production trust root must be a regular file, not a symlink" >&2
  exit 2
fi
root_path="$(realpath -e "$root_path")"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT

root_stage="$stage/etc/apollo-updated/trusted-root.json"
install -D -m 0644 "$root_path" "$root_stage"
"$repo_dir/target/release/apollo-updated" verify-root "$root_stage" \
  --reject-keys-from "$repo_dir/qualification/test-root.json"

install -D -m 0755 "$repo_dir/target/release/apollo-updated" \
  "$stage/usr/sbin/apollo-updated"
install -D -m 0755 "$repo_dir/target/release/apollo-updatectl" \
  "$stage/usr/sbin/apollo-updatectl"
install -D -m 0644 "$repo_dir/packaging/apollo-updated.service" \
  "$stage/usr/lib/systemd/system/apollo-updated.service"
install -D -m 0644 "$repo_dir/packaging/apollo-updated.socket" \
  "$stage/usr/lib/systemd/system/apollo-updated.socket"
install -D -m 0644 "$repo_dir/packaging/apollo-updated-supervisor.service" \
  "$stage/usr/lib/systemd/system/apollo-updated-supervisor.service"
install -D -m 0644 "$repo_dir/packaging/apollo-updated-supervisor.socket" \
  "$stage/usr/lib/systemd/system/apollo-updated-supervisor.socket"
install -D -m 0644 "$repo_dir/packaging/apollo-updated.sysusers" \
  "$stage/usr/lib/sysusers.d/apollo-updated.conf"
install -D -m 0644 "$repo_dir/packaging/apollo-updated.tmpfiles" \
  "$stage/usr/lib/tmpfiles.d/apollo-updated.conf"
install -D -m 0644 "$repo_dir/packaging/README.md" \
  "$stage/usr/share/doc/apollo-updated/README.md"
install -D -m 0644 "$repo_dir/packaging/generation-protocol.md" \
  "$stage/usr/share/doc/apollo-updated/generation-protocol.md"
install -D -m 0644 "$repo_dir/config.example.toml" \
  "$stage/usr/share/doc/apollo-updated/config.example.toml"
mkdir -p "$(dirname "$output_path")"
tar --owner=0 --group=0 --numeric-owner -C "$stage" -czf "$output_path" .
if tar -tzf "$output_path" | grep -Eq '(^|/)(qualification|apollo-updated-fixture)(/|$)'; then
  echo "runtime archive unexpectedly contains qualification material" >&2
  rm -f "$output_path"
  exit 1
fi
