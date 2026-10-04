//! Qualification supervisor. It keeps the listening socket open in this
//! process and launches two fixture generations with the same inherited fd.

use super::{GenerationHandle, Supervisor};
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::net::TcpListener as StdTcpListener;
use std::os::unix::net::UnixStream;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    os::fd::{AsRawFd, RawFd},
    process::Child,
    sync::Mutex,
    time::{Duration, Instant},
};

#[path = "fixture/test_support.rs"]
mod test_support;

pub(crate) const LISTENER_FD: RawFd = 3;
pub(crate) const CONTROL_FD: RawFd = 4;
pub(crate) const MAX_CONTROL_LINE: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CommandKind {
    Health,
    Activate,
    Drain,
    Resume,
    Stop,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ControlRequest {
    pub(crate) id: u64,
    pub(crate) command: CommandKind,
    pub(crate) timeout_ms: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ControlReply {
    pub(crate) id: u64,
    pub(crate) pid: u32,
    pub(crate) version: String,
    pub(crate) ok: bool,
    pub(crate) healthy: bool,
    pub(crate) accepting: bool,
    pub(crate) active_connections: u64,
    pub(crate) listener_installed: bool,
}

struct Generation {
    child: Child,
    control: UnixStream,
    inbound_buffer: Vec<u8>,
    service: String,
    package_id: String,
    generation_id: String,
    invocation_id: String,
    digest: String,
    version: String,
    next_request: u64,
    healthy: bool,
    accepting: bool,
    active_connections: u64,
    crash_before_commit: bool,
}

struct State {
    active_pid: Option<u32>,
    generations: BTreeMap<u32, Generation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffOutcome {
    Committed { previous_pid: u32, serving_pid: u32 },
    CandidateUnhealthy,
    CandidateUnavailable,
    DrainTimedOut,
    RecoveryRequired,
}

/// An in-process qualification adapter. The fixture executable implements a
/// bounded private control protocol on fd 4 and receives the listening socket
/// on fd 3. It is not a production systemd adapter.
pub struct InProcessFixtureSupervisor {
    listener: StdTcpListener,
    lifecycle: Mutex<()>,
    state: Mutex<State>,
}

impl InProcessFixtureSupervisor {
    pub fn new(listener: StdTcpListener) -> anyhow::Result<Self> {
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            lifecycle: Mutex::new(()),
            state: Mutex::new(State {
                active_pid: None,
                generations: BTreeMap::new(),
            }),
        })
    }

    pub fn listener_fd(&self) -> RawFd {
        self.listener.as_raw_fd()
    }

    fn handle_for_pid(&self, pid: u32) -> anyhow::Result<GenerationHandle> {
        let state = self.state.lock().unwrap();
        let generation = state
            .generations
            .get(&pid)
            .context("fixture generation not found")?;
        Ok(GenerationHandle {
            service: generation.service.clone(),
            package_id: generation.package_id.clone(),
            generation_id: generation.generation_id.clone(),
            pid,
            invocation_id: generation.invocation_id.clone(),
            version: generation.version.clone(),
            digest: generation.digest.clone(),
        })
    }

    pub fn health_of_pid(&self, pid: u32, timeout_ms: u32) -> anyhow::Result<bool> {
        let handle = self.handle_for_pid(pid)?;
        Supervisor::health_of_generation(self, &handle, timeout_ms)
    }

    pub fn activate_pid(&self, pid: u32) -> anyhow::Result<()> {
        let handle = self.handle_for_pid(pid)?;
        Supervisor::activate_generation(self, &handle)
    }

    pub fn drain_pid(&self, pid: u32, timeout_ms: u32) -> anyhow::Result<bool> {
        let handle = self.handle_for_pid(pid)?;
        Supervisor::drain_generation(self, &handle, timeout_ms)
    }

    pub fn resume_pid(&self, pid: u32) -> anyhow::Result<()> {
        let handle = self.handle_for_pid(pid)?;
        Supervisor::resume_generation(self, &handle)
    }

    pub fn set_initial_active(&self, pid: u32, timeout_ms: u32) -> anyhow::Result<()> {
        let _lifecycle = self.lifecycle.lock().unwrap();
        ensure!(
            self.health_of_pid(pid, timeout_ms)?,
            "initial fixture is unhealthy"
        );
        self.activate_pid(pid)?;
        self.state.lock().unwrap().active_pid = Some(pid);
        Ok(())
    }

    /// The candidate stays gated while the old generation drains. The shared
    /// listener remains bound, queuing new connections until activation.
    pub fn handoff(
        &self,
        previous_pid: u32,
        candidate_pid: u32,
        health_timeout_ms: u32,
        drain_timeout_ms: u32,
    ) -> anyhow::Result<HandoffOutcome> {
        let _lifecycle = self.lifecycle.lock().unwrap();
        ensure!(
            self.state.lock().unwrap().active_pid == Some(previous_pid),
            "previous generation is not the active fixture"
        );
        match self.health_of_pid(candidate_pid, health_timeout_ms) {
            Ok(true) => {}
            Ok(false) => return Ok(HandoffOutcome::CandidateUnhealthy),
            Err(_) => return Ok(HandoffOutcome::CandidateUnavailable),
        }
        let previous_drained = self
            .drain_pid(previous_pid, drain_timeout_ms)
            .unwrap_or(false);
        if !previous_drained {
            return self.abort_candidate(
                previous_pid,
                candidate_pid,
                drain_timeout_ms,
                HandoffOutcome::DrainTimedOut,
            );
        }
        if !self
            .wait_for_drain(previous_pid, drain_timeout_ms, Some(candidate_pid))
            .unwrap_or(false)
        {
            let outcome = if self
                .health_of_pid(candidate_pid, health_timeout_ms)
                .unwrap_or(false)
            {
                HandoffOutcome::DrainTimedOut
            } else {
                HandoffOutcome::CandidateUnavailable
            };
            return self.abort_candidate(previous_pid, candidate_pid, drain_timeout_ms, outcome);
        }
        match self.health_of_pid(candidate_pid, health_timeout_ms) {
            Ok(true) => {}
            Ok(false) => {
                return self.abort_candidate(
                    previous_pid,
                    candidate_pid,
                    drain_timeout_ms,
                    HandoffOutcome::CandidateUnhealthy,
                );
            }
            Err(_) => {
                return self.abort_candidate(
                    previous_pid,
                    candidate_pid,
                    drain_timeout_ms,
                    HandoffOutcome::CandidateUnavailable,
                );
            }
        }
        if self.activate_pid(candidate_pid).is_err() {
            return self.abort_candidate(
                previous_pid,
                candidate_pid,
                drain_timeout_ms,
                HandoffOutcome::CandidateUnavailable,
            );
        }
        match self.health_of_pid(candidate_pid, health_timeout_ms) {
            Ok(true) => {}
            Ok(false) => {
                return self.abort_candidate(
                    previous_pid,
                    candidate_pid,
                    drain_timeout_ms,
                    HandoffOutcome::CandidateUnhealthy,
                );
            }
            Err(_) => {
                return self.abort_candidate(
                    previous_pid,
                    candidate_pid,
                    drain_timeout_ms,
                    HandoffOutcome::CandidateUnavailable,
                );
            }
        }
        let candidate_exited = {
            let mut state = self.state.lock().unwrap();
            ensure!(
                state.active_pid == Some(previous_pid),
                "active fixture changed during handoff"
            );
            let candidate = Self::generation_mut(&mut state, candidate_pid)?;
            if candidate.child.try_wait()?.is_some() {
                candidate.healthy = false;
                candidate.accepting = false;
                candidate.active_connections = 0;
                true
            } else {
                let crash_before_commit = candidate.crash_before_commit;
                state.active_pid = Some(candidate_pid);
                if crash_before_commit {
                    let candidate = Self::generation_mut(&mut state, candidate_pid)?;
                    candidate.child.kill()?;
                    candidate.child.wait()?;
                }
                false
            }
        };
        if candidate_exited {
            return self.abort_candidate(
                previous_pid,
                candidate_pid,
                drain_timeout_ms,
                HandoffOutcome::CandidateUnavailable,
            );
        }
        if !self
            .health_of_pid(candidate_pid, health_timeout_ms)
            .unwrap_or(false)
        {
            self.state.lock().unwrap().active_pid = Some(previous_pid);
            self.resume_pid(previous_pid)?;
            return self.abort_candidate(
                previous_pid,
                candidate_pid,
                drain_timeout_ms,
                HandoffOutcome::CandidateUnavailable,
            );
        }
        Ok(HandoffOutcome::Committed {
            previous_pid,
            serving_pid: candidate_pid,
        })
    }

    fn abort_candidate(
        &self,
        previous_pid: u32,
        candidate_pid: u32,
        timeout_ms: u32,
        outcome: HandoffOutcome,
    ) -> anyhow::Result<HandoffOutcome> {
        // Restore the previous generation before gating the candidate so the
        // shared listener always has an accepting generation during recovery.
        self.resume_pid(previous_pid)?;
        self.state.lock().unwrap().active_pid = Some(previous_pid);
        if self.mark_exited(candidate_pid)? {
            return Ok(outcome);
        }
        let candidate_drained = self.drain_pid(candidate_pid, timeout_ms).unwrap_or(false)
            && self
                .wait_for_drain(candidate_pid, timeout_ms, None)
                .unwrap_or(false);
        if candidate_drained {
            Ok(outcome)
        } else if self.mark_exited(candidate_pid)? {
            Ok(outcome)
        } else {
            Ok(HandoffOutcome::RecoveryRequired)
        }
    }

    fn mark_exited(&self, pid: u32) -> anyhow::Result<bool> {
        let mut state = self.state.lock().unwrap();
        let generation = Self::generation_mut(&mut state, pid)?;
        if generation.child.try_wait()?.is_some() {
            generation.healthy = false;
            generation.accepting = false;
            generation.active_connections = 0;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn wait_for_drain(
        &self,
        pid: u32,
        timeout_ms: u32,
        must_remain_healthy: Option<u32>,
    ) -> anyhow::Result<bool> {
        let deadline = Instant::now() + Duration::from_millis(u64::from(timeout_ms.max(1)));
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            let wait_ms = remaining.as_millis().clamp(1, 10) as u32;
            let mut state = self.state.lock().unwrap();
            let reply = Self::send(
                Self::generation_mut(&mut state, pid)?,
                CommandKind::Health,
                wait_ms,
            )?;
            if !reply.accepting && reply.active_connections == 0 {
                return Ok(true);
            }
            drop(state);
            if let Some(candidate_pid) = must_remain_healthy
                && !self.health_of_pid(candidate_pid, wait_ms).unwrap_or(false)
            {
                return Ok(false);
            }
            std::thread::sleep(remaining.min(Duration::from_millis(10)));
        }
    }

    fn send(
        generation: &mut Generation,
        command: CommandKind,
        timeout_ms: u32,
    ) -> anyhow::Result<ControlReply> {
        generation.next_request = generation
            .next_request
            .checked_add(1)
            .context("fixture request id overflow")?;
        let request = ControlRequest {
            id: generation.next_request,
            command,
            timeout_ms,
        };
        let mut bytes = serde_json::to_vec(&request)?;
        bytes.push(b'\n');
        super::fd_transfer::send_marker(&generation.control, None)?;
        generation.control.write_all(&bytes)?;
        generation.control.flush()?;

        let deadline = Instant::now() + Duration::from_millis(u64::from(timeout_ms.max(1)));
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                anyhow::bail!("fixture supervisor response timed out");
            }
            generation.control.set_read_timeout(Some(remaining))?;
            let line = read_control_line(&mut generation.control, &mut generation.inbound_buffer)?;
            let reply: ControlReply = serde_json::from_slice(&line)?;
            if reply.id < request.id {
                continue;
            }
            ensure!(
                reply.id == request.id,
                "fixture response id is out of order"
            );
            ensure!(reply.pid == generation.child.id(), "fixture PID changed");
            ensure!(
                reply.version == generation.version,
                "fixture version changed"
            );
            generation.healthy = reply.healthy;
            generation.accepting = reply.accepting;
            generation.active_connections = reply.active_connections;
            return Ok(reply);
        }
    }

    fn generation_mut<'a>(state: &'a mut State, pid: u32) -> anyhow::Result<&'a mut Generation> {
        state
            .generations
            .get_mut(&pid)
            .context("fixture PID is not a tracked generation")
    }
}

impl Drop for InProcessFixtureSupervisor {
    fn drop(&mut self) {
        if let Ok(state) = self.state.get_mut() {
            for generation in state.generations.values_mut() {
                let _ = generation.child.kill();
                let _ = generation.child.wait();
            }
        }
    }
}

fn read_control_line(stream: &mut UnixStream, buffered: &mut Vec<u8>) -> anyhow::Result<Vec<u8>> {
    loop {
        if let Some(end) = buffered.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<_> = buffered.drain(..=end).collect();
            line.pop();
            return Ok(line);
        }
        if buffered.len() > MAX_CONTROL_LINE {
            anyhow::bail!("fixture control line exceeded its limit");
        }
        let mut chunk = [0_u8; 512];
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            anyhow::bail!("fixture supervisor control channel closed");
        }
        buffered.extend_from_slice(&chunk[..read]);
    }
}

#[path = "fixture/supervisor_impl.rs"]
mod supervisor_impl;

#[path = "fixture_daemon.rs"]
mod fixture_daemon;
pub use fixture_daemon::run_fixture_daemon;
