use super::{CONTROL_FD, CommandKind, Generation, InProcessFixtureSupervisor, LISTENER_FD};
use crate::supervisor::{GenerationHandle, GenerationStatus, Supervisor, SupervisorStatus};
use anyhow::{Context, ensure};
use std::{
    fs, io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

impl Supervisor for InProcessFixtureSupervisor {
    fn status(&self, service: &str) -> anyhow::Result<SupervisorStatus> {
        let mut state = self.state.lock().unwrap();
        for generation in state.generations.values_mut() {
            if generation.child.try_wait()?.is_some() {
                generation.healthy = false;
                generation.accepting = false;
                generation.active_connections = 0;
            }
        }
        let generations = state
            .generations
            .iter()
            .filter(|(_, generation)| generation.service == service)
            .map(|(pid, generation)| GenerationStatus {
                pid: *pid,
                version: Some(generation.version.clone()),
                package_id: generation.package_id.clone(),
                generation_id: generation.generation_id.clone(),
                invocation_id: generation.invocation_id.clone(),
                digest: generation.digest.clone(),
                serving: generation.accepting,
                healthy: generation.healthy,
            })
            .collect();
        Ok(SupervisorStatus {
            service: service.to_owned(),
            unit_state: if state.active_pid.is_some() {
                "active"
            } else {
                "inactive"
            }
            .into(),
            current_main_pid: state.active_pid,
            generations,
            listener_owner: Some(std::process::id()),
        })
    }

    fn supports_reversible_handoff(&self, _service: &str) -> anyhow::Result<bool> {
        Ok(self.listener.local_addr().is_ok())
    }

    fn listener_source(
        &self,
        _service: &str,
    ) -> anyhow::Result<Option<crate::supervisor::ListenerSource>> {
        Ok(Some(crate::supervisor::ListenerSource::InheritedFd {
            fd: self.listener_fd(),
        }))
    }

    fn process_is_alive(&self, handle: &GenerationHandle) -> anyhow::Result<bool> {
        let mut state = self.state.lock().unwrap();
        let Some(generation) = state.generations.get_mut(&handle.pid) else {
            return Ok(false);
        };
        if ensure_handle(generation, handle).is_err() {
            return Ok(false);
        }
        Ok(generation.child.try_wait()?.is_none())
    }

    fn start_successor(
        &self,
        service: &str,
        package_id: &str,
        generation_id: &str,
        immutable_tree: &Path,
        listener: Option<&crate::supervisor::ListenerSource>,
    ) -> anyhow::Result<GenerationHandle> {
        let _lifecycle = self.lifecycle.lock().unwrap();
        ensure!(
            self.state.lock().unwrap().generations.len() < 2,
            "fixture supervisor already tracks two generations"
        );
        ensure!(
            !generation_id.is_empty()
                && generation_id.len() <= 64
                && generation_id
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
            "invalid fixture generation identity"
        );
        ensure!(
            !self
                .state
                .lock()
                .unwrap()
                .generations
                .values()
                .any(|generation| {
                    generation.service == service && generation.generation_id == generation_id
                }),
            "fixture generation identity already exists"
        );
        ensure!(
            matches!(
                listener,
                Some(crate::supervisor::ListenerSource::InheritedFd { fd })
                    if *fd == self.listener_fd()
            ),
            "fixture listener fd mismatch"
        );
        let version = immutable_tree
            .file_name()
            .and_then(|name| name.to_str())
            .context("fixture tree must be named by version")?
            .to_owned();
        let digest = fs::read_to_string(immutable_tree.join("digest.sha256"))?
            .trim()
            .to_ascii_lowercase();
        ensure!(
            digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "fixture tree has an invalid digest sidecar"
        );
        let invocation_id = uuid::Uuid::new_v4().to_string();
        let payload_tree = immutable_tree.join("payload.tree");
        let executable = if immutable_tree.join("fixture-daemon").is_file() {
            immutable_tree.join("fixture-daemon")
        } else {
            payload_tree.join("fixture-daemon")
        };
        ensure!(executable.is_file(), "fixture tree has no fixture-daemon");
        let marker_root = if payload_tree.is_dir() {
            &payload_tree
        } else {
            immutable_tree
        };
        let (parent_control, child_control) = UnixStream::pair()?;
        let listener_copy = duplicate_fd(self.listener_fd())?;
        let child_control_fd = child_control.as_raw_fd();
        let listener_copy_fd = listener_copy.as_raw_fd();
        let mut command = Command::new(executable);
        command
            .arg("--version")
            .arg(&version)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if marker_root.join(".fixture-unhealthy").exists() {
            command.arg("--unhealthy");
        }
        if marker_root.join(".fixture-crash-on-activate").exists() {
            command.arg("--crash-on-activate");
        }
        if marker_root.join(".fixture-crash-after-activate").exists() {
            command.arg("--crash-after-activate");
        }
        if marker_root.join(".fixture-drain-delay").exists() {
            command.arg("--drain-delay");
        }
        // SAFETY: the pre-exec closure uses only dup2 on already-open fds.
        #[allow(unsafe_code)]
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(listener_copy_fd, LISTENER_FD) < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::dup2(child_control_fd, CONTROL_FD) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().context("spawn fixture successor")?;
        drop(child_control);
        parent_control.set_write_timeout(Some(Duration::from_secs(1)))?;
        parent_control.set_read_timeout(Some(Duration::from_secs(1)))?;
        let pid = child.id();
        let generation = Generation {
            child,
            control: parent_control,
            inbound_buffer: Vec::new(),
            service: service.to_owned(),
            package_id: package_id.to_owned(),
            generation_id: generation_id.to_owned(),
            invocation_id: invocation_id.clone(),
            digest: digest.clone(),
            version,
            next_request: 0,
            healthy: false,
            accepting: false,
            active_connections: 0,
            crash_before_commit: marker_root.join(".fixture-crash-before-commit").exists(),
        };
        self.state
            .lock()
            .unwrap()
            .generations
            .insert(pid, generation);
        Ok(GenerationHandle {
            service: service.to_owned(),
            package_id: package_id.to_owned(),
            generation_id: generation_id.to_owned(),
            pid,
            invocation_id,
            version: immutable_tree
                .file_name()
                .and_then(|name| name.to_str())
                .context("fixture tree must be version-named")?
                .to_owned(),
            digest,
        })
    }

    fn health_of_generation(
        &self,
        handle: &GenerationHandle,
        timeout_ms: u32,
    ) -> anyhow::Result<bool> {
        let mut state = self.state.lock().unwrap();
        let generation = Self::generation_mut(&mut state, handle.pid)?;
        ensure_handle(generation, handle)?;
        let reply = Self::send(generation, CommandKind::Health, timeout_ms)?;
        Ok(reply.ok && reply.healthy)
    }

    fn drain_generation(&self, handle: &GenerationHandle, timeout_ms: u32) -> anyhow::Result<bool> {
        let mut state = self.state.lock().unwrap();
        let generation = Self::generation_mut(&mut state, handle.pid)?;
        ensure_handle(generation, handle)?;
        let reply = Self::send(generation, CommandKind::Drain, timeout_ms)?;
        Ok(reply.ok && !reply.accepting)
    }

    fn wait_for_drain(
        &self,
        handle: &GenerationHandle,
        timeout_ms: u32,
        must_remain_healthy: Option<&GenerationHandle>,
    ) -> anyhow::Result<bool> {
        self.wait_for_drain(
            handle.pid,
            timeout_ms,
            must_remain_healthy.map(|generation| generation.pid),
        )
    }

    fn activate_generation(&self, handle: &GenerationHandle) -> anyhow::Result<()> {
        let mut state = self.state.lock().unwrap();
        let generation = Self::generation_mut(&mut state, handle.pid)?;
        ensure_handle(generation, handle)?;
        let reply = Self::send(generation, CommandKind::Activate, 1000)?;
        ensure!(
            reply.ok && reply.accepting,
            "fixture candidate did not activate"
        );
        Ok(())
    }

    fn resume_generation(&self, handle: &GenerationHandle) -> anyhow::Result<()> {
        let mut state = self.state.lock().unwrap();
        let generation = Self::generation_mut(&mut state, handle.pid)?;
        ensure_handle(generation, handle)?;
        let reply = Self::send(generation, CommandKind::Resume, 1000)?;
        ensure!(
            reply.ok && reply.accepting,
            "fixture generation did not resume"
        );
        Ok(())
    }

    fn stop_generation(&self, handle: &GenerationHandle) -> anyhow::Result<()> {
        let _lifecycle = self.lifecycle.lock().unwrap();
        let mut state = self.state.lock().unwrap();
        ensure_handle(Self::generation_mut(&mut state, handle.pid)?, handle)?;
        if Self::generation_mut(&mut state, handle.pid)?
            .child
            .try_wait()?
            .is_some()
        {
            state.generations.remove(&handle.pid);
            return Ok(());
        }
        let was_active = state.active_pid == Some(handle.pid);
        let generation = Self::generation_mut(&mut state, handle.pid)?;
        let status = Self::send(generation, CommandKind::Health, 1000)?;
        ensure!(
            !was_active || !status.accepting,
            "refusing to stop the serving generation"
        );
        ensure!(
            !status.accepting && status.active_connections == 0,
            "fixture generation must be drained before stop"
        );
        let reply = Self::send(generation, CommandKind::Stop, 1000)?;
        ensure!(reply.ok, "fixture has accepted connections still in flight");
        generation.child.wait()?;
        state.generations.remove(&handle.pid);
        if state.active_pid == Some(handle.pid) {
            state.active_pid = None;
        }
        Ok(())
    }

    fn commit_active(
        &self,
        previous: Option<&GenerationHandle>,
        candidate: &GenerationHandle,
    ) -> anyhow::Result<()> {
        let _lifecycle = self.lifecycle.lock().unwrap();
        let mut state = self.state.lock().unwrap();
        ensure_handle(Self::generation_mut(&mut state, candidate.pid)?, candidate)?;
        if let Some(previous) = previous {
            ensure_handle(Self::generation_mut(&mut state, previous.pid)?, previous)?;
        }
        ensure!(
            state.active_pid == previous.map(|generation| generation.pid),
            "serving fixture changed before commit"
        );
        let candidate_generation = Self::generation_mut(&mut state, candidate.pid)?;
        let reply = Self::send(candidate_generation, CommandKind::Health, 1000)?;
        ensure!(
            reply.healthy && reply.accepting,
            "candidate is not serving at commit"
        );
        ensure!(
            candidate_generation.child.try_wait()?.is_none(),
            "candidate exited before commit"
        );
        state.active_pid = Some(candidate.pid);
        Ok(())
    }
}

fn ensure_handle(generation: &Generation, handle: &GenerationHandle) -> anyhow::Result<()> {
    ensure!(
        generation.service == handle.service
            && generation.package_id == handle.package_id
            && generation.generation_id == handle.generation_id
            && generation.invocation_id == handle.invocation_id
            && generation.version == handle.version
            && generation.digest == handle.digest,
        "stale or mismatched fixture generation handle"
    );
    Ok(())
}

#[allow(unsafe_code)]
fn duplicate_fd(fd: std::os::fd::RawFd) -> io::Result<OwnedFd> {
    // SAFETY: fcntl duplicates the live descriptor and returns an owned fd.
    let copied = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10) };
    if copied < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: copied is a fresh descriptor returned by F_DUPFD_CLOEXEC.
    Ok(unsafe { OwnedFd::from_raw_fd(copied) })
}
