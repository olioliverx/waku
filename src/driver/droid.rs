//! Factory Droid driver.
//!
//! `droid exec --input-format stream-jsonrpc --output-format stream-jsonrpc`
//! serves one newline-delimited JSON-RPC session per process. Waku spawns one
//! process per Waku session, drives it with `droid.*` requests, and adapts the
//! `droid.session_notification` event stream and the `droid.request_permission`
//! / `droid.ask_user` server requests to provider-neutral [`DriverEvent`]s.
//! Resuming a Waku session reconnects to the same Factory session through
//! `droid.load_session`, which keeps Factory's own session continuity intact.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow};
use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::activity;
use crate::driver::{
    DriverControl, DriverEventSender, DriverEventSink, DriverStartOptions, SessionOptions,
};
use crate::model::{
    ActivityKind, DriverEvent, InteractionMode, PermissionOption, ProviderKind,
    ProviderResumeCursor, RuntimeMode,
};

/// The envelope version every Droid JSON-RPC message carries. Required by the
/// CLI parser; a missing or different value rejects the whole message.
const FACTORY_API_VERSION: &str = "1.0.0";
/// How long `droid.initialize_session` may take before Waku gives up. The
/// process connects the user's MCP servers before answering, so a slow server
/// can stretch the handshake well past a normal RPC round trip.
const INIT_TIMEOUT: Duration = Duration::from_secs(90);
/// `droid exec` exits when its stdin closes; give it a moment to flush a
/// `close_session` ack before killing it.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn next_request_id() -> String {
    let sequence = REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("waku-{sequence}")
}

enum CommandMessage {
    Prompt(String),
    Steer(String),
    Cancel,
    Respond {
        request_id: String,
        option_id: String,
    },
    ApplyOptions(SessionOptions),
    Shutdown,
}

enum PendingInteraction {
    Approval {
        rpc_id: String,
    },
    Question {
        rpc_id: String,
        answers: HashMap<String, (usize, String, String)>,
    },
}

/// One parsed line from the process stdout, classified by envelope `type`.
enum WireMessage {
    Response {
        id: String,
        result: Option<Value>,
        error: Option<String>,
    },
    Request {
        id: String,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Exited,
}

pub struct DroidDriver {
    child: Option<Child>,
    commands: Sender<CommandMessage>,
}

impl DroidDriver {
    pub fn start(options: DriverStartOptions, events: DriverEventSender) -> anyhow::Result<Self> {
        let DriverStartOptions {
            binary,
            cwd,
            mode,
            interaction_mode,
            model,
            reasoning_effort,
            service_tier: _,
            agent_preset: _,
            computer_use_enabled: _,
            provider_cursor,
        } = options;
        let (resume_session_id, resuming) = match provider_cursor {
            Some(ProviderResumeCursor::Droid { session_id }) if !session_id.is_empty() => {
                (Some(session_id), true)
            }
            Some(cursor) => {
                return Err(anyhow!(
                    "cannot resume Factory Droid from a {} cursor",
                    cursor.provider().display_name()
                ));
            }
            None => (None, false),
        };

        let mut command = crate::command_env::command(binary);
        command
            .args([
                "exec",
                "--input-format",
                "stream-jsonrpc",
                "--output-format",
                "stream-jsonrpc",
                "--auto",
                autonomy_level(mode, interaction_mode),
            ])
            .current_dir(&cwd)
            .envs(crate::command_env::shell_environment())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = crate::command_env::spawn(&mut command)
            .with_context(|| format!("could not launch {}", ProviderKind::Droid.command()))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Droid did not expose stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Droid did not expose stdout"))?;
        let stderr_lines = Arc::new(Mutex::new(Vec::<String>::new()));
        {
            let stderr = child
                .stderr
                .take()
                .ok_or_else(|| anyhow!("Droid did not expose stderr"))?;
            let stderr_lines = Arc::clone(&stderr_lines);
            thread::Builder::new()
                .name("waku-droid-stderr".into())
                .spawn(move || {
                    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                        let line = line.trim().to_owned();
                        if line.is_empty() {
                            continue;
                        }
                        let mut lines = stderr_lines.lock();
                        if lines.len() == 128 {
                            lines.remove(0);
                        }
                        lines.push(line);
                    }
                })?;
        }

        let (messages, message_rx) = unbounded();
        thread::Builder::new()
            .name("waku-droid-reader".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    let Ok(value) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    let Some(message) = classify(&value) else {
                        continue;
                    };
                    if messages.send(message).is_err() {
                        return;
                    }
                }
                let _ = messages.send(WireMessage::Exited);
            })?;

        let session_id = if resuming {
            let session_id = resume_session_id.as_deref().unwrap_or_default().to_owned();
            let response = send_request(
                &mut stdin,
                &message_rx,
                "droid.load_session",
                json!({
                    "sessionId": session_id,
                    "loadAllMessages": false,
                }),
                INIT_TIMEOUT,
            )?;
            // Permission requests a previous client left unanswered would hang
            // the resumed turn forever; reject them so the agent moves on.
            for pending in response
                .get("pendingPermissions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(request_id) = pending.get("requestId").and_then(Value::as_str) {
                    let _ = write_envelope(
                        &mut stdin,
                        &json!({
                            "jsonrpc": "2.0",
                            "factoryApiVersion": FACTORY_API_VERSION,
                            "type": "response",
                            "id": request_id,
                            "result": {"selectedOption": "cancel"},
                        }),
                    );
                }
            }
            for pending in response
                .get("pendingAskUserRequests")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(request_id) = pending.get("requestId").and_then(Value::as_str) {
                    let _ = write_envelope(
                        &mut stdin,
                        &json!({
                            "jsonrpc": "2.0",
                            "factoryApiVersion": FACTORY_API_VERSION,
                            "type": "response",
                            "id": request_id,
                            "result": {"cancelled": true, "answers": []},
                        }),
                    );
                }
            }
            session_id
        } else {
            let mut params = json!({
                "machineId": "waku",
                "cwd": cwd.to_string_lossy(),
                "interactionMode": if interaction_mode == InteractionMode::Plan {
                    "spec"
                } else {
                    "auto"
                },
                "autonomyLevel": autonomy_level(mode, interaction_mode),
            });
            if let Some(model) = model {
                params["modelId"] = Value::String(model);
            }
            if let Some(reasoning_effort) = reasoning_effort {
                params["reasoningEffort"] = Value::String(reasoning_effort);
            }
            let response = send_request(
                &mut stdin,
                &message_rx,
                "droid.initialize_session",
                params,
                INIT_TIMEOUT,
            )?;
            response
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("Droid initialize_session returned no session ID"))?
        };

        let _ = events.send(DriverEvent::Connected {
            provider_cursor: Some(ProviderResumeCursor::Droid { session_id }),
        });

        let (commands, command_rx) = unbounded();
        let worker_events = events;
        let worker_stdin = Arc::new(Mutex::new(stdin));
        thread::Builder::new()
            .name("waku-droid-driver".into())
            .spawn(move || {
                let mut state = StreamState {
                    turn_active: false,
                    auto_approve: mode != RuntimeMode::Ask,
                    tools: HashMap::new(),
                    pending: HashMap::new(),
                };
                loop {
                    crossbeam_channel::select! {
                        recv(command_rx) -> message => {
                            let Ok(message) = message else { return; };
                            if !handle_command(message, &worker_stdin, &worker_events, &mut state) {
                                return;
                            }
                        }
                        recv(message_rx) -> message => {
                            let Ok(message) = message else { return; };
                            if !handle_message(
                                message,
                                &worker_stdin,
                                &worker_events,
                                &mut state,
                                &stderr_lines,
                            ) {
                                return;
                            }
                        }
                    }
                }
            })?;

        Ok(Self {
            child: Some(child),
            commands,
        })
    }
}

struct StreamState {
    turn_active: bool,
    auto_approve: bool,
    tools: HashMap<String, (ActivityKind, String)>,
    pending: HashMap<String, PendingInteraction>,
}

/// Map Waku's access model onto Droid's autonomy levels and interaction modes.
/// `off` asks before every operation (Waku's Supervised mode), `low` covers
/// ordinary file edits, and the higher levels relax progressively, mirroring
/// the `--auto` ladder Droid documents for exec.
fn autonomy_level(mode: RuntimeMode, interaction_mode: InteractionMode) -> &'static str {
    if interaction_mode == InteractionMode::Plan || mode == RuntimeMode::Plan {
        "low"
    } else {
        match mode {
            RuntimeMode::Ask => "off",
            RuntimeMode::AutoAcceptEdits => "low",
            RuntimeMode::Auto => "medium",
            RuntimeMode::FullAccess => "high",
            RuntimeMode::Plan => "low",
        }
    }
}

fn classify(value: &Value) -> Option<WireMessage> {
    let message_type = value.get("type").and_then(Value::as_str)?;
    match message_type {
        "response" => {
            let id = value.get("id").and_then(Value::as_str)?.to_owned();
            let error = value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            Some(WireMessage::Response {
                id,
                result: value.get("result").cloned(),
                error,
            })
        }
        "request" => {
            let id = value.get("id").and_then(Value::as_str)?.to_owned();
            let method = value.get("method").and_then(Value::as_str)?.to_owned();
            let params = value.get("params").cloned().unwrap_or(Value::Null);
            Some(WireMessage::Request { id, method, params })
        }
        "notification" => {
            let method = value.get("method").and_then(Value::as_str)?.to_owned();
            let params = value.get("params").cloned().unwrap_or(Value::Null);
            Some(WireMessage::Notification { method, params })
        }
        _ => None,
    }
}

fn write_request(stdin: &Mutex<ChildStdin>, request: &Value) -> anyhow::Result<()> {
    write_envelope(&mut *stdin.lock(), request)
}

fn write_envelope(writer: &mut impl Write, value: &Value) -> anyhow::Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn request(method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "factoryApiVersion": FACTORY_API_VERSION,
        "type": "request",
        "id": next_request_id(),
        "method": method,
        "params": params,
    })
}

/// Send one request and wait for its response, skipping notifications and
/// server requests that arrive meanwhile. Used only for the session handshake,
/// before the worker thread owns the message stream.
fn send_request(
    stdin: &mut ChildStdin,
    message_rx: &Receiver<WireMessage>,
    method: &str,
    params: Value,
    timeout: Duration,
) -> anyhow::Result<Value> {
    let request_id = next_request_id();
    let request = json!({
        "jsonrpc": "2.0",
        "factoryApiVersion": FACTORY_API_VERSION,
        "type": "request",
        "id": request_id,
        "method": method,
        "params": params,
    });
    write_envelope(stdin, &request)?;
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| anyhow!("Droid {method} timed out"))?;
        let message = message_rx
            .recv_timeout(remaining)
            .context("Droid process exited during the handshake")?;
        let WireMessage::Response { id, result, error } = message else {
            continue;
        };
        if id != request_id {
            continue;
        }
        if let Some(error) = error {
            return Err(anyhow!("Droid {method} failed: {error}"));
        }
        return Ok(result.unwrap_or(Value::Null));
    }
}

fn handle_command(
    message: CommandMessage,
    stdin: &Arc<Mutex<ChildStdin>>,
    events: &impl DriverEventSink,
    state: &mut StreamState,
) -> bool {
    match message {
        CommandMessage::Prompt(text) => {
            if !state.turn_active {
                state.turn_active = true;
                let _ = events.send(DriverEvent::TurnStarted);
            }
            if let Err(error) = write_request(
                stdin,
                &request("droid.add_user_message", json!({"text": text})),
            ) {
                state.turn_active = false;
                let _ = events.send(DriverEvent::Error(format!(
                    "Droid rejected the prompt: {error}"
                )));
                let _ = events.send(DriverEvent::TurnFinished {
                    success: false,
                    summary: Some("Droid could not start the turn".into()),
                });
            }
        }
        CommandMessage::Steer(text) => {
            let result = write_request(
                stdin,
                &request(
                    "droid.add_user_message",
                    json!({"text": text, "queuePlacement": "end_of_turn"}),
                ),
            );
            match result {
                Ok(()) => {
                    let _ = events.send(DriverEvent::SteerAccepted { message: text });
                }
                Err(error) => {
                    let _ = events.send(DriverEvent::SteerRejected {
                        message: text,
                        reason: error.to_string(),
                    });
                }
            }
        }
        CommandMessage::Cancel => {
            for pending in state.pending.values() {
                let (rpc_id, result) = match pending {
                    PendingInteraction::Approval { rpc_id } => {
                        (rpc_id.clone(), json!({"selectedOption": "cancel"}))
                    }
                    PendingInteraction::Question { rpc_id, .. } => {
                        (rpc_id.clone(), json!({"cancelled": true, "answers": []}))
                    }
                };
                respond(stdin, &rpc_id, &result);
            }
            state.pending.clear();
            if let Err(error) = write_request(stdin, &request("droid.interrupt_session", json!({})))
            {
                let _ = events.send(DriverEvent::Error(format!(
                    "Droid could not cancel the turn: {error}"
                )));
            }
        }
        CommandMessage::Respond {
            request_id,
            option_id,
        } => {
            let Some(pending) = state.pending.remove(&request_id) else {
                return true;
            };
            match pending {
                PendingInteraction::Approval { rpc_id } => {
                    respond(stdin, &rpc_id, &json!({"selectedOption": option_id}));
                }
                PendingInteraction::Question { rpc_id, answers } => match answers.get(&option_id) {
                    Some((index, question, answer)) => respond(
                        stdin,
                        &rpc_id,
                        &json!({
                            "cancelled": false,
                            "answers": [
                                {"index": index, "question": question, "answer": answer}
                            ],
                        }),
                    ),
                    None => {
                        let _ = events.send(DriverEvent::Error(
                                "Droid rejected the interaction response: the selected answer is unavailable"
                                    .into(),
                            ));
                    }
                },
            }
        }
        CommandMessage::ApplyOptions(options) => {
            let mut params = json!({});
            if let Some(model) = options.model {
                params["modelId"] = Value::String(model);
            }
            if let Some(reasoning_effort) = options.reasoning_effort {
                params["reasoningEffort"] = Value::String(reasoning_effort);
            }
            params["autonomyLevel"] =
                Value::String(autonomy_level(options.mode, options.interaction_mode).to_owned());
            match write_request(stdin, &request("droid.update_session_settings", params)) {
                Ok(()) => {
                    state.auto_approve = options.mode != RuntimeMode::Ask;
                }
                Err(error) => {
                    let _ = events.send(DriverEvent::Error(format!(
                        "Droid could not apply the session options: {error}"
                    )));
                }
            }
        }
        CommandMessage::Shutdown => {
            let _ = write_request(
                stdin,
                &request("droid.close_session", json!({"reason": "other"})),
            );
            // Dropping the locked stdin closes the pipe; `droid exec` exits.
            let _ = stdin.lock().flush();
            return false;
        }
    }
    true
}

fn respond(stdin: &Arc<Mutex<ChildStdin>>, rpc_id: &str, result: &Value) {
    let response = json!({
        "jsonrpc": "2.0",
        "factoryApiVersion": FACTORY_API_VERSION,
        "type": "response",
        "id": rpc_id,
        "result": result,
    });
    let _ = write_request(stdin, &response);
}

fn handle_message(
    message: WireMessage,
    stdin: &Arc<Mutex<ChildStdin>>,
    events: &impl DriverEventSink,
    state: &mut StreamState,
    stderr_lines: &Arc<Mutex<Vec<String>>>,
) -> bool {
    match message {
        WireMessage::Response { error, .. } => {
            if let Some(error) = error {
                let _ = events.send(DriverEvent::Error(format!("Droid: {error}")));
            }
            true
        }
        WireMessage::Request { id, method, params } => match method.as_str() {
            "droid.request_permission" => {
                handle_permission_request(&id, &params, stdin, events, state)
            }
            "droid.ask_user" => handle_ask_user(&id, &params, stdin, events, state),
            other => {
                // Unknown server requests are answered with a rejection so the
                // agent never hangs on our silence.
                respond(stdin, &id, &json!({"selectedOption": "cancel"}));
                let _ = events.send(DriverEvent::Error(format!(
                    "Droid sent an unsupported request: {other}"
                )));
                true
            }
        },
        WireMessage::Notification { method, params } => {
            if method != "droid.session_notification" {
                return true;
            }
            let Some(notification) = params.get("notification") else {
                return true;
            };
            handle_notification(notification, events, state)
        }
        WireMessage::Exited => {
            let tail = stderr_lines
                .lock()
                .iter()
                .rev()
                .take(6)
                .rev()
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            let message = if tail.is_empty() {
                "Droid process exited unexpectedly".to_owned()
            } else {
                format!("Droid process exited unexpectedly:\n{tail}")
            };
            let _ = events.send(DriverEvent::Error(message));
            let _ = events.send(DriverEvent::ProcessExited);
            false
        }
    }
}

/// Auto-approval mirrors the ACP driver: any non-Supervised mode picks the
/// most permissive option (always, then once) without interrupting the user.
fn handle_permission_request(
    id: &str,
    params: &Value,
    stdin: &Arc<Mutex<ChildStdin>>,
    events: &impl DriverEventSink,
    state: &mut StreamState,
) -> bool {
    let options = params
        .get("options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|option| {
            let value = option.get("value").and_then(Value::as_str)?;
            let label = option.get("label").and_then(Value::as_str).unwrap_or(value);
            Some(PermissionOption {
                id: value.to_owned(),
                label: label.to_owned(),
                allow: value.starts_with("proceed_"),
            })
        })
        .collect::<Vec<_>>();
    let tool_uses = params
        .get("toolUses")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("toolUse").cloned())
        .collect::<Vec<_>>();

    if state.auto_approve {
        let choice = options
            .iter()
            .find(|option| {
                option.id == "proceed_always"
                    || option.id.starts_with("proceed_always_")
                    || option.id == "proceed_auto_run"
            })
            .or_else(|| options.iter().find(|option| option.id == "proceed_once"))
            .or_else(|| {
                options
                    .iter()
                    .find(|option| option.allow && !option.id.starts_with("proceed_edit"))
            });
        let result = choice
            .map(|option| json!({"selectedOption": option.id}))
            .unwrap_or_else(|| json!({"selectedOption": "cancel"}));
        respond(stdin, id, &result);
        return true;
    }

    let first = tool_uses.first();
    let name = first
        .and_then(|tool_use| tool_use.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("Tool");
    let title = first
        .and_then(|tool_use| activity::input_title(tool_use.get("input")))
        .unwrap_or_else(|| name.to_owned());
    let detail = first
        .and_then(|tool_use| tool_use.get("input"))
        .and_then(activity::format_json)
        .unwrap_or_else(|| tr!("permission.agent_wants_to", action = name));
    state.pending.insert(
        id.to_owned(),
        PendingInteraction::Approval {
            rpc_id: id.to_owned(),
        },
    );
    if events
        .send(DriverEvent::Permission {
            request_id: id.to_owned(),
            title,
            detail,
            options,
        })
        .is_err()
    {
        state.pending.remove(id);
        respond(stdin, id, &json!({"selectedOption": "cancel"}));
    }
    true
}

/// AskUser questions are answered from the same permission surface: one
/// single-select question at a time, matching the DeepSeek driver's contract.
fn handle_ask_user(
    id: &str,
    params: &Value,
    stdin: &Arc<Mutex<ChildStdin>>,
    events: &impl DriverEventSink,
    state: &mut StreamState,
) -> bool {
    let supported = params
        .get("questions")
        .and_then(Value::as_array)
        .is_some_and(|questions| questions.len() == 1)
        && params
            .pointer("/questions/0/multiSelect")
            .and_then(Value::as_bool)
            != Some(true)
        && params
            .pointer("/questions/0/options")
            .and_then(Value::as_array)
            .is_some_and(|options| !options.is_empty());
    if !supported {
        let message = "Waku currently supports one single-select Droid question at a time";
        respond(stdin, id, &json!({"cancelled": true, "answers": []}));
        let _ = events.send(DriverEvent::Error(message.into()));
        return true;
    }

    let question = &params["questions"][0];
    let question_text = question
        .get("question")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let mut answers = HashMap::new();
    let options = question
        .get("options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, option)| {
            let label = option.as_str()?.to_owned();
            let option_id = format!("answer-{index}");
            answers.insert(
                option_id.clone(),
                (index, question_text.clone(), label.clone()),
            );
            Some(PermissionOption {
                id: option_id,
                label,
                allow: false,
            })
        })
        .collect::<Vec<_>>();
    state.pending.insert(
        id.to_owned(),
        PendingInteraction::Question {
            rpc_id: id.to_owned(),
            answers,
        },
    );
    let title = question
        .get("topic")
        .and_then(Value::as_str)
        .filter(|topic| !topic.trim().is_empty())
        .unwrap_or("Droid question")
        .to_owned();
    if events
        .send(DriverEvent::Permission {
            request_id: id.to_owned(),
            title,
            detail: question_text,
            options,
        })
        .is_err()
    {
        state.pending.remove(id);
        respond(stdin, id, &json!({"cancelled": true, "answers": []}));
    }
    true
}

fn handle_notification(
    notification: &Value,
    events: &impl DriverEventSink,
    state: &mut StreamState,
) -> bool {
    let Some(notification_type) = notification.get("type").and_then(Value::as_str) else {
        return true;
    };
    match notification_type {
        "assistant_text_delta" => {
            if let Some(text) = notification
                .get("textDelta")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                let _ = events.send(DriverEvent::TextDelta(text.to_owned()));
            }
        }
        "thinking_text_delta" => {
            if let Some(text) = notification
                .get("textDelta")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                let _ = events.send(DriverEvent::ReasoningDelta(text.to_owned()));
            }
        }
        "tool_call" => {
            if let Some(tool_use) = notification.get("toolUse") {
                tool_activity(tool_use, None, false, false, events, state);
            }
        }
        "tool_result" => {
            let tool_use_id = notification
                .get("toolUseId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let failed = notification.get("isError").and_then(Value::as_bool) == Some(true);
            if let Some(tool_use_id) = tool_use_id {
                tool_activity(
                    &json!({"id": tool_use_id}),
                    notification.get("content"),
                    true,
                    failed,
                    events,
                    state,
                );
            }
        }
        "agent_turn_completed" => {
            state.turn_active = false;
            let reason = notification
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("completed");
            let (success, summary) = match reason {
                "completed" | "cancelled" => (true, None),
                other => (
                    false,
                    Some(tr!("session.agent_stopped_reason", reason = other)),
                ),
            };
            let _ = events.send(DriverEvent::TurnFinished { success, summary });
        }
        "session_token_usage_changed" => {
            let context_tokens = notification.get("tokenUsage").and_then(|usage| {
                let field = |name: &str| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
                let total = field("inputTokens")
                    + field("cacheCreationTokens")
                    + field("cacheReadTokens")
                    + field("outputTokens");
                (total > 0).then_some(total)
            });
            if context_tokens.is_some() {
                let _ = events.send(DriverEvent::UsageUpdated {
                    context_tokens,
                    context_window: None,
                });
            }
        }
        "session_title_updated" => {
            let title = notification
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let _ = events.send(DriverEvent::AutoTitleUpdated(title));
        }
        "error" => {
            let message = notification
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Droid reported an error");
            let _ = events.send(DriverEvent::Error(message.to_owned()));
        }
        // Echoed messages, MCP lifecycle, hooks, compaction, and queue
        // bookkeeping have no transcript representation.
        _ => {}
    }
    true
}

fn tool_activity(
    tool_use: &Value,
    output: Option<&Value>,
    complete: bool,
    failed: bool,
    events: &impl DriverEventSink,
    state: &mut StreamState,
) {
    let id = tool_use
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let name = tool_use
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("Tool");
    let kind = ActivityKind::from_tool_name(name);
    let arguments = tool_use.get("input").filter(|value| !value.is_null());
    let stored = id.as_ref().and_then(|id| {
        if complete {
            state.tools.remove(id)
        } else {
            state.tools.get(id).cloned()
        }
    });
    let title = activity::input_title(arguments)
        .or_else(|| stored.as_ref().map(|(_, title)| title.clone()))
        .unwrap_or_else(|| name.to_owned());
    if !complete && let Some(id) = id.as_ref() {
        state.tools.insert(id.clone(), (kind, title.clone()));
    }
    let item =
        activity::tool_activity(id, kind, title, arguments, output, output, failed, complete);
    let _ = events.send(DriverEvent::RichActivity(item));
}

impl DriverControl for DroidDriver {
    fn prompt(&self, prompt: String) {
        let _ = self.commands.send(CommandMessage::Prompt(prompt));
    }

    fn supports_steer(&self) -> bool {
        true
    }

    fn steer(&self, prompt: String) {
        let _ = self.commands.send(CommandMessage::Steer(prompt));
    }

    fn cancel(&self) {
        let _ = self.commands.send(CommandMessage::Cancel);
    }

    fn respond(&self, request_id: String, option_id: String) {
        let _ = self.commands.send(CommandMessage::Respond {
            request_id,
            option_id,
        });
    }

    fn apply_options(&self, options: SessionOptions) -> bool {
        self.commands
            .send(CommandMessage::ApplyOptions(options))
            .is_ok()
    }

    fn rollback(&self, _turns: usize) -> anyhow::Result<Option<ProviderResumeCursor>> {
        anyhow::bail!("conversation rollback is not supported by Droid")
    }

    fn fork(&self, _turns_to_remove: usize) -> anyhow::Result<ProviderResumeCursor> {
        anyhow::bail!("conversation forking is not supported by Droid")
    }
}

impl Drop for DroidDriver {
    fn drop(&mut self) {
        let _ = self.commands.send(CommandMessage::Shutdown);
        if let Some(mut child) = self.child.take() {
            let deadline = Instant::now() + SHUTDOWN_GRACE;
            while Instant::now() < deadline {
                if child.try_wait().ok().flatten().is_some() {
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autonomy_levels_follow_the_access_ladder() {
        assert_eq!(
            autonomy_level(RuntimeMode::Ask, InteractionMode::Build),
            "off"
        );
        assert_eq!(
            autonomy_level(RuntimeMode::AutoAcceptEdits, InteractionMode::Build),
            "low"
        );
        assert_eq!(
            autonomy_level(RuntimeMode::Auto, InteractionMode::Build),
            "medium"
        );
        assert_eq!(
            autonomy_level(RuntimeMode::FullAccess, InteractionMode::Build),
            "high"
        );
        assert_eq!(
            autonomy_level(RuntimeMode::FullAccess, InteractionMode::Plan),
            "low"
        );
    }

    #[test]
    fn classifies_wire_envelopes() {
        let response = classify(&json!({
            "jsonrpc": "2.0",
            "type": "response",
            "id": "1",
            "result": {"sessionId": "abc"}
        }))
        .unwrap();
        assert!(matches!(
            response,
            WireMessage::Response { id, .. } if id == "1"
        ));

        let request = classify(&json!({
            "jsonrpc": "2.0",
            "type": "request",
            "id": "2",
            "method": "droid.request_permission",
            "params": {}
        }))
        .unwrap();
        assert!(matches!(
            request,
            WireMessage::Request { method, .. } if method == "droid.request_permission"
        ));

        let notification = classify(&json!({
            "jsonrpc": "2.0",
            "type": "notification",
            "method": "droid.session_notification",
            "params": {"notification": {"type": "agent_turn_completed"}}
        }))
        .unwrap();
        assert!(matches!(notification, WireMessage::Notification { .. }));
    }
}
