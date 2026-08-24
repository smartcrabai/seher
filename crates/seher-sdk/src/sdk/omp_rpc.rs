//! Subprocess backend for the oh-my-pi (`omp`) RPC protocol.
//!
//! Mirrors the TypeScript Pi RPC runner, but oh-my-pi serves host tools
//! natively over stdio (`set_host_tools` / `host_tool_call`), so no extension
//! file or TCP bridge is needed. The omp-side session id is only known after
//! spawn (via `get_state`), so freshly spawned sessions are rekeyed in the
//! registry from their placeholder UUID to the real omp session id.
//!
//! Wire notes verified against omp 17.3.5:
//! - First stdout frame is `{"type":"ready","protocolVersion":1,...}`; we stay
//!   on protocol v1 (never send `negotiate_protocol`).
//! - Prompt completion is `agent_end` with `isTerminal != false`; there is no
//!   `agent_settled`. A prompt ack carrying `data.agentInvoked == false`, or a
//!   later `prompt_result` frame, also completes a local-only prompt.
//! - `extension_ui_request` frames (e.g. the built-in `setWidget`) MUST be
//!   answered with `extension_ui_response` `cancelled: true`; an unanswered
//!   request stalls the agent loop even with `--no-extensions`.

use std::collections::HashMap;
#[cfg(not(unix))]
use std::ffi::OsString;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::process::CommandExt;

use crate::sdk::cancel::CancelToken;
use crate::sdk::errors::{NON_RETRYABLE_PREFIX, RunError};
#[cfg(unix)]
use crate::sdk::pi_rpc::runtime_path_for_candidate;
use crate::sdk::pi_rpc::{
    StderrTail, agent_messages_error, append_stderr_redacted, canonical_cwd, classified_chunk,
    classified_chunk_with_source, merged_child_path, message_end_error, network_error_reason,
    parse_jsonl_frame, provider_api_key_env, read_jsonl_line, resolve_candidate_program,
    session_header_cwd, session_state_cwd, terminate_process, write_json_line,
};
use crate::sdk::pi_runner::{PiRunOutput, StreamChunk};
use crate::sdk::tool::SeherTool;

const PACKAGE: &str = "@oh-my-pi/pi-coding-agent";
const CONTROL_RESPONSE_WAIT: Duration = Duration::from_secs(5);
const CLOSE_WAIT: Duration = Duration::from_millis(500);
// Package managers can spend tens of seconds downloading a cold candidate.
// This remains bounded and cancellation-aware, while close keeps its short deadline.
const HANDSHAKE_WAIT: Duration = Duration::from_secs(30);

type Registry = HashMap<SessionKey, Arc<SessionEntry>>;
static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn registry() -> &'static Mutex<Registry> {
    &REGISTRY
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct SessionKey {
    cwd: PathBuf,
    id: String,
}

#[derive(Debug)]
struct SessionEntry {
    tx: mpsc::Sender<WorkerCommand>,
    busy: AtomicBool,
    prompt_active: AtomicBool,
    prompt_acknowledged: AtomicBool,
    initialized: AtomicBool,
    closing: AtomicBool,
    process: Arc<Mutex<Option<Child>>>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    pending: Mutex<HashMap<String, mpsc::Sender<Result<serde_json::Value, String>>>>,
    close_deadline: Mutex<Option<Instant>>,
    startup_error: Mutex<Option<String>>,
    done: Arc<(Mutex<bool>, Condvar)>,
    fingerprint: String,
    /// Real omp-side session id, learned from the `get_state` handshake.
    omp_session_id: Mutex<Option<String>>,
}

enum WorkerCommand {
    Prompt {
        prompt: String,
        output: mpsc::Sender<StreamChunk>,
        cancel: CancelToken,
    },
    Control {
        command: serde_json::Value,
        response: mpsc::Sender<Result<serde_json::Value, String>>,
    },
}

#[derive(Clone, Default)]
pub struct OmpRpcRunnerOptions {
    pub cancel: CancelToken,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub thinking: Option<String>,
    pub system_prompt: Option<String>,
    /// Additional prompt text appended to omp's system prompt.
    pub append_system_prompt: Option<String>,
    pub working_directory: Option<PathBuf>,
    pub env: indexmap::IndexMap<String, String>,
    pub tools: Vec<SeherTool>,
    /// Override the first candidate executable. Intended for embedders and tests.
    pub omp_bin: Option<PathBuf>,
}

impl std::fmt::Debug for OmpRpcRunnerOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OmpRpcRunnerOptions")
            .field("cancelled", &self.cancel.is_cancelled())
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("api_key", &self.api_key.as_ref().map(|_| "***"))
            .field("thinking", &self.thinking)
            .field("system_prompt", &self.system_prompt)
            .field("append_system_prompt", &self.append_system_prompt)
            .field("working_directory", &self.working_directory)
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .field(
                "tools",
                &self.tools.iter().map(|tool| &tool.name).collect::<Vec<_>>(),
            )
            .field("omp_bin", &self.omp_bin)
            .finish()
    }
}

pub struct OmpRpcRunner {
    opts: OmpRpcRunnerOptions,
}

impl OmpRpcRunner {
    #[must_use]
    pub fn new(opts: OmpRpcRunnerOptions) -> Self {
        Self { opts }
    }

    /// Stream one prompt. `resume` is the omp session id to continue, or `None`
    /// for a new session (whose id is reported via [`StreamChunk::Session`]).
    #[must_use]
    pub fn stream(&self, prompt: String, resume: Option<String>) -> mpsc::Receiver<StreamChunk> {
        let (output_tx, output_rx) = mpsc::channel();
        let opts = self.opts.clone();
        thread::spawn(move || stream_prompt(&opts, &prompt, resume.as_deref(), &output_tx));
        output_rx
    }

    /// Drain a stream into the shared output/error contract.
    /// # Errors
    ///
    /// Returns [`RunError`] when the omp process or its RPC stream fails.
    pub fn run(&self, prompt: String, resume: Option<String>) -> Result<PiRunOutput, RunError> {
        let receiver = self.stream(prompt, resume);
        let mut text = String::new();
        let mut session_id = String::new();
        loop {
            match receiver.recv() {
                Ok(StreamChunk::Delta(delta)) => text.push_str(&delta),
                Ok(StreamChunk::Session(id)) => session_id = id,
                Ok(StreamChunk::Done(final_text)) => {
                    return Ok(PiRunOutput {
                        text: if final_text.is_empty() {
                            text
                        } else {
                            final_text
                        },
                        session_id,
                    });
                }
                Ok(StreamChunk::Limit(error)) => {
                    return Err(RunError::Limit {
                        error,
                        partial: text,
                    });
                }
                Ok(StreamChunk::Error(message)) => {
                    return Err(RunError::Other {
                        message,
                        partial: text,
                    });
                }
                Err(_) => {
                    return Err(RunError::Other {
                        message: "omp rpc runner channel closed".into(),
                        partial: text,
                    });
                }
            }
        }
    }

    /// Stop the session owned by this runner, if present.
    #[must_use]
    pub fn close_omp_session(&self, id: &str) -> bool {
        close_omp_session(id, self.opts.working_directory.as_deref())
    }

    /// Send a confirmed omp RPC session-control command.
    ///
    /// The command must be a JSON object containing a supported `type`. Idle
    /// controls are serialized with prompt I/O; `steer`, `follow_up`, and
    /// `abort` are accepted during an active prompt and written directly to
    /// the live RPC stdin.
    ///
    /// # Errors
    ///
    /// Returns an error when the session is missing, busy, closing, or the
    /// worker cannot receive the command.
    pub fn send_command(
        &self,
        session_id: &str,
        command: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        send_command(session_id, self.opts.working_directory.as_deref(), command)
    }

    /// Cancel and tear down a running prompt without replaying it.
    ///
    /// # Errors
    ///
    /// Returns an error when the session is not running.
    pub fn cancel_omp_session(&self, session_id: &str) -> Result<serde_json::Value, String> {
        if close_omp_session(session_id, self.opts.working_directory.as_deref()) {
            Ok(serde_json::json!({"aborted": true}))
        } else {
            Err(format!("omp session '{session_id}' is not running"))
        }
    }
}

/// Send a confirmed omp RPC session-control command to an existing session.
///
/// # Errors
///
/// Returns an error when the command is unsupported, the session is missing,
/// busy, closing, or the worker cannot receive the command.
pub fn send_command(
    session_id: &str,
    cwd: Option<&Path>,
    command: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let command_type = command_type(&command)?;
    let key = SessionKey {
        cwd: canonical_cwd(cwd),
        id: session_id.to_string(),
    };
    let entry = registry()
        .lock()
        .map_err(|_| "omp registry poisoned".to_string())?
        .get(&key)
        .cloned()
        .ok_or_else(|| format!("omp session '{session_id}' is not running"))?;
    if entry.closing.load(Ordering::Acquire) {
        return Err(format!("omp session '{session_id}' is closing"));
    }
    if entry.busy.load(Ordering::Acquire) {
        if entry.prompt_active.load(Ordering::Acquire)
            && matches!(command_type, "steer" | "follow_up" | "abort")
        {
            return send_busy_control(&entry, &command);
        }
        return Err(format!(
            "omp session '{session_id}' is busy; '{command_type}' is only safe while idle"
        ));
    }
    if entry
        .busy
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        if entry.prompt_active.load(Ordering::Acquire)
            && matches!(command_type, "steer" | "follow_up" | "abort")
        {
            return send_busy_control(&entry, &command);
        }
        return Err(format!(
            "omp session '{session_id}' is busy; '{command_type}' is only safe while idle"
        ));
    }
    let (response_tx, response_rx) = mpsc::channel();
    if entry
        .tx
        .send(WorkerCommand::Control {
            command,
            response: response_tx,
        })
        .is_err()
    {
        entry.busy.store(false, Ordering::Release);
        return Err("omp session worker stopped".to_string());
    }
    response_rx
        .recv()
        .map_err(|_| "omp session worker stopped".to_string())?
}

fn command_type(command: &serde_json::Value) -> Result<&str, String> {
    command
        .as_object()
        .and_then(|object| object.get("type"))
        .and_then(serde_json::Value::as_str)
        .filter(|kind| {
            matches!(
                *kind,
                "steer"
                    | "follow_up"
                    | "abort"
                    | "get_state"
                    | "get_messages"
                    | "new_session"
                    | "switch_session"
            )
        })
        .ok_or_else(|| "unsupported Omp RPC session-control command".to_string())
}

fn send_busy_control(
    entry: &SessionEntry,
    command: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let id = format!("seher-control-{}", uuid::Uuid::new_v4());
    let mut request = command
        .as_object()
        .cloned()
        .ok_or_else(|| "Omp RPC command must be an object".to_string())?;
    request.insert("id".into(), serde_json::Value::String(id.clone()));
    let (response_tx, response_rx) = mpsc::channel();
    entry
        .pending
        .lock()
        .map_err(|_| "omp pending response state poisoned".to_string())?
        .insert(id.clone(), response_tx);
    let write_result = match entry
        .stdin
        .lock()
        .map_err(|_| "omp stdin state poisoned".to_string())
    {
        Ok(mut stdin) => match stdin.as_mut() {
            Some(stdin) => write_json_line(stdin, &serde_json::Value::Object(request))
                .map_err(|error| error.to_string()),
            None => Err("Omp RPC stdin closed".to_string()),
        },
        Err(error) => Err(error),
    };
    if let Err(error) = write_result {
        let _ = entry.pending.lock().map(|mut pending| pending.remove(&id));
        return Err(error);
    }
    match response_rx.recv_timeout(CONTROL_RESPONSE_WAIT) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            let _ = entry.pending.lock().map(|mut pending| pending.remove(&id));
            Err("Omp RPC control response timed out".to_string())
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let _ = entry.pending.lock().map(|mut pending| pending.remove(&id));
            Err("omp session worker stopped".to_string())
        }
    }
}

fn route_pending_response(entry: &SessionEntry, frame: &serde_json::Value) -> bool {
    let Some(id) = frame.get("id").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let waiter = entry
        .pending
        .lock()
        .ok()
        .and_then(|mut pending| pending.remove(id));
    let Some(waiter) = waiter else { return false };
    let result = if frame.get("success").and_then(serde_json::Value::as_bool) == Some(true) {
        Ok(frame.get("data").cloned().unwrap_or_else(|| frame.clone()))
    } else {
        Err(frame
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Omp RPC command failed")
            .to_string())
    };
    let _ = waiter.send(result);
    true
}

/// Stop an omp RPC session, if it is registered.
#[must_use]
pub fn close_omp_session(id: &str, cwd: Option<&Path>) -> bool {
    let key = SessionKey {
        cwd: canonical_cwd(cwd),
        id: id.to_string(),
    };
    let entry = {
        let Ok(sessions) = registry().lock() else {
            return false;
        };
        let Some(entry) = sessions.get(&key).cloned() else {
            return false;
        };
        entry.closing.store(true, Ordering::Release);
        entry
    };
    close_session_entry(&key, &entry);
    true
}

fn close_session_entry(key: &SessionKey, entry: &Arc<SessionEntry>) {
    let deadline = Instant::now() + CLOSE_WAIT;
    if let Ok(mut close_deadline) = entry.close_deadline.lock() {
        *close_deadline = Some(deadline);
    }

    let watchdog_entry = Arc::clone(entry);
    thread::spawn(move || {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_default();
        thread::sleep(remaining);
        let done = watchdog_entry.done.0.lock().is_ok_and(|done| *done);
        if !done {
            terminate_process(&watchdog_entry.process);
        }
    });

    // Abort first, then close stdin so idle and active RPC processes cannot keep
    // the worker alive until the long process timeout.
    let stdin = Arc::clone(&entry.stdin);
    let process = Arc::clone(&entry.process);
    thread::spawn(move || match stdin.try_lock() {
        Ok(mut stdin) => {
            if let Some(stdin) = stdin.as_mut() {
                let _ = write_json_line(
                    stdin,
                    &serde_json::json!({"id": format!("seher-close-{}", uuid::Uuid::new_v4()), "type": "abort"}),
                );
            }
            stdin.take();
        }
        Err(_) => terminate_process(&process),
    });
    let deadline = Instant::now() + CLOSE_WAIT;
    if let Ok(mut close_deadline) = entry.close_deadline.lock() {
        *close_deadline = Some(deadline);
    }
    let (done_lock, done_cv) = &*entry.done;
    let Ok(mut done) = done_lock.lock() else {
        terminate_process(&entry.process);
        fail_pending_responses(entry, "omp session closed");
        remove_if_same(key, entry);
        remove_entry_identity(entry);
        return;
    };
    while !*done {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        match done_cv.wait_timeout(done, remaining) {
            Ok((next, _)) => done = next,
            Err(error) => {
                done = error.into_inner().0;
                terminate_process(&entry.process);
                break;
            }
        }
    }
    if !*done {
        terminate_process(&entry.process);
    }
    drop(done);
    fail_pending_responses(entry, "omp session closed");
    remove_if_same(key, entry);
    remove_entry_identity(entry);
}

/// Stop all omp RPC sessions.
pub fn close_all_omp_sessions() {
    let entries = {
        let Ok(sessions) = registry().lock() else {
            return;
        };
        let entries = sessions
            .iter()
            .map(|(key, entry)| (key.clone(), Arc::clone(entry)))
            .collect::<Vec<_>>();
        for (_, entry) in &entries {
            entry.closing.store(true, Ordering::Release);
        }
        entries
    };
    for (key, entry) in entries {
        close_session_entry(&key, &entry);
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "stream startup owns session reservation and admission state"
)]
fn stream_prompt(
    opts: &OmpRpcRunnerOptions,
    prompt: &str,
    resume: Option<&str>,
    output: &mpsc::Sender<StreamChunk>,
) {
    if let Err(error) = validate_tool_names(&opts.tools) {
        let _ = output.send(StreamChunk::Error(error));
        return;
    }
    if opts.cancel.is_cancelled() {
        let _ = output.send(classified_chunk(
            "omp session cancelled before worker startup",
            opts.provider.as_deref().unwrap_or("omp"),
        ));
        return;
    }
    let id = resume.map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_string);
    let key = SessionKey {
        cwd: canonical_cwd(opts.working_directory.as_deref()),
        id: id.clone(),
    };
    let entry = match reserve_session(&key, opts, resume) {
        Ok(entry) => entry,
        Err(error) => {
            let _ = output.send(StreamChunk::Error(error));
            return;
        }
    };
    let startup_deadline = Instant::now() + HANDSHAKE_WAIT;
    while !entry.initialized.load(Ordering::Acquire) {
        if opts.cancel.is_cancelled()
            || entry.closing.load(Ordering::Acquire)
            || Instant::now() >= startup_deadline
        {
            entry.closing.store(true, Ordering::Release);
            terminate_process(&entry.process);
            break;
        }
        let done = entry.done.0.lock().map_or(true, |done| *done);
        if done {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    if !entry.initialized.load(Ordering::Acquire) {
        let message = if opts.cancel.is_cancelled() {
            "omp session cancelled during handshake".to_string()
        } else {
            entry
                .startup_error
                .lock()
                .ok()
                .and_then(|error| error.clone())
                .unwrap_or_else(|| "omp session worker stopped before startup".to_string())
        };
        remove_if_same(&key, &entry);
        let _ = output.send(classified_chunk(
            &message,
            opts.provider.as_deref().unwrap_or("omp"),
        ));
        return;
    }
    if entry.closing.load(Ordering::Acquire) {
        let _ = output.send(StreamChunk::Error(format!(
            "omp session '{}' is closing",
            key.id
        )));
        return;
    }
    if entry.busy.swap(true, Ordering::AcqRel) {
        let _ = output.send(StreamChunk::Error(format!(
            "omp session '{}' is busy",
            key.id
        )));
        return;
    }
    entry.prompt_active.store(true, Ordering::Release);
    // omp assigns the session id during the handshake; never expose the
    // placeholder UUID reserved before spawn.
    let session_id = entry
        .omp_session_id
        .lock()
        .ok()
        .and_then(|slot| slot.clone());
    let Some(session_id) = session_id else {
        entry.prompt_active.store(false, Ordering::Release);
        entry.busy.store(false, Ordering::Release);
        let _ = output.send(StreamChunk::Error(
            "Omp RPC session id unavailable".to_string(),
        ));
        return;
    };
    let _ = output.send(StreamChunk::Session(session_id));
    if entry
        .tx
        .send(WorkerCommand::Prompt {
            prompt: prompt.to_string(),
            output: output.clone(),
            cancel: opts.cancel.clone(),
        })
        .is_err()
    {
        entry.prompt_active.store(false, Ordering::Release);
        entry.busy.store(false, Ordering::Release);
        remove_if_same(&key, &entry);
        let message = if opts.cancel.is_cancelled() {
            "omp session cancelled before worker startup"
        } else {
            "omp session worker stopped"
        };
        let _ = output.send(classified_chunk(
            message,
            opts.provider.as_deref().unwrap_or("omp"),
        ));
    }
}

fn reserve_session(
    key: &SessionKey,
    opts: &OmpRpcRunnerOptions,
    resume: Option<&str>,
) -> Result<Arc<SessionEntry>, String> {
    let fingerprint = options_fingerprint(opts);
    let mut sessions = registry()
        .lock()
        .map_err(|_| "omp registry poisoned".to_string())?;
    if let Some(entry) = sessions.get(key) {
        if entry.closing.load(Ordering::Acquire) {
            return Err(format!("omp session '{}' is closing", key.id));
        }
        if entry.fingerprint != fingerprint {
            return Err(format!(
                "omp session '{}' was started with different provider/model/thinking/credentials/environment/prompt/tools",
                key.id
            ));
        }
        return Ok(Arc::clone(entry));
    }
    let (tx, rx) = mpsc::channel();
    let process = Arc::new(Mutex::new(None));
    let stdin = Arc::new(Mutex::new(None));
    let done = Arc::new((Mutex::new(false), Condvar::new()));
    let entry = Arc::new(SessionEntry {
        tx,
        busy: AtomicBool::new(false),
        prompt_active: AtomicBool::new(false),
        prompt_acknowledged: AtomicBool::new(false),
        initialized: AtomicBool::new(false),
        closing: AtomicBool::new(false),
        process: Arc::clone(&process),
        stdin: Arc::clone(&stdin),
        pending: Mutex::new(HashMap::new()),
        close_deadline: Mutex::new(None),
        startup_error: Mutex::new(None),
        done: Arc::clone(&done),
        fingerprint,
        omp_session_id: Mutex::new(None),
    });
    sessions.insert(key.clone(), Arc::clone(&entry));
    let key = key.clone();
    let opts = opts.clone();
    let resume = resume.map(str::to_string);
    let worker_entry = Arc::clone(&entry);
    thread::spawn(move || worker_loop(key, &opts, &rx, &worker_entry, resume.as_deref()));
    Ok(entry)
}

fn options_fingerprint(opts: &OmpRpcRunnerOptions) -> String {
    let mut env = opts.env.iter().collect::<Vec<_>>();
    env.sort_by(|left, right| left.0.cmp(right.0));
    let tools = opts
        .tools
        .iter()
        .map(|tool| {
            serde_json::json!({
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
                "handler": Arc::as_ptr(&tool.handler).cast::<()>() as usize,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "provider": opts.provider, "model": opts.model, "thinking": opts.thinking,
        "api_key": opts.api_key, "env": env, "system_prompt": opts.system_prompt,
        "append_system_prompt": opts.append_system_prompt,
        "tools": tools,
    })
    .to_string()
}

fn worker_loop(
    mut key: SessionKey,
    opts: &OmpRpcRunnerOptions,
    rx: &mpsc::Receiver<WorkerCommand>,
    entry: &Arc<SessionEntry>,
    resume: Option<&str>,
) {
    let cancelled_before_start = opts.cancel.is_cancelled();
    let result = run_worker(&mut key, opts, rx, entry, resume);
    let queued_error = result
        .err()
        .or_else(|| {
            cancelled_before_start
                .then(|| "omp session cancelled before worker startup".to_string())
        })
        .or_else(|| {
            entry
                .closing
                .load(Ordering::Acquire)
                .then(|| "omp session is closing".to_string())
        });
    if !entry.initialized.load(Ordering::Acquire)
        && let Some(message) = &queued_error
        && let Ok(mut startup_error) = entry.startup_error.lock()
    {
        *startup_error = Some(message.clone());
    }
    if let Some(message) = queued_error {
        entry.prompt_active.store(false, Ordering::Release);
        entry.busy.store(false, Ordering::Release);
        remove_if_same(&key, entry);
        while let Ok(command) = rx.try_recv() {
            match command {
                WorkerCommand::Prompt { output, .. } => {
                    let _ = output.send(classified_chunk(
                        &message,
                        opts.provider.as_deref().unwrap_or("omp"),
                    ));
                }
                WorkerCommand::Control { response, .. } => {
                    entry.busy.store(false, Ordering::Release);
                    let _ = response.send(Err(message.clone()));
                }
            }
        }
    }
    let (done_lock, done_cv) = &*entry.done;
    if let Ok(mut done) = done_lock.lock() {
        *done = true;
        done_cv.notify_all();
    }
}

fn fail_pending_responses(entry: &SessionEntry, message: &str) {
    let waiters = entry
        .pending
        .lock()
        .ok()
        .map(|mut pending| {
            pending
                .drain()
                .map(|(_, waiter)| waiter)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for waiter in waiters {
        let _ = waiter.send(Err(message.to_string()));
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "RPC worker owns the session state machine"
)]
fn run_worker(
    key: &mut SessionKey,
    opts: &OmpRpcRunnerOptions,
    rx: &mpsc::Receiver<WorkerCommand>,
    entry: &Arc<SessionEntry>,
    resume: Option<&str>,
) -> Result<(), String> {
    if entry.closing.load(Ordering::Acquire) {
        return Ok(());
    }
    if opts.cancel.is_cancelled() {
        return Err("omp session cancelled before worker startup".into());
    }
    let session_dir = omp_session_dir();
    std::fs::create_dir_all(&session_dir)
        .map_err(|e| format!("failed to create Omp session directory: {e}"))?;
    let process = spawn_candidate(opts, key, &session_dir, resume, entry)?;
    // omp does not accept a caller-chosen session id; rekey the registry from
    // the placeholder UUID to the id reported by the handshake.
    if resume.is_none()
        && let Err(error) = rekey_session(key, &key.cwd.clone(), &process.session_id, entry)
    {
        terminate_process(&entry.process);
        let _ = process.stderr.finish();
        return Err(error);
    }
    if resume.is_none() {
        key.id.clone_from(&process.session_id);
    }
    *entry
        .omp_session_id
        .lock()
        .map_err(|_| "omp session id state poisoned".to_string())? = Some(process.session_id);
    *entry
        .stdin
        .lock()
        .map_err(|_| "omp stdin state poisoned".to_string())? = Some(process.stdin);
    entry.initialized.store(true, Ordering::Release);
    let mut stdout = BufReader::new(process.stdout);
    let mut stderr = Some(process.stderr);
    loop {
        let command = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) if entry.closing.load(Ordering::Acquire) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let exited = if let Ok(mut slot) = entry.process.lock() {
                    if let Some(child) = slot.as_mut() {
                        child.try_wait().map_err(|e| e.to_string())?.is_some()
                    } else {
                        false
                    }
                } else {
                    false
                };
                if exited {
                    terminate_process(&entry.process);
                    if let Some(tail) = stderr.take() {
                        let _ = tail.finish();
                    }
                    return Err("Omp RPC process exited while idle".into());
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if entry.closing.load(Ordering::Acquire) {
            match command {
                WorkerCommand::Control { response, .. } => {
                    entry.busy.store(false, Ordering::Release);
                    let _ = response.send(Err("omp session is closing".to_string()));
                }
                WorkerCommand::Prompt { output, .. } => {
                    let _ = output.send(classified_chunk(
                        "omp session is closing",
                        opts.provider.as_deref().unwrap_or("omp"),
                    ));
                }
            }
            break;
        }
        match command {
            WorkerCommand::Control { command, response } => {
                let result =
                    control_once(&entry.stdin, &mut stdout, &command, key, entry, &opts.tools);
                entry.busy.store(false, Ordering::Release);
                let _ = response.send(result);
            }
            WorkerCommand::Prompt {
                prompt,
                output,
                cancel,
            } => {
                entry.prompt_active.store(true, Ordering::Release);
                entry.prompt_acknowledged.store(false, Ordering::Release);
                let prompt_result = prompt_once(
                    &entry.stdin,
                    &mut stdout,
                    &prompt,
                    &output,
                    entry,
                    &cancel,
                    &opts.tools,
                );
                match prompt_result {
                    Ok(text) => {
                        entry.prompt_acknowledged.store(false, Ordering::Release);
                        entry.prompt_active.store(false, Ordering::Release);
                        entry.busy.store(false, Ordering::Release);
                        let _ = output.send(StreamChunk::Done(text));
                        for _ in 0..16 {
                            if child_exited_entry(entry) {
                                entry.prompt_active.store(false, Ordering::Release);
                                entry.busy.store(false, Ordering::Release);
                                remove_if_same(key, entry);
                                return Ok(());
                            }
                            thread::sleep(Duration::from_millis(10));
                        }
                    }
                    Err(error) => {
                        let acknowledged = entry.prompt_acknowledged.swap(false, Ordering::AcqRel);
                        entry.prompt_active.store(false, Ordering::Release);
                        // Keep busy set until process, stderr, and registry teardown finish.
                        terminate_process(&entry.process);
                        let detail = stderr.take().map(StderrTail::finish).unwrap_or_default();
                        let source = if acknowledged && is_omp_process_failure(&error) {
                            format!("{NON_RETRYABLE_PREFIX}{error}")
                        } else {
                            error.clone()
                        };
                        let message = append_stderr(&source, &detail, opts);
                        let _ = output.send(classified_chunk_with_source(
                            &source,
                            &message,
                            opts.provider.as_deref().unwrap_or("omp"),
                        ));
                        return Err(error);
                    }
                }
            }
        }
        if entry.closing.load(Ordering::Acquire) {
            break;
        }
    }
    reap_worker_process(entry, stderr);
    Ok(())
}

fn reap_worker_process(entry: &SessionEntry, stderr: Option<StderrTail>) {
    if entry.closing.load(Ordering::Acquire) {
        let deadline = entry
            .close_deadline
            .lock()
            .ok()
            .and_then(|deadline| *deadline)
            .unwrap_or_else(|| Instant::now() + CLOSE_WAIT);
        while !child_exited_entry(entry) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if !child_exited_entry(entry) {
            terminate_process(&entry.process);
        }
    } else if let Ok(mut child) = entry.process.lock()
        && let Some(child) = child.as_mut()
    {
        let _ = child.wait();
    }
    if let Some(tail) = stderr {
        let _ = tail.finish();
    }
}

struct SpawnedProcess {
    stdin: ChildStdin,
    stdout: std::process::ChildStdout,
    stderr: StderrTail,
    session_id: String,
}

fn append_stderr(message: &str, stderr: &str, opts: &OmpRpcRunnerOptions) -> String {
    let mut secrets: Vec<&str> = Vec::new();
    if let Some(secret) = &opts.api_key {
        secrets.push(secret);
    }
    secrets.extend(
        opts.env
            .values()
            .filter(|value| !value.is_empty())
            .map(String::as_str),
    );
    append_stderr_redacted(message, stderr, &secrets)
}

fn candidate_command(
    opts: &OmpRpcRunnerOptions,
    key: &SessionKey,
    program: &str,
    args: &[String],
) -> Command {
    let program = resolve_candidate_program(program, &opts.env);
    #[cfg(unix)]
    let runtime_path = runtime_path_for_candidate(Path::new(&program));
    #[cfg(not(unix))]
    let runtime_path: Option<OsString> = None;
    let mut command = Command::new(&program);
    command
        .args(args)
        .current_dir(&key.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.env_clear();
    for (name, value) in &opts.env {
        command.env(name, value);
    }
    command.env(
        "PATH",
        merged_child_path(opts.env.get("PATH").map(String::as_str), runtime_path),
    );
    if let (Some(provider), Some(key)) = (opts.provider.as_deref(), opts.api_key.as_deref())
        && let Some(name) = provider_api_key_env(provider)
        && !opts.env.contains_key(name)
    {
        command.env(name, key);
    }
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
}

type ChildPipes = (
    Option<ChildStdin>,
    Option<std::process::ChildStdout>,
    Option<std::process::ChildStderr>,
);

fn install_child(entry: &SessionEntry, child: Child) -> Result<(), String> {
    let mut slot = entry
        .process
        .lock()
        .map_err(|_| "omp process state poisoned".to_string())?;
    *slot = Some(child);
    Ok(())
}

fn take_child_pipes(entry: &SessionEntry) -> Result<ChildPipes, String> {
    let mut slot = entry
        .process
        .lock()
        .map_err(|_| "omp process state poisoned".to_string())?;
    let Some(child) = slot.as_mut() else {
        return Err("Omp RPC process was not installed".into());
    };
    Ok((child.stdin.take(), child.stdout.take(), child.stderr.take()))
}

type HandshakeResult = (
    OmpHandshake,
    BufReader<std::process::ChildStdout>,
    Option<ChildStdin>,
);
type HandshakeReceiver = mpsc::Receiver<HandshakeResult>;

fn start_handshake(
    stdout: std::process::ChildStdout,
    stdin: ChildStdin,
    expected_session_id: Option<String>,
    tools: Vec<SeherTool>,
) -> (HandshakeReceiver, Option<thread::JoinHandle<()>>) {
    let (tx, rx) = mpsc::channel();
    let thread = thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut stdin = Some(stdin);
        let result = handshake(
            &mut reader,
            &mut stdin,
            expected_session_id.as_deref(),
            &tools,
        );
        let _ = tx.send((result, reader, stdin));
    });
    (rx, Some(thread))
}

#[expect(
    clippy::too_many_lines,
    reason = "candidate startup owns fallback and handshake state"
)]
fn spawn_candidate(
    opts: &OmpRpcRunnerOptions,
    key: &SessionKey,
    session_dir: &Path,
    resume: Option<&str>,
    entry: &SessionEntry,
) -> Result<SpawnedProcess, String> {
    let candidates = candidate_commands(opts, resume, session_dir);
    let mut last_error = String::from("no Omp RPC candidate succeeded");
    for (program, args) in candidates {
        if opts.cancel.is_cancelled() {
            return Err("omp session cancelled during startup".into());
        }
        let mut command = candidate_command(opts, key, &program, &args);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                last_error = format!("failed to spawn Omp RPC candidate: {error}");
                continue;
            }
        };
        install_child(entry, child)?;
        let (mut stdin, stdout, stderr) = match take_child_pipes(entry) {
            Ok(pipes) => pipes,
            Err(error) => {
                terminate_process(&entry.process);
                last_error = error;
                continue;
            }
        };
        let (Some(stdin), Some(stdout), Some(stderr)) = (stdin.take(), stdout, stderr) else {
            terminate_process(&entry.process);
            last_error = "Omp RPC child pipes were unavailable".into();
            continue;
        };
        let stderr = StderrTail::start(stderr);
        let (rx, mut handshake_thread) = start_handshake(
            stdout,
            stdin,
            resume.map(str::to_string),
            opts.tools.clone(),
        );
        let deadline = Instant::now() + HANDSHAKE_WAIT;
        let received = loop {
            if entry.closing.load(Ordering::Acquire) || Instant::now() >= deadline {
                terminate_process(&entry.process);
                if let Some(thread) = handshake_thread.take() {
                    let _ = thread.join();
                }
                if entry.closing.load(Ordering::Acquire) {
                    let _ = stderr.finish();
                    return Err("Omp RPC session closed during handshake".into());
                }
                break None;
            }
            if opts.cancel.is_cancelled() {
                terminate_process(&entry.process);
                if let Some(thread) = handshake_thread.take() {
                    let _ = thread.join();
                }
                let _ = stderr.finish();
                return Err("omp session cancelled during handshake".into());
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(value) => break Some(value),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break None,
            }
        };
        if opts.cancel.is_cancelled() {
            terminate_process(&entry.process);
            if let Some(thread) = handshake_thread.take() {
                let _ = thread.join();
            }
            let _ = stderr.finish();
            return Err("omp session cancelled during handshake".into());
        }
        let Some((result, reader, stdin)) = received else {
            terminate_process(&entry.process);
            let detail = stderr.finish();
            if opts.cancel.is_cancelled() {
                return Err("omp session cancelled during handshake".into());
            }
            last_error = append_stderr("Omp RPC candidate exited during handshake", &detail, opts);
            continue;
        };
        if let Some(thread) = handshake_thread.take() {
            let _ = thread.join();
        }
        if opts.cancel.is_cancelled() {
            terminate_process(&entry.process);
            let _ = stderr.finish();
            return Err("omp session cancelled during handshake".into());
        }
        match result {
            OmpHandshake::Accepted { session_id } => {
                let Some(stdin) = stdin else {
                    terminate_process(&entry.process);
                    let detail = stderr.finish();
                    last_error =
                        append_stderr("Omp RPC handshake lost the child stdin", &detail, opts);
                    continue;
                };
                return Ok(SpawnedProcess {
                    stdin,
                    stdout: reader.into_inner(),
                    stderr,
                    session_id,
                });
            }
            OmpHandshake::ExecutionError(error) => {
                terminate_process(&entry.process);
                let detail = stderr.finish();
                return Err(append_stderr(&error, &detail, opts));
            }
            OmpHandshake::Next(error) => {
                terminate_process(&entry.process);
                let detail = stderr.finish();
                last_error = append_stderr(&error, &detail, opts);
            }
        }
    }
    if opts.cancel.is_cancelled() {
        return Err("omp session cancelled during startup".into());
    }
    Err(last_error)
}

fn control_once(
    stdin: &Arc<Mutex<Option<ChildStdin>>>,
    stdout: &mut BufReader<std::process::ChildStdout>,
    command: &serde_json::Value,
    key: &mut SessionKey,
    entry: &Arc<SessionEntry>,
    tools: &[SeherTool],
) -> Result<serde_json::Value, String> {
    let kind = command_type(command)?;
    if kind == "switch_session" {
        let session_path = command
            .get("sessionPath")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "Omp RPC switch_session omitted sessionPath".to_string())?;
        let session_path = Path::new(session_path);
        let session_path = if session_path.is_absolute() {
            session_path.to_path_buf()
        } else {
            key.cwd.join(session_path)
        };
        let target_cwd = session_header_cwd(&session_path).ok_or_else(|| {
            "Omp RPC switch_session target has no readable session cwd".to_string()
        })?;
        if target_cwd != key.cwd {
            return Err(format!(
                "Omp RPC switch_session across working directories is unsupported (target {})",
                target_cwd.display()
            ));
        }
    }
    let id = format!("seher-control-{}", uuid::Uuid::new_v4());
    let mut request = command
        .as_object()
        .cloned()
        .ok_or_else(|| "Omp RPC command must be an object".to_string())?;
    request.insert("id".into(), serde_json::Value::String(id.clone()));
    {
        let mut stdin_guard = stdin
            .lock()
            .map_err(|_| "omp stdin state poisoned".to_string())?;
        let Some(stdin) = stdin_guard.as_mut() else {
            return Err("Omp RPC stdin closed".into());
        };
        write_json_line(stdin, &serde_json::Value::Object(request))
            .map_err(|error| error.to_string())?;
    }
    let value = read_control_response(stdin, stdout, &id, entry, tools)?;
    let mut new_session_id = None;
    if matches!(kind, "new_session" | "switch_session")
        && value.get("cancelled").and_then(serde_json::Value::as_bool) != Some(true)
    {
        let state_id = format!("seher-state-{}", uuid::Uuid::new_v4());
        let state = serde_json::json!({"id": state_id, "type": "get_state"});
        let mut stdin_guard = stdin
            .lock()
            .map_err(|_| "omp stdin state poisoned".to_string())?;
        let Some(child_stdin) = stdin_guard.as_mut() else {
            return Err("Omp RPC stdin closed".into());
        };
        write_json_line(child_stdin, &state).map_err(|error| error.to_string())?;
        drop(stdin_guard);
        let state = read_control_response(stdin, stdout, &state_id, entry, tools)?;
        let new_id = state
            .get("sessionId")
            .or_else(|| state.get("session_id"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "Omp RPC get_state response omitted sessionId".to_string())?;
        let destination_cwd = session_state_cwd(&state).unwrap_or_else(|| key.cwd.clone());
        if kind == "switch_session" && destination_cwd != key.cwd {
            entry.closing.store(true, Ordering::Release);
            terminate_process(&entry.process);
            remove_if_same(key, entry);
            remove_entry_identity(entry);
            return Err(format!(
                "Omp RPC switch_session changed working directory to {}, which is not supported",
                destination_cwd.display()
            ));
        }
        if let Err(error) = rekey_session(key, &destination_cwd, new_id, entry) {
            entry.closing.store(true, Ordering::Release);
            terminate_process(&entry.process);
            remove_if_same(key, entry);
            remove_entry_identity(entry);
            return Err(error);
        }
        key.cwd = destination_cwd;
        key.id = new_id.to_string();
        *entry
            .omp_session_id
            .lock()
            .map_err(|_| "omp session id state poisoned".to_string())? = Some(new_id.to_string());
        new_session_id = Some(new_id.to_string());
    }
    if let Some(new_id) = new_session_id {
        match value {
            serde_json::Value::Object(mut object) => {
                object.insert("sessionId".into(), serde_json::Value::String(new_id));
                return Ok(serde_json::Value::Object(object));
            }
            value => return Ok(serde_json::json!({"data": value, "sessionId": new_id})),
        }
    }
    Ok(value)
}

fn read_control_response(
    stdin: &Arc<Mutex<Option<ChildStdin>>>,
    stdout: &mut BufReader<std::process::ChildStdout>,
    id: &str,
    entry: &SessionEntry,
    tools: &[SeherTool],
) -> Result<serde_json::Value, String> {
    let deadline = Instant::now() + CONTROL_RESPONSE_WAIT;
    loop {
        if entry.closing.load(Ordering::Acquire) {
            return Err("Omp RPC session closed".into());
        }
        let line = match read_jsonl_line_until(stdout, None, entry, Some(deadline)) {
            Ok(Some(line)) => line,
            Ok(None) => return Err("Omp RPC process exited while handling command".to_string()),
            Err(error) => {
                entry.closing.store(true, Ordering::Release);
                terminate_process(&entry.process);
                fail_pending_responses(entry, "Omp RPC control response timed out");
                return Err(error);
            }
        };
        let frame =
            parse_jsonl_frame(&line).map_err(|error| format!("invalid Omp RPC JSONL: {error}"))?;
        let frame_type = frame.get("type").and_then(serde_json::Value::as_str);
        route_aux_frame(stdin, entry, tools, &frame)?;
        if frame_type != Some("response")
            || frame.get("id").and_then(serde_json::Value::as_str) != Some(id)
        {
            continue;
        }
        if frame.get("success").and_then(serde_json::Value::as_bool) != Some(true) {
            return Err(frame
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Omp RPC command failed")
                .to_string());
        }
        return Ok(frame.get("data").cloned().unwrap_or(frame));
    }
}

fn rekey_session(
    old: &SessionKey,
    new_cwd: &Path,
    new_id: &str,
    entry: &Arc<SessionEntry>,
) -> Result<(), String> {
    let mut sessions = registry()
        .lock()
        .map_err(|_| "omp registry poisoned".to_string())?;
    if !sessions
        .get(old)
        .is_some_and(|current| Arc::ptr_eq(current, entry))
    {
        return Err("omp session registry identity changed during session control".into());
    }
    let new = SessionKey {
        cwd: new_cwd.to_path_buf(),
        id: new_id.to_string(),
    };
    if &new != old
        && sessions
            .get(&new)
            .is_some_and(|current| !Arc::ptr_eq(current, entry))
    {
        return Err(format!(
            "omp session '{}' already exists under {}",
            new_id,
            new_cwd.display()
        ));
    }
    if &new != old {
        sessions.remove(old);
        sessions.insert(new, Arc::clone(entry));
    }
    Ok(())
}

#[expect(
    clippy::too_many_lines,
    reason = "prompt loop owns the streaming state machine"
)]
fn prompt_once(
    stdin: &Arc<Mutex<Option<ChildStdin>>>,
    stdout: &mut BufReader<std::process::ChildStdout>,
    prompt: &str,
    output: &mpsc::Sender<StreamChunk>,
    entry: &SessionEntry,
    cancel: &CancelToken,
    tools: &[SeherTool],
) -> Result<String, String> {
    if child_exited_entry(entry) {
        return Err("Omp RPC process exited before prompting".into());
    }
    {
        let mut stdin_guard = stdin
            .lock()
            .map_err(|_| "omp stdin state poisoned".to_string())?;
        let Some(stdin) = stdin_guard.as_mut() else {
            return Err("Omp RPC stdin closed".into());
        };
        write_json_line(
            stdin,
            &serde_json::json!({"id":"seher-prompt","type":"prompt","message":prompt}),
        )
        .map_err(|error| error.to_string())?;
    }
    entry.prompt_acknowledged.store(true, Ordering::Release);
    let mut acknowledged = false;
    let mut assistant_error = None;
    loop {
        if cancel.is_cancelled() {
            abort_and_reap(stdin, entry);
            return Err("Omp RPC session cancelled".into());
        }
        let line = match read_jsonl_line_cancel(stdout, cancel, entry) {
            Ok(Some(line)) => line,
            Ok(None) => return Err("Omp RPC process exited while prompting".into()),
            Err(_error) if cancel.is_cancelled() => {
                abort_and_reap(stdin, entry);
                return Err("Omp RPC session cancelled".into());
            }
            Err(error) => return Err(error),
        };
        let frame = parse_jsonl_frame(&line).map_err(|e| format!("invalid Omp RPC JSONL: {e}"))?;
        if cancel.is_cancelled() {
            abort_and_reap(stdin, entry);
            return Err("Omp RPC session cancelled".into());
        }
        match frame.get("type").and_then(serde_json::Value::as_str) {
            Some("response")
                if frame.get("id").and_then(serde_json::Value::as_str) == Some("seher-prompt") =>
            {
                if frame.get("command").and_then(serde_json::Value::as_str) != Some("prompt")
                    || frame
                        .get("success")
                        .and_then(serde_json::Value::as_bool)
                        .is_none()
                {
                    return Err("malformed Omp RPC prompt acknowledgement".into());
                }
                acknowledged = true;
                entry.prompt_acknowledged.store(true, Ordering::Release);
                if frame["success"] == false {
                    entry.prompt_acknowledged.store(false, Ordering::Release);
                    return Err(frame
                        .get("error")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("Omp RPC prompt failed")
                        .to_string());
                }
                // Local-only prompts never invoke the agent; the ack is the
                // completion signal.
                if frame
                    .get("data")
                    .and_then(|data| data.get("agentInvoked"))
                    .and_then(serde_json::Value::as_bool)
                    == Some(false)
                {
                    entry.prompt_active.store(false, Ordering::Release);
                    drain_pending_after_settled(stdin, stdout, entry, tools)?;
                    return Ok(String::new());
                }
            }
            Some("response" | "host_tool_call" | "extension_ui_request") => {
                route_aux_frame(stdin, entry, tools, &frame)?;
            }
            Some("message_update") => {
                let event = frame
                    .get("assistantMessageEvent")
                    .unwrap_or(&serde_json::Value::Null);
                if event.get("type").and_then(serde_json::Value::as_str) == Some("text_delta")
                    && let Some(delta) = event.get("delta").and_then(serde_json::Value::as_str)
                {
                    let _ = output.send(StreamChunk::Delta(delta.to_string()));
                }
                if event.get("type").and_then(serde_json::Value::as_str) == Some("message_end") {
                    assistant_error = message_end_error(event);
                }
            }
            Some("agent_end")
                if frame.get("isTerminal").and_then(serde_json::Value::as_bool) != Some(false) =>
            {
                if !acknowledged {
                    return Err("Omp RPC settled before prompt acknowledgement".into());
                }
                let error = network_error_reason(&frame)
                    .or_else(|| agent_messages_error(&frame))
                    .or_else(|| assistant_error.take());
                // Close prompt control admission before routing final responses.
                entry.prompt_active.store(false, Ordering::Release);
                drain_pending_after_settled(stdin, stdout, entry, tools)?;
                if let Some(error) = error {
                    entry.prompt_acknowledged.store(false, Ordering::Release);
                    return Err(error);
                }
                return Ok(String::new());
            }
            Some("prompt_result")
                if frame.get("id").and_then(serde_json::Value::as_str) == Some("seher-prompt") =>
            {
                if !acknowledged {
                    return Err("Omp RPC settled before prompt acknowledgement".into());
                }
                entry.prompt_active.store(false, Ordering::Release);
                drain_pending_after_settled(stdin, stdout, entry, tools)?;
                if let Some(error) = assistant_error.take() {
                    entry.prompt_acknowledged.store(false, Ordering::Release);
                    return Err(error);
                }
                return Ok(String::new());
            }
            _ => {}
        }
    }
}

/// Answer an `extension_ui_request` with `cancelled`. omp's built-in widgets
/// (e.g. `setWidget`) block the agent loop until the host responds, even with
/// `--no-extensions`; seher has no UI, so every request is cancelled.
fn dismiss_extension_ui(stdin: &Arc<Mutex<Option<ChildStdin>>>, frame: &serde_json::Value) {
    let Some(id) = frame.get("id").and_then(serde_json::Value::as_str) else {
        return;
    };
    if let Ok(mut guard) = stdin.lock()
        && let Some(stdin) = guard.as_mut()
    {
        let _ = write_json_line(
            stdin,
            &serde_json::json!({"type": "extension_ui_response", "id": id, "cancelled": true}),
        );
    }
}

/// Build the `host_tool_result` frame answering a `host_tool_call`.
fn host_tool_result_frame(id: &str, outcome: &Result<String, String>) -> serde_json::Value {
    match outcome {
        Ok(text) => serde_json::json!({
            "type": "host_tool_result",
            "id": id,
            "result": {"content": [{"type": "text", "text": text}]},
        }),
        Err(error) => serde_json::json!({
            "type": "host_tool_result",
            "id": id,
            "result": {"content": [{"type": "text", "text": error}]},
            "isError": true,
        }),
    }
}

/// Serve one `host_tool_call` frame: run the matching [`SeherTool`] handler and
/// write the `host_tool_result` answer. `host_tool_cancel` never applies:
/// handlers are synchronous and non-interruptible.
fn handle_host_tool_call(
    stdin: &Arc<Mutex<Option<ChildStdin>>>,
    tools: &[SeherTool],
    frame: &serde_json::Value,
) -> Result<(), String> {
    let id = frame
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "Omp RPC host_tool_call omitted id".to_string())?;
    let name = frame
        .get("toolName")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let arguments = frame
        .get("arguments")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let outcome = match tools.iter().find(|tool| tool.name == name) {
        Some(tool) => match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (tool.handler)(arguments)
        })) {
            Ok(result) => result,
            Err(_) => Err("Seher tool panicked".to_string()),
        },
        None => Err(format!("unknown host tool: {name}")),
    };
    let result = host_tool_result_frame(id, &outcome);
    let mut guard = stdin
        .lock()
        .map_err(|_| "omp stdin state poisoned".to_string())?;
    let Some(stdin) = guard.as_mut() else {
        return Err("Omp RPC stdin closed".into());
    };
    write_json_line(stdin, &result).map_err(|error| error.to_string())
}
fn route_aux_frame(
    stdin: &Arc<Mutex<Option<ChildStdin>>>,
    entry: &SessionEntry,
    tools: &[SeherTool],
    frame: &serde_json::Value,
) -> Result<(), String> {
    match frame.get("type").and_then(serde_json::Value::as_str) {
        Some("response") => {
            let _ = route_pending_response(entry, frame);
            Ok(())
        }
        Some("host_tool_call") => handle_host_tool_call(stdin, tools, frame),
        Some("extension_ui_request") => {
            dismiss_extension_ui(stdin, frame);
            Ok(())
        }
        _ => Ok(()),
    }
}

fn read_jsonl_line_cancel(
    stdout: &mut BufReader<std::process::ChildStdout>,
    cancel: &CancelToken,
    entry: &SessionEntry,
) -> Result<Option<String>, String> {
    read_jsonl_line_until(stdout, Some(cancel), entry, None)
}

fn read_jsonl_line_until(
    stdout: &mut BufReader<std::process::ChildStdout>,
    cancel: Option<&CancelToken>,
    _entry: &SessionEntry,
    deadline: Option<Instant>,
) -> Result<Option<String>, String> {
    #[cfg(unix)]
    {
        loop {
            if cancel.is_some_and(CancelToken::is_cancelled) {
                return Err("Omp RPC session cancelled".into());
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err("Omp RPC response routing deadline expired".into());
            }
            if !stdout.buffer().is_empty() {
                return read_jsonl_line(stdout)
                    .map_err(|error| format!("invalid Omp RPC JSONL: {error}"));
            }
            let mut pollfd = libc::pollfd {
                fd: stdout.get_ref().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let timeout = deadline.map_or(50, |deadline| {
                deadline
                    .checked_duration_since(Instant::now())
                    .map_or(0, |remaining| remaining.as_millis().min(50) as i32)
            });
            let ready = unsafe { libc::poll(&raw mut pollfd, 1, timeout) };
            if ready < 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            if ready == 0 {
                continue;
            }
            return read_jsonl_line(stdout)
                .map_err(|error| format!("invalid Omp RPC JSONL: {error}"));
        }
    }
    #[cfg(not(unix))]
    {
        if cancel.is_some_and(CancelToken::is_cancelled) {
            return Err("Omp RPC session cancelled".into());
        }
        let stop = Arc::new(AtomicBool::new(false));
        let watcher_stop = Arc::clone(&stop);
        let watcher_cancel = cancel.cloned();
        let process = Arc::clone(&_entry.process);
        let watcher = thread::spawn(move || {
            while !watcher_stop.load(Ordering::Acquire) {
                if watcher_cancel
                    .as_ref()
                    .is_some_and(CancelToken::is_cancelled)
                    || deadline.is_some_and(|deadline| Instant::now() >= deadline)
                {
                    terminate_process(&process);
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        let result =
            read_jsonl_line(stdout).map_err(|error| format!("invalid Omp RPC JSONL: {error}"));
        stop.store(true, Ordering::Release);
        if watcher.is_finished() {
            let _ = watcher.join();
        }
        result
    }
}

fn drain_pending_after_settled(
    stdin: &Arc<Mutex<Option<ChildStdin>>>,
    stdout: &mut BufReader<std::process::ChildStdout>,
    entry: &SessionEntry,
    tools: &[SeherTool],
) -> Result<(), String> {
    let deadline = Instant::now() + CONTROL_RESPONSE_WAIT;
    loop {
        let pending = entry
            .pending
            .lock()
            .map_err(|_| "omp pending response state poisoned".to_string())?
            .is_empty();
        if pending {
            return Ok(());
        }
        let line = read_jsonl_line_until(stdout, None, entry, Some(deadline))?
            .ok_or_else(|| "Omp RPC process exited while routing control response".to_string())?;
        let frame =
            parse_jsonl_frame(&line).map_err(|error| format!("invalid Omp RPC JSONL: {error}"))?;
        route_aux_frame(stdin, entry, tools, &frame)?;
    }
}

fn abort_and_reap(stdin: &Arc<Mutex<Option<ChildStdin>>>, entry: &SessionEntry) {
    if let Ok(mut guard) = stdin.lock()
        && let Some(stdin) = guard.as_mut()
    {
        let _ = write_json_line(
            stdin,
            &serde_json::json!({"id": format!("seher-cancel-{}", uuid::Uuid::new_v4()), "type": "abort"}),
        );
    }
    let deadline = Instant::now() + Duration::from_millis(250);
    loop {
        let exited = entry
            .process
            .lock()
            .ok()
            .and_then(|mut child| {
                child
                    .as_mut()
                    .map(|child| child.try_wait().ok().flatten().is_some())
            })
            .unwrap_or(true);
        if exited || Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    if !child_exited_entry(entry) {
        terminate_process(&entry.process);
    }
}

fn candidate_commands(
    opts: &OmpRpcRunnerOptions,
    resume_id: Option<&str>,
    session_dir: &Path,
) -> Vec<(String, Vec<String>)> {
    // seher injects its own skills appendix via --append-system-prompt and
    // manages tools itself; omp's auto-loaded extensions/skills/rules would
    // duplicate that, and extensions can emit extension_ui_request frames.
    let mut base = vec![
        "--mode".into(),
        "rpc".into(),
        "--session-dir".into(),
        session_dir.display().to_string(),
        "--no-extensions".into(),
        "--no-skills".into(),
        "--no-rules".into(),
    ];
    if let Some(resume_id) = resume_id {
        base.extend(["--resume".into(), resume_id.into()]);
    }
    if let Some(provider) = &opts.provider {
        base.extend(["--provider".into(), provider.clone()]);
    }
    if let Some(model) = &opts.model {
        base.extend(["--model".into(), model.clone()]);
    }
    if let Some(thinking) = &opts.thinking {
        base.extend(["--thinking".into(), thinking.clone()]);
    }
    if let Some(system_prompt) = &opts.system_prompt {
        base.extend(["--system-prompt".into(), system_prompt.clone()]);
    }
    if let Some(append_system_prompt) = &opts.append_system_prompt {
        base.extend([
            "--append-system-prompt".into(),
            append_system_prompt.clone(),
        ]);
    }
    if let Some(bin) = &opts.omp_bin {
        return vec![(bin.display().to_string(), base)];
    }
    vec![
        ("omp".into(), base.clone()),
        (
            "bunx".into(),
            [vec!["--yes".into(), PACKAGE.into()], base.clone()].concat(),
        ),
        (
            "npx".into(),
            [vec!["--yes".into(), PACKAGE.into()], base].concat(),
        ),
    ]
}

enum OmpHandshake {
    Accepted { session_id: String },
    ExecutionError(String),
    Next(String),
}

/// Build the `set_host_tools` frame registering every [`SeherTool`].
fn set_host_tools_frame(tools: &[SeherTool]) -> serde_json::Value {
    serde_json::json!({
        "id": "seher-host-tools",
        "type": "set_host_tools",
        "tools": tools.iter().map(|tool| serde_json::json!({
            "name": tool.name,
            "label": tool.name,
            "description": tool.description,
            "parameters": tool.parameters,
        })).collect::<Vec<_>>(),
    })
}

/// Handshake state machine: wait for `ready`, ask `get_state`, optionally
/// register host tools. `expected` pins the resumed session id.
#[expect(
    clippy::too_many_lines,
    reason = "handshake owns the startup protocol state machine"
)]
fn handshake(
    reader: &mut BufReader<std::process::ChildStdout>,
    stdin: &mut Option<ChildStdin>,
    expected: Option<&str>,
    tools: &[SeherTool],
) -> OmpHandshake {
    let mut session_id: Option<String> = None;
    loop {
        let line = match read_jsonl_line(reader) {
            Ok(Some(line)) => line,
            Ok(None) => {
                return OmpHandshake::Next("Omp RPC candidate exited during handshake".into());
            }
            Err(error) => {
                return OmpHandshake::Next(format!(
                    "invalid Omp RPC JSONL during handshake: {error}"
                ));
            }
        };
        let frame = match parse_jsonl_frame(&line) {
            Ok(frame) => frame,
            Err(error) => {
                return OmpHandshake::Next(format!(
                    "invalid Omp RPC JSONL during handshake: {error}"
                ));
            }
        };
        match frame.get("type").and_then(serde_json::Value::as_str) {
            Some("ready") => {
                let Some(stdin) = stdin.as_mut() else {
                    return OmpHandshake::Next("Omp RPC handshake lost the child stdin".into());
                };
                if let Err(error) = write_json_line(
                    stdin,
                    &serde_json::json!({"id":"seher-handshake","type":"get_state"}),
                ) {
                    return OmpHandshake::Next(format!("Omp RPC handshake write failed: {error}"));
                }
            }
            Some("extension_ui_request") => {
                if let Some(id) = frame.get("id").and_then(serde_json::Value::as_str)
                    && let Some(stdin) = stdin.as_mut()
                {
                    let _ = write_json_line(
                        stdin,
                        &serde_json::json!({"type": "extension_ui_response", "id": id, "cancelled": true}),
                    );
                }
            }
            Some("response")
                if frame.get("id").and_then(serde_json::Value::as_str)
                    == Some("seher-handshake") =>
            {
                if frame.get("success").and_then(serde_json::Value::as_bool) == Some(false) {
                    return OmpHandshake::ExecutionError(
                        frame
                            .get("error")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("Omp RPC get_state failed")
                            .to_string(),
                    );
                }
                let data = frame
                    .get("data")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let id = data.get("sessionId").and_then(serde_json::Value::as_str);
                if frame.get("success").and_then(serde_json::Value::as_bool) != Some(true)
                    || frame.get("command").and_then(serde_json::Value::as_str) != Some("get_state")
                    || id.is_none()
                {
                    return OmpHandshake::Next("Omp RPC get_state handshake was invalid".into());
                }
                let id = id.unwrap_or_default().to_string();
                if expected.is_some_and(|expected| expected != id) {
                    return OmpHandshake::Next("Omp RPC resume opened the wrong session".into());
                }
                session_id = Some(id);
                if tools.is_empty() {
                    return OmpHandshake::Accepted {
                        session_id: session_id.unwrap_or_default(),
                    };
                }
                let Some(stdin) = stdin.as_mut() else {
                    return OmpHandshake::Next("Omp RPC handshake lost the child stdin".into());
                };
                if let Err(error) = write_json_line(stdin, &set_host_tools_frame(tools)) {
                    return OmpHandshake::Next(format!(
                        "Omp RPC set_host_tools write failed: {error}"
                    ));
                }
            }
            Some("response")
                if frame.get("id").and_then(serde_json::Value::as_str)
                    == Some("seher-host-tools") =>
            {
                if frame.get("success").and_then(serde_json::Value::as_bool) != Some(true) {
                    return OmpHandshake::ExecutionError(
                        frame
                            .get("error")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("Omp RPC set_host_tools failed")
                            .to_string(),
                    );
                }
                let mut registered = frame
                    .get("data")
                    .and_then(|data| data.get("toolNames"))
                    .and_then(serde_json::Value::as_array)
                    .map(|names| {
                        names
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(str::to_string)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let mut requested = tools
                    .iter()
                    .map(|tool| tool.name.clone())
                    .collect::<Vec<_>>();
                registered.sort_unstable();
                requested.sort_unstable();
                if registered != requested {
                    return OmpHandshake::ExecutionError(format!(
                        "Omp RPC set_host_tools returned tool names {registered:?}, requested {requested:?}"
                    ));
                }
                return OmpHandshake::Accepted {
                    session_id: session_id.unwrap_or_default(),
                };
            }
            Some(_) => {}
            None => return OmpHandshake::Next("Omp RPC handshake frame had no type".into()),
        }
    }
}

fn is_omp_process_failure(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("process exited")
        || lower.contains("broken pipe")
        || lower.contains("stdin closed")
        || lower.contains("invalid omp rpc jsonl")
        || lower.contains("malformed omp rpc")
}

fn omp_session_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local/share")
        })
        .join("seher")
        .join("omp-sessions")
}

fn remove_if_same(key: &SessionKey, entry: &Arc<SessionEntry>) {
    let Ok(mut sessions) = registry().lock() else {
        return;
    };
    if sessions
        .get(key)
        .is_some_and(|current| Arc::ptr_eq(current, entry))
    {
        sessions.remove(key);
    }
}

fn remove_entry_identity(entry: &Arc<SessionEntry>) {
    let Ok(mut sessions) = registry().lock() else {
        return;
    };
    sessions.retain(|_, current| !Arc::ptr_eq(current, entry));
}

fn child_exited_entry(entry: &SessionEntry) -> bool {
    entry
        .process
        .lock()
        .ok()
        .and_then(|mut child| {
            child
                .as_mut()
                .map(|child| child.try_wait().ok().flatten().is_some())
        })
        .unwrap_or(false)
}

fn validate_tool_names(tools: &[SeherTool]) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for tool in tools {
        if tool.name.is_empty() {
            return Err("custom tool name must not be empty".to_string());
        }
        if !seen.insert(tool.name.as_str()) {
            return Err(format!("duplicate custom tool name '{}'", tool.name));
        }
    }
    Ok(())
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests may panic on unexpected fixtures")]
mod tests {
    use super::*;

    #[test]
    fn candidate_commands_assemble_base_resume_and_optional_flags() {
        let opts = OmpRpcRunnerOptions {
            provider: Some("openai-codex".into()),
            model: Some("gpt-5.6".into()),
            thinking: Some("high".into()),
            system_prompt: Some("sys".into()),
            append_system_prompt: Some("skills".into()),
            ..Default::default()
        };
        let commands = candidate_commands(&opts, Some("sid-1"), Path::new("/tmp/sessions"));
        assert_eq!(commands.len(), 3);
        assert_eq!(commands[0].0, "omp");
        assert_eq!(commands[1].0, "bunx");
        assert_eq!(commands[2].0, "npx");
        assert!(
            commands[1]
                .1
                .starts_with(&["--yes".to_string(), PACKAGE.to_string()])
        );
        let args = &commands[0].1;
        assert!(args.windows(2).any(|pair| pair == ["--mode", "rpc"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--session-dir", "/tmp/sessions"])
        );
        assert!(args.windows(2).any(|pair| pair == ["--resume", "sid-1"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--provider", "openai-codex"])
        );
        assert!(args.windows(2).any(|pair| pair == ["--model", "gpt-5.6"]));
        assert!(args.windows(2).any(|pair| pair == ["--thinking", "high"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--system-prompt", "sys"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--append-system-prompt", "skills"])
        );
        for flag in ["--no-extensions", "--no-skills", "--no-rules"] {
            assert!(args.iter().any(|arg| arg == flag));
        }

        let fresh = candidate_commands(&OmpRpcRunnerOptions::default(), None, Path::new("/tmp"));
        assert!(!fresh[0].1.iter().any(|arg| arg == "--resume"));

        let over = OmpRpcRunnerOptions {
            omp_bin: Some(PathBuf::from("fake-omp")),
            api_key: Some("secret".into()),
            ..Default::default()
        };
        let commands = candidate_commands(&over, None, Path::new("/tmp"));
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].0, "fake-omp");
        assert!(!commands[0].1.iter().any(|arg| arg == "secret"));
    }

    #[test]
    fn terminal_agent_end_classification() {
        let terminal = serde_json::json!({"type":"agent_end","messages":[],"isTerminal":true});
        let absent = serde_json::json!({"type":"agent_end","messages":[]});
        let continuing = serde_json::json!({"type":"agent_end","messages":[],"isTerminal":false});
        let is_terminal = |frame: &serde_json::Value| {
            frame.get("type").and_then(serde_json::Value::as_str) == Some("agent_end")
                && frame.get("isTerminal").and_then(serde_json::Value::as_bool) != Some(false)
        };
        assert!(is_terminal(&terminal));
        assert!(is_terminal(&absent));
        assert!(!is_terminal(&continuing));
    }

    #[test]
    fn agent_end_extracts_assistant_error_from_messages_tail() {
        let frame = serde_json::json!({
            "type": "agent_end",
            "isTerminal": true,
            "messages": [
                {"role": "user"},
                {"role": "assistant", "stopReason": "error", "errorMessage": "HTTP 429 quota exceeded"},
            ],
        });
        assert_eq!(
            agent_messages_error(&frame).as_deref(),
            Some("HTTP 429 quota exceeded")
        );

        let network_frame = serde_json::json!({
            "type": "agent_end",
            "isTerminal": true,
            "messages": [
                {"role": "assistant", "stopReason": "network_error"},
            ],
        });
        assert_eq!(
            agent_messages_error(&network_frame).as_deref(),
            Some(crate::sdk::errors::NETWORK_ERROR_REASON)
        );
    }

    #[test]
    fn local_only_prompt_completion_frames() {
        let ack = serde_json::json!({
            "id": "seher-prompt", "type": "response", "command": "prompt",
            "success": true, "data": {"agentInvoked": false},
        });
        assert_eq!(
            ack.get("data")
                .and_then(|data| data.get("agentInvoked"))
                .and_then(serde_json::Value::as_bool),
            Some(false)
        );
        let result = serde_json::json!({"type": "prompt_result", "id": "seher-prompt", "agentInvoked": false});
        assert_eq!(
            result.get("type").and_then(serde_json::Value::as_str),
            Some("prompt_result")
        );
        assert_eq!(
            result.get("id").and_then(serde_json::Value::as_str),
            Some("seher-prompt")
        );
    }

    #[test]
    fn host_tool_result_frame_shapes() {
        let ok = host_tool_result_frame("call-1", &Ok("done".to_string()));
        assert_eq!(ok["type"], "host_tool_result");
        assert_eq!(ok["id"], "call-1");
        assert_eq!(ok["result"]["content"][0]["text"], "done");
        assert!(ok.get("isError").is_none());

        let err = host_tool_result_frame("call-2", &Err("boom".to_string()));
        assert_eq!(err["result"]["content"][0]["text"], "boom");
        assert_eq!(err["isError"], true);

        let unknown = host_tool_result_frame("call-3", &Err("unknown host tool: nope".to_string()));
        assert_eq!(
            unknown["result"]["content"][0]["text"],
            "unknown host tool: nope"
        );
        assert_eq!(unknown["isError"], true);
    }

    #[test]
    fn set_host_tools_frame_lists_all_tools() {
        let tool = SeherTool::new(
            "echo",
            "echo back",
            serde_json::json!({"type":"object","properties":{"message":{"type":"string"}},"required":["message"]}),
            Arc::new(|input| Ok(input.to_string())),
        );
        let frame = set_host_tools_frame(&[tool]);
        assert_eq!(frame["id"], "seher-host-tools");
        assert_eq!(frame["type"], "set_host_tools");
        assert_eq!(frame["tools"][0]["name"], "echo");
        assert_eq!(frame["tools"][0]["label"], "echo");
        assert_eq!(frame["tools"][0]["description"], "echo back");
        assert_eq!(frame["tools"][0]["parameters"]["required"][0], "message");
    }

    #[test]
    fn options_fingerprint_tracks_handler_identity_and_fields() {
        let parameters = serde_json::json!({"type": "object"});
        let shared: crate::sdk::tool::ToolHandler = Arc::new(|_| Ok(String::new()));
        let base = OmpRpcRunnerOptions {
            model: Some("m".into()),
            tools: vec![SeherTool::new(
                "tool",
                "tool",
                parameters.clone(),
                Arc::clone(&shared),
            )],
            ..Default::default()
        };
        let same = OmpRpcRunnerOptions {
            model: Some("m".into()),
            tools: vec![SeherTool::new("tool", "tool", parameters.clone(), shared)],
            ..Default::default()
        };
        let different_handler = OmpRpcRunnerOptions {
            model: Some("m".into()),
            tools: vec![SeherTool::new(
                "tool",
                "tool",
                parameters,
                Arc::new(|_| Ok(String::new())),
            )],
            ..Default::default()
        };
        let different_model = OmpRpcRunnerOptions {
            model: Some("other".into()),
            ..Default::default()
        };
        assert_eq!(options_fingerprint(&base), options_fingerprint(&same));
        assert_ne!(
            options_fingerprint(&base),
            options_fingerprint(&different_handler)
        );
        assert_ne!(
            options_fingerprint(&base),
            options_fingerprint(&different_model)
        );
    }

    #[test]
    fn tool_name_validation_rejects_duplicates_and_empty() {
        let tool = |name: &str| {
            SeherTool::new(
                name,
                "desc",
                serde_json::json!({"type":"object"}),
                Arc::new(|_| Ok(String::new())),
            )
        };
        assert!(validate_tool_names(&[tool("a"), tool("b")]).is_ok());
        assert!(validate_tool_names(&[tool("a"), tool("a")]).is_err());
        assert!(validate_tool_names(&[tool("")]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn fake_omp_process_streams_and_reuses_session() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("fake-omp");
        let count = dir.path().join("spawns");
        std::fs::write(
            &script,
            r#"#!/bin/sh
printf '%s\n' spawn >> "$OMP_COUNT"
sid=omp-test-session
printf '%s\n' '{"type":"ready","protocolVersion":1}'
while IFS= read -r line; do
  case "$line" in
    *get_state*) printf '{"id":"seher-handshake","type":"response","command":"get_state","success":true,"data":{"sessionId":"%s"}}\n' "$sid" ;;
    *set_host_tools*) printf '%s\n' '{"id":"seher-host-tools","type":"response","command":"set_host_tools","success":true,"data":{"toolNames":["echo"]}}' ;;
    *prompt*) printf '%s\n' '{"id":"seher-prompt","type":"response","command":"prompt","success":true}' '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"ok"}}' '{"type":"agent_end","isTerminal":true,"messages":[]}' ;;
    *abort*) exit 0 ;;
  esac
done
"#,
        )
        .expect("script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        let echo = SeherTool::new(
            "echo",
            "echo",
            serde_json::json!({"type":"object"}),
            Arc::new(|input| Ok(input.to_string())),
        );
        let runner = OmpRpcRunner::new(OmpRpcRunnerOptions {
            omp_bin: Some(script),
            working_directory: Some(dir.path().to_path_buf()),
            env: [("OMP_COUNT".into(), count.display().to_string())].into(),
            tools: vec![echo],
            ..Default::default()
        });
        let first = runner.run("one".into(), None).expect("first prompt");
        assert_eq!(first.text, "ok");
        let second = runner
            .run("two".into(), Some(first.session_id.clone()))
            .expect("resumed prompt");
        assert_eq!(second.session_id, first.session_id);
        assert_eq!(
            std::fs::read_to_string(count)
                .expect("spawn count")
                .lines()
                .count(),
            1
        );
        assert!(runner.close_omp_session(&first.session_id));
        assert!(!runner.close_omp_session(&first.session_id));
    }

    #[test]
    fn limit_classification_matches_pi_vocabulary() {
        assert!(matches!(
            classified_chunk("quota exceeded", "omp"),
            StreamChunk::Limit(_)
        ));
        assert!(matches!(
            classified_chunk("HTTP 500", "omp"),
            StreamChunk::Error(_)
        ));
        assert!(matches!(
            classified_chunk("Omp RPC process exited while prompting", "omp"),
            StreamChunk::Error(_)
        ));
        assert!(is_omp_process_failure(
            "Omp RPC process exited while prompting"
        ));
        assert!(is_omp_process_failure("Omp RPC stdin closed"));
        assert!(!is_omp_process_failure("HTTP 429 quota exceeded"));
    }
}
