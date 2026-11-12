//! The local MCP gateway.
//!
//! Every agent session connects to `http://127.0.0.1:<port>/mcp/<name>` instead of
//! starting its own copy of each server. The gateway answers the handshake and the
//! tool lists from a cache, so connecting costs nothing. The real server process is
//! started only when an agent actually calls a tool, and stopped again after it has
//! been idle for the configured minutes.

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::child::{Child, PROTOCOL_VERSION};
use crate::config::{Config, Mode, ServerConfig, cache_dir};

const DISCOVER: &str = "server/discover";
const LIST_METHODS: [&str; 4] = ["tools/list", "prompts/list", "resources/list", "resources/templates/list"];
const CALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const SUPPORTED_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// What a server said about itself, kept on disk so later sessions need no process.
#[derive(Clone, Default, Serialize, Deserialize)]
struct Cache {
    /// Command line the cache belongs to; a config change invalidates it.
    key: String,
    init: Value,
    lists: HashMap<String, Value>,
}

#[derive(Default)]
struct ServerState {
    shared: Option<Arc<Child>>,
    /// Per-session children, keyed by the agent's MCP session id.
    sessions: HashMap<String, Arc<Child>>,
    cache: Option<Cache>,
    calls: u64,
    last_call: Option<SystemTime>,
    last_error: Option<String>,
    starts: u64,
}

#[derive(Clone)]
pub struct LogLine {
    pub time: SystemTime,
    pub server: String,
    pub text: String,
}

/// A snapshot for the dashboard.
#[derive(Clone)]
pub struct ServerView {
    pub config: ServerConfig,
    pub running: Vec<RunningChild>,
    pub cached_tools: Option<usize>,
    pub calls: u64,
    pub last_call: Option<SystemTime>,
    pub last_error: Option<String>,
    pub starts: u64,
    pub sessions_seen: usize,
}

#[derive(Clone)]
pub struct RunningChild {
    pub pid: u32,
    pub session: Option<String>,
    pub idle: Duration,
    pub uptime: Duration,
    pub busy: bool,
}

pub struct Hub {
    pub config: RwLock<Config>,
    states: Mutex<HashMap<String, ServerState>>,
    start_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    spawn_lock: Mutex<()>,
    sessions_seen: Mutex<HashMap<String, (String, Instant)>>,
    log: Mutex<VecDeque<LogLine>>,
    pub show_requested: AtomicBool,
    pub ui_ctx: Mutex<Option<eframe::egui::Context>>,
    next_session: AtomicU64,
    pub listening: AtomicBool,
}

impl Hub {
    pub fn new(config: Config) -> Arc<Self> {
        Arc::new(Self {
            config: RwLock::new(config),
            states: Mutex::default(),
            start_locks: Mutex::default(),
            spawn_lock: Mutex::default(),
            sessions_seen: Mutex::default(),
            log: Mutex::default(),
            show_requested: AtomicBool::new(false),
            ui_ctx: Mutex::default(),
            next_session: AtomicU64::new(1),
            listening: AtomicBool::new(false),
        })
    }

    pub fn log(&self, server: &str, text: impl Into<String>) {
        let text = text.into();
        log::info!("[{server}] {text}");
        let mut log = self.log.lock().unwrap();
        if log.len() >= 500 {
            log.pop_front();
        }
        let line = format!("{} [{server}] {text}\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
        log.push_back(LogLine { time: SystemTime::now(), server: server.to_string(), text });
        drop(log);
        append_log_file(&line);
        self.repaint();
    }

    pub fn log_lines(&self) -> Vec<LogLine> {
        self.log.lock().unwrap().iter().cloned().collect()
    }

    pub fn repaint(&self) {
        if let Some(ctx) = self.ui_ctx.lock().unwrap().as_ref() {
            ctx.request_repaint();
        }
    }

    fn server_config(&self, name: &str) -> Option<ServerConfig> {
        self.config.read().unwrap().server(name).cloned()
    }

    // ---------------------------------------------------------------- cache

    fn cache_key(server: &ServerConfig) -> String {
        format!("{} {}", server.command, server.args.join(" "))
    }

    fn cache(&self, server: &ServerConfig) -> Option<Cache> {
        let mut states = self.states.lock().unwrap();
        let state = states.entry(server.name.clone()).or_default();
        if state.cache.is_none() {
            let path = cache_dir().join(format!("{}.json", server.name));
            state.cache = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok());
        }
        state.cache.clone().filter(|c| c.key == Self::cache_key(server))
    }

    fn update_cache(&self, server: &ServerConfig, f: impl FnOnce(&mut Cache)) {
        let mut states = self.states.lock().unwrap();
        let state = states.entry(server.name.clone()).or_default();
        let key = Self::cache_key(server);
        let mut cache = state.cache.take().filter(|c| c.key == key).unwrap_or(Cache { key, ..Default::default() });
        f(&mut cache);
        let _ = std::fs::create_dir_all(cache_dir());
        if let Ok(text) = serde_json::to_string(&cache) {
            let _ = std::fs::write(cache_dir().join(format!("{}.json", server.name)), text);
        }
        state.cache = Some(cache);
    }

    pub fn clear_cache(&self, name: &str) {
        if let Some(state) = self.states.lock().unwrap().get_mut(name) {
            state.cache = None;
        }
        let _ = std::fs::remove_file(cache_dir().join(format!("{name}.json")));
    }

    // ------------------------------------------------------------- children

    /// Returns a running child for this server (and session, in per-session mode),
    /// starting one if needed. Only one start runs at a time per child slot.
    fn ensure_child(&self, server: &ServerConfig, session: &str, why: &str) -> anyhow::Result<Arc<Child>> {
        if !server.enabled {
            anyhow::bail!("{} is turned off in Dozer (open Dozer > MCP on demand > Turn on)", server.name);
        }
        let slot = match server.mode {
            Mode::Shared => server.name.clone(),
            Mode::PerSession => format!("{}/{session}", server.name),
        };
        let lock = self.start_locks.lock().unwrap().entry(slot).or_default().clone();
        let _guard = lock.lock().unwrap();

        if let Some(child) = self.current_child(server, session) {
            return Ok(child);
        }
        self.log(&server.name, format!("starting for {why}"));
        let started = Instant::now();
        // npx breaks when several copies unpack packages at the same moment, so starts
        // run one at a time across all servers, and a failed start is tried once more.
        let spawned = {
            let _one_at_a_time = self.spawn_lock.lock().unwrap();
            Child::spawn(server).or_else(|err| {
                self.log(&server.name, format!("start failed ({err}), trying again"));
                std::thread::sleep(Duration::from_secs(1));
                Child::spawn(server)
            })
        };
        let child = match spawned {
            Ok(c) => c,
            Err(err) => {
                let msg = err.to_string();
                self.log(&server.name, format!("failed to start: {msg}"));
                self.states.lock().unwrap().entry(server.name.clone()).or_default().last_error = Some(msg);
                return Err(err);
            }
        };
        self.log(&server.name, format!("ready in {:.1}s (pid {})", started.elapsed().as_secs_f32(), child.pid));
        {
            let mut states = self.states.lock().unwrap();
            let state = states.entry(server.name.clone()).or_default();
            state.starts += 1;
            state.last_error = None;
            match server.mode {
                Mode::Shared => state.shared = Some(child.clone()),
                Mode::PerSession => {
                    state.sessions.insert(session.to_string(), child.clone());
                }
            }
        }
        let init = child.init.get().cloned().unwrap_or(Value::Null);
        self.update_cache(server, |c| c.init = init);
        Ok(child)
    }

    fn current_child(&self, server: &ServerConfig, session: &str) -> Option<Arc<Child>> {
        let states = self.states.lock().unwrap();
        let state = states.get(&server.name)?;
        let child = match server.mode {
            Mode::Shared => state.shared.clone(),
            Mode::PerSession => state.sessions.get(session).cloned(),
        }?;
        child.is_alive().then_some(child)
    }

    /// Stops every process of a server now.
    pub fn stop(&self, name: &str, reason: &str) {
        let children: Vec<Arc<Child>> = {
            let mut states = self.states.lock().unwrap();
            let Some(state) = states.get_mut(name) else { return };
            state.shared.take().into_iter().chain(state.sessions.drain().map(|(_, c)| c)).collect()
        };
        if !children.is_empty() {
            for c in &children {
                c.kill();
            }
            self.log(name, format!("stopped {} process(es): {reason}", children.len()));
        }
    }

    pub fn stop_all(&self) {
        let names: Vec<String> = self.states.lock().unwrap().keys().cloned().collect();
        for name in names {
            self.stop(&name, "Dozer is closing");
        }
    }

    /// Starts a server, reads its lists into the cache, and stops it again
    /// unless something else is using it.
    pub fn warm_up(self: &Arc<Self>, name: &str) {
        let Some(server) = self.server_config(name) else { return };
        let hub = self.clone();
        std::thread::spawn(move || {
            let was_running = hub.current_child(&server, "warmup").is_some();
            match hub.ensure_child(&server, "warmup", "a tool-list refresh") {
                Ok(child) => {
                    let n = hub.refresh_lists(&server, &child);
                    hub.log(&server.name, format!("cached {n} tools"));
                    if !was_running && server.mode == Mode::Shared && child.in_flight.load(Ordering::Relaxed) == 0 {
                        hub.stop(&server.name, "warm-up done");
                    }
                    if server.mode == Mode::PerSession {
                        hub.drop_session_child(&server.name, "warmup");
                    }
                }
                Err(_) => {}
            }
        });
    }

    fn drop_session_child(&self, name: &str, session: &str) {
        let child = self.states.lock().unwrap().get_mut(name).and_then(|s| s.sessions.remove(session));
        if let Some(c) = child {
            c.kill();
            self.log(name, format!("stopped session process (pid {})", c.pid));
        }
    }

    /// Asks a running child for its lists and stores them. Returns the tool count.
    fn refresh_lists(&self, server: &ServerConfig, child: &Child) -> usize {
        let caps = child.init.get().map(|i| i["capabilities"].clone()).unwrap_or(Value::Null);
        let mut lists = HashMap::new();
        for method in LIST_METHODS {
            let cap = method.split('/').next().unwrap_or("");
            if caps.get(cap).is_none() {
                continue;
            }
            if let Ok(resp) = child.request(method, json!({}), Duration::from_secs(60))
                && let Some(result) = resp.get("result")
            {
                lists.insert(method.to_string(), result.clone());
            }
        }
        // Newer clients probe `server/discover`; keep the server's own answer (result or error).
        if let Ok(resp) = child.request(DISCOVER, json!({}), Duration::from_secs(30)) {
            let mut keep = json!({});
            if let Some(r) = resp.get("result") {
                keep["result"] = r.clone();
            } else if let Some(e) = resp.get("error") {
                keep["error"] = e.clone();
            }
            lists.insert(DISCOVER.to_string(), keep);
        }
        let tools = lists.get("tools/list").and_then(|r| r["tools"].as_array()).map_or(0, |a| a.len());
        self.update_cache(server, |c| c.lists.extend(lists));
        child.lists_changed.store(false, Ordering::Relaxed);
        tools
    }

    /// Stops processes that have had no tool call for their idle time.
    fn reap_idle(&self) {
        let config = self.config.read().unwrap().clone();
        let mut to_kill: Vec<(String, Arc<Child>, &'static str)> = Vec::new();
        {
            let mut states = self.states.lock().unwrap();
            for (name, state) in states.iter_mut() {
                let limit = config
                    .server(name)
                    .map(|s| Duration::from_secs(u64::from(s.idle_minutes.max(1)) * 60))
                    .unwrap_or(Duration::from_secs(60));
                let idle = |c: &Arc<Child>| c.in_flight.load(Ordering::Relaxed) == 0 && c.idle_for() >= limit;
                if let Some(c) = &state.shared
                    && (!c.is_alive() || idle(c))
                {
                    let reason = if c.is_alive() { "idle" } else { "exited" };
                    to_kill.push((name.clone(), state.shared.take().unwrap(), reason));
                }
                let dead: Vec<String> = state
                    .sessions
                    .iter()
                    .filter(|(_, c)| !c.is_alive() || idle(c))
                    .map(|(k, _)| k.clone())
                    .collect();
                for key in dead {
                    let c = state.sessions.remove(&key).unwrap();
                    let reason = if c.is_alive() { "idle" } else { "exited" };
                    to_kill.push((name.clone(), c, reason));
                }
            }
        }
        for (name, child, reason) in to_kill {
            let mins = child.idle_for().as_secs() / 60;
            child.kill();
            if reason == "idle" {
                self.log(&name, format!("stopped pid {} after {mins} min without use", child.pid));
            } else {
                self.log(&name, format!("process {} exited: {}", child.pid, child.stderr_tail()));
            }
        }
        // Forget sessions nobody used for a day.
        self.sessions_seen.lock().unwrap().retain(|_, (_, t)| t.elapsed() < Duration::from_secs(86_400));
    }

    pub fn views(&self) -> Vec<ServerView> {
        let config = self.config.read().unwrap().clone();
        let seen = self.sessions_seen.lock().unwrap();
        let states = self.states.lock().unwrap();
        config
            .servers
            .iter()
            .map(|server| {
                let state = states.get(&server.name);
                let mut running = Vec::new();
                if let Some(state) = state {
                    let entries = state
                        .shared
                        .iter()
                        .map(|c| (None, c))
                        .chain(state.sessions.iter().map(|(k, c)| (Some(k.clone()), c)));
                    for (session, c) in entries {
                        if c.is_alive() {
                            running.push(RunningChild {
                                pid: c.pid,
                                session,
                                idle: c.idle_for(),
                                uptime: c.started.elapsed(),
                                busy: c.in_flight.load(Ordering::Relaxed) > 0,
                            });
                        }
                    }
                }
                let cache = state.and_then(|s| s.cache.as_ref()).filter(|c| c.key == Self::cache_key(server));
                ServerView {
                    config: server.clone(),
                    running,
                    cached_tools: cache
                        .and_then(|c| c.lists.get("tools/list"))
                        .and_then(|r| r["tools"].as_array())
                        .map(|a| a.len()),
                    calls: state.map_or(0, |s| s.calls),
                    last_call: state.and_then(|s| s.last_call),
                    last_error: state.and_then(|s| s.last_error.clone()),
                    starts: state.map_or(0, |s| s.starts),
                    sessions_seen: seen.values().filter(|(n, _)| n == &server.name).count(),
                }
            })
            .collect()
    }

    /// Loads caches from disk so the dashboard can show tool counts at once.
    pub fn preload_caches(&self) {
        let servers = self.config.read().unwrap().servers.clone();
        for s in &servers {
            let _ = self.cache(s);
        }
    }

    // ------------------------------------------------------------ protocol

    fn handle_message(&self, server: &ServerConfig, session: &str, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        let Some(id) = id else {
            // Notification: pass cancellations on to a running process, drop the rest.
            if method == "notifications/cancelled"
                && let Some(c) = self.current_child(server, session)
            {
                c.notify(method, params);
            }
            return None;
        };
        if msg.get("method").is_none() {
            return None; // a response to something we never asked
        }

        let reply = |result: anyhow::Result<Value>| -> Value {
            match result {
                Ok(resp) => {
                    let mut out = json!({"jsonrpc":"2.0","id":id});
                    if let Some(r) = resp.get("result") {
                        out["result"] = r.clone();
                    } else if let Some(e) = resp.get("error") {
                        out["error"] = e.clone();
                    } else {
                        out["result"] = resp;
                    }
                    out
                }
                Err(err) => json!({"jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":err.to_string()}}),
            }
        };

        match method {
            "ping" | "logging/setLevel" => Some(json!({"jsonrpc":"2.0","id":id,"result":{}})),
            DISCOVER => {
                // Replay what the server answered before; never start a process just to be probed.
                let cached = self.cache(server).and_then(|c| c.lists.get(DISCOVER).cloned());
                Some(reply(Ok(cached.unwrap_or_else(|| {
                    json!({"error": {"code": -32601, "message": "Method not found: server/discover"}})
                }))))
            }
            "initialize" => Some(reply(self.initialize(server, session, &params).map(|r| json!({"result": r})))),
            m if LIST_METHODS.contains(&m) && params.get("cursor").is_none() => Some(reply(self.list(server, session, m))),
            _ => {
                {
                    let mut states = self.states.lock().unwrap();
                    let state = states.entry(server.name.clone()).or_default();
                    state.calls += 1;
                    state.last_call = Some(SystemTime::now());
                }
                let why = match params.get("name").and_then(|n| n.as_str()) {
                    Some(tool) => format!("{method} {tool}"),
                    None => method.to_string(),
                };
                let result = self.ensure_child(server, session, &why).and_then(|child| {
                    let resp = child.request(method, params, CALL_TIMEOUT);
                    if child.lists_changed.load(Ordering::Relaxed) {
                        self.refresh_lists(server, &child);
                    }
                    resp
                });
                if let Err(err) = &result {
                    self.states.lock().unwrap().entry(server.name.clone()).or_default().last_error = Some(err.to_string());
                }
                self.repaint();
                Some(reply(result))
            }
        }
    }

    fn initialize(&self, server: &ServerConfig, session: &str, params: &Value) -> anyhow::Result<Value> {
        let init = match self.cache(server).map(|c| c.init).filter(|i| !i.is_null()) {
            Some(init) => init,
            None => {
                let child = self.ensure_child(server, session, "a first connection (nothing cached yet)")?;
                let init = child.init.get().cloned().unwrap_or(Value::Null);
                // Fill the lists while the process is up, so the next session needs none.
                self.refresh_lists(server, &child);
                init
            }
        };
        let mut result = init;
        let asked = params["protocolVersion"].as_str().unwrap_or(PROTOCOL_VERSION);
        let version = if SUPPORTED_VERSIONS.contains(&asked) { asked } else { PROTOCOL_VERSION };
        result["protocolVersion"] = json!(version);
        // The gateway does not stream notifications back, so don't promise them.
        if let Some(caps) = result.get_mut("capabilities").and_then(|c| c.as_object_mut()) {
            for cap in ["tools", "prompts", "resources"] {
                if let Some(c) = caps.get_mut(cap).and_then(|c| c.as_object_mut()) {
                    c.remove("listChanged");
                    c.remove("subscribe");
                }
            }
        }
        Ok(result)
    }

    fn list(&self, server: &ServerConfig, session: &str, method: &str) -> anyhow::Result<Value> {
        if let Some(cache) = self.cache(server)
            && !cache.init.is_null()
        {
            let cap = method.split('/').next().unwrap_or("");
            if cache.init["capabilities"].get(cap).is_none() {
                let key = match method {
                    "resources/templates/list" => "resourceTemplates",
                    m => m.split('/').next().unwrap_or("tools"),
                };
                return Ok(json!({"result": {key: []}}));
            }
        }
        if let Some(cache) = self.cache(server)
            && let Some(result) = cache.lists.get(method)
        {
            let changed = self.current_child(server, session).is_some_and(|c| c.lists_changed.load(Ordering::Relaxed));
            if !changed {
                return Ok(json!({"result": result}));
            }
        }
        let child = self.ensure_child(server, session, &format!("{method} (not cached)"))?;
        let resp = child.request(method, json!({}), Duration::from_secs(120))?;
        if let Some(result) = resp.get("result") {
            let result = result.clone();
            self.update_cache(server, |c| {
                c.lists.insert(method.to_string(), result);
            });
        }
        Ok(resp)
    }

    // ---------------------------------------------------------------- http

    fn handle_http(self: &Arc<Self>, mut req: Request) {
        let port = self.config.read().unwrap().port;
        let header = |req: &Request, name: &str| {
            req.headers()
                .iter()
                .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
                .map(|h| h.value.as_str().to_string())
        };

        // Only this machine, and never a web page (blocks DNS rebinding and CSRF).
        let host_ok = header(&req, "host").is_some_and(|h| h == format!("127.0.0.1:{port}") || h == format!("localhost:{port}"));
        let origin_ok = header(&req, "origin").is_none_or(|o| o == format!("http://127.0.0.1:{port}") || o == format!("http://localhost:{port}"));
        if !host_ok || !origin_ok {
            let _ = req.respond(Response::from_string("forbidden").with_status_code(403));
            return;
        }

        let url = req.url().to_string();
        let path = url.split('?').next().unwrap_or("").to_string();
        let method = req.method().clone();

        if path == "/health" {
            let _ = req.respond(json_response(&json!({"app": "dozer", "version": env!("CARGO_PKG_VERSION")}), 200));
            return;
        }
        if path == "/show" && method == Method::Post && header(&req, "x-dozer").is_some() {
            self.show_requested.store(true, Ordering::Relaxed);
            self.repaint();
            let _ = req.respond(Response::empty(204));
            return;
        }

        let Some(name) = path.strip_prefix("/mcp/").map(|n| n.trim_end_matches('/').to_string()) else {
            let _ = req.respond(Response::from_string("not found").with_status_code(404));
            return;
        };
        let Some(server) = self.server_config(&name) else {
            let _ = req.respond(Response::from_string(format!("Dozer has no server named {name}")).with_status_code(404));
            return;
        };

        let session = header(&req, "mcp-session-id");
        match method {
            Method::Get => {
                // No server-to-client stream; agents fall back to plain responses.
                let _ = req.respond(Response::empty(405).with_header(hdr("Allow", "POST, DELETE")));
            }
            Method::Delete => {
                if let Some(s) = session {
                    if server.mode == Mode::PerSession {
                        self.drop_session_child(&name, &s);
                    }
                    self.sessions_seen.lock().unwrap().remove(&s);
                }
                let _ = req.respond(Response::empty(200));
            }
            Method::Post => {
                let mut body = String::new();
                if req.as_reader().read_to_string(&mut body).is_err() {
                    let _ = req.respond(Response::from_string("bad body").with_status_code(400));
                    return;
                }
                let Ok(parsed) = serde_json::from_str::<Value>(&body) else {
                    let err = json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}});
                    let _ = req.respond(json_response(&err, 400));
                    return;
                };
                let messages = match &parsed {
                    Value::Array(a) => a.clone(),
                    other => vec![other.clone()],
                };
                let is_init = messages.iter().any(|m| m["method"] == "initialize");
                let session = match (session, is_init) {
                    (_, true) => format!(
                        "{:x}{:x}",
                        SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_nanos()),
                        self.next_session.fetch_add(1, Ordering::Relaxed)
                    ),
                    (Some(s), false) => s,
                    // A client that skipped the session header: treat as one shared session.
                    (None, false) => "default".to_string(),
                };
                self.sessions_seen.lock().unwrap().insert(session.clone(), (name.clone(), Instant::now()));

                let replies: Vec<Value> =
                    messages.iter().filter_map(|m| self.handle_message(&server, &session, m)).collect();
                let mut response = if replies.is_empty() {
                    Response::from_data(Vec::new()).with_status_code(202)
                } else if matches!(parsed, Value::Array(_)) {
                    json_response(&Value::Array(replies), 200)
                } else {
                    json_response(&replies[0], 200)
                };
                if is_init {
                    response = response.with_header(hdr("Mcp-Session-Id", &session));
                }
                let _ = req.respond(response);
            }
            _ => {
                let _ = req.respond(Response::empty(405));
            }
        }
    }
}

fn hdr(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid header")
}

fn json_response(value: &Value, status: u16) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_data(serde_json::to_vec(value).unwrap_or_default())
        .with_status_code(status)
        .with_header(hdr("Content-Type", "application/json"))
}

/// Binds the port. Fails if another Dozer (or anything else) already has it.
pub fn bind(port: u16) -> anyhow::Result<Server> {
    Server::http(("127.0.0.1", port)).map_err(|e| anyhow::anyhow!("port {port} is busy: {e}"))
}

pub fn run(hub: Arc<Hub>, server: Server) {
    hub.listening.store(true, Ordering::Relaxed);
    let reaper = hub.clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(10));
            reaper.reap_idle();
        }
    });
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            let hub = hub.clone();
            std::thread::spawn(move || hub.handle_http(req));
        }
    });
}

/// Asks an already running Dozer to show its window. True if one answered.
pub fn ask_running_instance_to_show(port: u16) -> bool {
    use std::io::Write;
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(1)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let req = format!("POST /show HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Dozer: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 32];
    let n = stream.read(&mut buf).unwrap_or(0);
    String::from_utf8_lossy(&buf[..n]).contains(" 204")
}

/// Appends to `%APPDATA%\\Dozer\\activity.log`, starting over when it passes 2 MB.
fn append_log_file(line: &str) {
    use std::io::Write;
    let path = crate::config::data_dir().join("activity.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 2 * 1024 * 1024) {
        let _ = std::fs::rename(&path, path.with_extension("old.log"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}
