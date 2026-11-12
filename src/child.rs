//! One running MCP server process, spoken to over stdio (JSON-RPC, one message per line).

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::config::ServerConfig;
use crate::winutil::{CREATE_NO_WINDOW, Job};

pub const PROTOCOL_VERSION: &str = "2025-06-18";

pub struct Child {
    pub pid: u32,
    pub started: Instant,
    /// The server's answer to `initialize` (capabilities, serverInfo, instructions).
    pub init: OnceLock<Value>,
    /// Requests sent and not yet answered; the idle timer never stops a busy child.
    pub in_flight: AtomicUsize,
    pub last_used: Mutex<Instant>,
    /// Set when the server says its tool/prompt/resource list changed.
    pub lists_changed: AtomicBool,
    alive: Arc<AtomicBool>,
    stdin: Mutex<ChildStdin>,
    pending: Arc<Mutex<HashMap<i64, Sender<Value>>>>,
    next_id: AtomicI64,
    stderr: Arc<Mutex<VecDeque<String>>>,
    job: Job,
    _process: Mutex<std::process::Child>,
}

impl Child {
    /// Starts the server and completes the MCP handshake.
    pub fn spawn(server: &ServerConfig) -> anyhow::Result<Arc<Child>> {
        let program = resolve_program(&server.command);
        let mut cmd = Command::new(&program);
        cmd.args(&server.args)
            .envs(&server.env)
            .current_dir(dirs::home_dir().unwrap_or_else(std::env::temp_dir))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW);
        let mut process = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("cannot start {}: {e}", program.display()))?;
        let job = Job::new()?;
        if let Err(err) = job.assign(&process) {
            log::warn!("{}: {err}; child processes may outlive a stop", server.name);
        }

        let stdin = process.stdin.take().expect("stdin is piped");
        let stdout = process.stdout.take().expect("stdout is piped");
        let stderr_pipe = process.stderr.take().expect("stderr is piped");
        let pid = process.id();

        let alive = Arc::new(AtomicBool::new(true));
        let pending: Arc<Mutex<HashMap<i64, Sender<Value>>>> = Arc::default();
        let stderr: Arc<Mutex<VecDeque<String>>> = Arc::default();

        // stderr: keep the last lines for error messages.
        {
            let stderr = stderr.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stderr_pipe).lines().map_while(Result::ok) {
                    let mut buf = stderr.lock().unwrap();
                    if buf.len() >= 40 {
                        buf.pop_front();
                    }
                    buf.push_back(line);
                }
            });
        }

        let child = Arc::new(Child {
            pid,
            started: Instant::now(),
            init: OnceLock::new(),
            in_flight: AtomicUsize::new(0),
            last_used: Mutex::new(Instant::now()),
            lists_changed: AtomicBool::new(false),
            alive: alive.clone(),
            stdin: Mutex::new(stdin),
            pending: pending.clone(),
            next_id: AtomicI64::new(1),
            stderr: stderr.clone(),
            job,
            _process: Mutex::new(process),
        });

        // stdout: route answers to waiting requests, answer the server's own requests.
        {
            let weak = Arc::downgrade(&child);
            let name = server.name.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                        continue; // servers sometimes print plain logs to stdout
                    };
                    let has_method = msg.get("method").is_some();
                    match (msg.get("id"), has_method) {
                        (Some(id), false) => {
                            if let Some(id) = id.as_i64()
                                && let Some(tx) = pending.lock().unwrap().remove(&id)
                            {
                                let _ = tx.send(msg);
                            }
                        }
                        (Some(id), true) => {
                            // A request from the server (roots, sampling, elicitation).
                            let reply = match msg["method"].as_str() {
                                Some("ping") => json!({"jsonrpc":"2.0","id":id,"result":{}}),
                                Some("roots/list") => json!({"jsonrpc":"2.0","id":id,"result":{"roots":[]}}),
                                _ => json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"not supported through Dozer"}}),
                            };
                            if let Some(c) = weak.upgrade() {
                                let _ = c.write(&reply);
                            }
                        }
                        (None, true) => {
                            if msg["method"].as_str().is_some_and(|m| m.ends_with("/list_changed"))
                                && let Some(c) = weak.upgrade()
                            {
                                c.lists_changed.store(true, Ordering::Relaxed);
                            }
                        }
                        _ => {}
                    }
                }
                alive.store(false, Ordering::Relaxed);
                // Wake everyone still waiting so they fail fast.
                for (_, tx) in pending.lock().unwrap().drain() {
                    let _ = tx.send(json!({"error":{"code":-32000,"message":format!("{name} exited")}}));
                }
            });
        }

        let init = child.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"roots": {"listChanged": false}},
                "clientInfo": {"name": "dozer", "version": env!("CARGO_PKG_VERSION")}
            }),
            Duration::from_secs(180),
        );
        let init = match init {
            Ok(v) if v.get("result").is_some() => v["result"].clone(),
            Ok(v) => {
                child.kill();
                anyhow::bail!("initialize failed: {}", v["error"]);
            }
            Err(err) => {
                let tail = child.stderr_tail();
                child.kill();
                anyhow::bail!("{err}{}", if tail.is_empty() { String::new() } else { format!(" — {tail}") });
            }
        };
        child.notify("notifications/initialized", json!({}));

        let _ = child.init.set(init);
        Ok(child)
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    pub fn stderr_tail(&self) -> String {
        let buf = self.stderr.lock().unwrap();
        buf.iter().rev().take(3).rev().cloned().collect::<Vec<_>>().join(" | ")
    }

    fn write(&self, msg: &Value) -> std::io::Result<()> {
        let mut stdin = self.stdin.lock().unwrap();
        let mut line = serde_json::to_vec(msg)?;
        line.push(b'\n');
        stdin.write_all(&line)?;
        stdin.flush()
    }

    pub fn notify(&self, method: &str, params: Value) {
        let _ = self.write(&json!({"jsonrpc":"2.0","method":method,"params":params}));
    }

    /// Sends a request and waits for the whole response message (`result` or `error`).
    pub fn request(&self, method: &str, params: Value, timeout: Duration) -> anyhow::Result<Value> {
        if !self.is_alive() {
            anyhow::bail!("server process has exited");
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.in_flight.fetch_add(1, Ordering::Relaxed);
        *self.last_used.lock().unwrap() = Instant::now();

        let mut msg = json!({"jsonrpc":"2.0","id":id,"method":method});
        if !params.is_null() {
            msg["params"] = params;
        }
        let result = match self.write(&msg) {
            Ok(()) => rx.recv_timeout(timeout).map_err(|_| anyhow::anyhow!("{method} timed out")),
            Err(err) => Err(anyhow::anyhow!("cannot write to server: {err}")),
        };
        self.pending.lock().unwrap().remove(&id);
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
        *self.last_used.lock().unwrap() = Instant::now();
        result
    }

    pub fn idle_for(&self) -> Duration {
        self.last_used.lock().unwrap().elapsed()
    }

    pub fn kill(&self) {
        self.alive.store(false, Ordering::Relaxed);
        self.job.terminate();
        if let Ok(mut p) = self._process.lock() {
            let _ = p.kill();
        }
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        self.kill();
    }
}

/// `npx` is really `npx.cmd`; std only adds `.exe` when searching PATH, so look it up here.
fn resolve_program(command: &str) -> PathBuf {
    let path = PathBuf::from(command);
    if path.extension().is_some() || path.is_absolute() {
        return path;
    }
    let exts = [".exe", ".cmd", ".bat", ".com"];
    if let Some(dirs) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&dirs) {
            for ext in exts {
                let candidate = dir.join(format!("{command}{ext}"));
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
    }
    path
}
