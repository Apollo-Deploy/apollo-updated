use crate::supervisor::GenerationHandle;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Idle,
    Downloading,
    Verified,
    Staged,
    HandingOff,
    VerifyingHealth,
    Draining,
    Committing,
    Committed,
    Failed,
    RollingBack,
    RolledBack,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Update,
    Rollback,
}

impl Default for OperationKind {
    fn default() -> Self {
        Self::Update
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageState {
    pub package_id: String,
    pub phase: Phase,
    pub active_version: Option<String>,
    pub previous_version: Option<String>,
    pub candidate_version: Option<String>,
    pub candidate_digest: Option<String>,
    #[serde(default)]
    pub active_pid: Option<u32>,
    #[serde(default)]
    pub handoff_previous_version: Option<String>,
    #[serde(default)]
    pub handoff_previous_pid: Option<u32>,
    #[serde(default)]
    pub candidate_pid: Option<u32>,
    #[serde(default)]
    pub retiring_pid: Option<u32>,
    pub highest_verified_version: Option<String>,
    #[serde(default)]
    pub operation_kind: OperationKind,
    pub operation_id: String,
    pub updated_unix_ms: u128,
    pub failure: Option<String>,
    #[serde(default)]
    pub active_generation: Option<GenerationHandle>,
    #[serde(default)]
    pub previous_generation: Option<GenerationHandle>,
    #[serde(default)]
    pub candidate_generation_id: Option<String>,
    #[serde(default)]
    pub candidate_generation: Option<GenerationHandle>,
    #[serde(default)]
    pub handoff_previous_generation: Option<GenerationHandle>,
    #[serde(default)]
    pub retiring_generation: Option<GenerationHandle>,
    #[serde(default)]
    pub recovery_generation_id: Option<String>,
    #[serde(default)]
    pub recovery_generation: Option<GenerationHandle>,
}

impl PackageState {
    pub fn initial(package_id: &str) -> Self {
        Self {
            package_id: package_id.to_owned(),
            phase: Phase::Idle,
            active_version: None,
            previous_version: None,
            candidate_version: None,
            candidate_digest: None,
            active_pid: None,
            handoff_previous_version: None,
            handoff_previous_pid: None,
            candidate_pid: None,
            retiring_pid: None,
            highest_verified_version: None,
            operation_kind: OperationKind::Update,
            operation_id: "none".to_owned(),
            updated_unix_ms: now_ms(),
            failure: None,
            active_generation: None,
            previous_generation: None,
            candidate_generation_id: None,
            candidate_generation: None,
            handoff_previous_generation: None,
            retiring_generation: None,
            recovery_generation_id: None,
            recovery_generation: None,
        }
    }
    pub fn touch(&mut self) {
        self.updated_unix_ms = now_ms();
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_millis())
}
