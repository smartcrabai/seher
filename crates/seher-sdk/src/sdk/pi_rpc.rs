//! Subprocess backend for the TypeScript Pi RPC protocol.
//!
//! The runner keeps one RPC process per canonical `(working_directory, session_id)`.
//! A session is reserved before spawning so concurrent prompts cannot replay or race.

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use crate::sdk::cancel::CancelToken;
use crate::sdk::errors::{
    LimitError, NETWORK_ERROR_REASON, NON_RETRYABLE_PREFIX, RunError, is_non_retryable_error,
};
use crate::sdk::pi_runner::{PiRunOutput, StreamChunk};
use crate::sdk::tool::SeherTool;
#[cfg(unix)]
use std::os::fd::AsRawFd;

const EXTENSION_TEMPLATE: &str = include_str!("pi_rpc_extension.ts");
const PACKAGE: &str = "@earendil-works/pi-coding-agent";
const CONTROL_RESPONSE_WAIT: Duration = Duration::from_secs(5);
const CLOSE_WAIT: Duration = Duration::from_millis(500);
// Package managers can spend tens of seconds downloading a cold candidate.
// This remains bounded and cancellation-aware, while close keeps its short deadline.
const HANDSHAKE_WAIT: Duration = Duration::from_secs(30);
// Pi and OMP agent_end events include the complete message history, so frame size grows with the session.
// A 1.25 MiB real-world history exceeded the former 1 MiB limit and caused a non-retryable failure.
// Keep a finite ceiling to prevent unbounded output from exhausting memory.
// ponytail: raise the fixed ceiling if observed session histories outgrow 64 MiB.
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 16 * 1024;
const MAX_BRIDGE_CONNECTIONS: usize = 32;
const BRIDGE_READ_WAIT: Duration = Duration::from_millis(250);

type Registry = HashMap<SessionKey, Arc<SessionEntry>>;
static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();

fn registry() -> &'static Mutex<Registry> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
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
pub struct PiRpcRunnerOptions {
    pub cancel: CancelToken,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub thinking: Option<String>,
    pub system_prompt: Option<String>,
    /// Additional prompt text appended to Pi's system prompt.
    pub append_system_prompt: Option<String>,
    pub working_directory: Option<PathBuf>,
    pub env: indexmap::IndexMap<String, String>,
    pub tools: Vec<SeherTool>,
    /// Override the first candidate executable. Intended for embedders and tests.
    pub pi_bin: Option<PathBuf>,
}

impl std::fmt::Debug for PiRpcRunnerOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PiRpcRunnerOptions")
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
            .field("pi_bin", &self.pi_bin)
            .finish()
    }
}

pub struct PiRpcRunner {
    opts: PiRpcRunnerOptions,
}

impl PiRpcRunner {
    #[must_use]
    pub fn new(opts: PiRpcRunnerOptions) -> Self {
        Self { opts }
    }

    /// Stream one prompt. `resume` is the session id to continue, or `None` for a new id.
    #[must_use]
    pub fn stream(&self, prompt: String, resume: Option<String>) -> mpsc::Receiver<StreamChunk> {
        let (output_tx, output_rx) = mpsc::channel();
        let opts = self.opts.clone();
        thread::spawn(move || stream_prompt(&opts, &prompt, resume.as_deref(), &output_tx));
        output_rx
    }
    /// Drain a stream into the shared Pi output/error contract.
    /// # Errors
    ///
    /// Returns [`RunError`] when the Pi process or its RPC stream fails.
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
                        message: "pi rpc runner channel closed".into(),
                        partial: text,
                    });
                }
            }
        }
    }

    /// Stop the session owned by this runner, if present.
    #[must_use]
    pub fn close_pi_session(&self, id: &str) -> bool {
        close_pi_session(id, self.opts.working_directory.as_deref())
    }

    /// Send a confirmed Pi RPC session-control command.
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
    pub fn cancel_pi_session(&self, session_id: &str) -> Result<serde_json::Value, String> {
        if close_pi_session(session_id, self.opts.working_directory.as_deref()) {
            Ok(serde_json::json!({"aborted": true}))
        } else {
            Err(format!("pi session '{session_id}' is not running"))
        }
    }
}

/// Send a confirmed Pi RPC session-control command to an existing session.
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
        .map_err(|_| "pi registry poisoned".to_string())?
        .get(&key)
        .cloned()
        .ok_or_else(|| format!("pi session '{session_id}' is not running"))?;
    if entry.closing.load(Ordering::Acquire) {
        return Err(format!("pi session '{session_id}' is closing"));
    }
    if entry.busy.load(Ordering::Acquire) {
        if entry.prompt_active.load(Ordering::Acquire)
            && matches!(command_type, "steer" | "follow_up" | "abort")
        {
            return send_busy_control(&entry, &command);
        }
        return Err(format!(
            "pi session '{session_id}' is busy; '{command_type}' is only safe while idle"
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
            "pi session '{session_id}' is busy; '{command_type}' is only safe while idle"
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
        return Err("pi session worker stopped".to_string());
    }
    response_rx
        .recv()
        .map_err(|_| "pi session worker stopped".to_string())?
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
                    | "fork"
                    | "clone"
            )
        })
        .ok_or_else(|| "unsupported Pi RPC session-control command".to_string())
}

fn send_busy_control(
    entry: &SessionEntry,
    command: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let id = format!("seher-control-{}", uuid::Uuid::new_v4());
    let mut request = command
        .as_object()
        .cloned()
        .ok_or_else(|| "Pi RPC command must be an object".to_string())?;
    request.insert("id".into(), serde_json::Value::String(id.clone()));
    let (response_tx, response_rx) = mpsc::channel();
    entry
        .pending
        .lock()
        .map_err(|_| "pi pending response state poisoned".to_string())?
        .insert(id.clone(), response_tx);
    let write_result = match entry
        .stdin
        .lock()
        .map_err(|_| "pi stdin state poisoned".to_string())
    {
        Ok(mut stdin) => match stdin.as_mut() {
            Some(stdin) => write_json_line(stdin, &serde_json::Value::Object(request))
                .map_err(|error| error.to_string()),
            None => Err("Pi RPC stdin closed".to_string()),
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
            Err("Pi RPC control response timed out".to_string())
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let _ = entry.pending.lock().map(|mut pending| pending.remove(&id));
            Err("pi session worker stopped".to_string())
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
            .unwrap_or("Pi RPC command failed")
            .to_string())
    };
    let _ = waiter.send(result);
    true
}

/// Stop a Pi RPC session, if it is registered.
#[must_use]
pub fn close_pi_session(id: &str, cwd: Option<&Path>) -> bool {
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
        fail_pending_responses(entry, "pi session closed");
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
    fail_pending_responses(entry, "pi session closed");
    remove_if_same(key, entry);
    remove_entry_identity(entry);
}

/// Stop all TS Pi RPC sessions.
pub fn close_all_pi_sessions() {
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

fn stream_prompt(
    opts: &PiRpcRunnerOptions,
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
            "pi session cancelled before worker startup",
            opts.provider.as_deref().unwrap_or("pi"),
        ));
        return;
    }
    let id = resume.map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_string);
    let key = SessionKey {
        cwd: canonical_cwd(opts.working_directory.as_deref()),
        id: id.clone(),
    };
    let entry = match reserve_session(&key, opts) {
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
            "pi session cancelled during handshake".to_string()
        } else {
            entry
                .startup_error
                .lock()
                .ok()
                .and_then(|error| error.clone())
                .unwrap_or_else(|| "pi session worker stopped before startup".to_string())
        };
        remove_if_same(&key, &entry);
        let _ = output.send(classified_chunk(
            &message,
            opts.provider.as_deref().unwrap_or("pi"),
        ));
        return;
    }
    if entry.closing.load(Ordering::Acquire) {
        let _ = output.send(StreamChunk::Error(format!(
            "pi session '{}' is closing",
            key.id
        )));
        return;
    }
    if entry.busy.swap(true, Ordering::AcqRel) {
        let _ = output.send(StreamChunk::Error(format!(
            "pi session '{}' is busy",
            key.id
        )));
        return;
    }
    entry.prompt_active.store(true, Ordering::Release);
    let _ = output.send(StreamChunk::Session(id));
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
            "pi session cancelled before worker startup"
        } else {
            "pi session worker stopped"
        };
        let _ = output.send(classified_chunk(
            message,
            opts.provider.as_deref().unwrap_or("pi"),
        ));
    }
}

fn reserve_session(
    key: &SessionKey,
    opts: &PiRpcRunnerOptions,
) -> Result<Arc<SessionEntry>, String> {
    let fingerprint = options_fingerprint(opts);
    let mut sessions = registry()
        .lock()
        .map_err(|_| "pi registry poisoned".to_string())?;
    if let Some(entry) = sessions.get(key) {
        if entry.closing.load(Ordering::Acquire) {
            return Err(format!("pi session '{}' is closing", key.id));
        }
        if entry.fingerprint != fingerprint {
            return Err(format!(
                "pi session '{}' was started with different provider/model/thinking/credentials/environment/prompt/tools",
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
    });
    sessions.insert(key.clone(), Arc::clone(&entry));
    let key = key.clone();
    let opts = opts.clone();
    let worker_entry = Arc::clone(&entry);
    thread::spawn(move || worker_loop(key, &opts, &rx, &worker_entry));
    Ok(entry)
}

fn options_fingerprint(opts: &PiRpcRunnerOptions) -> String {
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
    opts: &PiRpcRunnerOptions,
    rx: &mpsc::Receiver<WorkerCommand>,
    entry: &Arc<SessionEntry>,
) {
    let cancelled_before_start = opts.cancel.is_cancelled();
    let result = run_worker(&mut key, opts, rx, entry);
    let queued_error = result
        .err()
        .or_else(|| {
            cancelled_before_start.then(|| "pi session cancelled before worker startup".to_string())
        })
        .or_else(|| {
            entry
                .closing
                .load(Ordering::Acquire)
                .then(|| "pi session is closing".to_string())
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
                        opts.provider.as_deref().unwrap_or("pi"),
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
    opts: &PiRpcRunnerOptions,
    rx: &mpsc::Receiver<WorkerCommand>,
    entry: &Arc<SessionEntry>,
) -> Result<(), String> {
    if entry.closing.load(Ordering::Acquire) {
        return Ok(());
    }
    if opts.cancel.is_cancelled() {
        return Err("pi session cancelled before worker startup".into());
    }
    let session_dir = ts_session_dir();
    std::fs::create_dir_all(&session_dir)
        .map_err(|e| format!("failed to create Pi session directory: {e}"))?;
    let bridge = Bridge::new(&opts.tools)?;
    let process = spawn_candidate(opts, key, &session_dir, bridge.as_ref(), entry)?;
    *entry
        .stdin
        .lock()
        .map_err(|_| "pi stdin state poisoned".to_string())? = Some(process.stdin);
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
                    return Err("Pi RPC process exited while idle".into());
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if entry.closing.load(Ordering::Acquire) {
            match command {
                WorkerCommand::Control { response, .. } => {
                    entry.busy.store(false, Ordering::Release);
                    let _ = response.send(Err("pi session is closing".to_string()));
                }
                WorkerCommand::Prompt { output, .. } => {
                    let _ = output.send(classified_chunk(
                        "pi session is closing",
                        opts.provider.as_deref().unwrap_or("pi"),
                    ));
                }
            }
            break;
        }
        match command {
            WorkerCommand::Control { command, response } => {
                let result = control_once(&entry.stdin, &mut stdout, &command, key, entry);
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
                let prompt_result =
                    prompt_once(&entry.stdin, &mut stdout, &prompt, &output, entry, &cancel);
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
                        // Keep busy set until process, stderr, bridge, and registry teardown finish.
                        terminate_process(&entry.process);
                        let detail = stderr.take().map(StderrTail::finish).unwrap_or_default();
                        let source = if acknowledged && is_pi_process_failure(&error) {
                            format!("{NON_RETRYABLE_PREFIX}{error}")
                        } else {
                            error.clone()
                        };
                        let message = append_stderr(
                            &source,
                            &detail,
                            opts,
                            bridge.as_ref().map(|bridge| bridge.token.as_str()),
                        );
                        let _ = output.send(classified_chunk_with_source(
                            &source,
                            &message,
                            opts.provider.as_deref().unwrap_or("pi"),
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
}

pub(crate) struct StderrTail {
    bytes: Arc<Mutex<VecDeque<u8>>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl StderrTail {
    pub(crate) fn start(stderr: impl std::io::Read + Send + 'static) -> Self {
        let bytes = Arc::new(Mutex::new(VecDeque::with_capacity(MAX_STDERR_BYTES)));
        let target = Arc::clone(&bytes);
        let thread = thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut chunk = [0_u8; 4096];
            loop {
                match std::io::Read::read(&mut reader, &mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if let Ok(mut tail) = target.lock() {
                            for byte in &chunk[..n] {
                                if tail.len() == MAX_STDERR_BYTES {
                                    tail.pop_front();
                                }
                                tail.push_back(*byte);
                            }
                        }
                    }
                }
            }
        });
        Self {
            bytes,
            thread: Some(thread),
        }
    }

    pub(crate) fn finish(mut self) -> String {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let bytes = self
            .bytes
            .lock()
            .map(|tail| tail.iter().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        String::from_utf8_lossy(&bytes).trim().to_string()
    }
}

pub(crate) fn resolve_candidate_program(
    program: &str,
    env: &indexmap::IndexMap<String, String>,
) -> PathBuf {
    let path = Path::new(program);
    if path.components().count() > 1 {
        return path.to_path_buf();
    }
    let search_path = env
        .get("PATH")
        .map(String::as_str)
        .map(str::to_owned)
        .or_else(|| std::env::var("PATH").ok());
    if let Some(search_path) = search_path {
        for directory in std::env::split_paths(&search_path) {
            let base = if directory.as_os_str().is_empty() {
                path.to_path_buf()
            } else {
                directory.join(path)
            };
            #[cfg(windows)]
            let mut candidates = Vec::new();
            #[cfg(not(windows))]
            let mut candidates = Vec::new();
            #[cfg(windows)]
            if base.extension().is_none() {
                let extensions = env
                    .get("PATHEXT")
                    .cloned()
                    .or_else(|| std::env::var("PATHEXT").ok())
                    .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string());
                candidates.extend(extensions.split(';').filter_map(|ext| {
                    let ext = ext.trim();
                    (!ext.is_empty()).then_some(PathBuf::from(format!("{base}{}", ext)))
                }));
            }
            candidates.push(base.clone());
            if let Some(candidate) = candidates.into_iter().find(|candidate| candidate.is_file()) {
                return if candidate.is_absolute() {
                    candidate
                } else {
                    std::fs::canonicalize(&candidate).unwrap_or(candidate)
                };
            }
        }
    }
    path.to_path_buf()
}

#[cfg(unix)]
pub(crate) fn runtime_path_for_candidate(candidate: &Path) -> Option<OsString> {
    let source = std::fs::read_to_string(candidate).ok()?;
    let first_line = source.lines().next()?.trim();
    let shebang = first_line.strip_prefix("#!")?.trim();
    let mut words = shebang.split_whitespace();
    let interpreter = words.next()?;
    let interpreter = if Path::new(interpreter).file_name()? == "env" {
        let mut word = words.next()?;
        if word == "-S" {
            word = words.next()?;
        }
        word
    } else {
        return None;
    };
    let parent_path = std::env::var_os("PATH")?;
    let env: indexmap::IndexMap<String, String> = [(
        String::from("PATH"),
        parent_path.to_string_lossy().into_owned(),
    )]
    .into();
    let resolved = resolve_candidate_program(interpreter, &env);
    resolved
        .parent()
        .map(|parent| parent.as_os_str().to_os_string())
}

pub(crate) fn provider_api_key_env(provider: &str) -> Option<&'static str> {
    match provider.rsplit('/').next()?.to_ascii_lowercase().as_str() {
        "anthropic" | "claude" => Some("ANTHROPIC_API_KEY"),
        "openai" | "codex" | "openai-codex" => Some("OPENAI_API_KEY"),
        "google" | "gemini" | "google-gemini" => Some("GOOGLE_API_KEY"),
        "mistral" => Some("MISTRAL_API_KEY"),
        "cohere" => Some("COHERE_API_KEY"),
        "groq" => Some("GROQ_API_KEY"),
        "xai" | "grok" => Some("XAI_API_KEY"),
        "deepseek" => Some("DEEPSEEK_API_KEY"),
        "together" => Some("TOGETHER_API_KEY"),
        "perplexity" => Some("PERPLEXITY_API_KEY"),
        "cerebras" => Some("CEREBRAS_API_KEY"),
        "fireworks" => Some("FIREWORKS_API_KEY"),
        "openrouter" => Some("OPENROUTER_API_KEY"),
        "huggingface" | "hf" => Some("HUGGINGFACE_API_KEY"),
        "ai21" => Some("AI21_API_KEY"),
        "nvidia" => Some("NVIDIA_API_KEY"),
        "moonshot" | "kimi" => Some("MOONSHOT_API_KEY"),
        "minimax" => Some("MINIMAX_API_KEY"),
        "dashscope" | "qwen" => Some("DASHSCOPE_API_KEY"),
        _ => None,
    }
}

pub(crate) fn merged_child_path(configured: Option<&str>, runtime: Option<OsString>) -> OsString {
    let mut paths = configured
        .map(std::env::split_paths)
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    if let Some(runtime) = runtime {
        paths.push(PathBuf::from(runtime));
    }
    #[cfg(unix)]
    paths.extend(
        [
            "/usr/local/bin",
            "/opt/homebrew/bin",
            "/usr/bin",
            "/bin",
            "/usr/sbin",
            "/sbin",
        ]
        .into_iter()
        .map(PathBuf::from),
    );
    paths.dedup();
    std::env::join_paths(paths).unwrap_or_default()
}

fn candidate_command(
    opts: &PiRpcRunnerOptions,
    key: &SessionKey,
    bridge: Option<&Bridge>,
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
    // The child inherits the complete parent environment; explicit overlays
    // below win. This intentionally trades environment isolation for
    // compatibility: ambient secrets reach the child and its descendants
    // (stderr surfaced to callers is redacted accordingly), and inherited
    // interpreter-influencing variables such as NODE_OPTIONS with relative
    // paths can break child startup.
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
    if let Some(bridge) = bridge {
        bridge.configure_command(&mut command);
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
        .map_err(|_| "pi process state poisoned".to_string())?;
    *slot = Some(child);
    Ok(())
}

fn take_child_pipes(entry: &SessionEntry) -> Result<ChildPipes, String> {
    let mut slot = entry
        .process
        .lock()
        .map_err(|_| "pi process state poisoned".to_string())?;
    let Some(child) = slot.as_mut() else {
        return Err("Pi RPC process was not installed".into());
    };
    Ok((child.stdin.take(), child.stdout.take(), child.stderr.take()))
}

type HandshakeResult = (Handshake, BufReader<std::process::ChildStdout>);
type HandshakeReceiver = mpsc::Receiver<HandshakeResult>;

fn start_handshake(
    stdout: std::process::ChildStdout,
    expected_session_id: String,
) -> (HandshakeReceiver, Option<thread::JoinHandle<()>>) {
    let (tx, rx) = mpsc::channel();
    let thread = thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let result = handshake(&mut reader, &expected_session_id);
        let _ = tx.send((result, reader));
    });
    (rx, Some(thread))
}

#[expect(
    clippy::too_many_lines,
    reason = "candidate startup owns fallback and handshake state"
)]
fn spawn_candidate(
    opts: &PiRpcRunnerOptions,
    key: &SessionKey,
    session_dir: &Path,
    bridge: Option<&Bridge>,
    entry: &SessionEntry,
) -> Result<SpawnedProcess, String> {
    let candidates = candidate_commands(opts, &key.id, session_dir);
    let mut last_error = String::from("no Pi RPC candidate succeeded");
    for (program, args) in candidates {
        if opts.cancel.is_cancelled() {
            return Err("pi session cancelled during startup".into());
        }
        let mut command = candidate_command(opts, key, bridge, &program, &args);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                last_error = format!("failed to spawn Pi RPC candidate: {error}");
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
        let (Some(mut stdin), Some(stdout), Some(stderr)) = (stdin.take(), stdout, stderr) else {
            terminate_process(&entry.process);
            last_error = "Pi RPC child pipes were unavailable".into();
            continue;
        };
        let stderr = StderrTail::start(stderr);
        if let Err(error) = write_json_line(
            &mut stdin,
            &serde_json::json!({"id":"seher-handshake","type":"get_state"}),
        ) {
            terminate_process(&entry.process);
            let detail = stderr.finish();
            last_error = append_stderr(
                &error.to_string(),
                &detail,
                opts,
                bridge.map(|bridge| bridge.token.as_str()),
            );
            continue;
        }
        let (rx, mut handshake_thread) = start_handshake(stdout, key.id.clone());
        let deadline = Instant::now() + HANDSHAKE_WAIT;
        let received = loop {
            if entry.closing.load(Ordering::Acquire) || Instant::now() >= deadline {
                terminate_process(&entry.process);
                if let Some(thread) = handshake_thread.take() {
                    let _ = thread.join();
                }
                if entry.closing.load(Ordering::Acquire) {
                    let _ = stderr.finish();
                    return Err("Pi RPC session closed during handshake".into());
                }
                break None;
            }
            if opts.cancel.is_cancelled() {
                terminate_process(&entry.process);
                if let Some(thread) = handshake_thread.take() {
                    let _ = thread.join();
                }
                let _ = stderr.finish();
                return Err("pi session cancelled during handshake".into());
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
            return Err("pi session cancelled during handshake".into());
        }
        let Some((result, reader)) = received else {
            terminate_process(&entry.process);
            let detail = stderr.finish();
            if opts.cancel.is_cancelled() {
                return Err("pi session cancelled during handshake".into());
            }
            last_error = append_stderr(
                "Pi RPC candidate exited during handshake",
                &detail,
                opts,
                bridge.map(|bridge| bridge.token.as_str()),
            );
            continue;
        };
        if let Some(thread) = handshake_thread.take() {
            let _ = thread.join();
        }
        if opts.cancel.is_cancelled() {
            terminate_process(&entry.process);
            let _ = stderr.finish();
            return Err("pi session cancelled during handshake".into());
        }
        match result {
            Handshake::Accepted => {
                return Ok(SpawnedProcess {
                    stdin,
                    stdout: reader.into_inner(),
                    stderr,
                });
            }
            Handshake::ExecutionError(error) => {
                terminate_process(&entry.process);
                let detail = stderr.finish();
                return Err(append_stderr(
                    &error,
                    &detail,
                    opts,
                    bridge.map(|bridge| bridge.token.as_str()),
                ));
            }
            Handshake::Next(error) => {
                terminate_process(&entry.process);
                let detail = stderr.finish();
                last_error = append_stderr(
                    &error,
                    &detail,
                    opts,
                    bridge.map(|bridge| bridge.token.as_str()),
                );
            }
        }
    }
    if opts.cancel.is_cancelled() {
        return Err("pi session cancelled during startup".into());
    }
    Err(last_error)
}
fn control_once(
    stdin: &Arc<Mutex<Option<ChildStdin>>>,
    stdout: &mut BufReader<std::process::ChildStdout>,
    command: &serde_json::Value,
    key: &mut SessionKey,
    entry: &Arc<SessionEntry>,
) -> Result<serde_json::Value, String> {
    let kind = command_type(command)?;
    if kind == "switch_session" {
        let session_path = command
            .get("sessionPath")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "Pi RPC switch_session omitted sessionPath".to_string())?;
        let session_path = Path::new(session_path);
        let session_path = if session_path.is_absolute() {
            session_path.to_path_buf()
        } else {
            key.cwd.join(session_path)
        };
        let target_cwd = session_header_cwd(&session_path).ok_or_else(|| {
            "Pi RPC switch_session target has no readable session cwd".to_string()
        })?;
        if target_cwd != key.cwd {
            return Err(format!(
                "Pi RPC switch_session across working directories is unsupported (target {})",
                target_cwd.display()
            ));
        }
    }
    let id = format!("seher-control-{}", uuid::Uuid::new_v4());
    let mut request = command
        .as_object()
        .cloned()
        .ok_or_else(|| "Pi RPC command must be an object".to_string())?;
    request.insert("id".into(), serde_json::Value::String(id.clone()));
    {
        let mut stdin_guard = stdin
            .lock()
            .map_err(|_| "pi stdin state poisoned".to_string())?;
        let Some(stdin) = stdin_guard.as_mut() else {
            return Err("Pi RPC stdin closed".into());
        };
        write_json_line(stdin, &serde_json::Value::Object(request))
            .map_err(|error| error.to_string())?;
    }
    let value = read_control_response(stdout, &id, entry)?;
    let mut new_session_id = None;
    if matches!(kind, "new_session" | "switch_session" | "fork" | "clone")
        && value.get("cancelled").and_then(serde_json::Value::as_bool) != Some(true)
    {
        let state_id = format!("seher-state-{}", uuid::Uuid::new_v4());
        let state = serde_json::json!({"id": state_id, "type": "get_state"});
        let mut stdin_guard = stdin
            .lock()
            .map_err(|_| "pi stdin state poisoned".to_string())?;
        let Some(stdin) = stdin_guard.as_mut() else {
            return Err("Pi RPC stdin closed".into());
        };
        write_json_line(stdin, &state).map_err(|error| error.to_string())?;
        drop(stdin_guard);
        let state = read_control_response(stdout, &state_id, entry)?;
        let new_id = state
            .get("sessionId")
            .or_else(|| state.get("session_id"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "Pi RPC get_state response omitted sessionId".to_string())?;
        let destination_cwd = session_state_cwd(&state).unwrap_or_else(|| key.cwd.clone());
        if kind == "switch_session" && destination_cwd != key.cwd {
            entry.closing.store(true, Ordering::Release);
            terminate_process(&entry.process);
            remove_if_same(key, entry);
            remove_entry_identity(entry);
            return Err(format!(
                "Pi RPC switch_session changed working directory to {}, which is not supported",
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
    stdout: &mut BufReader<std::process::ChildStdout>,
    id: &str,
    entry: &SessionEntry,
) -> Result<serde_json::Value, String> {
    let deadline = Instant::now() + CONTROL_RESPONSE_WAIT;
    loop {
        if entry.closing.load(Ordering::Acquire) {
            return Err("Pi RPC session closed".into());
        }
        let line = match read_jsonl_line_until(stdout, None, entry, Some(deadline)) {
            Ok(Some(line)) => line,
            Ok(None) => return Err("Pi RPC process exited while handling command".to_string()),
            Err(error) => {
                entry.closing.store(true, Ordering::Release);
                terminate_process(&entry.process);
                fail_pending_responses(entry, "Pi RPC control response timed out");
                return Err(error);
            }
        };
        let frame =
            parse_jsonl_frame(&line).map_err(|error| format!("invalid Pi RPC JSONL: {error}"))?;
        if frame.get("type").and_then(serde_json::Value::as_str) == Some("response") {
            let _ = route_pending_response(entry, &frame);
        }
        if frame.get("type").and_then(serde_json::Value::as_str) != Some("response")
            || frame.get("id").and_then(serde_json::Value::as_str) != Some(id)
        {
            continue;
        }
        if frame.get("success").and_then(serde_json::Value::as_bool) != Some(true) {
            return Err(frame
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Pi RPC command failed")
                .to_string());
        }
        return Ok(frame.get("data").cloned().unwrap_or(frame));
    }
}

pub(crate) fn session_header_cwd(path: &Path) -> Option<PathBuf> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if line.trim().is_empty() {
            continue;
        }
        if let Some(cwd) = parse_session_header_cwd(&line) {
            return Some(cwd);
        }
        // oh-my-pi session files physically start with a fixed-width 256-byte
        // `type: "title"` slot before the header; retry past the slot.
        return session_header_cwd_after_title_slot(path);
    }
}

fn parse_session_header_cwd(line: &str) -> Option<PathBuf> {
    let header = serde_json::from_str::<serde_json::Value>(line).ok()?;
    if header.get("type").and_then(serde_json::Value::as_str) != Some("session") {
        return None;
    }
    header
        .get("cwd")
        .and_then(serde_json::Value::as_str)
        .map(|cwd| canonical_cwd(Some(Path::new(cwd))))
}

fn session_header_cwd_after_title_slot(path: &Path) -> Option<PathBuf> {
    use std::io::{Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(256)).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if line.trim().is_empty() {
            continue;
        }
        return parse_session_header_cwd(&line);
    }
}

pub(crate) fn session_state_cwd(state: &serde_json::Value) -> Option<PathBuf> {
    for key in ["cwd", "workingDirectory", "working_directory"] {
        if let Some(cwd) = state.get(key).and_then(serde_json::Value::as_str) {
            return Some(canonical_cwd(Some(Path::new(cwd))));
        }
    }
    state
        .get("sessionFile")
        .or_else(|| state.get("session_file"))
        .and_then(serde_json::Value::as_str)
        .and_then(|path| session_header_cwd(Path::new(path)))
}

fn rekey_session(
    old: &SessionKey,
    new_cwd: &Path,
    new_id: &str,
    entry: &Arc<SessionEntry>,
) -> Result<(), String> {
    let mut sessions = registry()
        .lock()
        .map_err(|_| "pi registry poisoned".to_string())?;
    if !sessions
        .get(old)
        .is_some_and(|current| Arc::ptr_eq(current, entry))
    {
        return Err("pi session registry identity changed during session control".into());
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
            "pi session '{}' already exists under {}",
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

fn prompt_once(
    stdin: &Arc<Mutex<Option<ChildStdin>>>,
    stdout: &mut BufReader<std::process::ChildStdout>,
    prompt: &str,
    output: &mpsc::Sender<StreamChunk>,
    entry: &SessionEntry,
    cancel: &CancelToken,
) -> Result<String, String> {
    if child_exited_entry(entry) {
        return Err("Pi RPC process exited before prompting".into());
    }
    {
        let mut stdin_guard = stdin
            .lock()
            .map_err(|_| "pi stdin state poisoned".to_string())?;
        let Some(stdin) = stdin_guard.as_mut() else {
            return Err("Pi RPC stdin closed".into());
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
            return Err("Pi RPC session cancelled".into());
        }
        let line = match read_jsonl_line_cancel(stdout, cancel, entry) {
            Ok(Some(line)) => line,
            Ok(None) => return Err("Pi RPC process exited while prompting".into()),
            Err(_error) if cancel.is_cancelled() => {
                abort_and_reap(stdin, entry);
                return Err("Pi RPC session cancelled".into());
            }
            Err(error) => return Err(error),
        };
        let frame = parse_jsonl_frame(&line).map_err(|e| format!("invalid Pi RPC JSONL: {e}"))?;
        if cancel.is_cancelled() {
            abort_and_reap(stdin, entry);
            return Err("Pi RPC session cancelled".into());
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
                    return Err("malformed Pi RPC prompt acknowledgement".into());
                }
                acknowledged = true;
                entry.prompt_acknowledged.store(true, Ordering::Release);
                if frame["success"] == false {
                    entry.prompt_acknowledged.store(false, Ordering::Release);
                    return Err(frame
                        .get("error")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("Pi RPC prompt failed")
                        .to_string());
                }
            }
            Some("response") => {
                let _ = route_pending_response(entry, &frame);
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
                if frame.get("willRetry").and_then(serde_json::Value::as_bool) != Some(true) =>
            {
                assistant_error =
                    network_error_reason(&frame).or_else(|| agent_messages_error(&frame));
            }
            Some("agent_settled") => {
                if !acknowledged {
                    return Err("Pi RPC settled before prompt acknowledgement".into());
                }
                // Close prompt control admission before routing final responses.
                entry.prompt_active.store(false, Ordering::Release);
                drain_pending_after_settled(stdout, entry)?;
                if let Some(error) = assistant_error {
                    entry.prompt_acknowledged.store(false, Ordering::Release);
                    return Err(error);
                }
                return Ok(String::new());
            }
            _ => {}
        }
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
                return Err("Pi RPC session cancelled".into());
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err("Pi RPC response routing deadline expired".into());
            }
            if !stdout.buffer().is_empty() {
                return read_jsonl_line(stdout)
                    .map_err(|error| format!("invalid Pi RPC JSONL: {error}"));
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
                .map_err(|error| format!("invalid Pi RPC JSONL: {error}"));
        }
    }
    #[cfg(not(unix))]
    {
        if cancel.is_some_and(CancelToken::is_cancelled) {
            return Err("Pi RPC session cancelled".into());
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
            read_jsonl_line(stdout).map_err(|error| format!("invalid Pi RPC JSONL: {error}"));
        stop.store(true, Ordering::Release);
        if watcher.is_finished() {
            let _ = watcher.join();
        }
        result
    }
}

fn drain_pending_after_settled(
    stdout: &mut BufReader<std::process::ChildStdout>,
    entry: &SessionEntry,
) -> Result<(), String> {
    let deadline = Instant::now() + CONTROL_RESPONSE_WAIT;
    loop {
        let pending = entry
            .pending
            .lock()
            .map_err(|_| "pi pending response state poisoned".to_string())?
            .is_empty();
        if pending {
            return Ok(());
        }
        let line = read_jsonl_line_until(stdout, None, entry, Some(deadline))?
            .ok_or_else(|| "Pi RPC process exited while routing control response".to_string())?;
        let frame =
            parse_jsonl_frame(&line).map_err(|error| format!("invalid Pi RPC JSONL: {error}"))?;
        if frame.get("type").and_then(serde_json::Value::as_str) == Some("response") {
            let _ = route_pending_response(entry, &frame);
        }
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
    opts: &PiRpcRunnerOptions,
    session_id: &str,
    session_dir: &Path,
) -> Vec<(String, Vec<String>)> {
    let mut base = vec![
        "--mode".into(),
        "rpc".into(),
        "--session-id".into(),
        session_id.into(),
        "--session-dir".into(),
        session_dir.display().to_string(),
    ];
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
    if let Some(bin) = &opts.pi_bin {
        return vec![(bin.display().to_string(), base)];
    }
    vec![
        ("pi".into(), base.clone()),
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

enum Handshake {
    Accepted,
    ExecutionError(String),
    Next(String),
}

fn handshake(
    reader: &mut BufReader<std::process::ChildStdout>,
    expected_session_id: &str,
) -> Handshake {
    loop {
        let line = match read_jsonl_line(reader) {
            Ok(Some(line)) => line,
            Ok(None) => return Handshake::Next("Pi RPC candidate exited during handshake".into()),
            Err(error) => {
                return Handshake::Next(format!("invalid Pi RPC JSONL during handshake: {error}"));
            }
        };
        match parse_jsonl_frame(&line) {
            Ok(frame) => match frame.get("type").and_then(serde_json::Value::as_str) {
                Some("response")
                    if frame.get("id").and_then(serde_json::Value::as_str)
                        == Some("seher-handshake") =>
                {
                    if frame.get("success").and_then(serde_json::Value::as_bool) == Some(false) {
                        return Handshake::ExecutionError(
                            frame
                                .get("error")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("Pi RPC get_state failed")
                                .to_string(),
                        );
                    }
                    let session_id = frame
                        .get("data")
                        .and_then(|data| data.get("sessionId"))
                        .and_then(serde_json::Value::as_str);
                    if frame.get("success").and_then(serde_json::Value::as_bool) != Some(true)
                        || frame.get("command").and_then(serde_json::Value::as_str)
                            != Some("get_state")
                        || session_id != Some(expected_session_id)
                    {
                        return Handshake::Next("Pi RPC get_state handshake was invalid".into());
                    }
                    return Handshake::Accepted;
                }
                Some("response") => {
                    return Handshake::Next("Pi RPC handshake response had the wrong id".into());
                }
                Some(_) => {}
                None => return Handshake::Next("Pi RPC handshake frame had no type".into()),
            },
            Err(error) => {
                return Handshake::Next(format!("invalid Pi RPC JSONL during handshake: {error}"));
            }
        }
    }
}

pub(crate) fn message_end_error(event: &serde_json::Value) -> Option<String> {
    if event.get("type").and_then(serde_json::Value::as_str) != Some("message_end") {
        return None;
    }
    let message = event.get("message").unwrap_or(event);
    if has_message_reason(event, NETWORK_ERROR_REASON)
        || has_message_reason(message, NETWORK_ERROR_REASON)
    {
        return Some(NETWORK_ERROR_REASON.to_string());
    }
    message_error(message)
}

fn has_message_reason(message: &serde_json::Value, reason: &str) -> bool {
    ["stopReason", "stop_reason", "finishReason", "finish_reason"]
        .iter()
        .any(|field| message.get(*field).and_then(serde_json::Value::as_str) == Some(reason))
}

pub(crate) fn network_error_reason(message: &serde_json::Value) -> Option<String> {
    has_message_reason(message, NETWORK_ERROR_REASON).then(|| NETWORK_ERROR_REASON.to_string())
}

pub(crate) fn agent_messages_error(frame: &serde_json::Value) -> Option<String> {
    frame
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .and_then(|messages| messages.iter().rev().find_map(message_error))
}

pub(crate) fn message_error(message: &serde_json::Value) -> Option<String> {
    if has_message_reason(message, NETWORK_ERROR_REASON) {
        return Some(NETWORK_ERROR_REASON.to_string());
    }
    if message
        .get("stopReason")
        .and_then(serde_json::Value::as_str)
        != Some("error")
    {
        return None;
    }
    let error = message
        .get("errorMessage")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("pi: assistant turn ended with stopReason error");
    Some(if error == NETWORK_ERROR_REASON {
        format!("provider error: {error}")
    } else {
        error.to_string()
    })
}

pub(crate) fn is_pi_process_failure(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("process exited")
        || lower.contains("broken pipe")
        || lower.contains("stdin closed")
        || lower.contains("invalid pi rpc jsonl")
        || lower.contains("malformed pi rpc")
}

pub(crate) fn classified_chunk_with_source(
    source: &str,
    display: &str,
    provider: &str,
) -> StreamChunk {
    if source == NETWORK_ERROR_REASON {
        StreamChunk::Error(NETWORK_ERROR_REASON.to_string())
    } else if is_non_retryable_error(source) {
        StreamChunk::Error(display.to_string())
    } else if is_pi_limit(source) {
        StreamChunk::Limit(LimitError {
            provider: provider.to_string(),
            reset_at: None,
        })
    } else {
        StreamChunk::Error(display.to_string())
    }
}

pub(crate) fn classified_chunk(message: &str, provider: &str) -> StreamChunk {
    classified_chunk_with_source(message, message, provider)
}

pub(crate) fn is_pi_limit(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("rate limit")
        || lower.contains("usage limit")
        || lower.contains("too many requests")
        || lower
            .split(|c: char| {
                c.is_whitespace()
                    || matches!(
                        c,
                        '(' | ')'
                            | '['
                            | ']'
                            | '{'
                            | '}'
                            | ','
                            | ';'
                            | ':'
                            | '.'
                            | '\''
                            | '"'
                            | '/'
                            | '\\'
                            | '!'
                            | '?'
                    )
            })
            .any(|token| {
                matches!(
                    token,
                    "ratelimit"
                        | "rate-limit"
                        | "rate-limited"
                        | "usagelimit"
                        | "usage-limit"
                        | "usage-limited"
                        | "quota"
                )
            })
        || contains_http_status(message, 429)
}

fn contains_http_status(message: &str, status: u16) -> bool {
    let needle = format!("HTTP {status}");
    message.match_indices(&needle).any(|(idx, _)| {
        message[idx + needle.len()..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_digit())
    })
}

pub(crate) fn write_json_line(
    writer: &mut impl Write,
    value: &serde_json::Value,
) -> std::io::Result<()> {
    serde_json::to_writer(&mut *writer, value).map_err(std::io::Error::other)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn read_jsonl_line_limited(
    reader: &mut impl BufRead,
    limit: usize,
) -> std::io::Result<Option<String>> {
    let mut bytes = Vec::new();
    let mut limited = (&mut *reader).take(limit.saturating_add(1) as u64);
    let read = limited.read_until(b'\n', &mut bytes)?;
    if read == 0 {
        return Ok(None);
    }
    if read > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "JSONL frame exceeds size limit",
        ));
    }
    if bytes.last() != Some(&b'\n') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "JSONL frame is not LF terminated",
        ));
    }
    String::from_utf8(bytes).map(Some).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "JSONL frame is not UTF-8")
    })
}

pub(crate) fn read_jsonl_line(reader: &mut impl BufRead) -> std::io::Result<Option<String>> {
    read_jsonl_line_limited(reader, MAX_FRAME_BYTES)
}

pub(crate) fn parse_jsonl_frame(line: &str) -> Result<serde_json::Value, serde_json::Error> {
    if !line.ends_with('\n') {
        return Err(serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "JSONL frame is not LF terminated",
        )));
    }
    let line = line.strip_suffix('\n').unwrap_or(line);
    let line = line.strip_suffix('\r').unwrap_or(line);
    serde_json::from_str(line)
}
pub(crate) fn terminate_process(process: &Arc<Mutex<Option<Child>>>) {
    if let Ok(mut child) = process.lock()
        && let Some(child) = child.as_mut()
    {
        #[cfg(unix)]
        if let Ok(pid) = i32::try_from(child.id())
            && unsafe { libc::kill(-pid, libc::SIGKILL) } == 0
        {
            let _ = child.wait();
            return;
        }
        #[cfg(not(unix))]
        if let Ok(pid) = i32::try_from(child.id()) {
            let _ = Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .status();
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

pub(crate) fn append_stderr(
    message: &str,
    stderr: &str,
    opts: &PiRpcRunnerOptions,
    bridge_token: Option<&str>,
) -> String {
    append_stderr_with_ambient_secrets(
        message,
        stderr,
        opts,
        bridge_token,
        &ambient_secret_values(),
    )
}

/// [`append_stderr`] with the ambient secret values supplied by the caller, so
/// tests can exercise redaction without mutating the process environment.
fn append_stderr_with_ambient_secrets(
    message: &str,
    stderr: &str,
    opts: &PiRpcRunnerOptions,
    bridge_token: Option<&str>,
    ambient_secrets: &[String],
) -> String {
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
    secrets.extend(ambient_secrets.iter().map(String::as_str));
    if let Some(token) = bridge_token {
        secrets.push(token);
    }
    append_stderr_redacted(message, stderr, &secrets)
}

/// Values of parent-environment variables whose names look secret-bearing
/// (`*KEY`, `*TOKEN`, `*SECRET`, …). RPC children inherit the parent
/// environment wholesale, so credentials that were never part of `opts.env`
/// can still be echoed by a failing child and must be redacted too.
///
/// This is best-effort by design: only names containing one of the needles
/// count as secret-bearing, so lookalike names without a needle (e.g.
/// `GH_PAT`) are not covered, and values shorter than 8 bytes are ignored to
/// limit false positives. Over-redaction of ordinary values whose variable
/// happens to contain a needle (e.g. `GIT_AUTHOR_NAME` via `AUTH`) is the
/// accepted trade-off: redacting too much only costs diagnostics, while
/// redacting too little leaks credentials.
pub(crate) fn ambient_secret_values() -> Vec<String> {
    ambient_secret_values_from(std::env::vars_os())
}

/// [`ambient_secret_values`] over an explicit variable list (testable seam).
pub(crate) fn ambient_secret_values_from(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<String> {
    const SECRET_NEEDLES: [&str; 7] = [
        "KEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "AUTH",
    ];
    vars.into_iter()
        .filter_map(|(name, value)| {
            let name = name.to_string_lossy().to_ascii_uppercase();
            let value = value.to_string_lossy().into_owned();
            (!value.is_empty()
                && value.len() >= 8
                && SECRET_NEEDLES.iter().any(|needle| name.contains(needle)))
            .then_some(value)
        })
        .collect()
}

/// Append a stderr tail to `message`, redacting every secret first.
pub(crate) fn append_stderr_redacted(message: &str, stderr: &str, secrets: &[&str]) -> String {
    if stderr.is_empty() {
        return message.to_string();
    }
    let mut detail = stderr.to_string();
    for secret in secrets {
        if !secret.is_empty() {
            detail = detail.replace(*secret, "[redacted]");
        }
    }
    format!("{message}: {}", detail.trim())
}
pub(crate) fn canonical_cwd(cwd: Option<&Path>) -> PathBuf {
    let path = cwd.map_or_else(
        || std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        Path::to_path_buf,
    );
    std::fs::canonicalize(&path).unwrap_or(path)
}

fn ts_session_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local/share")
        })
        .join("seher")
        .join("pi-ts-sessions")
}
/// Load the same user-wide skills appendix used by the in-process Pi backend.
pub(crate) fn load_hardcoded_skills_appendix(working_directory: Option<&Path>) -> Option<String> {
    let home = dirs::home_dir()?;
    let skills_dir = home.join(".agents/skills");
    if !skills_dir.is_dir() {
        return None;
    }
    let cwd = working_directory.map_or_else(
        || std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        Path::to_path_buf,
    );
    let options = pi::resources::LoadSkillsOptions {
        cwd,
        agent_dir: home.join(".agents"),
        skill_paths: vec![skills_dir],
        include_defaults: false,
    };
    let result = pi::resources::load_skills(options);
    (!result.skills.is_empty()).then(|| pi::resources::format_skills_for_prompt(&result.skills))
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

struct Bridge {
    listener: TcpListener,
    token: String,
    tempdir: tempfile::TempDir,
    spec_path: PathBuf,
    stop: Arc<AtomicBool>,
    connections: Arc<Mutex<Vec<TcpStream>>>,
    workers: Arc<Mutex<Vec<thread::JoinHandle<()>>>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Bridge {
    fn new(tools: &[SeherTool]) -> Result<Option<Self>, String> {
        if tools.is_empty() {
            return Ok(None);
        }
        validate_tool_names(tools)?;
        let tempdir = tempfile::Builder::new()
            .prefix("seher-pi-")
            .tempdir()
            .map_err(|e| format!("failed to create Pi extension directory: {e}"))?;
        let dir = tempdir.path();
        let spec_path = dir.join("spec.json");
        let extension_path = dir.join("extension.ts");
        let token = uuid::Uuid::new_v4().to_string();
        let specs = serde_json::json!({"tools": tools.iter().map(|tool| serde_json::json!({"name":tool.name,"description":tool.description,"parameters":tool.parameters})).collect::<Vec<_>>()});
        std::fs::write(
            &spec_path,
            serde_json::to_vec(&specs).map_err(|e| e.to_string())?,
        )
        .map_err(|e| format!("failed to write Pi tool spec: {e}"))?;
        std::fs::write(&extension_path, EXTENSION_TEMPLATE)
            .map_err(|e| format!("failed to write Pi extension: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, PermissionsExt::from_mode(0o700));
            let _ = std::fs::set_permissions(&extension_path, PermissionsExt::from_mode(0o600));
        }
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|e| format!("failed to bind Pi tool bridge: {e}"))?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let stop = Arc::new(AtomicBool::new(false));
        let connections = Arc::new(Mutex::new(Vec::new()));
        let workers = Arc::new(Mutex::new(Vec::new()));
        let listener_thread = listener.try_clone().map_err(|e| e.to_string())?;
        let stop_thread = Arc::clone(&stop);
        let connections_thread = Arc::clone(&connections);
        let workers_thread = Arc::clone(&workers);
        let token_thread = token.clone();
        let tools_thread = tools.to_vec();
        let thread = thread::spawn(move || {
            bridge_loop(
                &listener_thread,
                &token_thread,
                &tools_thread,
                &stop_thread,
                &connections_thread,
                &workers_thread,
            );
        });
        Ok(Some(Self {
            listener,
            token,
            tempdir,
            spec_path,
            stop,
            connections,
            workers,
            thread: Some(thread),
        }))
    }

    fn configure_command(&self, command: &mut Command) {
        let Ok(address) = self.listener.local_addr() else {
            return;
        };
        let Some(parent) = self.spec_path.parent() else {
            return;
        };
        let extension_path = parent.join("extension.ts");
        command.arg("--extension").arg(extension_path);
        command
            .env("SEHER_PI_TOOL_SPEC", &self.spec_path)
            .env("SEHER_PI_BRIDGE_HOST", address.ip().to_string())
            .env("SEHER_PI_BRIDGE_PORT", address.port().to_string())
            .env("SEHER_PI_BRIDGE_TOKEN", &self.token);
    }
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
impl Drop for Bridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Ok(connections) = self.connections.lock() {
            for connection in connections.iter() {
                let _ = connection.shutdown(Shutdown::Both);
            }
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Ok(mut workers) = self.workers.lock() {
            for worker in workers.drain(..) {
                if worker.is_finished() {
                    let _ = worker.join();
                }
                // A synchronous SeherTool handler may never return. Dropping an
                // unfinished JoinHandle detaches it so bridge shutdown is bounded.
            }
        }
        // TempDir cleanup is best effort; detached handlers do not borrow it.
        let _ = self.tempdir.path();
    }
}

fn bridge_loop(
    listener: &TcpListener,
    token: &str,
    tools: &[SeherTool],
    stop: &Arc<AtomicBool>,
    connections: &Arc<Mutex<Vec<TcpStream>>>,
    workers: &Arc<Mutex<Vec<thread::JoinHandle<()>>>>,
) {
    while !stop.load(Ordering::Acquire) {
        if let Ok(mut handles) = workers.lock() {
            let mut active = Vec::with_capacity(handles.len());
            for handle in handles.drain(..) {
                if handle.is_finished() {
                    let _ = handle.join();
                } else {
                    active.push(handle);
                }
            }
            *handles = active;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if workers
                    .lock()
                    .map_or(MAX_BRIDGE_CONNECTIONS, |workers| workers.len())
                    >= MAX_BRIDGE_CONNECTIONS
                {
                    continue;
                }
                let tracked = stream.try_clone().ok();
                if let Some(tracked) = tracked
                    && let Ok(mut list) = connections.lock()
                {
                    list.push(tracked);
                }
                let token = token.to_string();
                let tools = tools.to_owned();
                let peer = stream.peer_addr().ok();
                let connections_thread = Arc::clone(connections);
                let handle = thread::spawn(move || {
                    bridge_connection(stream, &token, &tools);
                    if let Some(peer) = peer
                        && let Ok(mut list) = connections_thread.lock()
                    {
                        list.retain(|connection| connection.peer_addr().ok() != Some(peer));
                    }
                });
                if let Ok(mut list) = workers.lock() {
                    list.push(handle);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
}

fn read_jsonl_deadline(
    stream: &mut TcpStream,
    deadline: Instant,
) -> std::io::Result<Option<String>> {
    let mut bytes = Vec::new();
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_default();
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "bridge authentication deadline expired",
            ));
        }
        stream.set_read_timeout(Some(remaining))?;
        let mut byte = [0_u8; 1];
        match stream.read(&mut byte)? {
            0 if bytes.is_empty() => return Ok(None),
            0 => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "bridge request ended before LF",
                ));
            }
            1 => {
                bytes.push(byte[0]);
                if bytes.len() > MAX_FRAME_BYTES {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "JSONL frame exceeds size limit",
                    ));
                }
                if byte[0] == b'\n' {
                    return String::from_utf8(bytes).map(Some).map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "JSONL frame is not UTF-8",
                        )
                    });
                }
            }
            _ => unreachable!(),
        }
    }
}

fn bridge_connection(mut stream: TcpStream, token: &str, tools: &[SeherTool]) {
    let Ok(Some(line)) = read_jsonl_deadline(&mut stream, Instant::now() + BRIDGE_READ_WAIT) else {
        return;
    };
    let response = match parse_jsonl_frame(&line) {
        Ok(request) if request.get("token").and_then(serde_json::Value::as_str) == Some(token) => {
            let Some(name) = request.get("tool").and_then(serde_json::Value::as_str) else {
                let _ = write_json_line(
                    &mut stream,
                    &serde_json::json!({"ok":false,"error":"invalid Seher tool name"}),
                );
                return;
            };
            let input = request
                .get("input")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            match tools.iter().find(|tool| tool.name == name) {
                Some(tool) => match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    (tool.handler)(input)
                })) {
                    Ok(Ok(result)) => serde_json::json!({"ok":true,"result":result}),
                    Ok(Err(error)) => serde_json::json!({"ok":false,"error":error}),
                    Err(_) => serde_json::json!({"ok":false,"error":"Seher tool panicked"}),
                },
                None => serde_json::json!({"ok":false,"error":"unknown Seher tool"}),
            }
        }
        _ => serde_json::json!({"ok":false,"error":"invalid Seher tool bridge request"}),
    };
    let _ = write_json_line(&mut stream, &response);
}

fn validate_tool_names(tools: &[SeherTool]) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for tool in tools {
        if pi::sdk::BUILTIN_TOOL_NAMES.contains(&tool.name.as_str()) {
            return Err(format!(
                "custom tool '{}' collides with a pi built-in tool ({})",
                tool.name,
                pi::sdk::BUILTIN_TOOL_NAMES.join(", ")
            ));
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
    fn strict_jsonl_keeps_unicode_separators_inside_strings() {
        let frame = parse_jsonl_frame("{\"text\":\"a\u{2028}b\"}\n").expect("valid JSONL");
        assert_eq!(frame["text"], "a\u{2028}b");
        assert!(parse_jsonl_frame("{\"text\":1}\u{2028}{\"text\":2}").is_err());
    }
    #[test]
    fn strict_jsonl_requires_lf_framing() {
        assert!(parse_jsonl_frame("{\"ok\":true}").is_err());
        assert!(parse_jsonl_frame("{\"ok\":true}\r").is_err());
        assert!(parse_jsonl_frame("{\"ok\":true}\n").is_ok());
    }

    #[test]
    fn jsonl_frame_exactly_at_the_limit_is_accepted() {
        let limit = 64;
        let mut input = vec![b'a'; limit - 1];
        input.push(b'\n');
        let mut reader = std::io::Cursor::new(input);

        let line = read_jsonl_line_limited(&mut reader, limit)
            .expect("frame at limit")
            .expect("frame exists");
        assert_eq!(line.len(), limit);
        assert!(line.ends_with('\n'));
    }

    #[test]
    fn jsonl_frame_over_the_limit_returns_invalid_data() {
        let limit = 64;
        let mut input = vec![b'a'; limit];
        input.push(b'\n');
        let mut reader = std::io::Cursor::new(input);

        let error = read_jsonl_line_limited(&mut reader, limit).expect_err("oversized frame");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("JSONL frame exceeds size limit"));
    }

    #[test]
    fn jsonl_reader_returns_none_at_eof() {
        let mut reader = std::io::Cursor::new(Vec::<u8>::new());
        assert_eq!(read_jsonl_line_limited(&mut reader, 64).expect("EOF"), None);
    }

    #[test]
    fn jsonl_frame_without_lf_returns_invalid_data() {
        let mut reader = std::io::Cursor::new(b"{\"ok\":true}".to_vec());
        let error = read_jsonl_line_limited(&mut reader, 64).expect_err("unterminated frame");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            error
                .to_string()
                .contains("JSONL frame is not LF terminated")
        );
    }

    #[test]
    fn jsonl_frame_with_invalid_utf8_returns_invalid_data() {
        let mut reader = std::io::Cursor::new(vec![0xff, b'\n']);
        let error = read_jsonl_line_limited(&mut reader, 64).expect_err("invalid UTF-8");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("JSONL frame is not UTF-8"));
    }

    #[test]
    fn jsonl_reader_preserves_consecutive_frame_boundaries() {
        let mut reader = std::io::Cursor::new(b"first\nsecond\n".to_vec());
        assert_eq!(
            read_jsonl_line_limited(&mut reader, 64).expect("first"),
            Some("first\n".into())
        );
        assert_eq!(
            read_jsonl_line_limited(&mut reader, 64).expect("second"),
            Some("second\n".into())
        );
        assert_eq!(read_jsonl_line_limited(&mut reader, 64).expect("EOF"), None);
    }

    fn oversized_agent_end_frame(error_message: Option<&str>) -> Vec<u8> {
        let text = "x".repeat(50_000);
        let mut messages = (0..26)
            .map(|_| {
                serde_json::json!({
                    "role": "assistant",
                    "content": [{"type": "text", "text": text.clone()}]
                })
            })
            .collect::<Vec<_>>();
        if let Some(error_message) = error_message {
            messages.push(serde_json::json!({
                "role": "assistant",
                "stopReason": "error",
                "errorMessage": error_message
            }));
        }
        let mut frame = serde_json::to_vec(&serde_json::json!({
            "type": "agent_end",
            "willRetry": false,
            "messages": messages
        }))
        .expect("agent_end fixture");
        frame.push(b'\n');
        assert!(frame.len() > 1024 * 1024 && frame.len() < 2 * 1024 * 1024);
        frame
    }

    #[test]
    fn large_agent_end_frames_are_read_and_preserve_assistant_errors() {
        let mut reader = std::io::Cursor::new(oversized_agent_end_frame(None));
        let line = read_jsonl_line(&mut reader)
            .expect("large frame")
            .expect("frame exists");
        let frame = parse_jsonl_frame(&line).expect("valid JSONL");
        assert_eq!(frame["type"], "agent_end");
        assert_eq!(
            frame["messages"]
                .as_array()
                .expect("messages")
                .iter()
                .rev()
                .find_map(message_error),
            None
        );

        let mut reader = std::io::Cursor::new(oversized_agent_end_frame(Some("boom")));
        let line = read_jsonl_line(&mut reader)
            .expect("large frame")
            .expect("frame exists");
        let frame = parse_jsonl_frame(&line).expect("valid JSONL");
        let error = frame["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .rev()
            .find_map(message_error);
        assert_eq!(error.as_deref(), Some("boom"));
    }

    #[test]
    fn thinking_uses_pi_cli_flag_and_limit_classifier_boundaries() {
        let opts = PiRpcRunnerOptions {
            thinking: Some("high".into()),
            ..Default::default()
        };
        let args = candidate_commands(&opts, "session", Path::new("/tmp/sessions"))[0]
            .1
            .clone();
        assert!(args.windows(2).any(|pair| pair == ["--thinking", "high"]));
        assert!(matches!(
            classified_chunk("quota exceeded", "pi"),
            StreamChunk::Limit(_)
        ));
        assert!(matches!(
            classified_chunk("HTTP 500", "pi"),
            StreamChunk::Error(_)
        ));
        assert!(matches!(
            classified_chunk("HTTP 4290", "pi"),
            StreamChunk::Error(_)
        ));
    }
    #[test]
    fn options_fingerprint_includes_tool_handler_identity() {
        let parameters = serde_json::json!({"type": "object"});
        let shared: crate::sdk::tool::ToolHandler = Arc::new(|_| Ok(String::new()));
        let same_handler = SeherTool::new("tool", "tool", parameters.clone(), Arc::clone(&shared));
        let cloned_handler = same_handler.clone();
        let different_handler =
            SeherTool::new("tool", "tool", parameters, Arc::new(|_| Ok(String::new())));
        let base = PiRpcRunnerOptions {
            tools: vec![same_handler],
            ..Default::default()
        };
        let clone = PiRpcRunnerOptions {
            tools: vec![cloned_handler],

            ..Default::default()
        };
        let different = PiRpcRunnerOptions {
            tools: vec![different_handler],
            ..Default::default()
        };
        assert_eq!(options_fingerprint(&base), options_fingerprint(&clone));
        assert_ne!(options_fingerprint(&base), options_fingerprint(&different));
    }

    #[test]
    fn assistant_error_is_terminal_but_tool_errors_are_not() {
        let error = serde_json::json!({
            "type": "message_end",
            "message": {"stopReason": "error", "errorMessage": "HTTP 429 quota exceeded"}
        });
        assert_eq!(
            message_end_error(&error).as_deref(),
            Some("HTTP 429 quota exceeded")
        );
        let tool = serde_json::json!({"type": "tool_execution_end", "isError": true});
        assert_eq!(message_end_error(&tool), None);
    }

    #[test]
    fn structured_network_error_uses_only_exact_reason_aliases() {
        for field in ["stopReason", "stop_reason", "finishReason", "finish_reason"] {
            let mut message = serde_json::json!({"errorMessage": "ignored"});
            message[field] = serde_json::Value::String(NETWORK_ERROR_REASON.to_string());
            assert_eq!(
                message_error(&message).as_deref(),
                Some(NETWORK_ERROR_REASON),
                "field {field}"
            );
        }
        assert_eq!(
            network_error_reason(&serde_json::json!({"stopReason": NETWORK_ERROR_REASON}))
                .as_deref(),
            Some(NETWORK_ERROR_REASON)
        );
        assert_eq!(
            message_end_error(&serde_json::json!({
                "type": "message_end",
                "finish_reason": NETWORK_ERROR_REASON,
                "message": {"stopReason": "stop"},
            }))
            .as_deref(),
            Some(NETWORK_ERROR_REASON)
        );
        assert_eq!(
            message_error(&serde_json::json!({
                "finish_reason": "network_error_extra",
                "errorMessage": "ordinary",
            }))
            .as_deref(),
            None
        );
        assert_eq!(
            message_error(&serde_json::json!({
                "stopReason": "error",
                "errorMessage": "ordinary",
            }))
            .as_deref(),
            Some("ordinary")
        );
        assert!(matches!(
            classified_chunk_with_source(
                NETWORK_ERROR_REASON,
                "network_error: stderr detail",
                "pi",
            ),
            StreamChunk::Error(message) if message == NETWORK_ERROR_REASON
        ));
        assert_eq!(
            message_error(&serde_json::json!({
                "finish_reason": "error",
                "errorMessage": "ordinary",
            }))
            .as_deref(),
            None
        );
        assert_ne!(
            message_error(&serde_json::json!({
                "stopReason": "error",
                "errorMessage": NETWORK_ERROR_REASON,
            }))
            .as_deref(),
            Some(NETWORK_ERROR_REASON)
        );
    }

    #[test]
    fn bridge_spec_does_not_contain_capability_token() {
        let tool = SeherTool::new(
            "echo",
            "echo",
            serde_json::json!({"type":"object"}),
            Arc::new(|input| Ok(input.to_string())),
        );
        let bridge = Bridge::new(&[tool])
            .expect("bridge")
            .expect("bridge enabled");
        let spec = std::fs::read_to_string(&bridge.spec_path).expect("spec");
        assert!(!spec.contains(&bridge.token));
    }

    #[test]
    fn extension_template_caches_bridge_before_scrubbing_environment() {
        let cache = EXTENSION_TEMPLATE
            .find("const bridgeConfig =")
            .expect("bridge cache");
        let scrub = EXTENSION_TEMPLATE
            .find("for (const key of Object.keys(process.env))")
            .expect("environment scrub");
        assert!(cache < scrub);
        assert!(EXTENSION_TEMPLATE.contains("Symbol.for(\"seher.pi.bridge.config\")"));
    }

    #[test]
    fn candidate_commands_include_rpc_handshake_arguments_without_logging_secrets() {
        let opts = PiRpcRunnerOptions {
            pi_bin: Some(PathBuf::from("fake-pi")),
            api_key: Some("secret".into()),
            ..Default::default()
        };
        let commands = candidate_commands(&opts, "session", Path::new("/tmp/sessions"));
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].0, "fake-pi");
        assert!(
            commands[0]
                .1
                .windows(2)
                .any(|pair| pair == ["--mode", "rpc"])
        );
        assert!(
            commands[0]
                .1
                .windows(2)
                .any(|pair| pair == ["--session-id", "session"])
        );
        assert!(!commands[0].1.iter().any(|arg| arg == "secret"));
        let append = PiRpcRunnerOptions {
            append_system_prompt: Some("skills".into()),
            ..Default::default()
        };
        let append_args = candidate_commands(&append, "session", Path::new("/tmp/sessions"))[0]
            .1
            .clone();
        assert!(
            append_args
                .windows(2)
                .any(|pair| pair == ["--append-system-prompt", "skills"])
        );
    }

    #[test]
    fn tool_bridge_returns_one_framed_response() {
        let tool = SeherTool::new(
            "echo",
            "echo",
            serde_json::json!({"type":"object"}),
            Arc::new(|input| Ok(input.to_string())),
        );
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            bridge_connection(stream, "token", &[tool]);
        });
        let mut client = TcpStream::connect(address).expect("connect");
        write_json_line(
            &mut client,
            &serde_json::json!({"token":"token","tool":"echo","input":{"x":1}}),
        )
        .expect("write");
        let mut response = String::new();
        BufReader::new(client)
            .read_line(&mut response)
            .expect("read");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(response.trim()).expect("response")["ok"],
            true
        );
        let _ = handle.join();
    }

    #[cfg(unix)]
    #[test]
    fn candidate_command_inherits_parent_environment_and_explicit_env_overrides_it() {
        use std::collections::HashMap;

        // Skip gracefully in environments without HOME (e.g. bare containers);
        // there is no inherited variable to compare against.
        let Ok(home) = std::env::var("HOME") else {
            return;
        };

        let dir = tempfile::tempdir().expect("tempdir");
        let explicit_key = "SEHER_PI_RPC_EXPLICIT_ENV_TEST";
        let opts = PiRpcRunnerOptions {
            env: [(explicit_key.into(), "configured-value".into())].into(),
            provider: Some("anthropic".into()),
            api_key: Some("test-provider-key".into()),
            ..Default::default()
        };
        let session = SessionKey {
            cwd: dir.path().to_path_buf(),
            id: "environment-test".into(),
        };

        // `/usr/bin/env` prints the child's inherited environment.
        let output = candidate_command(&opts, &session, None, "/usr/bin/env", &[])
            .output()
            .expect("run env");
        assert!(output.status.success());
        let child_env: HashMap<_, _> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();

        assert_eq!(
            child_env.get("HOME").map(String::as_str),
            Some(home.as_str())
        );
        assert_eq!(
            child_env.get(explicit_key).map(String::as_str),
            Some("configured-value")
        );
        // Provider credentials are injected under their canonical variable name.
        assert_eq!(
            child_env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("test-provider-key")
        );
        // The parent PATH is replaced wholesale by the curated child PATH, the
        // last surviving piece of the old sandboxing that shebang resolution
        // relies on.
        let expected_path = merged_child_path(None, None);
        assert_eq!(
            child_env.get("PATH").map(String::as_str),
            Some(expected_path.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn ambient_secret_values_filters_by_name_needle_and_value_length() {
        let vars = [
            ("AWS_SECRET_ACCESS_KEY", "supersecret99"), // SECRET needle, kept
            ("seher_test_token", "abcdefgh"),           // case-insensitive name match, kept
            ("MY_KEY", "short"),                        // value below the 8-byte threshold, dropped
            ("MY_TOKEN", ""),                           // empty value, dropped
            ("HOME", "/Users/example/long"),            // no needle in name, dropped
            ("GIT_AUTHOR_NAME", "Alice Smith"),         // over-redaction via AUTH, kept by design
        ];
        let mut values = ambient_secret_values_from(
            vars.iter()
                .map(|(name, value)| (OsString::from(name), OsString::from(value))),
        );
        values.sort();
        assert_eq!(
            values,
            vec![
                "Alice Smith".to_string(),
                "abcdefgh".to_string(),
                "supersecret99".to_string(),
            ]
        );
        // Exactly 8 bytes passes the >= threshold.
        assert_eq!(
            ambient_secret_values_from([(OsString::from("MY_KEY"), OsString::from("12345678"))]),
            vec!["12345678".to_string()]
        );
    }

    #[cfg(unix)]
    #[test]
    fn ambient_secret_values_handles_non_utf8_values_lossily() {
        use std::os::unix::ffi::OsStringExt;
        let values = ambient_secret_values_from([(
            OsString::from("MY_SECRET"),
            OsString::from_vec(b"abc\xffdefg".to_vec()),
        )]);
        assert_eq!(values, vec!["abc\u{FFFD}defg".to_string()]);
    }

    #[test]
    fn append_stderr_redacts_ambient_and_configured_secrets_on_surfaced_stderr() {
        let opts = PiRpcRunnerOptions {
            api_key: Some("configured-key-123456".into()),
            ..Default::default()
        };
        let ambient = vec!["ambient-secret-9999".to_string()];
        let message = append_stderr_with_ambient_secrets(
            "candidate failed",
            "fatal: AWS_SECRET_ACCESS_KEY=ambient-secret-9999 configured-key-123456",
            &opts,
            None,
            &ambient,
        );
        assert!(!message.contains("ambient-secret-9999"));
        assert!(!message.contains("configured-key-123456"));
        assert!(message.contains("[redacted]"));
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "downloads and launches the real TypeScript Pi RPC package"]
    fn real_ts_pi_rpc_get_state_smoke() {
        let candidates = [
            ("pi", vec!["--mode", "rpc", "--no-session"]),
            (
                "bunx",
                vec!["--yes", PACKAGE, "--mode", "rpc", "--no-session"],
            ),
            (
                "npx",
                vec!["--yes", PACKAGE, "--mode", "rpc", "--no-session"],
            ),
        ];
        let opts = PiRpcRunnerOptions::default();
        let (program, args) = candidates
            .into_iter()
            .find_map(|(program, args)| {
                let path = resolve_candidate_program(program, &opts.env);
                path.is_file().then_some((path, args))
            })
            .expect("pi, bunx, or npx must be available");
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn TypeScript Pi RPC");
        let mut stdin = child.stdin.take().expect("Pi RPC stdin");
        let stdout = child.stdout.take().expect("Pi RPC stdout");
        let stderr = child.stderr.take().expect("Pi RPC stderr");
        let stderr_thread = thread::spawn(|| {
            let mut reader = BufReader::new(stderr);
            let mut sink = String::new();
            let _ = reader.read_to_string(&mut sink);
        });
        let write_result = write_json_line(
            &mut stdin,
            &serde_json::json!({"id": "seher-smoke", "type": "get_state"}),
        );
        drop(stdin);
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .expect("read get_state response");
            let response = serde_json::from_str::<serde_json::Value>(line.trim())
                .expect("parse get_state response");
            let _ = tx.send(response);
        });
        let response = rx.recv_timeout(Duration::from_secs(30));
        let _ = child.kill();
        child.wait().expect("wait for TypeScript Pi RPC");
        let _ = stderr_thread.join();
        write_result.expect("write get_state");
        let response = response.expect("get_state response timeout");
        assert_eq!(response["type"], "response");
        assert_eq!(response["command"], "get_state");
        assert_eq!(response["success"], true);
        assert!(response["data"]["sessionId"].as_str().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn fake_rpc_process_streams_and_reuses_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("fake-pi");
        std::fs::write(&script, r#"#!/bin/sh
sid=
previous=
for arg in "$@"; do
  if [ "$previous" = "--session-id" ]; then sid="$arg"; fi
  previous="$arg"
done
while IFS= read -r line; do
  case "$line" in
    *get_state*) printf '{"id":"seher-handshake","type":"response","command":"get_state","success":true,"data":{"sessionId":"%s"}}\n' "$sid" ;;
    *prompt*) printf '%s\n' '{"id":"seher-prompt","type":"response","command":"prompt","success":true}' '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"ok"}}' '{"type":"agent_settled"}' ;;
    *abort*) exit 0 ;;
  esac
done
"#).expect("script");
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .expect("chmod");
        let runner = PiRpcRunner::new(PiRpcRunnerOptions {
            pi_bin: Some(script),
            working_directory: Some(dir.path().to_path_buf()),
            ..Default::default()
        });
        let first = runner.run("one".into(), None).expect("first prompt");
        assert_eq!(first.text, "ok");
        let second = runner
            .run("two".into(), Some(first.session_id.clone()))
            .expect("resume prompt");
        assert_eq!(second.session_id, first.session_id);
        assert!(runner.close_pi_session(&first.session_id));
        assert!(!runner.close_pi_session(&first.session_id));
    }
    #[cfg(unix)]
    fn fake_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, body).expect("script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        path
    }

    #[cfg(unix)]
    fn fake_rpc_body(extra: &str) -> String {
        format!(
            r#"#!/bin/sh
sid=
previous=
for arg in "$@"; do
  if [ "$previous" = "--session-id" ]; then sid="$arg"; fi
  previous="$arg"
done
while IFS= read -r line; do
  case "$line" in
    *get_state*) printf '{{"id":"seher-handshake","type":"response","command":"get_state","success":true,"data":{{"sessionId":"%s"}}}}\n' "$sid" ;;
    *prompt*) printf '%s\n' '{{"id":"seher-prompt","type":"response","command":"prompt","success":true}}' '{{"type":"message_update","assistantMessageEvent":{{"type":"text_delta","delta":"ok"}}}}' '{{"type":"agent_settled"}}'; {extra} ;;
    *abort*) exit 0 ;;
  esac
done
"#
        )
    }
    #[cfg(unix)]
    #[test]
    fn invalid_handshake_advances_to_next_candidate_but_failed_handshake_does_not() {
        let dir = tempfile::tempdir().expect("tempdir");
        fake_script(
            dir.path(),
            "pi",
            r#"#!/bin/sh
printf '%s\n' '{"id":"seher-handshake","type":"response","command":"get_state","success":true,"data":{"sessionId":"wrong"}}'
"#,
        );
        let bunx = fake_script(dir.path(), "bunx", &fake_rpc_body(""));
        fake_script(dir.path(), "npx", "#!/bin/sh\nexit 1\n");
        let opts = PiRpcRunnerOptions {
            working_directory: Some(dir.path().to_path_buf()),
            env: [("PATH".into(), dir.path().display().to_string())].into(),
            ..Default::default()
        };
        let runner = PiRpcRunner::new(opts);
        let output = runner
            .run("fallback".into(), None)
            .expect("fallback candidate");
        assert_eq!(output.text, "ok");
        assert!(bunx.exists());
        assert!(runner.close_pi_session(&output.session_id));

        let failed = fake_script(
            dir.path(),
            "failed",
            r#"#!/bin/sh
printf '%s\n' '{"id":"seher-handshake","type":"response","command":"get_state","success":false,"error":"bad state"}'
"#,
        );
        let error = PiRpcRunner::new(PiRpcRunnerOptions {
            pi_bin: Some(failed),
            working_directory: Some(dir.path().to_path_buf()),
            ..Default::default()
        })
        .run("no fallback".into(), None)
        .expect_err("failed handshake");
        assert!(matches!(error, RunError::Other { message, .. } if message.contains("bad state")));
    }

    #[cfg(unix)]
    #[test]
    fn session_control_idle_busy_and_crash_removal_behave() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = fake_script(
            dir.path(),
            "fake-pi",
            r#"#!/bin/sh
sid=
previous=
prompts=0
for arg in "$@"; do
  if [ "$previous" = "--session-id" ]; then sid="$arg"; fi
  previous="$arg"
done
while IFS= read -r line; do
  case "$line" in
    *get_state*) id=$(printf '%s' "$line" | sed 's/.*"id":"\([^"]*\)".*/\1/'); printf '{"id":"%s","type":"response","command":"get_state","success":true,"data":{"sessionId":"%s"}}\n' "$id" "$sid" ;;
    *prompt*) prompts=$((prompts + 1)); printf '%s\n' '{"id":"seher-prompt","type":"response","command":"prompt","success":true}'; if [ "$prompts" -eq 1 ]; then printf '%s\n' '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"ok"}}' '{"type":"agent_settled"}'; else sleep 1; fi ;;
    *steer*|*follow_up*) id=$(printf '%s' "$line" | sed 's/.*"id":"\([^"]*\)".*/\1/'); command=$(printf '%s' "$line" | sed 's/.*"type":"\([^"]*\)".*/\1/'); printf '{"id":"%s","type":"response","command":"%s","success":true}\n' "$id" "$command" ;;
    *abort*) exit 0 ;;
  esac
done
"#,
        );
        let opts = PiRpcRunnerOptions {
            pi_bin: Some(script),
            working_directory: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        let runner = PiRpcRunner::new(opts);
        let first = runner.run("one".into(), None).expect("prompt");
        let state = runner
            .send_command(&first.session_id, serde_json::json!({"type":"get_state"}))
            .expect("idle control");
        assert_eq!(state["sessionId"], first.session_id);
        let pending = runner.stream("hold".into(), Some(first.session_id.clone()));
        assert!(matches!(
            pending
                .recv_timeout(Duration::from_secs(2))
                .expect("prompt startup"),
            StreamChunk::Session(_)
        ));
        for kind in ["steer", "follow_up"] {
            let response = runner
                .send_command(
                    &first.session_id,
                    serde_json::json!({"type": kind, "message": "busy control"}),
                )
                .expect("busy interaction control");
            assert_eq!(response["success"], true);
            assert_eq!(response["command"], kind);
        }
        let busy = runner
            .send_command(&first.session_id, serde_json::json!({"type":"get_state"}))
            .expect_err("busy control");
        assert!(busy.contains("busy"));
        let reentry = runner
            .stream("again".into(), Some(first.session_id.clone()))
            .recv()
            .expect("reentry result");
        assert!(matches!(reentry, StreamChunk::Error(message) if message.contains("busy")));
        assert!(runner.close_pi_session(&first.session_id));
        drop(pending);

        let crash_script = fake_script(
            dir.path(),
            "crash-pi",
            r#"#!/bin/sh
sid=
previous=
prompts=0
for arg in "$@"; do [ "$previous" = "--session-id" ] && sid="$arg"; previous="$arg"; done
while IFS= read -r line; do
  case "$line" in
    *get_state*) printf '{"id":"seher-handshake","type":"response","command":"get_state","success":true,"data":{"sessionId":"%s"}}\n' "$sid" ;;
    *prompt*) prompts=$((prompts + 1)); printf '%s\n' '{"id":"seher-prompt","type":"response","command":"prompt","success":true}'; if [ "$prompts" -eq 1 ]; then printf '%s\n' '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"ok"}}' '{"type":"agent_settled"}'; else exit 0; fi ;;
  esac
done
"#,
        );
        let crash_runner = PiRpcRunner::new(PiRpcRunnerOptions {
            pi_bin: Some(crash_script),
            working_directory: Some(dir.path().to_path_buf()),
            ..Default::default()
        });
        let crashed = crash_runner
            .run("first".into(), None)
            .expect("first crash prompt");
        let crash = crash_runner
            .run("second".into(), Some(crashed.session_id.clone()))
            .expect_err("accepted process crash");
        assert!(
            matches!(crash, RunError::Other { message, .. } if message.contains("process exited") || message.contains("Broken pipe"))
        );
        let respawned = crash_runner
            .run("third".into(), Some(crashed.session_id))
            .expect("next call respawns");
        assert_eq!(respawned.text, "ok");
        assert!(crash_runner.close_pi_session(&respawned.session_id));
    }
    #[cfg(unix)]
    #[test]
    fn silent_prompt_cancellation_unblocks_reader_and_reaps_child() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = fake_script(
            dir.path(),
            "silent-pi",
            r#"#!/bin/sh
sid=
previous=
for arg in "$@"; do [ "$previous" = "--session-id" ] && sid="$arg"; previous="$arg"; done
while IFS= read -r line; do
  case "$line" in
    *get_state*) printf '{"id":"seher-handshake","type":"response","command":"get_state","success":true,"data":{"sessionId":"%s"}}\n' "$sid" ;;
    *prompt*) printf '%s\n' '{"id":"seher-prompt","type":"response","command":"prompt","success":true}'; sleep 60 ;;
    *abort*) exit 0 ;;
  esac
done
"#,
        );
        let cancel = CancelToken::new();
        let runner = PiRpcRunner::new(PiRpcRunnerOptions {
            pi_bin: Some(script),
            working_directory: Some(dir.path().to_path_buf()),
            cancel: cancel.clone(),
            ..Default::default()
        });
        let receiver = runner.stream("silent".into(), None);
        assert!(matches!(
            receiver.recv().expect("session event"),
            StreamChunk::Session(_)
        ));
        thread::sleep(Duration::from_millis(500));
        cancel.cancel();
        let error = loop {
            if let StreamChunk::Error(message) = receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("cancellation error")
            {
                break message;
            }
        };
        assert!(error.contains("cancel"), "{error}");
    }

    #[test]
    fn cancelled_before_worker_start_reports_clear_classified_error() {
        let cancel = CancelToken::new();
        cancel.cancel();
        let runner = PiRpcRunner::new(PiRpcRunnerOptions {
            cancel,
            provider: Some("test-provider".into()),
            ..Default::default()
        });
        let receiver = runner.stream("cancelled".into(), None);
        let chunk = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("startup cancellation");
        assert!(
            matches!(&chunk, StreamChunk::Error(message) if message == "pi session cancelled before worker startup"),
            "{chunk:?}"
        );
    }
    #[cfg(unix)]
    #[test]
    fn cancellation_during_handshake_reports_cancellation_not_candidate_exit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = fake_script(
            dir.path(),
            "slow-handshake",
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *get_state*) sleep 60 ;;
  esac
done
"#,
        );
        let cancel = CancelToken::new();
        let runner = PiRpcRunner::new(PiRpcRunnerOptions {
            pi_bin: Some(script),
            working_directory: Some(dir.path().to_path_buf()),
            cancel: cancel.clone(),
            ..Default::default()
        });
        let receiver = runner.stream("cancelled".into(), None);
        thread::sleep(Duration::from_millis(100));
        cancel.cancel();
        let error = loop {
            if let StreamChunk::Error(message) = receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("handshake cancellation")
            {
                break message;
            }
        };
        assert_eq!(error, "pi session cancelled during handshake");
    }

    #[test]
    fn candidate_resolution_uses_configured_path_before_system_lookup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let program = dir.path().join("pi");
        std::fs::write(&program, b"").expect("program");
        let opts = PiRpcRunnerOptions {
            env: [("PATH".into(), dir.path().display().to_string())].into(),
            ..Default::default()
        };
        assert_eq!(resolve_candidate_program("pi", &opts.env), program);
    }
    #[cfg(unix)]
    #[test]
    fn shebang_launcher_resolves_runtime_interpreter_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let launcher = dir.path().join("launcher");
        std::fs::write(&launcher, b"#!/usr/bin/env sh\n").expect("launcher");
        assert!(runtime_path_for_candidate(&launcher).is_some());
    }

    #[test]
    fn bridge_rejects_bad_token_unknown_tool_and_panics() {
        let panic_tool = SeherTool::new(
            "panic",
            "panic",
            serde_json::json!({"type":"object"}),
            Arc::new(|_| -> Result<String, String> { panic!("boom") }),
        );
        let echo = SeherTool::new(
            "echo",
            "echo",
            serde_json::json!({"type":"object"}),
            Arc::new(|input| Ok(input.to_string())),
        );
        for (request, expected) in [
            (
                serde_json::json!({"token":"bad","tool":"echo"}),
                "invalid Seher tool bridge request",
            ),
            (
                serde_json::json!({"token":"token","tool":"missing"}),
                "unknown Seher tool",
            ),
            (
                serde_json::json!({"token":"token","tool":"panic"}),
                "Seher tool panicked",
            ),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let address = listener.local_addr().expect("address");
            let tools = vec![echo.clone(), panic_tool.clone()];
            let handle = thread::spawn(move || {
                let (stream, _) = listener.accept().expect("accept");
                bridge_connection(stream, "token", &tools);
            });
            let mut client = TcpStream::connect(address).expect("connect");
            write_json_line(&mut client, &request).expect("write");
            let mut response = String::new();
            BufReader::new(client)
                .read_line(&mut response)
                .expect("read");
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(response.trim()).expect("json")["error"],
                expected
            );
            handle.join().expect("bridge thread");
        }
    }
}
