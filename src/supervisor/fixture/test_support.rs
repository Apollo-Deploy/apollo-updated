use super::InProcessFixtureSupervisor;
use crate::supervisor::{GenerationHandle, ListenerSource, Supervisor};
use std::path::Path;

impl InProcessFixtureSupervisor {
    /// Compatibility helper for fixture-only integration tests. Production
    /// lifecycle code uses the handle-based `Supervisor` trait exclusively.
    pub fn start_successor(
        &self,
        service: &str,
        tree: &Path,
        listener: Option<&ListenerSource>,
    ) -> anyhow::Result<u32> {
        let generation_id = uuid::Uuid::new_v4().to_string();
        Ok(
            Supervisor::start_successor(self, service, service, &generation_id, tree, listener)?
                .pid,
        )
    }

    pub fn generation_handle(&self, pid: u32) -> anyhow::Result<GenerationHandle> {
        self.handle_for_pid(pid)
    }

    pub fn stop_pid(&self, pid: u32) -> anyhow::Result<()> {
        let generation = self.handle_for_pid(pid)?;
        Supervisor::stop_generation(self, &generation)
    }

    pub fn commit_active(
        &self,
        previous_pid: Option<u32>,
        candidate_pid: u32,
    ) -> anyhow::Result<()> {
        let previous = previous_pid
            .map(|pid| self.handle_for_pid(pid))
            .transpose()?;
        let candidate = self.handle_for_pid(candidate_pid)?;
        Supervisor::commit_active(self, previous.as_ref(), &candidate)
    }
}
