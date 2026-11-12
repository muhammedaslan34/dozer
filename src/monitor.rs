//! Reads every process and sorts them into groups a person can act on.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::config::{ServerConfig, signature};
use crate::winutil::{MemoryStatus, memory_status};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Category {
    McpDirect,
    Dozer,
    StuckGit,
    Agents,
    DevServers,
    Browsers,
    Containers,
    Other,
}

impl Category {
    pub fn title(self) -> &'static str {
        match self {
            Category::McpDirect => "MCP servers started by each agent session",
            Category::Dozer => "MCP servers run by Dozer (on demand)",
            Category::StuckGit => "Stuck git scans (their parent has closed)",
            Category::Agents => "AI agents and their apps",
            Category::DevServers => "Dev servers, watchers and tests",
            Category::Browsers => "Browsers and web views",
            Category::Containers => "Docker and WSL",
            Category::Other => "Everything else",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Category::McpDirect => {
                "Every open Claude/Codex session starts its own copy of each server. Route them through Dozer so they start only when a tool is used."
            }
            Category::Dozer => "Started when an agent called a tool; stopped automatically after the idle time.",
            Category::StuckGit => "Usually `git ls-files` over a huge folder. Safe to end.",
            Category::Agents => "Close the sessions you no longer use; each one also keeps its own MCP servers.",
            Category::DevServers => "Dev servers you started. End the ones you are not working on.",
            Category::Browsers => "",
            Category::Containers => "WSL keeps memory until it is shut down: `wsl --shutdown`.",
            Category::Other => "",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Category::McpDirect => "MCP (per session)",
            Category::Dozer => "MCP (Dozer)",
            Category::StuckGit => "Stuck git",
            Category::Agents => "AI agent",
            Category::DevServers => "Dev server",
            Category::Browsers => "Browser",
            Category::Containers => "Docker/WSL",
            Category::Other => "App",
        }
    }
}

/// Whether ending a group is a good idea.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Safety {
    /// Nothing is lost; it comes back by itself when needed.
    Safe,
    /// Fine to close, but save your work in it first.
    Caution,
    /// Part of Windows or of this app. Ending it can crash or log you out.
    Keep,
}

impl Safety {
    pub fn label(self) -> &'static str {
        match self {
            Safety::Safe => "Safe to close",
            Safety::Caution => "Save work first",
            Safety::Keep => "Keep running",
        }
    }
}

/// Windows processes that must not be ended (crash, logout or restart).
const SYSTEM: [&str; 34] = [
    "system", "registry", "secure system", "memory compression", "idle", "smss.exe", "csrss.exe",
    "wininit.exe", "winlogon.exe", "services.exe", "lsass.exe", "lsaiso.exe", "svchost.exe",
    "dwm.exe", "fontdrvhost.exe", "msmpeng.exe", "nissrv.exe", "mpdefendercoreservice.exe",
    "securityhealthservice.exe", "spoolsv.exe", "wudfhost.exe", "audiodg.exe", "sihost.exe",
    "ctfmon.exe", "taskhostw.exe", "runtimebroker.exe", "dllhost.exe", "wmiprvse.exe",
    "searchindexer.exe", "nvdisplay.container.exe", "conhost.exe", "explorer.exe",
    "textinputhost.exe", "lsm.exe",
];

/// Windows parts that come back by themselves if ended.
const RESTARTS: [&str; 8] = [
    "searchhost.exe", "startmenuexperiencehost.exe", "shellexperiencehost.exe", "widgets.exe",
    "widgetboard.exe", "phoneexperiencehost.exe", "gamebar.exe", "useroobebroker.exe",
];

pub fn safety(category: Category, name: &str) -> (Safety, &'static str) {
    let n = name.to_lowercase();
    match category {
        Category::McpDirect => (Safety::Safe, "Only the agent session that started it loses those tools until it restarts."),
        Category::Dozer => (Safety::Safe, "Dozer starts it again on the next tool call."),
        Category::StuckGit => (Safety::Safe, "A leftover scan whose parent already closed. Nothing waits for it."),
        Category::DevServers => (Safety::Caution, "A dev server or test run you started. Fine to end if you are not using it."),
        Category::Agents if n == "dozer" => (Safety::Keep, "This app. Agents routed through Dozer need it."),
        Category::Agents => (Safety::Caution, "Ends the agent sessions and any task they are running."),
        Category::Browsers if n.starts_with("webview2") => (
            Safety::Caution,
            "The Edge web engine that draws this app's window. Ending it blanks the app; close the app itself instead.",
        ),
        Category::Browsers => (Safety::Caution, "Closes the browser. Tabs can be restored, unsaved forms are lost."),
        Category::Containers => (Safety::Caution, "Stops running containers and Linux shells. Prefer `wsl --shutdown` or quitting Docker."),
        Category::Other if SYSTEM.contains(&n.as_str()) => (Safety::Keep, "Part of Windows. Ending it can crash or sign you out."),
        Category::Other if RESTARTS.contains(&n.as_str()) => (Safety::Safe, "Part of Windows that restarts by itself when needed."),
        Category::Other => (Safety::Caution, "An app or helper. Save your work in it before closing."),
    }
}

#[derive(Clone, Debug)]
pub struct Proc {
    pub pid: u32,
    pub parent: Option<u32>,
    pub name: String,
    pub cmd: String,
    pub memory: u64,
    pub cwd: Option<String>,
    /// Total CPU time used so far, in milliseconds.
    pub cpu_ms: u64,
    /// Seconds since 1970.
    pub start_time: u64,
}

#[derive(Clone, Debug)]
pub struct Group {
    pub category: Category,
    pub label: String,
    pub memory: u64,
    pub procs: Vec<Proc>,
    pub safety: Safety,
    pub reason: &'static str,
    /// Top-most processes of the group; ending these ends the rest.
    pub roots: Vec<u32>,
}

#[derive(Clone, Default)]
pub struct Snapshot {
    pub memory: MemoryStatus,
    pub groups: Vec<Group>,
    pub process_count: usize,
    pub taken: Option<Instant>,
    pub cpu: f32,
    /// (RAM %, CPU %) samples, oldest first.
    pub history: Vec<(f32, f32)>,
    pub sessions: Vec<crate::sessions::AgentSession>,
}

impl Snapshot {
    pub fn category_total(&self, c: Category) -> (u64, usize) {
        self.groups
            .iter()
            .filter(|g| g.category == c)
            .fold((0, 0), |(m, n), g| (m + g.memory, n + g.procs.len()))
    }
}

pub struct Monitor {
    pub snapshot: Mutex<Arc<Snapshot>>,
    /// Scan often while the window is open, rarely while it is hidden.
    pub fast: AtomicBool,
    pub servers: Mutex<Vec<ServerConfig>>,
}

impl Monitor {
    pub fn start(servers: Vec<ServerConfig>, on_update: impl Fn() + Send + 'static) -> Arc<Self> {
        let monitor = Arc::new(Self {
            snapshot: Mutex::new(Arc::new(Snapshot::default())),
            fast: AtomicBool::new(true),
            servers: Mutex::new(servers),
        });
        let m = monitor.clone();
        std::thread::spawn(move || {
            let Ok(mut sys) = System::new() else {
                log::error!("cannot read processes");
                return;
            };
            let mut tracker = crate::sessions::Tracker::default();
            let mut history: std::collections::VecDeque<(f32, f32)> = Default::default();
            let mut first = true;
            loop {
                let servers = m.servers.lock().unwrap().clone();
                let mut snap = scan(&mut sys, &servers, &mut tracker);
                let mem = snap.memory;
                let ram = if mem.total > 0 { (mem.total - mem.available) as f32 * 100.0 / mem.total as f32 } else { 0.0 };
                if history.len() >= 120 {
                    history.pop_front();
                }
                // The first CPU reading has nothing to compare with and is meaningless.
                if first {
                    first = false;
                } else {
                    history.push_back((ram, snap.cpu));
                }
                snap.history = history.iter().copied().collect();
                *m.snapshot.lock().unwrap() = Arc::new(snap);
                on_update();
                let wait = if m.fast.load(Ordering::Relaxed) { 2 } else { 30 };
                std::thread::sleep(Duration::from_secs(wait));
            }
        });
        monitor
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot.lock().unwrap().clone()
    }
}

fn scan(sys: &mut System, servers: &[ServerConfig], tracker: &mut crate::sessions::Tracker) -> Snapshot {
    sys.refresh_cpu_usage();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_memory()
            .with_cpu()
            .with_cmd(UpdateKind::OnlyIfNotSet)
            .with_exe(UpdateKind::OnlyIfNotSet)
            .with_cwd(UpdateKind::OnlyIfNotSet),
    );
    let procs: HashMap<u32, Proc> = sys
        .processes()
        .iter()
        .map(|(pid, p)| {
            let mut cmd: String = p
                .cmd()
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" ");
            if cmd.is_empty()
                && let Some(exe) = p.exe()
            {
                cmd = exe.to_string_lossy().into_owned();
            }
            let pid = pid.as_u32();
            (
                pid,
                Proc {
                    pid,
                    parent: p.parent().map(|x| x.as_u32()),
                    name: p.name().to_string_lossy().into_owned(),
                    cmd,
                    memory: p.memory(),
                    cwd: p.cwd().map(|c| c.to_string_lossy().into_owned()).filter(|c| !c.is_empty()),
                    cpu_ms: p.accumulated_cpu_time(),
                    start_time: p.start_time(),
                },
            )
        })
        .collect();
    let groups = classify(&procs, servers, std::process::id());
    let sessions = crate::sessions::detect(&procs, tracker);
    Snapshot {
        memory: memory_status(),
        groups,
        process_count: procs.len(),
        taken: Some(Instant::now()),
        cpu: sys.global_cpu_usage(),
        history: Vec::new(),
        sessions,
    }
}

/// Signature -> the configured server names that share it (e.g. one package added three times).
fn signatures(servers: &[ServerConfig]) -> Vec<(String, Vec<String>)> {
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for s in servers {
        let sig = signature(s);
        if sig.len() >= 3 {
            map.entry(sig).or_default().push(s.name.clone());
        }
    }
    let mut v: Vec<_> = map.into_iter().collect();
    v.sort_by_key(|(sig, _)| std::cmp::Reverse(sig.len()));
    v
}

pub fn classify(procs: &HashMap<u32, Proc>, servers: &[ServerConfig], hub_pid: u32) -> Vec<Group> {
    let sigs = signatures(servers);
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for p in procs.values() {
        if let Some(pp) = p.parent {
            children.entry(pp).or_default().push(p.pid);
        }
    }
    let mut under_hub = HashSet::new();
    let mut stack = vec![hub_pid];
    while let Some(pid) = stack.pop() {
        for &c in children.get(&pid).into_iter().flatten() {
            if under_hub.insert(c) {
                stack.push(c);
            }
        }
    }

    // First pass: classify by the process itself.
    let mut key_of: HashMap<u32, (Category, String)> = HashMap::new();
    for p in procs.values() {
        if p.pid == hub_pid {
            key_of.insert(p.pid, (Category::Agents, "Dozer".into()));
            continue;
        }
        if let Some(k) = classify_one(p, procs, &sigs, under_hub.contains(&p.pid)) {
            key_of.insert(p.pid, k);
        }
    }
    // Second pass: console hosts and shell wrappers belong to whatever they serve.
    let mut resolved: HashMap<u32, (Category, String)> = HashMap::new();
    for p in procs.values() {
        let mut key = key_of.get(&p.pid).cloned();
        if key.is_none() {
            let wrapper = matches!(p.name.to_lowercase().as_str(), "conhost.exe" | "cmd.exe" | "openconsole.exe");
            if wrapper {
                // Inherit from the parent, or from a classified child (cmd /c npx ...).
                let mut cur = p.parent;
                for _ in 0..4 {
                    let Some(pp) = cur else { break };
                    if let Some(k) = key_of.get(&pp) {
                        key = Some(k.clone());
                        break;
                    }
                    cur = procs.get(&pp).and_then(|x| x.parent);
                }
                if key.is_none() {
                    key = children.get(&p.pid).into_iter().flatten().find_map(|c| key_of.get(c).cloned());
                }
            }
        }
        let key = key.unwrap_or_else(|| (Category::Other, p.name.clone()));
        resolved.insert(p.pid, key);
    }

    let mut groups: HashMap<(Category, String), Group> = HashMap::new();
    for p in procs.values() {
        let (category, label) = resolved[&p.pid].clone();
        let g = groups.entry((category, label.clone())).or_insert_with(|| {
            let (safety, reason) = safety(category, &label);
            Group { category, label, safety, reason, memory: 0, procs: Vec::new(), roots: Vec::new() }
        });
        g.memory += p.memory;
        g.procs.push(p.clone());
    }
    let mut out: Vec<Group> = groups.into_values().collect();
    for g in &mut out {
        let members: HashSet<u32> = g.procs.iter().map(|p| p.pid).collect();
        g.roots = g
            .procs
            .iter()
            .filter(|p| p.parent.is_none_or(|pp| !members.contains(&pp)))
            .map(|p| p.pid)
            .collect();
        g.procs.sort_by_key(|p| std::cmp::Reverse(p.memory));
    }
    out.sort_by(|a, b| a.category.cmp(&b.category).then(b.memory.cmp(&a.memory)));
    out
}

fn classify_one(p: &Proc, procs: &HashMap<u32, Proc>, sigs: &[(String, Vec<String>)], under_hub: bool) -> Option<(Category, String)> {
    let name = p.name.to_lowercase();
    let cmd = p.cmd.replace('\\', "/").to_lowercase();
    let is_runtime = matches!(name.as_str(), "node.exe" | "cmd.exe" | "python.exe" | "uv.exe" | "uvx.exe" | "bun.exe" | "deno.exe");

    if is_runtime {
        for (sig, names) in sigs {
            if cmd.contains(sig.as_str()) {
                let cat = if under_hub { Category::Dozer } else { Category::McpDirect };
                return Some((cat, names.join(", ")));
            }
        }
        if !under_hub && cmd.contains("mcp") && !cmd.contains("next-devtools") {
            return Some((Category::McpDirect, guess_mcp_name(&cmd)));
        }
    }
    if under_hub {
        return Some((Category::Dozer, "other".into()));
    }
    if name == "git.exe" && p.parent.is_none_or(|pp| !procs.contains_key(&pp)) {
        return Some((Category::StuckGit, "git".into()));
    }
    let agent = if name == "claude.exe" {
        if cmd.contains("windowsapps") {
            Some("Claude desktop app")
        } else if cmd.contains("stream-json") {
            Some("Claude Code sessions (desktop app)")
        } else {
            Some("Claude Code sessions (terminal)")
        }
    } else if name.starts_with("codex") || cmd.contains("@openai/codex") || cmd.contains("/openai/codex/") {
        Some("Codex")
    } else if name.starts_with("opencode") {
        Some("OpenCode")
    } else if name.starts_with("cursor") {
        Some("Cursor")
    } else if cmd.contains("/fleetai/") || name.starts_with("fleetai") {
        Some("FleetAI")
    } else if name.starts_with("kimi") || name.starts_with("qoder") || name.starts_with("grok") || name.starts_with("gemini") {
        Some("Other AI CLIs")
    } else {
        None
    };
    if let Some(a) = agent {
        return Some((Category::Agents, a.into()));
    }
    if name == "node.exe" || name == "esbuild.exe" {
        let dev = ["vite", "next", "tsx", "vitest", "turbo", "webpack", "nodemon", "jest", "run dev", "pnpm.mjs", "npm-cli.js", "tinypool", "esbuild", "astro", "nuxt", "expo", "metro"];
        if dev.iter().any(|d| cmd.contains(d)) {
            return Some((Category::DevServers, project_name(&cmd)));
        }
    }
    if name == "msedgewebview2.exe" {
        // The Edge engine inside another app: name it after that app.
        let mut cur = p.parent.and_then(|pp| procs.get(&pp));
        for _ in 0..6 {
            match cur {
                Some(x) if x.name.eq_ignore_ascii_case("msedgewebview2.exe") => cur = x.parent.and_then(|pp| procs.get(&pp)),
                _ => break,
            }
        }
        let owner = cur.map(|x| x.name.trim_end_matches(".exe").trim_end_matches(".Root").to_string()).unwrap_or_else(|| "closed app".into());
        return Some((Category::Browsers, format!("WebView2 \u{00b7} {owner}")));
    }
    let browser = ["chrome.exe", "msedge.exe", "firefox.exe", "brave.exe", "opera.exe"];
    if browser.contains(&name.as_str()) {
        return Some((Category::Browsers, name.trim_end_matches(".exe").to_string()));
    }
    if name.starts_with("vmmem") || name.contains("docker") || name == "wslservice.exe" || name == "wslhost.exe" {
        return Some((Category::Containers, name.trim_end_matches(".exe").to_string()));
    }
    None
}

fn guess_mcp_name(cmd: &str) -> String {
    cmd.split([' ', '"'])
        .filter(|t| t.contains("mcp"))
        .map(|t| t.trim_end_matches('/'))
        .map(|t| t.rsplit("/node_modules/").next().unwrap_or(t))
        .find(|t| !t.is_empty())
        .map(|t| t.chars().take(48).collect())
        .unwrap_or_else(|| "unknown MCP".into())
}

/// `D:/projects/x/app/node_modules/...` -> `x/app`.
fn project_name(cmd: &str) -> String {
    for token in cmd.split([' ', '"']) {
        if let Some(i) = token.find("/node_modules/") {
            let root = &token[..i];
            if root.contains("/appdata/roaming/npm") || root.contains("npm-cache") || root.ends_with("/nodejs") {
                return "npm / npx commands".into();
            }
            let parts: Vec<&str> = root.split('/').filter(|s| !s.is_empty()).collect();
            let tail = &parts[parts.len().saturating_sub(2)..];
            return tail.join("/");
        }
    }
    "node".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Mode;

    fn proc(pid: u32, parent: Option<u32>, name: &str, cmd: &str) -> Proc {
        Proc { pid, parent, name: name.into(), cmd: cmd.into(), memory: 100, cwd: None, cpu_ms: 0, start_time: 0 }
    }

    #[test]
    fn groups_mcp_wrappers_and_stuck_git() {
        let servers = vec![ServerConfig {
            name: "coolify".into(),
            command: "npx".into(),
            args: vec!["-y".into(), "@masonator/coolify-mcp".into()],
            env: Default::default(),
            mode: Mode::Shared,
            idle_minutes: 5,
            enabled: true,
            routed: false,
        }];
        let mut procs = HashMap::new();
        for p in [
            proc(1, None, "claude.exe", "C:/Users/u/.local/bin/claude.exe"),
            proc(2, Some(1), "cmd.exe", "cmd /c npx -y @masonator/coolify-mcp"),
            proc(3, Some(2), "node.exe", "node C:\\npm-cache\\_npx\\x\\node_modules\\@masonator\\coolify-mcp\\dist\\index.js"),
            proc(4, Some(2), "conhost.exe", "conhost.exe 0xffffffff"),
            proc(5, Some(999), "git.exe", "git ls-files --others"),
        ] {
            procs.insert(p.pid, p);
        }
        let groups = classify(&procs, &servers, 12345);
        let mcp = groups.iter().find(|g| g.category == Category::McpDirect).unwrap();
        assert_eq!(mcp.procs.len(), 3, "cmd, node and conhost belong to the server");
        assert_eq!(mcp.roots, vec![2]);
        assert!(groups.iter().any(|g| g.category == Category::StuckGit));
    }

    #[test]
    fn names_projects() {
        assert_eq!(project_name("node d:/projects/fullstack/marhaba/node_modules/.pnpm/next/x.js"), "fullstack/marhaba");
    }
}
