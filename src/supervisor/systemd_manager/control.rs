use super::{
    GenerationHandle, SystemdManager, current_pid, ensure_stoppable_pid, is_alive,
    record_for_handle, systemd_units,
};
use crate::supervisor::GenerationDrainStatus;
use crate::supervisor::fixture::{CommandKind, ControlReply, ControlRequest, MAX_CONTROL_LINE};
use anyhow::{Context, ensure};
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

static CONTROL_REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

impl SystemdManager {
    pub(super) fn health(
        &mut self,
        generation: &GenerationHandle,
        timeout_ms: u32,
    ) -> anyhow::Result<bool> {
        let record = record_for_handle(self, generation)?;
        if !is_alive(self, generation)? {
            return Ok(false);
        }
        let response = self.control(generation, CommandKind::Health, timeout_ms)?;
        Ok(response.pid == generation.pid
            && response.version == record.plan.version
            && response.ok
            && response.healthy)
    }

    pub(super) fn activate(&mut self, generation: &GenerationHandle) -> anyhow::Result<()> {
        let record = record_for_handle(self, generation)?;
        let listener_fd = if record.plan.listener_socket_unit.is_some() {
            Some(
                self.listener_fds
                    .get(&generation.service)
                    .context("package listener is absent from supervisor socket activation")?
                    .as_raw_fd(),
            )
        } else {
            None
        };
        let mut stream = self.connect_control(generation, &record, 1000)?;
        let health = Self::exchange(
            &mut stream,
            generation,
            &record,
            CommandKind::Health,
            1000,
            None,
        )?;
        ensure!(
            health.ok && health.healthy && !health.accepting && !health.listener_installed,
            "generation is unhealthy or already activated"
        );
        ensure!(
            is_alive(self, generation)?,
            "generation identity changed during activation preflight"
        );
        let response = Self::exchange(
            &mut stream,
            generation,
            &record,
            CommandKind::Activate,
            1000,
            listener_fd,
        )?;
        ensure!(
            response.ok
                && response.accepting
                && response.listener_installed == record.plan.listener_socket_unit.is_some(),
            "generation did not activate"
        );
        Ok(())
    }

    pub(super) fn drain(
        &mut self,
        generation: &GenerationHandle,
        timeout_ms: u32,
    ) -> anyhow::Result<bool> {
        let response = self.command(generation, CommandKind::Drain, timeout_ms)?;
        Ok(response.ok && !response.accepting)
    }

    pub(super) fn drain_status(
        &mut self,
        generation: &GenerationHandle,
    ) -> anyhow::Result<GenerationDrainStatus> {
        record_for_handle(self, generation)?;
        if !is_alive(self, generation)? {
            return Ok(GenerationDrainStatus {
                alive: false,
                accepting: false,
                active_connections: 0,
            });
        }
        let response = self.command(generation, CommandKind::Health, 500)?;
        ensure!(
            response.pid == generation.pid && response.version == generation.version,
            "generation identity changed while checking drain status"
        );
        Ok(GenerationDrainStatus {
            alive: true,
            accepting: response.accepting,
            active_connections: response.active_connections,
        })
    }

    pub(super) fn resume(&mut self, generation: &GenerationHandle) -> anyhow::Result<()> {
        let response = self.command(generation, CommandKind::Resume, 1000)?;
        ensure!(
            response.ok && response.accepting,
            "generation did not resume"
        );
        Ok(())
    }

    pub(super) fn stop(&mut self, generation: &GenerationHandle) -> anyhow::Result<()> {
        let record = record_for_handle(self, generation)?;
        let service = generation.service.as_str();
        let id = generation.generation_id.as_str();
        let current_main_pid = current_pid(&record.plan.service_unit)?;
        ensure_stoppable_pid(generation.pid, current_main_pid)?;
        if current_main_pid > 0 {
            ensure!(
                is_alive(self, generation)?,
                "generation invocation changed before stop"
            );
        }
        ensure!(
            current_main_pid == 0
                || self
                    .registry
                    .services
                    .get(service)
                    .and_then(|state| state.active_id.as_ref())
                    .is_none_or(|active_id| active_id != id),
            "refusing to stop the active generation"
        );
        if current_main_pid == generation.pid {
            let response = self.command(generation, CommandKind::Stop, 1000)?;
            ensure!(
                response.ok && !response.accepting && response.active_connections == 0,
                "generation must be drained before stop"
            );
        }
        systemd_units::systemctl(&["stop", &record.plan.service_unit])?;
        let _ = systemd_units::systemctl(&["stop", &record.plan.control_socket_unit]);
        let state = self.registry.services.get_mut(service).unwrap();
        state.generations.remove(id);
        if state.active_id.as_deref() == Some(id) {
            state.active_id = None;
        }
        self.save_registry()?;
        systemd_units::remove(&record.plan)?;
        Ok(())
    }

    pub(super) fn commit_active(
        &mut self,
        previous: Option<&GenerationHandle>,
        candidate: &GenerationHandle,
    ) -> anyhow::Result<()> {
        let candidate_record = record_for_handle(self, candidate)?;
        let service = &candidate.service;
        let candidate_id = &candidate.generation_id;
        let state = self
            .registry
            .services
            .get(service)
            .context("service state missing")?;
        let active_id = state.active_id.as_deref();
        ensure!(
            active_id == previous.map(|generation| generation.generation_id.as_str()),
            "active generation changed before commit"
        );
        ensure!(
            self.health(candidate, 1000)?,
            "candidate failed health at commit"
        );
        if let Some(previous) = previous {
            record_for_handle(self, previous)?;
            if is_alive(self, previous)? {
                let health = self.command(previous, CommandKind::Health, 1000)?;
                ensure!(
                    health.pid == previous.pid && health.version == previous.version,
                    "previous generation identity changed before commit"
                );
                ensure!(
                    !health.accepting,
                    "previous generation must be gated before commit"
                );
                ensure!(
                    health.active_connections == 0,
                    "previous generation must drain before commit"
                );
            }
        }
        let health = self.command(candidate, CommandKind::Health, 1000)?;
        ensure!(health.accepting, "candidate is not accepting at commit");
        systemd_units::commit_alias(service, &candidate_record.plan.service_unit)?;
        self.registry.services.get_mut(service).unwrap().active_id = Some(candidate_id.clone());
        self.save_registry()?;
        Ok(())
    }

    pub(super) fn command(
        &self,
        generation: &GenerationHandle,
        command: CommandKind,
        timeout_ms: u32,
    ) -> anyhow::Result<ControlReply> {
        self.control(generation, command, timeout_ms)
    }

    pub(super) fn control(
        &self,
        generation: &GenerationHandle,
        command: CommandKind,
        timeout_ms: u32,
    ) -> anyhow::Result<ControlReply> {
        self.control_with_listener(generation, command, timeout_ms, None)
    }

    fn control_with_listener(
        &self,
        generation: &GenerationHandle,
        command: CommandKind,
        timeout_ms: u32,
        listener_fd: Option<i32>,
    ) -> anyhow::Result<ControlReply> {
        let record = record_for_handle(self, generation)?;
        let mut stream = self.connect_control(generation, &record, timeout_ms)?;
        Self::exchange(
            &mut stream,
            generation,
            &record,
            command,
            timeout_ms,
            listener_fd,
        )
    }

    fn connect_control(
        &self,
        generation: &GenerationHandle,
        record: &super::GenerationRecord,
        timeout_ms: u32,
    ) -> anyhow::Result<UnixStream> {
        ensure!(
            is_alive(self, generation)?,
            "generation identity no longer matches systemd state"
        );
        let stream = UnixStream::connect(&record.plan.control_path)
            .context("connect generation control socket")?;
        let timeout = Duration::from_millis(u64::from(timeout_ms.clamp(1, 30_000)));
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        Ok(stream)
    }

    fn exchange(
        stream: &mut UnixStream,
        generation: &GenerationHandle,
        record: &super::GenerationRecord,
        command: CommandKind,
        timeout_ms: u32,
        listener_fd: Option<i32>,
    ) -> anyhow::Result<ControlReply> {
        let timeout = Duration::from_millis(u64::from(timeout_ms.clamp(1, 30_000)));
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let request = ControlRequest {
            id: CONTROL_REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed),
            command,
            timeout_ms,
        };
        crate::supervisor::fd_transfer::send_marker(stream, listener_fd)?;
        serde_json::to_writer(&mut *stream, &request)?;
        stream.write_all(b"\n")?;
        let mut line = Vec::new();
        BufReader::new(&mut *stream)
            .take((MAX_CONTROL_LINE + 1) as u64)
            .read_until(b'\n', &mut line)?;
        ensure!(
            line.len() <= MAX_CONTROL_LINE,
            "generation control reply too large"
        );
        let reply: ControlReply = serde_json::from_slice(&line)?;
        ensure!(
            reply.id == request.id,
            "generation control request id mismatch"
        );
        ensure!(
            reply.pid == generation.pid && reply.version == record.plan.version,
            "generation control reply identity mismatch"
        );
        Ok(reply)
    }
}
