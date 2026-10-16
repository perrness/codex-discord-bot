//! The narrow v2 protocol surface verified against `codex-cli 0.162.1`.
//! Keep forward-compatible optional fields, but never guess unknown approval responses.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RpcId {
    Number(i64),
    String(String),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    pub id: String,
    pub cwd: String,
    pub status: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadResponse {
    pub thread: Thread,
    pub cwd: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    pub approval_policy: Value,
    pub approvals_reviewer: String,
    pub sandbox: Value,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Turn {
    pub id: String,
    pub status: TurnStatus,
    #[serde(default)]
    pub error: Option<Value>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum TurnStatus {
    InProgress,
    Completed,
    Failed,
    Interrupted,
}

impl TurnStatus {
    pub fn outcome(self) -> Option<&'static str> {
        match self {
            Self::InProgress => None,
            Self::Completed => Some("completed"),
            Self::Failed => Some("failed"),
            Self::Interrupted => Some("interrupted"),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct TurnResponse {
    pub turn: Turn,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnEvent {
    pub thread_id: String,
    pub turn: Turn,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemEvent {
    pub thread_id: String,
    pub turn_id: String,
    pub item: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorEvent {
    pub thread_id: String,
    pub turn_id: String,
    pub error: Value,
    pub will_retry: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedEvent {
    pub thread_id: String,
    pub request_id: RpcId,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InteractiveParams {
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub item_id: Option<String>,
    #[serde(default)]
    pub questions: Vec<Question>,
    #[serde(default)]
    pub is_blocking: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Question {
    pub id: String,
    pub header: String,
    pub question: String,
    #[serde(default)]
    pub is_secret: bool,
    #[serde(default)]
    pub options: Option<Vec<QuestionOption>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone)]
pub enum Event {
    Notification {
        method: String,
        params: Value,
    },
    Request {
        id: RpcId,
        method: String,
        params: Value,
    },
}

pub const COMMAND_APPROVAL: &str = "item/commandExecution/requestApproval";
pub const FILE_APPROVAL: &str = "item/fileChange/requestApproval";
pub const PERMISSIONS_APPROVAL: &str = "item/permissions/requestApproval";
pub const USER_INPUT: &str = "item/tool/requestUserInput";

pub fn denied_response(method: &str) -> Option<Value> {
    use serde_json::json;
    match method {
        COMMAND_APPROVAL | FILE_APPROVAL => Some(json!({"decision":"cancel"})),
        PERMISSIONS_APPROVAL => Some(json!({"permissions":{},"scope":"turn"})),
        "mcpServer/elicitation/request" => Some(json!({"action":"decline","content":null})),
        _ => None,
    }
}
