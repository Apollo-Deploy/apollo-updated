#!/usr/bin/env bash
set -euo pipefail

if [[ "${APOLLO_UPDATED_SYSTEMD_FIXTURE:-}" != 1 || "$EUID" != 0 ]]; then
  echo "run as root on a disposable systemd Linux VM with APOLLO_UPDATED_SYSTEMD_FIXTURE=1" >&2
  exit 2
fi
if [[ "$(ps -p 1 -o comm= | tr -d ' ')" != systemd ]]; then
  echo "PID 1 is not systemd" >&2
  exit 2
fi

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
for binary in "$repo_dir/target/debug/apollo-updated" \
  "$repo_dir/target/debug/apollo-updatectl"; do
  if [[ ! -x "$binary" ]]; then
    echo "build the daemon and client first: $binary is missing" >&2
    exit 2
  fi
done
for path in /etc/apollo-updated /etc/systemd/system/apollo-updated.service \
  /etc/systemd/system/apollo-updated.socket /usr/sbin/apollo-updated \
  /usr/sbin/apollo-updatectl /var/lib/apollo-updated /run/apollo-updated; do
  if [[ -e "$path" ]]; then
    echo "refusing to replace existing path: $path" >&2
    exit 2
  fi
done
if getent passwd apollo-updated >/dev/null || getent group apollo-update >/dev/null \
  || getent group apollo-updated >/dev/null; then
  echo "refusing to replace existing updater account or group" >&2
  exit 2
fi

cleanup() {
  set +e
  systemctl stop apollo-updated.socket apollo-updated.service >/dev/null 2>&1
  rm -f /etc/systemd/system/apollo-updated.socket \
    /etc/systemd/system/apollo-updated.service /usr/sbin/apollo-updated \
    /usr/sbin/apollo-updatectl
  rm -rf /etc/apollo-updated /var/lib/apollo-updated /run/apollo-updated
  userdel apollo-updated >/dev/null 2>&1
  groupdel apollo-update >/dev/null 2>&1
  groupdel apollo-updated >/dev/null 2>&1
  systemctl daemon-reload >/dev/null 2>&1
}
trap cleanup EXIT

systemd-sysusers "$repo_dir/packaging/apollo-updated.sysusers"
install -D -o root -g root -m 0644 "$repo_dir/qualification/test-root.json" \
  /etc/apollo-updated/trusted-root.json
install -D -o root -g root -m 0644 "$repo_dir/packaging/apollo-updated.service" \
  /etc/systemd/system/apollo-updated.service
install -D -o root -g root -m 0644 "$repo_dir/packaging/apollo-updated.socket" \
  /etc/systemd/system/apollo-updated.socket
install -D -o root -g root -m 0755 "$repo_dir/target/debug/apollo-updated" \
  /usr/sbin/apollo-updated
install -D -o root -g root -m 0755 "$repo_dir/target/debug/apollo-updatectl" \
  /usr/sbin/apollo-updatectl
cat >/etc/apollo-updated/config.toml <<'CONFIG'
data_root = "/var/lib/apollo-updated"
socket_path = "/run/apollo-updated/control.sock"
allowed_group = "apollo-update"
max_artifact_size = 16777216
metadata_url = "file:///var/lib/apollo-updated/qualification/metadata"
targets_url = "file:///var/lib/apollo-updated/qualification/targets"
trusted_root = "/etc/apollo-updated/trusted-root.json"

[[packages]]
package_id = "fixture"
service_name = "fixture"
architecture = "aarch64-unknown-linux-gnu"
CONFIG
chown root:apollo-update /etc/apollo-updated/config.toml
chmod 0640 /etc/apollo-updated/config.toml
systemd-tmpfiles --create "$repo_dir/packaging/apollo-updated.tmpfiles"
systemctl daemon-reload
systemctl start apollo-updated.socket
result="$(runuser -u apollo-updated -g apollo-update -- /usr/sbin/apollo-updatectl status)"
grep -q '"ok": true' <<<"$result"
main_pid="$(systemctl show apollo-updated.service --property=MainPID --value)"
[[ "$main_pid" =~ ^[1-9][0-9]*$ ]]
printf '%s\n' "$result"
