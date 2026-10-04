use super::{
    GenerationDrainStatus, GenerationHandle, ListenerSource, Supervisor, SupervisorStatus,
    protocol::BrokerRequest,
    protocol::{BROKER_SOCKET, BrokerReply, MAX_BROKER_MESSAGE},
};
use anyhow::Context;
use serde::de::DeserializeOwned;
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    thread,
    time::{Duration, Instant},
};

/// Talks to the root-owned systemd generation broker. The updater never has
/// systemd manager authority and never passes its control listener to a package.
pub struct SystemdSupervisor;

impl SystemdSupervisor {
    fn call<T: DeserializeOwned>(request: BrokerRequest) -> anyhow::Result<T> {
        let mut stream = UnixStream::connect(BROKER_SOCKET).context("connect supervisor broker")?;
        let timeout = Some(Duration::from_secs(35));
        stream.set_read_timeout(timeout)?;
        stream.set_write_timeout(timeout)?;
        serde_json::to_writer(&mut stream, &request)?;
        stream.write_all(b"\n")?;
        let mut line = Vec::new();
        BufReader::new(stream)
            .take((MAX_BROKER_MESSAGE + 1) as u64)
            .read_until(b'\n', &mut line)?;
        anyhow::ensure!(
            line.len() <= MAX_BROKER_MESSAGE,
            "supervisor reply too large"
        );
        let reply: BrokerReply = serde_json::from_slice(&line)?;
        if !reply.ok {
            anyhow::bail!(
                reply
                    .error
                    .unwrap_or_else(|| "supervisor broker failed".into())
            );
        }
        serde_json::from_value(reply.result).context("decode supervisor reply")
    }
}

impl Supervisor for SystemdSupervisor {
    fn status(&self, service: &str) -> anyhow::Result<SupervisorStatus> {
        Self::call(BrokerRequest::Status {
            service: service.to_owned(),
        })
    }

    fn supports_reversible_handoff(&self, service: &str) -> anyhow::Result<bool> {
        Self::call(BrokerRequest::Supports {
            service: service.to_owned(),
        })
    }

    fn listener_source(&self, service: &str) -> anyhow::Result<Option<ListenerSource>> {
        Self::call(BrokerRequest::ListenerSource {
            service: service.to_owned(),
        })
    }

    fn process_is_alive(&self, generation: &GenerationHandle) -> anyhow::Result<bool> {
        Self::call(BrokerRequest::ProcessIsAlive {
            generation: generation.clone(),
        })
    }

    fn start_successor(
        &self,
        service: &str,
        package_id: &str,
        generation_id: &str,
        immutable_tree: &Path,
        listener: Option<&ListenerSource>,
    ) -> anyhow::Result<GenerationHandle> {
        Self::call(BrokerRequest::StartSuccessor {
            service: service.to_owned(),
            package_id: package_id.to_owned(),
            generation_id: generation_id.to_owned(),
            immutable_tree: immutable_tree.to_path_buf(),
            listener: listener.cloned(),
        })
    }

    fn health_of_generation(
        &self,
        generation: &GenerationHandle,
        timeout_ms: u32,
    ) -> anyhow::Result<bool> {
        Self::call(BrokerRequest::Health {
            generation: generation.clone(),
            timeout_ms,
        })
    }

    fn activate_generation(&self, generation: &GenerationHandle) -> anyhow::Result<()> {
        Self::call(BrokerRequest::Activate {
            generation: generation.clone(),
        })
    }

    fn drain_generation(
        &self,
        generation: &GenerationHandle,
        timeout_ms: u32,
    ) -> anyhow::Result<bool> {
        Self::call(BrokerRequest::Drain {
            generation: generation.clone(),
            timeout_ms,
        })
    }

    fn wait_for_drain(
        &self,
        generation: &GenerationHandle,
        timeout_ms: u32,
        must_remain_healthy: Option<&GenerationHandle>,
    ) -> anyhow::Result<bool> {
        let deadline = Instant::now() + Duration::from_millis(u64::from(timeout_ms));
        loop {
            if let Some(required) = must_remain_healthy {
                if !self.health_of_generation(required, 500)? {
                    anyhow::bail!(
                        "generation {} became unhealthy while waiting for {} to drain",
                        required.pid,
                        generation.pid
                    );
                }
            }
            let status: GenerationDrainStatus = Self::call(BrokerRequest::DrainStatus {
                generation: generation.clone(),
            })?;
            if !status.alive {
                anyhow::bail!(
                    "generation {} exited while waiting to drain",
                    generation.pid
                );
            }
            if !status.accepting && status.active_connections == 0 {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "generation {} did not drain within {timeout_ms} ms (accepting={}, active_connections={})",
                    generation.pid,
                    status.accepting,
                    status.active_connections
                );
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn resume_generation(&self, generation: &GenerationHandle) -> anyhow::Result<()> {
        Self::call(BrokerRequest::Resume {
            generation: generation.clone(),
        })
    }

    fn stop_generation(&self, generation: &GenerationHandle) -> anyhow::Result<()> {
        Self::call(BrokerRequest::Stop {
            generation: generation.clone(),
        })
    }

    fn commit_active(
        &self,
        previous: Option<&GenerationHandle>,
        candidate: &GenerationHandle,
    ) -> anyhow::Result<()> {
        Self::call(BrokerRequest::CommitActive {
            previous: previous.cloned(),
            candidate: candidate.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::supervisor::protocol::{BrokerReply, BrokerRequest, MAX_BROKER_MESSAGE};
    use serde_json::json;

    #[test]
    fn broker_protocol_is_bounded_and_typed() {
        let request = BrokerRequest::Status {
            service: "sample".into(),
        };
        let encoded = serde_json::to_vec(&request).unwrap();
        assert!(encoded.len() < MAX_BROKER_MESSAGE);
        let decoded: BrokerRequest = serde_json::from_slice(&encoded).unwrap();
        assert!(matches!(decoded, BrokerRequest::Status { service } if service == "sample"));
        assert!(
            serde_json::from_value::<BrokerReply>(json!({
                "ok": true,
                "result": {},
                "error": null,
                "extra": true
            }))
            .is_err()
        );
    }
}
