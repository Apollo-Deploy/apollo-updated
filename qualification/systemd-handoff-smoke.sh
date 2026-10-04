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
fixture_port="${APOLLO_UPDATED_FIXTURE_PORT:-39429}"
fixture_address="127.0.0.1:${fixture_port}"
if [[ ! "$fixture_port" =~ ^[0-9]+$ ]] || (( fixture_port < 1024 || fixture_port > 65535 )); then
  echo "APOLLO_UPDATED_FIXTURE_PORT must be between 1024 and 65535" >&2
  exit 2
fi

test_binary=""
for candidate in "$repo_dir"/target/debug/deps/systemd_handoff-*; do
  if [[ -x "$candidate" && ! -d "$candidate" ]]; then
    if [[ -n "$test_binary" ]]; then
      echo "more than one systemd_handoff test binary; rebuild the target and retry" >&2
      exit 2
    fi
    test_binary="$candidate"
  fi
done
for binary in "$repo_dir/target/debug/apollo-updated" \
  "$repo_dir/target/debug/apollo-updatectl" \
  "$repo_dir/target/debug/apollo-updated-fixture"; do
  if [[ ! -x "$binary" ]]; then
    echo "build the updater, fixture, and integration test first: $binary is missing" >&2
    exit 2
  fi
done
if [[ -z "$test_binary" ]]; then
  echo "build the systemd_handoff test first with cargo test --test systemd_handoff --no-run" >&2
  exit 2
fi
test_runner="/usr/lib/apollo-updated-qualification/systemd_handoff"
test_fixture="/usr/lib/apollo-updated-qualification/apollo-updated-fixture"

assert_unit_property() {
  local unit="$1" property="$2" expected="$3" actual
  actual="$(systemctl show --property="$property" --value "$unit")"
  if [[ "$actual" != "$expected" ]]; then
    echo "$unit $property expected '$expected', got '$actual'" >&2
    exit 1
  fi
}

service_name="apollo-updated-qualification-fixture"
socket_name="apollo-updated-qualification-fixture.socket"
broker_dropin="/etc/systemd/system/apollo-updated-supervisor.service.d/qualification-fixture.conf"
managed_paths=(
  /etc/apollo-updated
  /etc/systemd/system/apollo-updated.service
  /etc/systemd/system/apollo-updated.socket
  /etc/systemd/system/apollo-updated-supervisor.service
  /etc/systemd/system/apollo-updated-supervisor.socket
  /etc/systemd/system/apollo-updated-supervisor.service.d
  "/etc/systemd/system/${service_name}.service"
  "/etc/systemd/system/${socket_name}"
  /usr/sbin/apollo-updated
  /usr/sbin/apollo-updatectl
  /usr/lib/apollo-updated-qualification
  /usr/lib/tmpfiles.d/apollo-updated-qualification.conf
  /var/lib/apollo-updated
  /var/lib/apollo-updated-supervisor
  /run/apollo-updated
  /run/apollo-updated-generations
)
for path in "${managed_paths[@]}"; do
  if [[ -e "$path" || -L "$path" ]]; then
    echo "refusing to replace existing path: $path" >&2
    exit 2
  fi
done
for pattern in /etc/systemd/system/apollo-updated-gen-*.service \
  /etc/systemd/system/apollo-updated-control-*.socket; do
  compgen -G "$pattern" >/dev/null && {
    echo "refusing to replace existing managed generation units: $pattern" >&2
    exit 2
  }
done
if getent passwd apollo-updated >/dev/null || getent group apollo-update >/dev/null \
  || getent group apollo-updated >/dev/null; then
  echo "refusing to replace an existing updater account or group" >&2
  exit 2
fi
if systemctl is-active --quiet "$socket_name"; then
  echo "refusing to replace an active qualification listener" >&2
  exit 2
fi
python3 - "$fixture_port" <<'PY'
import socket, sys
s = socket.socket()
s.bind(("127.0.0.1", int(sys.argv[1])))
s.close()
PY

cleanup() {
  set +e
  systemctl stop "$service_name.service" "$socket_name" \
    apollo-updated.socket apollo-updated.service \
    apollo-updated-supervisor.socket apollo-updated-supervisor.service >/dev/null 2>&1
  for unit_file in /etc/systemd/system/apollo-updated-gen-*.service; do
    [[ -e "$unit_file" ]] || continue
    systemctl stop "$(basename "$unit_file")" >/dev/null 2>&1
  done
  for unit_file in /etc/systemd/system/apollo-updated-control-*.socket; do
    [[ -e "$unit_file" ]] || continue
    systemctl stop "$(basename "$unit_file")" >/dev/null 2>&1
  done
  rm -f /etc/systemd/system/apollo-updated-gen-*.service \
    /etc/systemd/system/apollo-updated-control-*.socket \
    "/etc/systemd/system/${service_name}.service" \
    "/etc/systemd/system/${socket_name}" \
    "$broker_dropin" /etc/systemd/system/apollo-updated.service \
    /etc/systemd/system/apollo-updated.socket \
    /etc/systemd/system/apollo-updated-supervisor.service \
    /etc/systemd/system/apollo-updated-supervisor.socket \
    /usr/sbin/apollo-updated /usr/sbin/apollo-updatectl
  rm -rf /usr/lib/apollo-updated-qualification
  rm -f /usr/lib/tmpfiles.d/apollo-updated-qualification.conf
  rmdir /etc/systemd/system/apollo-updated-supervisor.service.d >/dev/null 2>&1
  rm -rf /etc/apollo-updated /var/lib/apollo-updated \
    /var/lib/apollo-updated-supervisor /run/apollo-updated \
    /run/apollo-updated-generations
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
install -D -o root -g root -m 0644 "$repo_dir/packaging/apollo-updated-supervisor.service" \
  /etc/systemd/system/apollo-updated-supervisor.service
install -D -o root -g root -m 0644 "$repo_dir/packaging/apollo-updated-supervisor.socket" \
  /etc/systemd/system/apollo-updated-supervisor.socket
install -D -o root -g root -m 0755 "$repo_dir/target/debug/apollo-updated" \
  /usr/sbin/apollo-updated
install -D -o root -g root -m 0755 "$repo_dir/target/debug/apollo-updatectl" \
  /usr/sbin/apollo-updatectl
install -D -o root -g root -m 0755 "$test_binary" "$test_runner"
install -D -o root -g root -m 0755 "$repo_dir/target/debug/apollo-updated-fixture" \
  "$test_fixture"
install -D -o root -g root -m 0644 "$repo_dir/packaging/apollo-updated.tmpfiles" \
  /usr/lib/tmpfiles.d/apollo-updated-qualification.conf
cat >"/etc/systemd/system/${socket_name}" <<UNIT
[Unit]
Description=Disposable Apollo updater package listener

[Socket]
ListenStream=${fixture_address}
Accept=no
Service=apollo-updated-supervisor.service
FileDescriptorName=apollo-updated-listener-fixture
RemoveOnStop=no
UNIT
install -d -o root -g root -m 0755 /etc/systemd/system/apollo-updated-supervisor.service.d
cat >"$broker_dropin" <<UNIT
[Service]
Sockets=${socket_name}
UNIT
cat >/etc/apollo-updated/config.toml <<CONFIG
data_root = "/var/lib/apollo-updated"
socket_path = "/run/apollo-updated/control.sock"
allowed_group = "apollo-update"
max_artifact_size = 67108864
metadata_url = "file:///var/lib/apollo-updated/qualification/metadata"
targets_url = "file:///var/lib/apollo-updated/qualification/targets"
trusted_root = "/etc/apollo-updated/trusted-root.json"

[[packages]]
package_id = "fixture"
service_name = "${service_name}"
architecture = "aarch64-unknown-linux-gnu"

[packages.generation]
socket_unit = "${socket_name}"
entrypoint = "fixture-daemon"
runtime_uid = $(id -u nobody)
runtime_gid = $(id -g nobody)
argv = []
writable_paths = []
CONFIG
chown root:apollo-update /etc/apollo-updated/config.toml
chmod 0640 /etc/apollo-updated/config.toml
systemd-tmpfiles --create /usr/lib/tmpfiles.d/apollo-updated-qualification.conf
systemctl daemon-reload
systemd-analyze verify /etc/systemd/system/apollo-updated.service \
  /etc/systemd/system/apollo-updated.socket \
  /etc/systemd/system/apollo-updated-supervisor.service \
  /etc/systemd/system/apollo-updated-supervisor.socket \
  "/etc/systemd/system/${socket_name}"
systemctl start apollo-updated-supervisor.socket "$socket_name" apollo-updated.socket
assert_unit_property "$socket_name" Accept no
assert_unit_property "$socket_name" Triggers apollo-updated-supervisor.service
assert_unit_property "$socket_name" FileDescriptorName apollo-updated-listener-fixture
result="$(runuser -u apollo-updated -g apollo-update -- /usr/sbin/apollo-updatectl status)"
grep -q '"ok": true' <<<"$result"
runuser -u apollo-updated -g apollo-update -- env \
  APOLLO_UPDATED_SYSTEMD_FIXTURE=1 \
  APOLLO_UPDATED_FIXTURE_ADDRESS="$fixture_address" \
  APOLLO_UPDATED_FIXTURE_BINARY="$test_fixture" \
  "$test_runner" --nocapture
printf '%s\n' "$result"
