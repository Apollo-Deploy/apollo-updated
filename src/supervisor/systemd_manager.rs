#[path = "systemd_manager/control.rs"]
mod control;
#[path = "systemd_manager/identity.rs"]
mod identity;
#[path = "systemd_manager/reconcile.rs"]
mod reconcile;
use identity::{handle_for_record, is_alive, record_for_handle};
use reconcile::{ensure_stoppable_pid, reconcile_record_pid};

use super::{
    GenerationHandle, GenerationStatus, ListenerSource, SupervisorStatus,
    fixture::CommandKind,
    protocol::BrokerRequest,
    systemd_units::{self, UnitPlan},
};
use crate::settings::{AllowedPackage, Settings};
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    os::fd::OwnedFd,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
    thread,
    time::{Duration, Instant},
};

const REGISTRY: &str = "/var/lib/apollo-updated-supervisor/registry.json";

#[derive(Debug, Default, Serialize, Deserialize)]
struct Registry {
    services: BTreeMap<String, ServiceState>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ServiceState {
    active_id: Option<String>,
    generations: BTreeMap<String, GenerationRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GenerationRecord {
    pid: u32,
    #[serde(default)]
    invocation_id: String,
    plan: UnitPlan,
}

pub struct SystemdManager {
    settings: Settings,
    registry: Registry,
    listener_fds: BTreeMap<String, OwnedFd>,
}

impl SystemdManager {
    pub fn open(
        settings: Settings,
        listener_fds: BTreeMap<String, OwnedFd>,
    ) -> anyhow::Result<Self> {
        let registry = match fs::symlink_metadata(REGISTRY) {
            Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => {
                serde_json::from_slice(&fs::read(REGISTRY)?)?
            }
            Ok(_) => anyhow::bail!("supervisor registry is not a regular file"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Registry::default(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            settings,
            registry,
            listener_fds,
        })
    }

    pub fn dispatch(&mut self, request: BrokerRequest) -> anyhow::Result<Value> {
        match request {
            BrokerRequest::Status { service } => Self::json(self.status(&service)?),
            BrokerRequest::Supports { service } => Self::json(self.supports(&service)?),
            BrokerRequest::ListenerSource { service } => {
                Self::json(self.listener_source(&service)?)
            }
            BrokerRequest::ProcessIsAlive { generation } => {
                Self::json(self.process_is_alive(&generation)?)
            }
            BrokerRequest::StartSuccessor {
                service,
                package_id,
                generation_id,
                immutable_tree,
                listener,
            } => Self::json(self.start_successor(
                &service,
                &package_id,
                &generation_id,
                &immutable_tree,
                listener.as_ref(),
            )?),
            BrokerRequest::Health {
                generation,
                timeout_ms,
            } => Self::json(self.health(&generation, timeout_ms)?),
            BrokerRequest::Activate { generation } => {
                self.activate(&generation)?;
                Self::json(())
            }
            BrokerRequest::Drain {
                generation,
                timeout_ms,
            } => Self::json(self.drain(&generation, timeout_ms)?),
            BrokerRequest::DrainStatus { generation } => {
                Self::json(self.drain_status(&generation)?)
            }
            BrokerRequest::Resume { generation } => {
                self.resume(&generation)?;
                Self::json(())
            }
            BrokerRequest::Stop { generation } => {
                self.stop(&generation)?;
                Self::json(())
            }
            BrokerRequest::CommitActive {
                previous,
                candidate,
            } => {
                self.commit_active(previous.as_ref(), &candidate)?;
                Self::json(())
            }
        }
    }

    fn json<T: Serialize>(value: T) -> anyhow::Result<Value> {
        Ok(serde_json::to_value(value)?)
    }

    fn package(&self, service: &str) -> anyhow::Result<&AllowedPackage> {
        self.settings
            .packages
            .iter()
            .find(|item| item.service_name == service)
            .context(crate::error::UpdateError::SupervisorCannotHandoff)
    }

    fn supports(&self, service: &str) -> anyhow::Result<bool> {
        let package = self.package(service)?;
        let Some(policy) = &package.generation else {
            return Ok(false);
        };
        let socket_ok = policy.socket_unit.as_deref().is_none_or(|unit| {
            systemd_units::listener_socket_ready(unit, &package.package_id)
                && self.listener_fds.contains_key(service)
        });
        let has_current = self
            .registry
            .services
            .get(service)
            .and_then(|state| {
                state
                    .active_id
                    .as_ref()
                    .and_then(|id| state.generations.get(id))
            })
            .is_some();
        let unmanaged_service_active =
            systemd_units::active(&format!("{service}.service")) && !has_current;
        Ok(socket_ok
            && !unmanaged_service_active
            && (policy.socket_unit.is_some() || package.allow_listenerless))
    }

    fn listener_source(&self, service: &str) -> anyhow::Result<Option<ListenerSource>> {
        let package = self.package(service)?;
        let policy = package
            .generation
            .as_ref()
            .context(crate::error::UpdateError::SupervisorCannotHandoff)?;
        match &policy.socket_unit {
            Some(unit)
                if systemd_units::listener_socket_ready(unit, &package.package_id)
                    && self.listener_fds.contains_key(service) =>
            {
                Ok(Some(ListenerSource::SystemdSocket { unit: unit.clone() }))
            }
            Some(_) => anyhow::bail!(crate::error::UpdateError::SupervisorCannotHandoff),
            None if package.allow_listenerless => Ok(None),
            None => anyhow::bail!(crate::error::UpdateError::SupervisorCannotHandoff),
        }
    }

    fn start_successor(
        &mut self,
        service: &str,
        package_id: &str,
        generation_id: &str,
        tree: &Path,
        listener: Option<&ListenerSource>,
    ) -> anyhow::Result<GenerationHandle> {
        ensure!(
            self.supports(service)?,
            crate::error::UpdateError::SupervisorCannotHandoff
        );
        let package = self.package(service)?.clone();
        ensure!(
            package.package_id == package_id,
            "package identity mismatch"
        );
        let version = tree
            .file_name()
            .and_then(|value| value.to_str())
            .context("generation tree must be version-named")?
            .to_owned();
        let plan = systemd_units::prepare(
            &self.settings,
            &package,
            &version,
            generation_id,
            tree,
            listener,
        )
        .context("prepare immutable generation files")?;
        let live_generations = self.refresh_generation_processes(service)?;
        let live_count = live_generations.iter().filter(|(_, pid)| *pid > 0).count();
        let state = self
            .registry
            .services
            .entry(service.to_owned())
            .or_default();
        ensure!(
            live_count < 2,
            "systemd supervisor already tracks two live generations"
        );
        ensure!(
            state
                .generations
                .get(&plan.id)
                .is_none_or(|record| record.pid == 0),
            "generation is already running"
        );
        state.generations.insert(
            plan.id.clone(),
            GenerationRecord {
                pid: 0,
                invocation_id: String::new(),
                plan: plan.clone(),
            },
        );
        if let Err(error) = self.save_registry() {
            self.registry
                .services
                .get_mut(service)
                .unwrap()
                .generations
                .remove(&plan.id);
            let _ = systemd_units::remove(&plan);
            return Err(error);
        }

        let started = (|| {
            systemd_units::systemctl(&["start", &plan.control_socket_unit])
                .context("start generation control socket")?;
            systemd_units::systemctl(&["start", &plan.service_unit])
                .context("start generation service")?;
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let pid = systemd_units::property(&plan.service_unit, "MainPID")?
                    .parse::<u32>()
                    .unwrap_or(0);
                if pid > 0 {
                    let invocation_id =
                        systemd_units::property(&plan.service_unit, "InvocationID")?;
                    ensure!(
                        !invocation_id.is_empty(),
                        "generation has no systemd invocation ID"
                    );
                    return Ok((pid, invocation_id));
                }
                ensure!(
                    Instant::now() < deadline,
                    "generation service did not report a MainPID"
                );
                thread::sleep(Duration::from_millis(20));
            }
        })();
        match started {
            Ok((pid, invocation_id)) => {
                let record = self
                    .registry
                    .services
                    .get_mut(service)
                    .and_then(|state| state.generations.get_mut(&plan.id))
                    .context("started generation disappeared from registry")?;
                record.pid = pid;
                record.invocation_id = invocation_id;
                self.save_registry()?;
                handle_for_record(self, service, &plan.id)
            }
            Err(error) => {
                let running = systemd_units::property(&plan.service_unit, "MainPID")
                    .ok()
                    .and_then(|value| value.parse::<u32>().ok())
                    .filter(|pid| *pid > 0);
                if let Some(pid) = running {
                    let invocation_id = systemd_units::property(&plan.service_unit, "InvocationID")
                        .unwrap_or_default();
                    if let Some(record) = self
                        .registry
                        .services
                        .get_mut(service)
                        .and_then(|state| state.generations.get_mut(&plan.id))
                    {
                        record.pid = pid;
                        record.invocation_id = invocation_id;
                    }
                    let _ = self.save_registry();
                    return Err(error);
                }
                let _ = systemd_units::systemctl(&["stop", &plan.service_unit]);
                let _ = systemd_units::systemctl(&["stop", &plan.control_socket_unit]);
                self.registry
                    .services
                    .get_mut(service)
                    .unwrap()
                    .generations
                    .remove(&plan.id);
                systemd_units::remove(&plan)?;
                self.save_registry()?;
                Err(error)
            }
        }
    }

    fn status(&mut self, service: &str) -> anyhow::Result<SupervisorStatus> {
        self.package(service)?;
        let active_id = self
            .registry
            .services
            .get(service)
            .and_then(|state| state.active_id.clone());
        let processes = self.refresh_generation_processes(service)?;
        let mut generations = Vec::new();
        for (record, main_pid) in processes {
            if main_pid == 0 {
                continue;
            }
            let handle = handle_for_record(self, service, &record.plan.id)?;
            let reply = self.control(&handle, CommandKind::Health, 1000).ok();
            let healthy = reply.as_ref().is_some_and(|result| {
                result.healthy && result.pid == main_pid && result.version == record.plan.version
            });
            let serving = reply.as_ref().is_some_and(|result| {
                result.accepting && result.pid == main_pid && result.version == record.plan.version
            });
            generations.push(GenerationStatus {
                pid: main_pid,
                version: Some(record.plan.version),
                package_id: record.plan.package_id,
                generation_id: record.plan.id,
                invocation_id: handle.invocation_id,
                digest: record.plan.digest,
                serving,
                healthy,
            });
        }
        let active_record = active_id.as_ref().and_then(|id| {
            self.registry
                .services
                .get(service)
                .and_then(|state| state.generations.get(id))
        });
        let current_main_pid = active_record
            .map(|record| record.pid)
            .filter(|pid| *pid > 0);
        let unit_state = systemd_units::property(&format!("{service}.service"), "ActiveState")
            .unwrap_or_else(|_| "inactive".into());
        let mut serving = generations.iter().filter(|generation| generation.serving);
        let listener_owner = serving
            .next()
            .map(|generation| generation.pid)
            .filter(|_| serving.next().is_none());
        Ok(SupervisorStatus {
            service: service.into(),
            unit_state,
            current_main_pid,
            generations,
            listener_owner,
        })
    }

    fn refresh_generation_processes(
        &mut self,
        service: &str,
    ) -> anyhow::Result<Vec<(GenerationRecord, u32)>> {
        let records = self
            .registry
            .services
            .get(service)
            .map(|state| {
                state
                    .generations
                    .iter()
                    .map(|(id, record)| (id.clone(), record.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut processes = Vec::with_capacity(records.len());
        let mut changed = false;
        for (id, mut record) in records {
            let main_pid = current_pid(&record.plan.service_unit)?;
            if main_pid > 0 {
                let invocation_id =
                    systemd_units::property(&record.plan.service_unit, "InvocationID")?;
                ensure!(
                    !invocation_id.is_empty(),
                    "generation has no systemd invocation ID"
                );
                if record.invocation_id != invocation_id {
                    record.invocation_id = invocation_id;
                    changed = true;
                }
            }
            changed |= reconcile_record_pid(&mut record, main_pid);
            if let Some(stored) = self
                .registry
                .services
                .get_mut(service)
                .and_then(|state| state.generations.get_mut(&id))
            {
                stored.pid = record.pid;
                stored.invocation_id.clone_from(&record.invocation_id);
            }
            processes.push((record, main_pid));
        }
        if changed {
            self.save_registry()?;
        }
        Ok(processes)
    }

    fn process_is_alive(&mut self, generation: &GenerationHandle) -> anyhow::Result<bool> {
        is_alive(self, generation)
    }

    fn save_registry(&self) -> anyhow::Result<()> {
        let path = Path::new(REGISTRY);
        let parent = path.parent().context("registry has no parent")?;
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o755))?;
        let temp = parent.join(format!(".registry-{}.tmp", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        let mut file = options.open(&temp)?;
        let result = (|| {
            serde_json::to_writer(&mut file, &self.registry)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::rename(&temp, path)?;
            File::open(parent)?.sync_all()?;
            Ok::<_, anyhow::Error>(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    }
}

fn current_pid(unit: &str) -> anyhow::Result<u32> {
    let state = systemd_units::property(unit, "ActiveState")?;
    if state != "active" {
        return Ok(0);
    }
    Ok(systemd_units::property(unit, "MainPID")?
        .parse::<u32>()
        .unwrap_or(0))
}
