# Package generation control protocol

The supervisor starts each package executable as an unprivileged systemd
generation with its private Unix control listener on file descriptor 3. The
package must not accept application traffic until it receives `activate`.
Application listener descriptors are never present in the candidate's unit at
startup. After the old generation drains, the root broker transfers exactly
one listening stream socket with `SCM_RIGHTS` as part of the `activate`
request. The broker retains its own descriptor for recovery.

Each broker connection is a fresh local Unix stream connection. The broker
checks `SO_PEERCRED` against the generation's systemd `MainPID`. A request is
one marker byte followed by one JSON line:

```json
{"id":1,"command":"health","timeout_ms":1000}
```

The marker is byte `0xA7`. The broker sends one descriptor only with
`activate` for a listener package; all other requests carry no descriptor.
Requests are limited to 4 KiB. Commands are `health`, `activate`, `drain`,
`resume`, and `stop`. The process replies with one JSON line containing the
same `id`, its OS `pid`, configured `version`, `ok`, `healthy`, `accepting`,
`active_connections`, and `listener_installed`:

```json
{"id":1,"pid":1234,"version":"1.2.3","ok":true,"healthy":true,"accepting":false,"active_connections":0,"listener_installed":false}
```

`health` must describe this exact process without relying on the shared
application listener. `activate` validates and installs the transferred socket
while gated, then begins accepting and reports both `listener_installed` and
`accepting` as true. `drain` closes the accept gate before replying; the broker
waits for `active_connections` to reach zero while checking the candidate is
still healthy. `resume` reopens the gate after a failed handoff. `stop` closes
the gate and succeeds only when no application connections remain. Listenerless
packages receive no descriptor and report `listener_installed` as false.

This protocol is versioned by the package contract's `protocol_version`. Keep
the generation control socket private to root and the package process. The
fixture daemon and systemd handoff test in this repository provide a reference
implementation for the current version.
