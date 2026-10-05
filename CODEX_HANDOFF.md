# Codex Development Handoff

Prepared on 2026-10-04 for migration from the local development session to Codex Cloud. Repository: https://github.com/Apollo-Deploy/apollo-updated. Verified source baseline: `main` at `daf4fa75f39ede3e856cf17e3431350b93d8c43f`. This document records repository facts, historical observations, and unresolved requirements separately. It is a development handoff, not a production qualification or release approval.

## 1. Executive Summary

`apollo-updated` is a standalone, Linux-first Rust package updater with a local Unix API, TUF verification, immutable package storage, a durable lifecycle, a root-owned systemd supervisor broker, and an in-process fixture supervisor. The intended product must install, upgrade, recover, roll back, garbage-collect, update itself, and coordinate compatible package sets while preserving listeners, accepted connections, and unrelated workloads.

Current status: **PARTIALLY COMPLETED**. Substantial single-package verification, overlap handoff, rollback, recovery, policy, packaging, and fixture work exists. Required self-update, coordinated-set transactions, and safe garbage collection do not exist. The delivered systemd path has passed a disposable-VM handoff smoke, but the entire original qualification matrix and independent release approval have not passed. The current full suite was not rerun after the final safety changes.

The immediate continuation point is a conditional recovery ordering concern in `src/lifecycle/recovery/rollback/restore.rs`: one branch can commit the service alias before a distinct retiring generation's drain is checked. Whether that three-generation shape is reachable under the supervisor's two-generation limit remains unproved. Investigate reachability and add focused verification before declaring it fixed or harmless.

A separate, fundamental unresolved issue is the traffic trust boundary: an uncommitted package receives the application listener and can accept real traffic before durable commit. Cooperative fixture behavior does not enforce the threat model against a malicious candidate. Do not confuse the successful cooperative handoff tests with resolution of that issue.

The source release is public, MIT-licensed, and pushed. This handoff was created locally without changing implementation files; it is not committed or pushed. Supply it to Cloud separately or explicitly arrange its publication before expecting a fresh clone to contain it.

## 2. Original Objective

The original instruction was to build a production-grade, independent Rust daemon that atomically installs, upgrades, health-checks, rolls back, and garbage-collects declared packages without interrupting bound listeners, accepted connections, or workloads the package does not own. No dependency on another Apollo crate, daemon, or control-plane type is permitted. A generic signed manifest is the package contract.

The signing root and reversible supervisor are deliverables, not prerequisites supplied by a company platform. Missing external signing or supervisor infrastructure is not grounds to stop. Deliver a first-class systemd adapter and a fixture supervisor; refuse an individual incompatible package with a typed error instead of degrading to stop/start.

The original completion response was restricted to `APOLLO_UPDATED_PRODUCTION_COMPLETE`, `PARTIAL: <blocking gaps>`, or `BLOCKED: <reason>`. `BLOCKED` is allowed only for a requirement that cannot safely be met on Linux with the delivered components. Required behavior cannot be deferred to V2, TODO, or a later product phase. This separate handoff request explicitly forbids feature implementation and asks for a migration report; it does not certify completion of the original build.

Requirements later added or clarified: use Luna 6 at maximum reasoning for subagents instead of Astra; keep the public repository under the **Apollo-Deploy** organization; use MIT; exclude performance reports, verdicts, and similar development artifacts from the public release. The last instruction for this task is solely to reconstruct context and write this file.

## 3. Requirements and Constraints

Hard constraints:

- Preserve old service availability throughout download, verification, staging, successor startup, health, and bounded drain. No in-place executable overwrite, stop-old/start-new fallback, listener drop, unrelated package restart, or “came back within N seconds” interpretation of zero downtime.
- A failed health check, drain timeout, or successor exit before commit must leave or restore the old serving process, preserve the active pointer, and cause no traffic move. Rollback uses retained known-good content with the same overlap rule. Listenerless packages also start and verify the successor before retiring the old process.
- A compromised release source, untrusted package/manifest/URL, compromised uncommitted candidate, partial write, crash, power loss, and concurrent clients are in scope. The installed trust root and host supervisor adapter are trusted. Do not substitute cooperation by an untrusted package for an enforced supervisor boundary.
- No Apollo-specific package logic, cloud control plane, secret storage, networking policy, custom cryptography, arbitrary manifest commands, or deletion outside updater-owned paths. Preserve runtime microVMs, TAP attachments, log WAL, and other unowned workloads; model unowned work with a side process. Preserve edge-style accepted connections and configuration/route continuity by handoff.
- Use maintained crypto/TUF libraries. Install the configured root outside package/data trees, root-owned mode `0644`; refuse daemon startup for missing, unreadable, invalidly self-signed roots. Deliver a test root and offline test signing tooling, with a distinct production root path. Never ship a qualification private key or silently use the test root in production.
- TUF must cover signed root/targets/snapshot/timestamp, threshold signatures, monotonic versions, timestamp/snapshot expiry, current-root-authorized rotation, and binding of package ID, version, architecture, exact size, and digest. No delegations are needed unless a later requirement demonstrates the need. Reject substitution, mix-and-match, rollback, freeze, wrong identity/architecture, and endless downloads.
- Local API only, Unix socket, peer authentication. Mutations require root or the configured updater group. Ship daemon/client, hardened systemd units, sysusers/tmpfiles, and a dedicated unprivileged updater account. Keep source modules about 100–300 lines where practical, with a 500-line ceiling; tracked Rust sources currently comply. `Cargo.lock` is generated and exceeds 500 lines; do not misrepresent that as literal compliance by every tracked file.
- Keep every lifecycle transition durable before its next external effect. Crash recovery must converge to verified committed new service or retained known-good old service with the listener held, never a mixed tree, half pointer, truncated artifact, or availability gap. Serialize same-package operations; independent packages should progress independently. Failed recovery remains durable and retryable.

Required package fields: package ID, SemVer, architecture, artifact source, SHA-256, exact size, service, inherited-FD/SO_REUSEPORT/no-listener mode, typed health, health/drain/stabilization timing, compatibility bounds for updater/package/state/protocol, and optional coordinated-set membership/order. Typed health means generation-specific process/supervisor state plus optional bounded Unix request or locally allowlisted executable with fixed argv, expected exit, timeout, failure threshold, and stabilization. Reject manifest shell strings, executable overrides, relative health executable paths, and environment overrides. A locally configured relative package entrypoint is a separate constrained policy field.

Required coordinated behavior: stage every member while old services run, durably journal the set, hand off in dependency order, reverse already moved members after any failure, commit only when all successors serve, and recover to a complete compatible set. Required self-update: immutable successor, explicit inherited-control-FD handoff, successor local health before old exit, old service/pointers preserved on failure, and retained predecessor re-exec with the same FD after committed-successor failure.

Required resource controls: configured artifact maximum; disk headroom for download, extraction, and retained previous content; bounded idle/total download time; exact-length verification; no shell interpolation; only local admin allowlists or typed supervisor operations; secret-free structured audit; history retaining operation/version/digest/result/timestamps. GC must preserve active, previous, staged, serving, and rollback-history-referenced releases.

Original required state progression, per package and separately per coordinated set: `IDLE → DOWNLOADING → VERIFIED → STAGED → HANDING_OFF → VERIFYING_HEALTH → DRAINING → COMMITTED`, or `FAILED → ROLLING_BACK → ROLLED_BACK`. The implementation adds an internal `Committing` phase for replay across alias/pointer/history effects. An enum variant does not establish that every corresponding durable transition or set transaction is implemented. After commit, the service alias must execute the committed immutable tree; after failed rollback, retain known-good content and retry without stopping the healthy serving process.

Required CLI: `status`, `check`, `update <package>`, `update --all`, `rollback <package>`, `history <package>`, `verify <package>`, `gc`, `doctor`. `doctor` must cover trust expiry, state consistency, active integrity, supervisor reachability, listener ownership, and disk headroom. The 24 qualification cases and independent red-team gate are detailed below; none may be replaced by a weaker happy-path demonstration.

## 4. Development Rules

No applicable on-disk `AGENTS.md` was found in this repository or inspected parent instructions. The user repeatedly supplied the following rules directly; preserve them even if Cloud has no matching file, and read any new Cloud-provided instructions before editing:

- Search first with `rg`; read only relevant lines, and do not reread unchanged files unnecessarily. Prefer existing code or standard/framework facilities before adding the smallest complete change, preserving validation, error handling, and accessibility where applicable.
- Run the narrowest affected test first. Run the full suite once at the end after focused checks pass. Wait in one command for an external operation rather than repeated sleep/check turns. After two failed attempts at the same fix, stop that attempt and report what was learned.
- Delegate self-contained searches/investigations to a small, fast subagent so exploration reads do not fill the main conversation; keep decisions and edits with the main agent. Later explicit model steering was “Stop all astra sub agents and use luna 6 on max.” Select the available equivalent deliberately rather than silently returning to Astra. Do not load overlapping skills for the same framework.
- Lead with results, keep complete sentences, avoid narrating tool calls, preserve unrelated dirty-tree work, and suggest a new chat with a short handoff when the user switches to an unrelated task. Simulator accessibility-first guidance exists but is irrelevant to this Rust/Linux task.
- Inspect the real implementation and contracts before proposing UI or speculative fixes. Separate local fixture evidence, real systemd evidence, and production qualification. An ignored local file, a skipped integration test, or a discussed design is not proof of implementation.

Never reset, clean, stash, revert, discard, or overwrite work to prepare migration. During this handoff task, no features were implemented and no runtime tests were rerun. Do not publish internal qualification/performance/verdict material as a routine source-release side effect. The user subsequently requested this comprehensive document, including performance and security context, but did not explicitly ask to publish it.

## 5. Architecture

Authoritative code is under `src/`; integration tests under `tests/`; installation/protocol documents under `packaging/`; disposable qualification support under `qualification/`. The package manifest and toolchain are `Cargo.toml`, `Cargo.lock`, and `rust-toolchain.toml`. There are no tracked database migrations, CI workflows, cloud infrastructure, milestone plans, separate architecture documents, or persistent project instruction files. README and packaging documents are the existing user-facing documentation.

| Component | Actual responsibility / key paths |
| --- | --- |
| Entrypoints | `src/main.rs`: daemon, two Tokio workers; `src/bin/updatectl.rs` and `src/cli.rs`: CLI; `src/bin/fixture.rs`: disposable test daemon, excluded from production archive. |
| Configuration/policy | `src/settings.rs`, `src/settings/generation.rs`: root-admin configuration, trusted paths/root, package/service/architecture allowlist, exact health policy, fixed generation entrypoint/argv, runtime UID/GID, socket unit, writable paths. `config.example.toml` is an example, not deploy-ready policy. |
| Contract/state/errors | `src/contract.rs`, `src/state.rs`, `src/error.rs`: generic typed package/health/handoff compatibility, durable package state, typed refusal/errors. Coordinated-set types exist without transaction execution. |
| Verification | `src/tuf_client.rs`, `src/tuf_client_tests.rs`: Tough client, persistent TUF metadata, root checks, signed custom target identity, target download, compatibility/high-water checks. |
| Storage | `src/disk.rs`, `src/disk/space.rs`, `src/archive.rs`: confined download/extraction, reservations, immutable promotion, fsynced JSON, atomic pointers, per-package file locks. `src/disk/tests.rs` exercises storage edges. |
| Lifecycle | `src/lifecycle.rs`, `src/lifecycle/install.rs`, `health.rs`, `operator_rollback.rs`, `rollback.rs`, `commit_history.rs`: install/health/activation/drain/commit/compensation and known-good operator rollback. |
| Recovery | `src/lifecycle/recovery.rs`, `recovery/active.rs`, `commit.rs`, `rollback.rs`, `rollback/restore.rs`: phase replay, reboot recovery, partial commit repair, retirement, restore retained content while preserving serving processes. |
| Supervisor contract | `src/supervisor.rs`: full `GenerationHandle`, generation-specific operations and snapshots; `src/supervisor/protocol.rs`: typed request/reply/control protocol. |
| Privileged systemd side | `src/supervisor/broker.rs`, `systemd_manager.rs`, `systemd_manager/identity.rs`, `control.rs`, `reconcile.rs`, `systemd_units.rs`: peer-authenticated root broker, systemd identity, listener FD transfer, root-owned generations/registry/unit aliases, reconciliation. |
| Systemd client | `src/supervisor/systemd.rs`: unprivileged RPC adapter; polls drain snapshots without monopolizing the broker. `fd_transfer.rs` transfers FDs via `SCM_RIGHTS`. |
| Fixture side | `src/supervisor/fixture.rs`, `fixture/supervisor_impl.rs`, `fixture/test_support.rs`, `fixture_daemon.rs`: independent two-generation fixture supervisor and cooperative daemon/control protocol. |
| API/history/audit | `src/api.rs`: newline JSON Unix API, bounds/auth/locks/recovery; `src/history.rs`: durable replay-safe JSONL; `src/audit.rs`: structured stderr events. No audit-log retrieval API. |
| Delivery | `packaging/build-package.sh`, units/sysusers/tmpfiles, `packaging/README.md`, `generation-protocol.md`; `qualification/generate_test_root.py`, public test root, smoke scripts. |

Data flow: authenticated client → package lock/recovery barrier → TUF target/manifest verification and disk reservation → confined staging/promotion → durable transaction/generation intent → supervisor start → generation-specific health → activate successor → old drain/wait while monitoring successor → durable committing state → broker service alias commit → active/previous pointers/state/history → safe predecessor retirement. Activation currently precedes old drain and allows overlapping acceptors; see the unresolved trust-boundary and documentation mismatch below.

Signed target custom metadata uses `apollo_package`; a manifest target must be exactly `packages/<id>/<version>/<architecture>/manifest.json`, and its artifact target must stay in the matching package/version/architecture namespace with signed hash/length binding. Duplicate same-version targets and versions below the durable highest verified version are rejected. Current artifact download accepts signed TUF targets only; schema Local/Https alternatives are not direct unsigned download support.

Durable layout under the configured data root: `packages/<id>/versions/<version>/` contains payload, manifest, digest sidecar, and retained archive; atomic `active`/`previous` symlinks point within the version namespace; `staging/` holds partial work; `state/<id>.json` holds package state; `state/locks/` serializes packages; `trust/` holds Tough metadata; history is fsynced JSONL. JSON writes use temporary file, fsync, rename, and directory synchronization. Do not infer complete crash qualification merely from these primitives.

The privileged broker maintains a separate root-owned registry at `/var/lib/apollo-updated-supervisor/registry.json`, content-addressed package copies, service aliases under `/etc/systemd/system`, and transient generation/control paths under `/run`. It executes fixed systemctl operations, not shell commands. Package generation identity includes service, package ID, generation ID, PID, systemd invocation ID, version, and artifact digest. PID fields alone are informational, not stop/drain authority.

External requirements are Linux, systemd for its real adapter, and a TUF release source; no Apollo platform, database, registry service, NATS, cloud control plane, or existing company supervisor is required by this implementation. Maintained dependencies include Tough 0.21 with HTTP support, Tokio, Serde, SemVer, SHA-256, nix 0.30/libc, tar/zstd, fs2, UUID, and related support crates. Pin behavior to the committed lockfile.

## 6. Important Decisions

**Decision:** Use maintained Tough/TUF and one targets role, with an installed local root.  
**Reason:** Meet rollback/freeze/signature requirements without inventing a parallel security scheme or waiting for a service.  
**Alternatives considered:** Custom crypto, unsigned/hash-only packages, external signing infrastructure as a blocker, unnecessary delegation.  
**Why alternatives were rejected:** They weaken the required trust chain, create unnecessary scope, or violate the delivered-root requirement. Tough support is implemented; full adversarial rotation/expiry qualification remains outstanding.  
**Relevant files:** `src/tuf_client.rs`, `src/tuf_client_tests.rs`, `qualification/`, `packaging/build-package.sh`.

**Decision:** Root admin policy fixes package identity and the entire readiness/execution tuple.  
**Reason:** A signed but malicious manifest is still untrusted; it cannot choose updater-privileged commands, health sockets, runtime identities, or arbitrary argv.  
**Alternatives considered:** Allowlisting only the health executable path, trusting manifest arguments, running generations as updater/root.  
**Why alternatives were rejected:** Allowed binaries with attacker-chosen arguments or privileged sockets still enable escalation; shared updater credentials permit API/broker impersonation.  
**Relevant files:** `src/settings.rs`, `src/settings/generation.rs`, `src/contract.rs`, `src/lifecycle/health.rs`, `src/supervisor/systemd_units.rs`.

**Decision:** Split the unprivileged daemon from a narrowly authenticated root systemd broker.  
**Reason:** Unit/FD/root-owned storage management needs privileged authority while routine API/download work should not run as root.  
**Alternatives considered:** Entire updater privileged, arbitrary manifest hooks, requiring a preinstalled proprietary supervisor.  
**Why alternatives were rejected:** Excessive authority or external dependencies conflict with the generic, delivered-supervisor boundary.  
**Relevant files:** `src/supervisor/broker.rs`, `systemd.rs`, `systemd_manager.rs`, `systemd_units.rs`, packaging units.

**Decision:** Use complete generation handles plus authoritative systemd invocation identity for lifecycle operations.  
**Reason:** Bare PIDs and version-only selectors can target another package or a reboot-reused process. A UUID intent is durable before spawn.  
**Alternatives considered:** PID-only stop/drain, version-only lookup, silently adopting legacy/unmanaged processes.  
**Why alternatives were rejected:** PID reuse and stale unit invocations make those unsafe. Legacy in-flight states without sufficient identity fail closed; unmanaged adoption needs an explicit future safe contract.  
**Relevant files:** `src/supervisor.rs`, `src/state.rs`, `src/supervisor/systemd_manager/identity.rs`, `reconcile.rs`, lifecycle/recovery modules.

**Decision:** Keep generation trees immutable and commit pointers after serving/health/drain conditions.  
**Reason:** Preserve running binaries and retained rollback content across partial writes and crash replay. Operator rollback does not reduce the verified-version high-water mark.  
**Alternatives considered:** Overwrite binaries, rename a live mutable tree, lower rollback resistance when rolling back, delete failed state immediately.  
**Why alternatives were rejected:** These destroy availability or security evidence and make compensation non-deterministic.  
**Relevant files:** `src/disk.rs`, `archive.rs`, `state.rs`, `lifecycle/install.rs`, `operator_rollback.rs`, recovery modules.

**Decision:** Monitor successor liveness throughout predecessor drain; never stop a live undrained process merely because it is unhealthy.  
**Reason:** Health, liveness, acceptance, and drain completion are different facts. On timeout, restore previous acceptance and persist retryable compensation.  
**Alternatives considered:** Commit on drain timeout, force-kill unhealthy generations, stop candidate before restoring old acceptance.  
**Why alternatives were rejected:** Those paths can drop accepted connections or leave no serving generation. The remaining conditional ordering concern must still be verified.  
**Relevant files:** `src/lifecycle/install.rs`, `rollback.rs`, `recovery/rollback.rs`, `recovery/rollback/restore.rs`, lifecycle regression tests.

**Decision:** Poll drain status in the unprivileged client rather than blocking the serialized root broker.  
**Reason:** A real systemd smoke deadlocked when one `WaitForDrain` occupied the broker and prevented a concurrent request from finishing the held fixture connection.  
**Alternatives considered:** Increase timeout, force stop, weaken the drain test, keep a blocking broker request.  
**Why alternatives were rejected:** They hide or preserve the deadlock and violate the bounded-drain invariant. Polling allows status/completion requests to make progress.  
**Relevant files:** `src/supervisor/systemd.rs`, `broker.rs`, `systemd_manager.rs`, `tests/systemd_handoff.rs`.

**Decision:** Package generations start with a private control socket; the app listener is transferred after a same-stream health preflight and identity recheck.  
**Reason:** Separate private readiness from the public application listener and avoid trusting systemd listener peer credentials as the generation's PID.  
**Alternatives considered:** Passing all app FDs at spawn, trusting `SO_PEERCRED` of a systemd-created listener to identify the package, a drain-first/late-transfer gate.  
**Why alternatives were rejected:** Early raw FDs permit premature acceptance; listener peer credentials can identify PID 1; drain-first transfer creates an app-accept gap. **Current transfer still gives an uncommitted candidate a raw app FD and is not a completed security solution.**  
**Relevant files:** `src/supervisor/systemd_manager/control.rs`, `fd_transfer.rs`, `systemd_units.rs`, `fixture_daemon.rs`, `packaging/generation-protocol.md`.

**Decision:** Hold a cross-package space-reservation guard through extraction and installation.  
**Reason:** Dropping the reservation at the end of download lets concurrent extractions overcommit disk. Current budget includes artifact, retained content, and a fixed 8 GiB extraction allowance.  
**Alternatives considered:** Download-only headroom check, release reservation before extraction, lower the budget to make an undersized test tmpfs pass.  
**Why alternatives were rejected:** They fail the lifecycle resource invariant. Use a sufficiently large persistent test TMPDIR instead.  
**Relevant files:** `src/disk/space.rs`, `src/tuf_client.rs` (`VerifiedPackage` reservation lifetime), `src/lifecycle/install.rs`.

**Decision:** Compare decoded root public-key identity and validate the exact staged production root bytes.  
**Reason:** Byte comparison missed the same qualification key in reformatted JSON; copying a different file after validation creates a packaging TOCTOU.  
**Alternatives considered:** Root-file byte equality, key-ID-only comparison, validation before a later unrelated copy, checking in the test private key.  
**Why alternatives were rejected:** Formatting/hex-case/key-ID aliases bypass superficial checks; later copies can change the installed trust; private test credentials do not belong in production/public artifacts.  
**Relevant files:** `src/tuf_client.rs`, `src/cli.rs`, `src/tuf_client_tests.rs`, `packaging/build-package.sh`, `qualification/generate_test_root.py`.

## 7. Work Completed

**COMPLETED as source implementation, with verification limits below:** generic Rust binaries/contract and typed errors; root-admin policy validation; root self-signature/expiry/key-exclusion checks; Tough-backed signed target loading; bounded target download and hash/length checks; confined tar.zstd extraction; immutable promotion; atomic state/pointers; per-package locks; cross-package reservations; history/audit primitives. No database migration or Apollo integration was introduced.

**COMPLETED as source implementation:** root broker with Unix peer authentication, full generation handles, current MainPID/InvocationID reconciliation, root-owned generation copies/unit aliases, package-specific socket units, private control readiness, raw listener transfer, status of generations, and generation-specific health/drain/stop/commit/rollback operations. Listenerless packages require explicit local approval; unsupported handoff modes refuse rather than cold-restart.

**COMPLETED as source implementation:** install with intent before spawn, health/activation, drain with candidate monitoring, durable committing/pointer/history flow, safe retirement, automatic compensation, retained known-good operator rollback, and multiple recovery paths including dead committed generations, partial pointers, replay-idempotent history, interrupted rollback, and retryable failed drain. This statement does not resolve every recovery shape or prove all power-loss interleavings.

**COMPLETED fixes from this session:** tighten health argv/socket policy; normalize root-key identity; validate staged root bytes; retain extraction reservation; reject unsafe runtime UID/GID overlap; scope runtime cleanup per generation; reconcile stale/dead systemd records before capacity checks; replace PID-only authority; restore acceptance before retirement; preserve live generations on drain failure; poll drain outside the broker; count fixture accepted connections under the acceptance gate.

**COMPLETED delivery:** MIT license, README, example configuration, production package builder, updater/broker systemd units, sysusers/tmpfiles, public test root/generator, fixture and disposable systemd smoke support. Production archive excludes the fixture binary and qualification private keys. Public GitHub repository is under `Apollo-Deploy` with one initial source commit. Every completed source item above belongs to that commit; individual fixes have no separate committed history.

**COMPLETED observed checks:** historical Linux compilation/formatting and focused test passes; a signed ephemeral release through the production TUF verifier; cooperative fixture connection/rollback/drain/recovery cases; a real systemd handoff preserving an open connection. Scope and dates matter: the latest source is newer than the last full suite and the last real-systemd run.

## 8. Work in Progress

- **PARTIALLY COMPLETED:** recovery and safe retirement are implemented but the special restore branch's first service-alias commit ordering lacks a reachability proof/regression. Most recent independent review did not finish with approval.
- **PARTIALLY COMPLETED:** reversible app listener handoff works for cooperative fixtures/systemd packages. Trusted enforcement against a malicious uncommitted listener holder is absent. Current docs describe a different transfer/drain order than code.
- **PLANNED, not implemented:** updater self-update/bootstrap ownership protocol; coordinated-set execution, journal, ordered rollback and recovery; safe GC reference inventory/deletion. `update --all` and `gc` deliberately return errors. Coordinated types alone are not functionality.
- **PARTIALLY COMPLETED:** CLI/API/doctor shape exists, but no-argument `check` is inconsistent, check/verify side effects need a contract audit, and doctor lacks complete root-supervisor listener/owned-tree integrity evidence.
- **NEEDS VERIFICATION:** the full original signed qualification matrix, crash/power-loss campaign, actual 10,000 lifecycle churn, cross-package/stale-invocation authorization regressions, root rotation/freeze/rollback attacks, low-resource claims, and independent release approval.
- **BLOCKED:** no external-infrastructure blocker was established. A Cloud environment without systemd cannot supply real-systemd evidence itself; use a disposable VM. That environment limit is not product-level `BLOCKED`.
- **ABANDONED:** the attempted drain-first/late-FD design as a solution to the full invariant; bare-PID authority; stop/start/forced-timeout shortcuts; public performance/verdict artifacts. Do not resume these by accident.

There is no separate uncommitted feature patch at migration. Unfinished behavior is in the committed public baseline, not hidden in a stash or another branch.

## 9. Conversation Context That Is Not Obvious From Code

The updater build began before this directory was a Git repository. The public repository was created only after many iterations; its initial commit aggregates the entire source. Consequently, Git cannot reconstruct ordering or author individual bug fixes. The chronology here comes from the development conversation and observed runs, checked against final source where possible.

The public repository request explicitly excluded performance docs, verdicts, and similar reports. No retained red-team/performance artifact in the public tree means neither “review never happened” nor “approval happened”; reviews did find issues, fixes were made, and the latest review was interrupted before an approval. The original product gate remains unsatisfied. Keep this handoff local unless publication is explicitly arranged.

The main late-stage effort was generation-identity migration and safe recovery/drain ordering. An intermediate migration had approximately 141 compiler errors; those were resolved. Do not read that historical failure count as a current build blocker. The latest focus moved to listener ownership/security and a conditional distinct-retiring recovery branch, not to unrelated product expansion.

Historical source synchronization accidentally flattened `src/` into the repository root. Ignored top-level duplicate Rust files/directories remain locally, including old `api.rs`-style files and `bin/`, `disk/`, `lifecycle/`, `supervisor/`. They are obsolete snapshots, not Cargo inputs or Cloud requirements. Never edit/publish them, infer architecture from them, or use `rsync --delete` to synchronize the worktree. A fresh Git clone correctly lacks them.

An ignored local qualification private-key directory existed for disposable tests. Its contents were not needed for this handoff and must not be transferred into source or a production package. The latest signed verifier test generates an ephemeral root/key rather than depending on that private local directory. Cloud must generate its own disposable test material in a fresh temporary directory.

Real Linux work used a native ARM64 Debian 13 VM with systemd 257.13 and Rust 1.96.0. Its default cargo PATH sometimes selected Rust 1.85.1, so commands explicitly used `+1.96.0`. The local workstation is macOS; Linux credential/stat/control APIs caused host compile issues, so macOS checks cannot substitute for Linux verification.

The VM's `/tmp` was a roughly 5.9 GiB tmpfs with about 4.4 GiB free during one run, below the fixed 8 GiB extraction reservation. A signed verifier test failed with `InsufficientDisk`; a poisoned global test mutex then caused secondary failures. Moving TMPDIR to persistent `/home` storage (roughly 88 GiB free at that time) resolved that environmental cause. Those are historical capacity observations, not requirements to reproduce identical sizes or benchmark measurements.

The systemd smoke initially could not run qualification binaries from a user home under the restricted updater account. The script now stages root-readable fixture/test binaries under `/usr/lib/apollo-updated-qualification` and allows an explicit fixture-binary override. Port reuse/TIME_WAIT also produced misleading failures; use a fresh reserved test port and inspect pre-existing units/accounts/paths before the disposable smoke.

Two protocol/testing discoveries materially affected implementation: a serialized broker cannot block in drain while another request must finish an open connection; and systemd-created listener `SO_PEERCRED` cannot reliably identify a generation's MainPID. Same-stream control preflight plus systemd invocation identity is the implemented response to the latter, but does not make package-reported health/counters trustworthy against malicious code.

## 10. Bugs / Known Issues

**Issue 1 — Uncommitted package can accept real traffic.** Symptoms: `install.rs` activates the candidate before draining the old generation; both can accept. `control.rs` transfers the raw application listener before durable commit. Root cause: trusted supervisor owns the bound socket but relinquishes acceptance authority to untrusted candidate code. Attempts: drain-first/late-FD gating kept queued connections but created an acceptance gap; reverting to overlap restored availability while leaving malicious precommit exposure. State: **PARTIALLY COMPLETED, unresolved security/availability requirement**. Files: `src/lifecycle/install.rs` around activation/drain, `src/supervisor/systemd_manager/control.rs`, `fixture_daemon.rs`, protocol docs. Next: design and test a trusted enforcement mechanism, such as trusted accepted-socket dispatch, while reconciling the generic contract, two generations, rollback, accepted connections, and failure-before-commit semantics. Do not solve this solely by swapping the two calls or trusting `accepting=false` reports.

**Issue 2 — Conditional service commit before distinct retirement drain.** Symptoms: in `recovery/rollback/restore.rs`, the live-but-unhealthy previous / healthy candidate branch invokes `commit_active` near line 107, while handling a distinct retiring generation is later near lines 194–210. Root cause: early service-alias commit in a special branch, unlike the defensively reordered normal rollback path. Attempts: normal rollback's retirement ordering was fixed; this special branch remained. State: **NEEDS VERIFICATION**; a three-distinct-live-generation shape may be unreachable because the manager caps generations at two, so do not claim a proved exploitable HIGH. Files: that restore file, `src/lifecycle/recovery/rollback.rs`, supervisor capacity/reconcile logic, recovery tests. Next: map reachable durable state shapes across crashes/reboots/reconciliation; require all relevant drain waits before the first alias/pointer/terminal commit, with timeout preserving serving processes and retryability; prove unreachable shapes or add a focused regression and minimal fix.

**Issue 3 — Protocol documentation contradicts current code.** Symptoms: `packaging/README.md` describes draining old before app-FD transfer; `generation-protocol.md` also describes late transfer and a MainPID peer-credential claim. Current code activates new before old drain and uses authoritative systemd identity plus same-stream preflight. Cause: docs retained earlier designs. Attempts: control implementation changed but docs were not fully reconciled. State: **NEEDS VERIFICATION/documentation correction**. Next: after resolving traffic enforcement, update both documents to the actual protocol and its guarantees; until then, treat code as behavior and the original user contract as required behavior.

**Issue 4 — Self-update is absent.** Symptoms: installed `/usr/sbin/apollo-updated` is static; no verified successor/control-FD bootstrap, explicit self-handoff health, or retained predecessor re-exec path exists. Cause: required implementation never completed. State: **PLANNED**, not a platform limitation. Files: `src/main.rs`, API/lifecycle/supervisor, updater service/socket units. Next: design durable bootstrap ownership and failure recovery, implement it, and test successful/failed handoff and control calls before old exit without stop/start.

**Issue 5 — Coordinated transactions are absent.** Symptoms: `update --all` returns `SupervisorCannotHandoff`; coordinated-set schema has no transaction executor/journal/recovery. Cause: required multi-package implementation never completed. State: **PLANNED**. Files: `src/contract.rs`, `src/api.rs`, lifecycle/state. Next: stage all, persist set intent, execute declared order, reverse prior members on any failure, commit/recover the whole set, prove no mixed incompatible serving set.

**Issue 6 — GC is absent.** Symptoms: `gc` returns an error rather than deleting versions. Cause: safe reference inventory for active/previous/staged/serving/history/set/self-update content is not implemented. State: **PLANNED**; conservative refusal is preferable to unsafe deletion, but does not satisfy scope. Files: `src/api.rs`, disk/history/supervisor. Next: construct a complete owned reference graph and confined deletion with foreign-path/symlink/race tests.

**Issue 7 — API/doctor contract gaps.** Symptoms: CLI permits `check` with no package, while dispatch requires a package; `verify` persists candidate/high-water information without installing; `check` downloads/verifies but does not persist the same high-water update. Doctor covers useful state/pointer/root/disk/supervisor facts but its listener owner is derived from reported serving generations, not a complete trusted root inventory. Cause: incremental API behavior and incomplete diagnostics. State: **PARTIALLY COMPLETED**. Files: `src/cli.rs`, `src/api.rs`, `src/tuf_client.rs`, state. Next: define required all-package check semantics, audit mutation/auth/high-water behavior and downloaded-content cleanup, and add only meaningful contract tests; expand doctor to supported trusted evidence.

**Issue 8 — Qualification gap, not a demonstrated runtime failure.** Symptoms: newest code lacks a current full-suite run, genuine 10,000 update lifecycle churn, coordinated/self-update qualification, complete signed crash campaign, and final red-team approval. Cause: development/qualification incomplete; latest broad review interrupted. State: **NEEDS VERIFICATION**. Files: `tests/`, `qualification/`, all required new features. Next: focused fixes then one final full suite and dedicated complete qualification. Do not convert older passing results or skipped tests into a release verdict.

**Issue 9 — Download/restart and signed-source completeness need audit.** Symptoms: `Downloading` exists in the state schema, but the transfer occurs before install persists its verified transaction; source schema permits Local/Https shapes while the current downloader accepts TUF targets only. Cause: implementation narrower than the generic source/state contract. State: **PARTIALLY COMPLETED/NEEDS VERIFICATION**. Files: `src/tuf_client.rs`, `src/api.rs`, `src/state.rs`, disk staging. Next: prove crash-during-download cleanup/resume and monotonic verified-target behavior; implement any required source support without bypassing signed target binding. Do not add unsigned direct-download fallback.

## 11. Failed or Rejected Approaches

- Stop/start, force-stop on drain timeout, killing live unhealthy generations, or merely checking “unit active”: rejected by the availability invariant. Never weaken assertions or drain gates to obtain a passing smoke.
- Give the candidate a raw listener and assume its health/control flags prevent premature accepts: insufficient under the malicious-candidate threat model. Drain-first/late transfer also does not establish continuous app acceptance and complete no-traffic-move-on-failure semantics.
- Bare PID/version selectors and automatic adoption of unmanaged or legacy in-flight processes: unsafe across package boundaries, reboot PID reuse, and systemd invocation replacement. Preserve full-handle checks and explicit fail-closed results.
- Trust manifest probe argv/socket/environment just because a path is allowlisted: unsafe; validate the entire local-policy tuple. Running package UID equal to updater UID, or package GID equal to API authorization group, is forbidden.
- Identify a generation using `SO_PEERCRED` on a systemd-created listener: can yield PID 1. Use the recorded/current systemd invocation and same-stream readiness, while acknowledging untrusted package responses.
- `systemd-run --property=Sockets=...`: rejected in the observed systemd 257 environment. Generated root-owned service units with `Sockets=` and named activated listeners worked. `Accept=yes` per-connection services do not supply the required reusable listening-FD contract. Two shell/busctl signature quoting failures were tooling failures, not proof that systemd FD passing is impossible.
- Blocking `WaitForDrain` inside the single-threaded broker: deadlocks concurrent completion/status work. Client-side snapshot polling is the implemented fix; simply increasing the timeout is not a fix.
- Comparing root JSON bytes or only key IDs: bypassed by reformatting/hex case/aliases; compare decoded public-key algorithm/material. Validating one root then copying another: TOCTOU; validate the actual staged bytes used in the archive.
- Release disk reservation immediately after download, or reduce extraction allowance to fit test `/tmp`: concurrent overcommit or weakened safety. Keep guard lifetime and provision persistent storage instead.
- Global runtime-directory deletion when one generation retires: removes another generation's control socket. Cleanup must be scoped. Dead stale registry entries must be reconciled before rejecting two-generation capacity.
- Run the restricted systemd test binary from inaccessible `/home`, reuse a conflicting port, capture old generation identity after it is stopped, or read queued v2 responses before joining the handoff thread: these caused misleading failures. Use the staged accessible binary, fresh port, pre-stop handle capture, and correct test synchronization.
- Flatten source into root or use destructive sync/cleanup to fix it: obsolete ignored snapshots resulted. Cargo's tracked `src/` tree is authoritative. Public qualification/performance/verdict documents were excluded at the user's request.

## 12. Tests and Verification

Available tests: library tests cover contract/root/target/storage/policy/reconciliation; `tests/fixture_handoff.rs` has seven fixture tests; `tests/update_lifecycle.rs` and its submodules currently have 18 lifecycle tests; `tests/systemd_handoff.rs` has one environment-gated real-systemd test. Its ordinary run prints a skip and returns success unless `APOLLO_UPDATED_SYSTEMD_FIXTURE=1`; that success is not real-systemd evidence.

Commands below are run from the repository on Linux with the pinned toolchain; historical commands are shown without local SSH wrappers. Do not assume an old pass covers today's HEAD or a future patch.

| Historical observation | Command / result | Practical limit |
| --- | --- | --- |
| Last recovered full all-targets run, 2026-10-04 approximately 06:46–06:49 UTC | `cargo +1.96.0 test --all-targets`: library 30 passed, fixture 7 passed, lifecycle 17 passed; gated systemd test returned success without real execution; exit 0. | Before full-handle migration and later overlap/drain/recovery reorder and ephemeral signing-test change. **Not current full-suite verification.** |
| Subsequent affected-suite runs | Lifecycle 17/17 and fixture 7/7 passed after identity migration. | Before the latest 18th recovery/drain test and final reorder. Do not claim all current 18 passed together. |
| Latest recovery-focused run, approximately 09:51 UTC | `cargo +1.96.0 test --test update_lifecycle recovery`: 12 passed, 6 filtered, 97.28 seconds, exit 0. Earlier matching run was 97.17 seconds. | Focused cooperative fixture recovery evidence, not every lifecycle case or power-loss campaign. |
| Latest signed verifier test, approximately 09:52 UTC | `cargo +1.96.0 test --lib signed_qualification_release_loads_through_the_production_verifier`: 1 passed, 29 filtered, 0.05 seconds, exit 0. | Uses newly generated ephemeral signing material; not full TUF attack/rotation qualification. |
| Late formatting/compile checks, approximately 09:06 UTC | Pinned `cargo fmt --check` / `cargo check --all-targets` passed. | Before the publication-time ephemeral signing-test adjustment; rerun on Cloud/current source. |
| Last actual systemd handoff, approximately 09:02 UTC | `sudo -n env APOLLO_UPDATED_SYSTEMD_FIXTURE=1 APOLLO_UPDATED_FIXTURE_PORT=39437 qualification/systemd-handoff-smoke.sh`: 1 passed, 13.59 seconds, exit 0; disposable cleanup performed. | Native ARM64 VM; before final retirement reorder. Test constructs `VerifiedPackage` directly and uses artifact-only reservation, not signed TUF → systemd end-to-end. Choose a fresh port, not necessarily 39437. |

Manual packaging verification used a distinct fresh production root, checked installed root ownership `0:0` and mode `0644`, validated the same staged bytes, and checked exclusion of fixture/qualification private material. Rejection checks covered reformatted and hex-case variants of the qualification key, oversized root input (>1 MiB), and symlink root input. This is meaningful local packaging evidence, not production key custody/rotation certification.

The available fixture cases include inherited-listener/open-connection/unowned-side-process preservation; unhealthy successor/drain timeout; successor crash during activation/commit/old drain; queued-connection compensation; retained-release operator rollback with unchanged high water; crash/reboot recovery, pointer/history replay; unresolved transaction refusal; missing previous restart; live unhealthy previous restoration; and timeout preserving a retryable recovery generation. Source names are explicit in the tests; inspect the relevant submodule before extending it.

Original qualification matrix, with conservative current status. Every case must ultimately assert active/previous release, generation/serving identity, listener ownership, health, and absence of partial files; every injected crash must restart the updater and prove deterministic listener-preserving convergence. Existing narrower tests do not automatically satisfy all those assertions.

| # | Required case | Current evidence / status |
| --- | --- | --- |
| 1 | Successful upgrade | **PARTIALLY COMPLETED:** single-package lifecycle and real-systemd cooperative smoke; complete signed end-to-end assertion set still needed. |
| 2 | Corrupt artifact | **PARTIALLY COMPLETED:** digest/length verification implemented; complete signed corrupt-release/API/recovery qualification needed. |
| 3 | Invalid signature | **PARTIALLY COMPLETED:** invalid bootstrap signature tests and Tough verification; complete bad release metadata campaign needed. |
| 4 | Wrong package or architecture | **PARTIALLY COMPLETED:** signed target identity/policy binding tests/code; full release/serving-state assertions needed. |
| 5 | Rollback attack | **PARTIALLY COMPLETED:** TUF metadata persistence and verified-version high water; adversarial end-to-end rollback resistance unqualified. |
| 6 | Stale/frozen metadata | **PARTIALLY COMPLETED:** safe expiry and root-expiry reporting; freeze/snapshot/timestamp campaign unqualified. |
| 7 | Truncated/endless download | **PARTIALLY COMPLETED:** bounded length/hash/idle/total behavior and partial-size cleanup tests; live endless/truncation campaign needed. |
| 8 | Insufficient disk | **PARTIALLY COMPLETED:** reservation checks and observed correct InsufficientDisk refusal; full lifecycle/state/serving assertions needed. |
| 9 | Crash during download | **PLANNED qualification:** durable download boundary/partial cleanup/restart convergence not proved. |
| 10 | Crash after staging | **PARTIALLY COMPLETED:** persisted-state recovery fixtures; actual signed kill/power-loss campaign needed. |
| 11 | Crash during handoff | **PARTIALLY COMPLETED:** recovery/crash fixtures; complete durable failpoint/restart campaign needed. |
| 12 | New daemon fails to start | **PARTIALLY COMPLETED:** startup/compensation paths exist; full signed failed-exec qualification needed. |
| 13 | Starts but fails health | **PARTIALLY COMPLETED:** cooperative fixture failure preserves old; malicious precommit traffic boundary unresolved. |
| 14 | Crash during stabilization | **PARTIALLY COMPLETED:** successor crash/commit/drain regressions; exact signed stabilization restart qualification needed. |
| 15 | Rollback interrupted by crash | **PARTIALLY COMPLETED:** interrupted-rollback recovery tests; exhaustive retained-tree/identity/listener assertions needed. |
| 16 | Coordinated multi-package partial failure | **PLANNED:** transaction execution/recovery absent. |
| 17 | Updater self-update / failed handoff | **PLANNED:** bootstrap implementation absent. |
| 18 | 10,000 update/rollback cycles, no FD/state/temp leaks | **PARTIALLY COMPLETED:** fixture toggles the same two generations 10,000 times and compares `/proc/self/fd`; no full download/stage/state/temp lifecycle churn. |
| 19 | Listener open/accepting throughout successful handoff, no refusal | **PARTIALLY COMPLETED:** cooperative fixture and real-systemd evidence; complete signed/adversarial continuity qualification needed. |
| 20 | Health failure before drain, old listener/pointer unchanged | **PARTIALLY COMPLETED:** cooperative fixture evidence; raw-FD malicious candidate case unresolved. |
| 21 | Successor dies during drain, old restored before probe, pointer matches | **PARTIALLY COMPLETED:** fixture crash-during-drain/compensation tests; current signed/systemd/restart campaign needed. |
| 22 | Edge-style existing connection survives | **PARTIALLY COMPLETED:** open TCP connection survives fixture/systemd smoke; no production route/config-state proof. |
| 23 | Self-update accepts control call before old exit | **PLANNED:** self-update absent. |
| 24 | Coordinated second member health failure hands first back without gap | **PLANNED:** coordinated rollback absent. |

Known observed failures were environmental undersized tmpfs/poisoned test mutex, intermediate migration compile errors, earlier smoke deadlock, permissions, port conflicts, and invalid test synchronization; do not label them current unexplained test failures. Conversely, there is no recovered current complete-suite result to claim a clean release. No runtime test was executed during the documentation-only handoff.

Recommended verification order after the first bounded fix: focused recovery regression; relevant library/fixture/lifecycle filters; `cargo +1.96.0 fmt --check` and `cargo +1.96.0 check --locked --all-targets`; then one `cargo +1.96.0 test --locked --all-targets`. Run real systemd separately in a disposable VM with the explicit gate. Expand testing only for changes, failures, or unresolved risks; implementation-mirroring tests are not substitutes for behavior/security assertions.

## 13. Performance Information

The original goal included low daemon RSS/CPU and bounded resource use. There is no recovered trustworthy throughput, latency distribution, RSS, CPU, allocation, or production-load benchmark for current code. No numerical production performance claim is justified. The test durations above are test execution times, not service benchmarks. The public repository intentionally contains no performance/verdict report.

Implemented bounds/choices: two Tokio workers; 64 concurrent API slots; 64 KiB request bounds; 10-second API idle read timeout; artifact maximum from config (example 1 GiB, validation cap 64 GiB); manifest/root bounds of 1 MiB; artifact stream idle 45 seconds and total 1,800 seconds; TUF transport 1,800-second timeout, 10-second connect timeout, two tries; fixed 8 GiB extraction reservation plus artifact/retained content. Confirm each bound in current source before changing it. These are safety limits, not profiling findings.

Download/verification/extraction happen while the old service runs; the intended critical path is successor readiness/activation/drain/commit. Root broker requests are serialized; eliminating blocking drain there was required for correctness as well as progress. The 10,000 toggle test addresses one FD leak class only; state/temp growth and true update churn remain unmeasured. After correctness gates, measure representative workloads in a disposable environment and keep results separate from source publication unless requested.

## 14. Security Considerations

Trust boundaries: root-admin configuration and installed TUF root authorize immutable target content and allowed local actions; the unprivileged updater authenticates local clients; the root broker authenticates updater UID/root and performs constrained privileged generation operations. Package payload, signed manifest, URL, and candidate health/drain replies remain untrusted. Signed authorization is not proof a package behaves honestly.

Configuration/root files require protected ancestors, no symlink traversal, root ownership, bounded size, and non-writable admin permissions. The trusted root is outside `data_root` and checked before opening the API. Production packager verifies a staged root and rejects public-key identity overlap with the qualification root, including JSON/hex reformatting. Root rotation is delegated to Tough's authenticated root chain; dedicated end-to-end rotation testing is still required.

API mutation authorization uses connected-socket `SO_PEERCRED` and Linux `SO_PEERGROUPS` for primary/supplementary group membership; do not replace it with `/proc/<pid>/status` group lookup because process identity can change. The broker uses connected-peer credentials and bounded requests. Package runtime UID cannot equal updater UID and its GID cannot equal the API authorized group. Health executable/probe tuples, service/socket names, entrypoint/argv, and writable paths come from root-admin policy.

Archives reject escaping paths, symlinks/hardlinks/special members and enforce extraction limits. Owned store paths and pointers are confined. Root broker reconstructs/validates its immutable copy before privileged service use. History carries digests and operation IDs and is replay-idempotent. GC has no safe deletion implementation yet, so refusal is intentional rather than permitting foreign deletion.

Outstanding security concerns are malicious candidate listener acceptance; unqualified state corruption/symlink race/cross-package identity cases; conditional early service commit; incomplete self-update/set/GC boundaries; download/high-water/API side effects; and the lack of a complete adversarial qualification campaign. Do not treat broker peer auth or full handles as sufficient proof that every cross-package invocation path is safe; dedicated regressions remain needed.

Original independent red-team attacks to preserve: signature bypass; rollback/freeze; manifest tampering; package substitution; path traversal; symlink races; malicious archive; oversized download; hook/command injection; foreign-file deletion; state corruption; concurrent activation; rollback corruption; self-update failure; listener drop/forced stop-start; mixed-generation coordinated sets; and drain timeout stopping old before new is healthy. Fix findings and repeat until an independent review records `RED_TEAM_RELEASE_APPROVED` with **0 CRITICAL, 0 HIGH, and 0 unresolved mandatory MEDIUM**. That approval has not been obtained. User model steering and applicable review instructions govern agent selection.

Never copy private keys, tokens, passwords, credential-bearing URLs, or secrets into logs, this document, source, or package output. No actual secret values are included here. Generate disposable signing material locally and use a separately administered production root; production signing custody is not provided by a checked-in test key.

## 15. Environment and Configuration

Use Linux for authoritative compilation/testing. Manifest is Rust edition 2024, package version 0.1.0, `rust-version = "1.96"`; `rust-toolchain.toml` pins 1.96.0 with rustfmt/clippy. Cargo is the package manager and `Cargo.lock` is committed. Install a native C toolchain; pkg-config/OpenSSL development headers may be required by the locked dependencies on the chosen distribution. Python 3 and OpenSSL CLI support qualification signing. No database or Apollo service is needed for unit/fixture tests.

Provide a persistent writable TMPDIR with more than the fixed 8 GiB reservation available, plus artifact/build/retained-content headroom. Do not reduce safety budgets to accommodate a small tmpfs. On Linux, a bootstrap command sequence is:

```sh
rustup toolchain install 1.96.0 --component rustfmt --component clippy
mkdir -p "$PWD/.cloud-tmp"
export TMPDIR="$PWD/.cloud-tmp"
df -h "$TMPDIR"
cargo +1.96.0 check --locked --all-targets
cargo +1.96.0 test --locked --test update_lifecycle recovery -- --nocapture
```

`.cloud-tmp` is an example local scratch directory, not an existing tracked requirement; keep it out of commits. Use another suitably sized persistent scratch path if appropriate. Run the final full suite once after focused work. If downloads/toolchain installation are unavailable, report the concrete environment limit without inventing successful verification.

Configuration defaults/example: `APOLLO_UPDATED_CONFIG` selects a config path, default `/etc/apollo-updated/config.toml`; example `data_root = "/var/lib/apollo-updated"`, API `/run/apollo-updated/control.sock`, maximum artifact size 1 GiB, installed root under `/etc/apollo-updated/`. Set the production root path explicitly according to packaging docs. Package policy declares exact ID/service/architecture, readiness tuples, generation entrypoint/argv, separate non-root UID/GID, socket unit and allowed writable paths. Example accounts/paths are not automatically correct for your machine.

Architecture strings are exact opaque policy/target values. The example uses `x86_64-linux-gnu`; the real systemd fixture used `aarch64-unknown-linux-gnu`. Do not silently normalize or substitute Rust triples without revising the contract and tests. Supported package/state/protocol formats are version 1; updater min/max compatibility uses SemVer.

Environment variable names to retain: `APOLLO_UPDATED_CONFIG`; systemd `LISTEN_PID`, `LISTEN_FDS`, `LISTEN_FDNAMES`; generated package protocol `APOLLO_PACKAGE_VERSION`, `APOLLO_UPDATED_CONTROL_FD`, `APOLLO_UPDATED_EXPECT_LISTENER_FD`; qualification `APOLLO_UPDATED_SYSTEMD_FIXTURE`, `APOLLO_UPDATED_FIXTURE_PORT`, `APOLLO_UPDATED_FIXTURE_ADDRESS`, `APOLLO_UPDATED_FIXTURE_BINARY`; and `TMPDIR`. The smoke sets the address and binary overrides for its staged test runner. Do not transfer local SSH credentials, host aliases, root private keys, or private test files as a Cloud prerequisite.

Fresh disposable root generation: `python3 qualification/generate_test_root.py /absolute/fresh-output-directory`; use `mktemp -d` to choose a fresh output. The generator refuses to overwrite the checked-in qualification root in place. The checked-in public test root is not a production root and Cloud need not possess its original private key.

Production archive command: `packaging/build-package.sh /absolute/output.tar.gz /absolute/production-root.json`. It builds release daemon/client binaries, copies the root into staging, validates those bytes and rejects qualification-key overlap, then packages `/usr/sbin` binaries, root/config example, units, sysusers/tmpfiles. It does not package the fixture binary. Validate/install config and runtime IDs on a disposable target before enabling services. Packaging docs currently suggest doctor before enabling the socket, even though doctor uses the API; verify/correct that installation ordering when touching the docs.

Real systemd requires systemd as PID 1, sudo/root installation privileges, compatible socket-activated units, and a disposable host. Prebuild `cargo +1.96.0 build --bins` and `cargo +1.96.0 test --test systemd_handoff --no-run`; inspect the newer `qualification/systemd-handoff-smoke.sh`, select a fresh safe port, then run it with the explicit environment gate. It installs temporary policy/accounts/units/binaries and cleans them up; inspect pre-existing resources first. Do not run this smoke on a production node. The older `systemd-socket-smoke.sh` tested only an earlier control-socket path and omits newer broker dependencies; it is not the current qualification substitute.

## 16. Git / Working Tree State

- Repository root locally: `/Users/tihan-nico/Developer/Apollo-Deploy/Open-Source/apollo-updated`; the parent `Open-Source` directory is not this repository. Remote: `https://github.com/Apollo-Deploy/apollo-updated.git`, public, MIT, owner **Apollo-Deploy**.
- Branch: `main`. HEAD and `origin/main`: `daf4fa75f39ede3e856cf17e3431350b93d8c43f`, **Initial public release of Apollo Updated**, authored/committed 2026-10-04 11:53:10 +0200. Live remote branch was checked and matched. This is the sole commit; no per-fix commits, development branch, or PR was created for the earlier work.
- Before this handoff: `git status --short --branch` was clean; `git diff` and `git diff --cached` were empty; no untracked files. There were 74 tracked files. Ignored build output, old duplicate root source, and private local qualification material were not part of that clean tracked-source claim.
- After this handoff: only `CODEX_HANDOFF.md` is new/untracked. No staged changes, no modified tracked implementation, no feature patch, no stash/reset/cleanup. The handoff itself is **not committed/pushed**; the complete source baseline is committed/pushed.
- Safe migration: clone the source, provide this document separately, provision Linux/toolchain/scratch space, and reverify. Ignored private/root duplicate files are not needed. Cloud cannot read an untracked file from GitHub; attach/copy this file into the Cloud session or explicitly arrange a reviewed documentation commit/push. Do not assume the earlier public source-release request authorizes publishing this internal context.

## 17. Remaining Work

### P0 — Continue immediately

1. Transfer/read this handoff, inspect Cloud instructions and live Git state, verify Linux/toolchain/headroom, and run the narrow existing recovery filter to establish a current baseline. Preserve unrelated work and distinguish environment failures.
2. Investigate the conditional early `commit_active` in `recovery/rollback/restore.rs`. Map reachable previous/candidate/recovery/retiring handles under the two-generation cap and crash/reboot reconciliation. Add the focused drain-timeout/retry ordering regression or a test-backed unreachability proof; make the smallest needed validation-preserving change.
3. Rerun affected focused tests and required formatting/compile checks, then one full suite at the end. Record exactly which source and paths were verified; no release-approval claim.

### P1 — Required next

1. Resolve trusted traffic ownership for malicious uncommitted candidates. Enforce the original failure-before-commit/no-traffic-move and continuous-availability contract in trusted components; add an adversarial daemon that ignores cooperative accept/drain flags. Reconcile protocol docs with the proven design.
2. Implement updater self-update with durable control-FD ownership, successor health, failure-preserved predecessor, and committed-successor fallback re-exec. Qualify failed/successful handoffs and control calls before old exit.
3. Implement coordinated-set stage/journal/order/reverse/recovery and both second-member failure cases. Qualify no incompatible mixed serving set and no listener gap in member rollback.
4. Implement safe GC after enumerating every required reference including serving, history, set, staging, and self-update state; prove foreign paths and symlink races cannot delete unrelated content.
5. Complete all 24 signed qualification cases with required state/identity/listener/partial-file assertions, genuine 10,000 lifecycle churn, deterministic crash/power-loss convergence, real systemd, and independent red-team approval with the specified severity threshold. These are mandatory, not V2 scope.

### P2 — Important later

1. Audit/fix no-argument `check`, read/mutation authorization and verified-version semantics; strengthen doctor with trusted ownership/integrity evidence; qualify download crash and any required source formats without unsigned fallback.
2. Add targeted cross-package/stale-invocation/legacy-state/reconciliation security regressions, root rotation/expiry/freeze/rollback and disk-concurrency campaigns. Complete retryable recovery shapes not covered by the first task.
3. Correct stale packaging/protocol/install-order docs and old smoke guidance; document safe legacy/unmanaged transition policy rather than silently adopting PIDs. Required production compatibility cannot be waived by this prioritization.
4. Establish repeatable Linux CI and representative RSS/CPU/latency/resource profiling once correctness is demonstrated; keep hardware/environment/source provenance. There is currently no CI workflow or performance baseline.

### P3 — Optional / future

1. Ergonomic documentation, additional distributions/architectures, and optional telemetry can follow required correctness/security gates. Reuse vetted components and preserve generic ownership boundaries.
2. Clean obsolete ignored local snapshots only if explicitly authorized and demonstrably unnecessary; they are not a Cloud source prerequisite. Add delegated targets or additional handoff modes only for an evidenced requirement. This category does not move any original mandatory behavior to V2.

## 18. Recommended Next Task

Perform a bounded recovery-safety investigation and focused fix, without expanding into self-update, sets, GC, or redesigning the listener in the same patch. Inspect `src/lifecycle/recovery/rollback/restore.rs`, `src/lifecycle/recovery/rollback.rs`, `src/state.rs`, full-handle identity/reconciliation/capacity logic, and `tests/update_lifecycle/recovery_missing_previous.rs` plus related recovery submodules.

First reconstruct the exact shapes that reach the branch where a live previous generation fails health but the candidate remains healthy. Determine whether a distinct live `retiring` handle can coexist, including durable state from an interrupted earlier recovery, dead/reused PIDs, root registry reconciliation, or a reboot. The source-level early alias commit is real; exploitability/reachability is uncertain. Do not invent a three-live-generation production state simply to justify a finding, and do not use the two-generation cap as an unsupported dismissal.

The manager's two-live-generation check counts registry-tracked generations, and process refresh iterates registry records (`src/supervisor/systemd_manager.rs`). It is not a complete inventory of all host units. Investigate whether crash-created orphaned units, stale records, or restarted/untracked generations are reachable within the supported lifecycle and threat model before using the cap as an unreachability proof; their existence has not been demonstrated.

Assert that every live generation being retired completes its required drain wait before the **first** `commit_active`, active/previous pointer change, or terminal transaction state; keep the chosen serving generation available. If a required drain times out, no live process is stopped, the serving generation and pointers remain correct, and a durable state permits safe retry. After the cause clears, retry converges and history is not duplicated. Include cross-package/stale handle checks if the investigated path exposes them; do not add unrelated test scaffolding.

If reachable, add the minimal regression with authoritative generation identities and fix ordering/compensation; if unreachable, document the exact validated preconditions and add a regression proving fail-closed rejection/reconciliation. Run the narrow test first, relevant recovery filters next, then formatting/compile and one full suite. Report any real-systemd verification limit separately. Preserve the unresolved malicious listener issue as the next major task; this bounded fix cannot yield `APOLLO_UPDATED_PRODUCTION_COMPLETE`.

## 19. Suggested Cloud Bootstrap Prompt

```text
Continue development of apollo-updated from CODEX_HANDOFF.md, which I have supplied in this repository/session. Read the whole document and all applicable AGENTS.md or Cloud instructions before acting. Inspect the repository, source, tests, and live Git state rather than blindly trusting the handoff. Expected baseline is main at daf4fa75f39ede3e856cf17e3431350b93d8c43f; preserve any later or uncommitted work and explain differences.

Verify Linux, Rust 1.96.0, and persistent TMPDIR headroom greater than the fixed 8 GiB extraction reservation plus build/artifact needs. Run the narrow recovery baseline first. A skipped systemd test is not real-systemd evidence; use a disposable VM for that path. Do not lower safety budgets or weaken drain gates to make tests pass.

Begin with section 18's bounded recovery task: investigate the early commit_active branch in src/lifecycle/recovery/rollback/restore.rs, prove the reachability or rejection of a distinct live retiring generation under full-handle/two-generation/reconciliation rules, and ensure all required drain waits precede the first service alias, pointer, or terminal commit. A drain timeout must preserve serving processes and durable retryability. Add meaningful focused verification and the smallest required fix. Run targeted checks before one final full suite.

Preserve the generic Rust/TUF/root-policy/root-broker/immutable-storage architecture, full generation identities, fail-closed policy, overlap rollback, and no-stop/start/no-unowned-workload-interruption constraints. Do not reimplement completed work, edit ignored root source snapshots, or repeat rejected approaches without new evidence. Delegate self-contained searches to a small fast agent; retain the user's Luna 6 maximum-reasoning preference for subagents and keep decisions/edits in the main agent.

The malicious uncommitted candidate listener boundary is still unresolved; self-update, coordinated transactions, safe GC, complete signed 24-case qualification, genuine 10,000 lifecycle churn, and independent red-team release approval remain mandatory. Older passes are scoped historical evidence. Do not claim production completion or approval. Keep public source publication separate from internal performance/verdict/handoff material unless I authorize publication. Report the result and exact verification limits, then continue through the documented priorities without reducing original scope to V2.
```
