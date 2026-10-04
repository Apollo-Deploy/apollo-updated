# Installing apollo-updated

Production packages include the administrator-provisioned TUF public root at
`/etc/apollo-updated/trusted-root.json`, owned by `root:root` with mode `0644`.
Build with the public root produced by the operator's offline root ceremony:

```sh
packaging/build-package.sh /absolute/path/apollo-updated.tar.gz \
  /absolute/path/production-root.json
```

The builder verifies the root signature using the daemon's TUF verifier and
refuses the qualification root. The production root must not be copied from
`qualification/test-root.json`. The daemon validates the root role threshold
signature before it opens the control socket. Keep the private root key offline;
only the public root is installed by the package.

For a manual deployment that does not use the runtime archive, provision the
public root before enabling the socket unit. Keep the private root key offline
and use it only to authorize the initial root and signed root rotations. Install
the approved public root with:

```sh
install -o root -g root -m 0644 approved-root.json \
  /etc/apollo-updated/trusted-root.json
```

Set `trusted_root = "/etc/apollo-updated/trusted-root.json"` in the root-owned
`config.toml`, then run `apollo-updatectl doctor` before enabling
`apollo-updated.socket`. The qualification signing key under
`qualification/private/` must never be used for production releases.

The updater reads this path from `trusted_root` in `config.toml`. Keep it outside
`data_root` and outside all package version directories so package replacement
cannot alter the trust anchor. The `qualification/private/` test key is never a
production signing key and must be excluded from runtime packages and images.

Install the service and socket units, sysusers and tmpfiles definitions from
this directory. Start `apollo-updated.socket` and
`apollo-updated-supervisor.socket`. The updater remains unprivileged. The
root-owned supervisor broker owns the configured application listener sockets
and installs immutable package generations under root-owned paths.

Each mutable package needs a root-owned `generation` policy in `config.toml`.
See [`../config.example.toml`](../config.example.toml) for the fields. Its
`entrypoint`, fixed `argv`, runtime UID/GID, and writable paths come only from
local policy. The runtime identity must be a dedicated non-root account and
must not share the updater authorization UID or group.

For a listener package, set `generation.socket_unit` to a root-owned systemd
socket unit configured like this (use the package ID in the descriptor name):

```ini
[Socket]
ListenStream=/run/example-daemon/listener.sock
Accept=no
Service=apollo-updated-supervisor.service
FileDescriptorName=apollo-updated-listener-example-daemon
```

Add that socket to the broker service's `Sockets=` list using a root-owned
drop-in, then start it:

```ini
[Service]
Sockets=example-daemon.socket
```

The broker requires one active descriptor for every configured package socket
at startup and checks the descriptor names against local policy. It keeps the
bound listener while a candidate starts privately and passes a duplicate to
that candidate only after the previous generation has stopped accepting and
drained. Package processes must implement the private generation control ABI
described in [`generation-protocol.md`](generation-protocol.md); they must stay
gated until `Activate`, drain existing requests before acknowledging `Drain`,
and support `Resume` for compensation and rollback.

Build the runtime archive with `packaging/build-package.sh`, passing a
production root as its second argument. It stages the daemon, client, root and
systemd packaging files; qualification roots and fixture binaries are not
included. On a disposable Debian ARM64 systemd VM, build the debug binaries and
integration test with the pinned Rust toolchain, then run:

```sh
cargo build --bins
cargo test --test systemd_handoff --no-run
sudo env APOLLO_UPDATED_SYSTEMD_FIXTURE=1 qualification/systemd-handoff-smoke.sh
```

The handoff qualification checks two real systemd generations, preserves an
open connection through the handoff, and verifies new traffic reaches the
committed generation. It refuses pre-existing updater paths/accounts and
removes only the paths and units it installs. The older
`systemd-socket-smoke.sh` checks only updater API socket activation.
