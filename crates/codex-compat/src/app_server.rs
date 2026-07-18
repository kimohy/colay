use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::{CodexRequest, CodexSandbox};

/// Matches the process layer's maximum JSONL frame size.
pub const DEFAULT_MAX_APP_SERVER_MESSAGE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AppServerId {
    Number(i64),
    String(String),
}

impl std::fmt::Display for AppServerId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Number(value) => value.fmt(formatter),
            Self::String(value) => value.fmt(formatter),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppServerClientInfo {
    pub name: String,
    pub title: String,
    pub version: String,
}

impl Default for AppServerClientInfo {
    fn default() -> Self {
        Self {
            name: "colay".to_owned(),
            title: "Colay".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppServerRequest {
    pub method: String,
    pub id: AppServerId,
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppServerNotification {
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppServerProtocolError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppServerResponse {
    pub id: AppServerId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<AppServerProtocolError>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AppServerMessage {
    Request(AppServerRequest),
    Response(AppServerResponse),
    Notification(AppServerNotification),
}

impl AppServerMessage {
    /// Parses one stable stdio JSONL message. App Server intentionally omits
    /// the JSON-RPC `jsonrpc` header on the wire.
    ///
    /// # Errors
    ///
    /// Returns [`AppServerError`] when JSON is malformed or the request,
    /// response, or notification shape is ambiguous.
    pub fn parse_line(line: &str) -> Result<Self, AppServerError> {
        let value: Value = serde_json::from_str(line)
            .map_err(|error| AppServerError::MalformedJson(error.to_string()))?;
        let object = value.as_object().ok_or(AppServerError::InvalidMessage)?;

        match (
            object.get("method").and_then(Value::as_str),
            object.contains_key("id"),
        ) {
            (Some(_), true) => serde_json::from_value(value)
                .map(Self::Request)
                .map_err(|error| AppServerError::InvalidFields(error.to_string())),
            (Some(_), false) => serde_json::from_value(value)
                .map(Self::Notification)
                .map_err(|error| AppServerError::InvalidFields(error.to_string())),
            (None, true) => {
                let response: AppServerResponse = serde_json::from_value(value)
                    .map_err(|error| AppServerError::InvalidFields(error.to_string()))?;
                if response.result.is_some() == response.error.is_some() {
                    return Err(AppServerError::ResponseShape);
                }
                Ok(Self::Response(response))
            }
            (None, false) => Err(AppServerError::InvalidMessage),
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AppServerError {
    #[error("malformed App Server JSON: {0}")]
    MalformedJson(String),
    #[error("invalid App Server message")]
    InvalidMessage,
    #[error("invalid App Server fields: {0}")]
    InvalidFields(String),
    #[error("App Server response must contain exactly one of result or error")]
    ResponseShape,
    #[error("App Server message is not newline terminated")]
    UnterminatedMessage,
    #[error("App Server message is not valid UTF-8")]
    InvalidUtf8,
    #[error("App Server message of {actual} bytes exceeds the {limit} byte limit")]
    MessageTooLarge { actual: usize, limit: usize },
    #[error("invalid App Server session state: {0}")]
    InvalidState(String),
    #[error("unexpected App Server response id {actual}; expected {expected}")]
    UnexpectedResponse {
        expected: AppServerId,
        actual: AppServerId,
    },
    #[error("App Server {method} failed with code {code}: {message}")]
    RequestFailed {
        method: String,
        code: i64,
        message: String,
    },
    #[error("App Server {method} response is missing {field}")]
    MissingField { method: String, field: &'static str },
    #[error("unsupported App Server request {0}")]
    UnsupportedServerRequest(String),
    #[error("unknown App Server lifecycle notification {0}")]
    UnknownLifecycle(String),
    #[error(
        "App Server outputSchema requires a decoded schema and is not available in this adapter"
    )]
    UnsupportedOutputSchema,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextInput {
    #[serde(rename = "type")]
    pub input_type: String,
    pub text: String,
}

impl TextInput {
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            input_type: "text".to_owned(),
            text: text.into(),
        }
    }
}

/// Stable-protocol message builder. It intentionally never opts into
/// `experimentalApi` and never uses WebSocket transport.
#[derive(Debug, Clone)]
pub struct StableAppServerClient {
    next_id: i64,
    client_info: AppServerClientInfo,
}

impl StableAppServerClient {
    #[must_use]
    pub const fn new(client_info: AppServerClientInfo) -> Self {
        Self {
            next_id: 0,
            client_info,
        }
    }

    #[must_use]
    pub fn initialize(&mut self) -> AppServerRequest {
        self.request(
            "initialize",
            json!({
                "clientInfo": self.client_info,
            }),
        )
    }

    #[must_use]
    pub fn initialized() -> AppServerNotification {
        AppServerNotification {
            method: "initialized".to_owned(),
            params: json!({}),
        }
    }

    #[must_use]
    pub fn thread_start(&mut self, request: &CodexRequest) -> AppServerRequest {
        self.request("thread/start", thread_params(request, None))
    }

    #[must_use]
    pub fn thread_resume(
        &mut self,
        thread_id: impl Into<String>,
        request: &CodexRequest,
    ) -> AppServerRequest {
        self.request(
            "thread/resume",
            thread_params(request, Some(thread_id.into())),
        )
    }

    #[must_use]
    pub fn turn_start(
        &mut self,
        thread_id: impl Into<String>,
        request: &CodexRequest,
    ) -> AppServerRequest {
        let mut params = Map::new();
        params.insert("threadId".to_owned(), Value::String(thread_id.into()));
        params.insert(
            "input".to_owned(),
            json!([TextInput::new(request.prompt.clone())]),
        );
        if let Some(effort) = request.effort {
            params.insert(
                "effort".to_owned(),
                Value::String(effort.as_cli_value().to_owned()),
            );
        }
        self.request("turn/start", Value::Object(params))
    }

    #[must_use]
    pub fn turn_interrupt(&mut self, thread_id: &str, turn_id: &str) -> AppServerRequest {
        self.request(
            "turn/interrupt",
            json!({
                "threadId": thread_id,
                "turnId": turn_id,
            }),
        )
    }

    fn request(&mut self, method: &str, params: Value) -> AppServerRequest {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        AppServerRequest {
            method: method.to_owned(),
            id: AppServerId::Number(id),
            params,
        }
    }
}

impl Default for StableAppServerClient {
    fn default() -> Self {
        Self::new(AppServerClientInfo::default())
    }
}

fn thread_params(request: &CodexRequest, thread_id: Option<String>) -> Value {
    let mut params = Map::new();
    if let Some(thread_id) = thread_id {
        params.insert("threadId".to_owned(), Value::String(thread_id));
    }
    if let Some(model) = request.model.as_ref().filter(|model| !model.is_empty()) {
        params.insert("model".to_owned(), Value::String(model.clone()));
    }
    params.insert(
        "cwd".to_owned(),
        Value::String(request.working_directory.to_string_lossy().into_owned()),
    );
    params.insert(
        "sandbox".to_owned(),
        Value::String(
            match request.sandbox {
                CodexSandbox::ReadOnly => "read-only",
                CodexSandbox::WorkspaceWrite => "workspace-write",
            }
            .to_owned(),
        ),
    );
    Value::Object(params)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppServerSessionPlan {
    pub request: CodexRequest,
    pub client_info: AppServerClientInfo,
    pub max_message_bytes: usize,
}

impl AppServerSessionPlan {
    #[must_use]
    pub fn new(request: CodexRequest) -> Self {
        Self {
            request,
            client_info: AppServerClientInfo::default(),
            max_message_bytes: DEFAULT_MAX_APP_SERVER_MESSAGE_BYTES,
        }
    }

    /// Validates fields that cannot be represented on the stable protocol
    /// without reading external data or opting into experimental fields.
    ///
    /// # Errors
    ///
    /// Returns [`AppServerError`] for an invalid message limit or a path-based
    /// output schema.
    pub fn validate(&self) -> Result<(), AppServerError> {
        if self.max_message_bytes == 0
            || self.max_message_bytes > DEFAULT_MAX_APP_SERVER_MESSAGE_BYTES
        {
            return Err(AppServerError::MessageTooLarge {
                actual: self.max_message_bytes,
                limit: DEFAULT_MAX_APP_SERVER_MESSAGE_BYTES,
            });
        }
        if self.request.output_schema.is_some() {
            return Err(AppServerError::UnsupportedOutputSchema);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AppServerStep {
    pub outbound: Vec<Vec<u8>>,
    /// Canonical Codex JSONL events consumed by the existing compatibility
    /// parser. Unknown optional notifications remain opaque events.
    pub events: Vec<Value>,
    pub completed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionPhase {
    Created,
    AwaitingInitialize { id: AppServerId },
    AwaitingThread { id: AppServerId, method: String },
    AwaitingTurn { id: AppServerId },
    Running,
    Completed,
}

/// Deterministic stable App Server handshake/turn state machine. It performs no
/// I/O; the provider runtime owns the process and feeds it complete JSONL
/// frames.
#[derive(Debug, Clone)]
pub struct StableAppServerSession {
    plan: AppServerSessionPlan,
    client: StableAppServerClient,
    phase: SessionPhase,
    thread_id: Option<String>,
    thread_started_emitted: bool,
    turn_started_emitted: bool,
    side_effect_observed: bool,
    latest_usage: Value,
}

impl StableAppServerSession {
    /// Creates a stable-only protocol session.
    ///
    /// # Errors
    ///
    /// Returns [`AppServerError`] when the plan requires an unsupported field.
    pub fn new(plan: AppServerSessionPlan) -> Result<Self, AppServerError> {
        plan.validate()?;
        Ok(Self {
            client: StableAppServerClient::new(plan.client_info.clone()),
            plan,
            phase: SessionPhase::Created,
            thread_id: None,
            thread_started_emitted: false,
            turn_started_emitted: false,
            side_effect_observed: false,
            latest_usage: json!({}),
        })
    }

    /// Starts the initialize handshake.
    ///
    /// # Errors
    ///
    /// Returns [`AppServerError`] if the session was already started or the
    /// encoded frame exceeds the configured bound.
    pub fn start(&mut self) -> Result<Vec<u8>, AppServerError> {
        if self.phase != SessionPhase::Created {
            return Err(AppServerError::InvalidState(
                "initialize may be sent only once".to_owned(),
            ));
        }
        let request = self.client.initialize();
        let frame = self.encode(&request)?;
        self.phase = SessionPhase::AwaitingInitialize {
            id: request.id.clone(),
        };
        Ok(frame)
    }

    /// Consumes one complete newline-terminated App Server frame.
    ///
    /// # Errors
    ///
    /// Returns [`AppServerError`] on malformed framing, failed requests,
    /// missing lifecycle fields, or unknown lifecycle notifications.
    pub fn handle_frame(&mut self, frame: &[u8]) -> Result<AppServerStep, AppServerError> {
        if frame.len() > self.plan.max_message_bytes {
            return Err(AppServerError::MessageTooLarge {
                actual: frame.len(),
                limit: self.plan.max_message_bytes,
            });
        }
        if !frame.ends_with(b"\n") {
            return Err(AppServerError::UnterminatedMessage);
        }
        let line = std::str::from_utf8(&frame[..frame.len().saturating_sub(1)])
            .map_err(|_| AppServerError::InvalidUtf8)?;
        let message = AppServerMessage::parse_line(line)?;
        match message {
            AppServerMessage::Response(response) => self.handle_response(&response),
            AppServerMessage::Notification(notification) => self.handle_notification(&notification),
            AppServerMessage::Request(request) => self.handle_server_request(request),
        }
    }

    #[must_use]
    pub const fn can_fallback(&self) -> bool {
        !self.side_effect_observed && !matches!(self.phase, SessionPhase::Completed)
    }

    #[must_use]
    pub const fn is_completed(&self) -> bool {
        matches!(self.phase, SessionPhase::Completed)
    }

    fn handle_response(
        &mut self,
        response: &AppServerResponse,
    ) -> Result<AppServerStep, AppServerError> {
        match self.phase.clone() {
            SessionPhase::AwaitingInitialize { id } => {
                expect_response(response, &id, "initialize")?;
                let initialized = StableAppServerClient::initialized();
                let thread = if let Some(thread_id) = self.plan.request.resume_session.clone() {
                    self.client.thread_resume(thread_id, &self.plan.request)
                } else {
                    self.client.thread_start(&self.plan.request)
                };
                let method = thread.method.clone();
                self.phase = SessionPhase::AwaitingThread {
                    id: thread.id.clone(),
                    method,
                };
                Ok(AppServerStep {
                    outbound: vec![self.encode(&initialized)?, self.encode(&thread)?],
                    events: Vec::new(),
                    completed: false,
                })
            }
            SessionPhase::AwaitingThread { id, method } => {
                let result = expect_response(response, &id, &method)?;
                let thread_id = result
                    .pointer("/thread/id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| AppServerError::MissingField {
                        method: method.clone(),
                        field: "result.thread.id",
                    })?;
                self.thread_id = Some(thread_id.clone());
                let turn = self
                    .client
                    .turn_start(thread_id.clone(), &self.plan.request);
                self.phase = SessionPhase::AwaitingTurn {
                    id: turn.id.clone(),
                };
                let mut events = Vec::new();
                if !self.thread_started_emitted {
                    events.push(json!({"type": "thread.started", "thread_id": thread_id}));
                    self.thread_started_emitted = true;
                }
                Ok(AppServerStep {
                    outbound: vec![self.encode(&turn)?],
                    events,
                    completed: false,
                })
            }
            SessionPhase::AwaitingTurn { id } => {
                let result = expect_response(response, &id, "turn/start")?;
                if result.pointer("/turn/id").and_then(Value::as_str).is_none() {
                    return Err(AppServerError::MissingField {
                        method: "turn/start".to_owned(),
                        field: "result.turn.id",
                    });
                }
                self.phase = SessionPhase::Running;
                let mut events = Vec::new();
                if !self.turn_started_emitted {
                    events.push(json!({"type": "turn.started"}));
                    self.turn_started_emitted = true;
                }
                Ok(AppServerStep {
                    outbound: Vec::new(),
                    events,
                    completed: false,
                })
            }
            SessionPhase::Created | SessionPhase::Running | SessionPhase::Completed => {
                Err(AppServerError::InvalidState(
                    "response arrived without a matching request".to_owned(),
                ))
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn handle_notification(
        &mut self,
        notification: &AppServerNotification,
    ) -> Result<AppServerStep, AppServerError> {
        let method = notification.method.as_str();
        let mut events = Vec::new();
        let mut completed = false;
        match method {
            "thread/started" => {
                let thread_id = notification
                    .params
                    .pointer("/thread/id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| AppServerError::MissingField {
                        method: method.to_owned(),
                        field: "params.thread.id",
                    })?;
                if !self.thread_started_emitted {
                    events.push(json!({"type": "thread.started", "thread_id": thread_id}));
                    self.thread_started_emitted = true;
                }
            }
            "turn/started" => {
                if !self.turn_started_emitted {
                    events.push(json!({"type": "turn.started"}));
                    self.turn_started_emitted = true;
                }
            }
            "item/started" | "item/completed" => {
                let phase = if method == "item/started" {
                    "item.started"
                } else {
                    "item.completed"
                };
                let item = notification.params.get("item").ok_or_else(|| {
                    AppServerError::MissingField {
                        method: method.to_owned(),
                        field: "params.item",
                    }
                })?;
                if item
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| matches!(kind, "commandExecution" | "fileChange"))
                {
                    self.side_effect_observed = true;
                }
                events.extend(normalize_item(phase, item));
            }
            "thread/tokenUsage/updated" => {
                self.latest_usage = normalize_usage(&notification.params);
                events.push(opaque_notification(method, &notification.params));
            }
            "error" => {
                let error = notification
                    .params
                    .get("error")
                    .cloned()
                    .unwrap_or_default();
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Codex App Server error");
                events.push(json!({
                    "type": "error",
                    "message": message,
                    "error": error,
                }));
            }
            "turn/completed" => {
                let turn = notification.params.get("turn").ok_or_else(|| {
                    AppServerError::MissingField {
                        method: method.to_owned(),
                        field: "params.turn",
                    }
                })?;
                let status = turn.get("status").and_then(Value::as_str).ok_or_else(|| {
                    AppServerError::MissingField {
                        method: method.to_owned(),
                        field: "params.turn.status",
                    }
                })?;
                match status {
                    "completed" => events.push(json!({
                        "type": "turn.completed",
                        "usage": self.latest_usage,
                    })),
                    "failed" | "interrupted" => {
                        let error = turn.get("error").cloned().unwrap_or_default();
                        let message = error.get("message").and_then(Value::as_str).unwrap_or(
                            if status == "interrupted" {
                                "turn interrupted"
                            } else {
                                "turn failed"
                            },
                        );
                        events.push(json!({
                            "type": "turn.failed",
                            "message": message,
                            "error": error,
                        }));
                    }
                    _ => {
                        return Err(AppServerError::InvalidFields(format!(
                            "turn/completed has invalid status {status}"
                        )));
                    }
                }
                self.phase = SessionPhase::Completed;
                completed = true;
            }
            // Stable, non-lifecycle notifications that are useful audit data
            // but need no domain-specific interpretation.
            "turn/diff/updated"
            | "turn/plan/updated"
            | "thread/status/changed"
            | "thread/name/updated"
            | "warning"
            | "configWarning"
            | "serverRequest/resolved" => {
                events.push(opaque_notification(method, &notification.params));
            }
            optional if optional.starts_with("item/") => {
                events.push(opaque_notification(optional, &notification.params));
            }
            lifecycle if lifecycle.starts_with("thread/") || lifecycle.starts_with("turn/") => {
                return Err(AppServerError::UnknownLifecycle(lifecycle.to_owned()));
            }
            optional => events.push(opaque_notification(optional, &notification.params)),
        }
        Ok(AppServerStep {
            outbound: Vec::new(),
            events,
            completed,
        })
    }

    fn handle_server_request(
        &self,
        request: AppServerRequest,
    ) -> Result<AppServerStep, AppServerError> {
        let safe_decline = matches!(
            request.method.as_str(),
            "item/commandExecution/requestApproval"
                | "item/fileChange/requestApproval"
                | "execCommandApproval"
                | "applyPatchApproval"
        );
        if !safe_decline {
            return Err(AppServerError::UnsupportedServerRequest(request.method));
        }
        let response = AppServerResponse {
            id: request.id,
            result: Some(json!({"decision": "decline"})),
            error: None,
        };
        Ok(AppServerStep {
            outbound: vec![self.encode(&response)?],
            events: vec![opaque_notification(
                "approval/declined",
                &json!({"method": request.method}),
            )],
            completed: false,
        })
    }

    fn encode<T: Serialize>(&self, message: &T) -> Result<Vec<u8>, AppServerError> {
        let mut frame = serde_json::to_vec(message)
            .map_err(|error| AppServerError::InvalidFields(error.to_string()))?;
        frame.push(b'\n');
        if frame.len() > self.plan.max_message_bytes {
            return Err(AppServerError::MessageTooLarge {
                actual: frame.len(),
                limit: self.plan.max_message_bytes,
            });
        }
        Ok(frame)
    }
}

fn expect_response<'a>(
    response: &'a AppServerResponse,
    expected: &AppServerId,
    method: &str,
) -> Result<&'a Value, AppServerError> {
    if &response.id != expected {
        return Err(AppServerError::UnexpectedResponse {
            expected: expected.clone(),
            actual: response.id.clone(),
        });
    }
    if let Some(error) = &response.error {
        return Err(AppServerError::RequestFailed {
            method: method.to_owned(),
            code: error.code,
            message: error.message.clone(),
        });
    }
    response
        .result
        .as_ref()
        .ok_or_else(|| AppServerError::MissingField {
            method: method.to_owned(),
            field: "result",
        })
}

fn normalize_item(phase: &str, item: &Value) -> Vec<Value> {
    let Some(item_type) = item.get("type").and_then(Value::as_str) else {
        return vec![json!({"type": phase, "item": item})];
    };
    if item_type == "fileChange" {
        let changes = item
            .get("changes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|change| change.get("path").and_then(Value::as_str))
            .collect::<Vec<_>>();
        if !changes.is_empty() {
            return changes
                .into_iter()
                .enumerate()
                .map(|(index, path)| {
                    json!({
                        "type": phase,
                        "item": {
                            "type": "file_change",
                            "id": item.get("id").and_then(Value::as_str).map(|id| format!("{id}:{index}")),
                            "path": path,
                            "status": item.get("status"),
                        }
                    })
                })
                .collect();
        }
    }

    let normalized_type = match item_type {
        "agentMessage" => "agent_message",
        "commandExecution" => "command_execution",
        "fileChange" => "file_change",
        "mcpToolCall" => "mcp_tool_call",
        "webSearch" => "web_search",
        "reasoning" => "reasoning",
        "plan" => "plan",
        other => other,
    };
    let command = item.get("command").map(|value| {
        value
            .as_str()
            .map_or_else(|| value.to_string(), ToOwned::to_owned)
    });
    let reasoning = item.get("summary").and_then(Value::as_array).map(|values| {
        values
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    });
    vec![json!({
        "type": phase,
        "item": {
            "type": normalized_type,
            "id": item.get("id"),
            "text": item.get("text").cloned().or_else(|| reasoning.map(Value::String)),
            "command": command,
            "status": item.get("status"),
            "exit_code": item.get("exitCode"),
            "path": item.get("path"),
            "server": item.get("server"),
            "tool": item.get("tool"),
            "query": item.get("query"),
        }
    })]
}

fn normalize_usage(params: &Value) -> Value {
    let usage = params
        .pointer("/tokenUsage/last")
        .or_else(|| params.pointer("/tokenUsage/total"))
        .unwrap_or(&Value::Null);
    json!({
        "input_tokens": usage.get("inputTokens").and_then(Value::as_u64),
        "cached_input_tokens": usage.get("cachedInputTokens").and_then(Value::as_u64),
        "output_tokens": usage.get("outputTokens").and_then(Value::as_u64),
        "reasoning_output_tokens": usage.get("reasoningOutputTokens").and_then(Value::as_u64),
    })
}

fn opaque_notification(method: &str, params: &Value) -> Value {
    json!({
        "type": format!("app_server.{}", method.replace('/', ".")),
        "method": method,
        "params": params,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::ReasoningEffort;

    fn request() -> CodexRequest {
        CodexRequest {
            working_directory: PathBuf::from("repo"),
            prompt: "inspect".to_owned(),
            model: None,
            effort: Some(ReasoningEffort::High),
            sandbox: CodexSandbox::ReadOnly,
            resume_session: None,
            output_schema: None,
        }
    }

    #[test]
    fn initialization_never_opts_into_experimental_api() {
        let initialize = StableAppServerClient::default().initialize();
        assert_eq!(initialize.method, "initialize");
        assert!(initialize.params.get("capabilities").is_none());
        assert_eq!(initialize.params["clientInfo"]["name"], "colay");
        assert_eq!(initialize.params["clientInfo"]["title"], "Colay");
    }

    #[test]
    fn empty_model_is_omitted_and_stable_sandbox_is_set() {
        let mut request = request();
        request.model = Some(String::new());
        let message = StableAppServerClient::default().thread_start(&request);
        assert!(message.params.get("model").is_none());
        assert_eq!(
            message.params.get("sandbox").and_then(Value::as_str),
            Some("read-only")
        );
    }

    #[test]
    fn response_accepts_string_ids_and_requires_one_payload() {
        let parsed = AppServerMessage::parse_line(r#"{"id":"one","result":{},"error":null}"#);
        assert!(matches!(parsed, Ok(AppServerMessage::Response(_))));
        let invalid = AppServerMessage::parse_line(r#"{"id":1}"#);
        assert_eq!(invalid, Err(AppServerError::ResponseShape));
    }

    #[test]
    fn stable_session_drives_handshake_and_normalizes_completion() -> Result<(), AppServerError> {
        let mut session = StableAppServerSession::new(AppServerSessionPlan::new(request()))?;
        let initialize = session.start()?;
        assert!(String::from_utf8_lossy(&initialize).contains("initialize"));

        let step = session.handle_frame(b"{\"id\":0,\"result\":{}}\n")?;
        assert_eq!(step.outbound.len(), 2);
        assert!(String::from_utf8_lossy(&step.outbound[0]).contains("initialized"));
        assert!(String::from_utf8_lossy(&step.outbound[1]).contains("thread/start"));

        let step =
            session.handle_frame(b"{\"id\":1,\"result\":{\"thread\":{\"id\":\"thr-1\"}}}\n")?;
        assert!(String::from_utf8_lossy(&step.outbound[0]).contains("turn/start"));
        assert_eq!(step.events[0]["type"], "thread.started");

        let step =
            session.handle_frame(b"{\"id\":2,\"result\":{\"turn\":{\"id\":\"turn-1\"}}}\n")?;
        assert_eq!(step.events[0]["type"], "turn.started");
        let _ = session.handle_frame(b"{\"method\":\"thread/tokenUsage/updated\",\"params\":{\"threadId\":\"thr-1\",\"turnId\":\"turn-1\",\"tokenUsage\":{\"last\":{\"inputTokens\":8,\"cachedInputTokens\":2,\"outputTokens\":3,\"reasoningOutputTokens\":1},\"total\":{}}}}\n")?;
        let step = session
            .handle_frame(b"{\"method\":\"turn/completed\",\"params\":{\"threadId\":\"thr-1\",\"turn\":{\"id\":\"turn-1\",\"items\":[],\"status\":\"completed\"}}}\n")?;
        assert!(step.completed);
        assert_eq!(step.events[0]["usage"]["input_tokens"], 8);
        Ok(())
    }

    #[test]
    fn approvals_are_declined_never_accepted() -> Result<(), AppServerError> {
        let mut session = StableAppServerSession::new(AppServerSessionPlan::new(request()))?;
        let _ = session.start();
        let step = session
            .handle_frame(b"{\"method\":\"item/commandExecution/requestApproval\",\"id\":\"approval-1\",\"params\":{}}\n")?;
        assert!(String::from_utf8_lossy(&step.outbound[0]).contains("decline"));
        assert!(!String::from_utf8_lossy(&step.outbound[0]).contains("accept"));
        Ok(())
    }

    #[test]
    fn malformed_or_overlong_frames_fail_closed() -> Result<(), AppServerError> {
        let mut plan = AppServerSessionPlan::new(request());
        plan.max_message_bytes = 32;
        let mut session = StableAppServerSession::new(plan)?;
        assert!(session.handle_frame(b"{}\n").is_err());
        assert!(session.handle_frame(&[b'x'; 33]).is_err());
        Ok(())
    }
}
