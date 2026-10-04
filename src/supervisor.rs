use serde::{Deserialize, Serialize};
use std::{os::fd::RawFd, path::Path};

#[path = "supervisor/broker.rs"]
pub(crate) mod broker;
#[path = "supervisor/fd_transfer.rs"]
pub(crate) mod fd_transfer;
#[path = "supervisor/fixture.rs"]
pub mod fixture;
#[path = "supervisor/protocol.rs"]
pub(crate) mod protocol;
#[path = "supervisor/systemd.rs"]
mod systemd;
#[path = "supervisor/systemd_manager.rs"]
mod systemd_manager;
#[path = "supervisor/systemd_units.rs"]
mod systemd_units;
pub use systemd::SystemdSupervisor;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerationStatus {
    pub pid: u32,
    pub version: Option<String>,
    pub package_id: String,
    pub generation_id: String,
    pub invocation_id: String,
    pub digest: String,
    pub serving: bool,
    pub healthy: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(crate) struct GenerationDrainStatus {
    pub alive: bool,
    pub accepting: bool,
    pub active_connections: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GenerationHandle {
    pub service: String,
    pub package_id: String,
    pub generation_id: String,
    pub pid: u32,
    pub invocation_id: String,
    pub version: String,
    pub digest: String,
}

impl GenerationStatus {
    pub fn handle(&self, service: &str) -> Option<GenerationHandle> {
        Some(GenerationHandle {
            service: service.to_owned(),
            package_id: self.package_id.clone(),
            generation_id: self.generation_id.clone(),
            pid: self.pid,
            invocation_id: self.invocation_id.clone(),
            version: self.version.clone()?,
            digest: self.digest.clone(),
        })
    }
}

impl SupervisorStatus {
    pub fn generation(&self, handle: &GenerationHandle) -> Option<&GenerationStatus> {
        (self.service == handle.service).then_some(())?;
        self.generations.iter().find(|generation| {
            generation.pid == handle.pid
                && generation.generation_id == handle.generation_id
                && generation.invocation_id == handle.invocation_id
                && generation.package_id == handle.package_id
                && generation.version.as_deref() == Some(handle.version.as_str())
                && generation.digest == handle.digest
        })
    }

    pub fn handle_for_pid(&self, pid: u32) -> Option<GenerationHandle> {
        let mut matches = self
            .generations
            .iter()
            .filter(|generation| generation.pid == pid);
        let generation = matches.next()?;
        matches
            .next()
            .is_none()
            .then(|| generation.handle(&self.service))
            .flatten()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupervisorStatus {
    pub service: String,
    pub unit_state: String,
    pub current_main_pid: Option<u32>,
    pub generations: Vec<GenerationStatus>,
    pub listener_owner: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ListenerSource {
    InheritedFd { fd: RawFd },
    SystemdSocket { unit: String },
}

pub trait Supervisor: Send + Sync {
    fn status(&self, service: &str) -> anyhow::Result<SupervisorStatus>;
    fn supports_reversible_handoff(&self, service: &str) -> anyhow::Result<bool>;
    /// Identify the package traffic listener, never the updater control socket.
    /// Systemd can lend its socket unit to each generation without exposing an
    /// application listener descriptor to the updater process.
    fn listener_source(&self, service: &str) -> anyhow::Result<Option<ListenerSource>>;
    /// Report whether this exact tracked generation still exists. Health is a
    /// separate property; an unhealthy live process still needs a safe drain.
    fn process_is_alive(&self, generation: &GenerationHandle) -> anyhow::Result<bool>;
    fn start_successor(
        &self,
        service: &str,
        package_id: &str,
        generation_id: &str,
        immutable_tree: &Path,
        listener: Option<&ListenerSource>,
    ) -> anyhow::Result<GenerationHandle>;
    fn health_of_generation(
        &self,
        generation: &GenerationHandle,
        timeout_ms: u32,
    ) -> anyhow::Result<bool>;
    fn activate_generation(&self, generation: &GenerationHandle) -> anyhow::Result<()>;
    fn drain_generation(
        &self,
        generation: &GenerationHandle,
        timeout_ms: u32,
    ) -> anyhow::Result<bool>;
    fn wait_for_drain(
        &self,
        generation: &GenerationHandle,
        timeout_ms: u32,
        must_remain_healthy: Option<&GenerationHandle>,
    ) -> anyhow::Result<bool>;
    fn resume_generation(&self, generation: &GenerationHandle) -> anyhow::Result<()>;
    fn stop_generation(&self, generation: &GenerationHandle) -> anyhow::Result<()>;
    fn commit_active(
        &self,
        previous: Option<&GenerationHandle>,
        candidate: &GenerationHandle,
    ) -> anyhow::Result<()>;
}
