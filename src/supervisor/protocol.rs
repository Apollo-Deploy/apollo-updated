use super::{GenerationHandle, ListenerSource};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

pub const BROKER_SOCKET: &str = "/run/apollo-updated/supervisor.sock";
pub const MAX_BROKER_MESSAGE: usize = 64 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum BrokerRequest {
    Status {
        service: String,
    },
    Supports {
        service: String,
    },
    ListenerSource {
        service: String,
    },
    ProcessIsAlive {
        generation: GenerationHandle,
    },
    StartSuccessor {
        service: String,
        package_id: String,
        generation_id: String,
        immutable_tree: PathBuf,
        listener: Option<ListenerSource>,
    },
    Health {
        generation: GenerationHandle,
        timeout_ms: u32,
    },
    Activate {
        generation: GenerationHandle,
    },
    Drain {
        generation: GenerationHandle,
        timeout_ms: u32,
    },
    DrainStatus {
        generation: GenerationHandle,
    },
    Resume {
        generation: GenerationHandle,
    },
    Stop {
        generation: GenerationHandle,
    },
    CommitActive {
        previous: Option<GenerationHandle>,
        candidate: GenerationHandle,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrokerReply {
    pub ok: bool,
    pub result: Value,
    pub error: Option<String>,
}
