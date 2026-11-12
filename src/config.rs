//! Settings file: which MCP servers the hub runs, and how.
//!
//! Lives in `%APPDATA%\Dozer\config.json`. It can hold API keys copied from
//! the Claude config, so it stays in the user profile, never in a repository.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const APP_NAME: &str = "Dozer";
pub const DEFAULT_PORT: u16 = 47800;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// One process serves every agent session (stateless servers: APIs, GitHub, Coolify).
    Shared,
    /// Each agent session gets its own process (browsers, anything with state).
    PerSession,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub mode: Mode,
    /// Stop the process after this many minutes without a tool call.
    pub idle_minutes: u32,
    /// When false the hub refuses to start this server.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// True once Claude Code was pointed at the hub for this server.
    #[serde(default)]
    pub routed: bool,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub port: u16,
    #[serde(default)]
    pub servers: Vec<ServerConfig>,
    /// End agent sessions that have done nothing for `session_idle_minutes`.
    #[serde(default)]
    pub auto_close_sessions: bool,
    #[serde(default = "default_session_idle")]
    pub session_idle_minutes: u32,
    /// Show the small always-on-top widget.
    #[serde(default)]
    pub widget: bool,
    #[serde(default)]
    pub widget_pos: Option<[f32; 2]>,
}

fn default_session_idle() -> u32 {
    60
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            servers: Vec::new(),
            auto_close_sessions: false,
            session_idle_minutes: default_session_idle(),
            widget: false,
            widget_pos: None,
        }
    }
}

pub fn data_dir() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(std::env::temp_dir);
    let dir = base.join(APP_NAME);
    // Earlier builds were called McpHub; carry their settings over once.
    let old = base.join("McpHub");
    if !dir.join("config.json").exists() && old.join("config.json").is_file() && std::fs::rename(&old, &dir).is_err() {
        copy_dir(&old, &dir);
    }
    dir
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    let _ = std::fs::create_dir_all(to);
    for entry in std::fs::read_dir(from).into_iter().flatten().flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            let _ = std::fs::copy(entry.path(), target);
        }
    }
}

pub fn cache_dir() -> PathBuf {
    data_dir().join("cache")
}

fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

impl Config {
    pub fn load() -> Self {
        match std::fs::read_to_string(config_path()) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|err| {
                log::error!("config.json is invalid, using defaults: {err}");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(data_dir())?;
        let tmp = config_path().with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(tmp, config_path())?;
        Ok(())
    }

    pub fn server(&self, name: &str) -> Option<&ServerConfig> {
        self.servers.iter().find(|s| s.name == name)
    }

    pub fn url_for(&self, name: &str) -> String {
        format!("http://127.0.0.1:{}/mcp/{name}", self.port)
    }

    /// Copies stdio servers from `~/.claude.json` that the hub does not know yet.
    /// Returns the names that were added.
    pub fn import_from_claude(&mut self) -> anyhow::Result<Vec<String>> {
        let found = read_claude_stdio_servers()?;
        let mut added = Vec::new();
        for server in found {
            if self.server(&server.name).is_none() {
                added.push(server.name.clone());
                self.servers.push(server);
            }
        }
        Ok(added)
    }
}

pub fn claude_config_path() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".claude.json")
}

/// Remote (HTTP/SSE) servers in the Claude config that do not point at Dozer:
/// (name, url). They run on another machine and use no RAM here.
pub fn read_claude_remote_servers(port: u16) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(claude_config_path()) else { return Vec::new() };
    let Ok(root) = serde_json::from_str::<serde_json::Value>(&text) else { return Vec::new() };
    let local = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    root.get("mcpServers")
        .and_then(|v| v.as_object())
        .into_iter()
        .flatten()
        .filter_map(|(name, def)| {
            let url = def.get("url")?.as_str()?;
            (!local.iter().any(|l| url.contains(l.as_str()))).then(|| (name.clone(), url.to_string()))
        })
        .collect()
}

/// Reads the user-scope `mcpServers` from the Claude config. HTTP servers are
/// skipped: they already cost no local process.
pub fn read_claude_stdio_servers() -> anyhow::Result<Vec<ServerConfig>> {
    let text = std::fs::read_to_string(claude_config_path())?;
    let root: serde_json::Value = serde_json::from_str(&text)?;
    let mut out = Vec::new();
    let Some(servers) = root.get("mcpServers").and_then(|v| v.as_object()) else {
        return Ok(out);
    };
    for (name, def) in servers {
        let Some(command) = def.get("command").and_then(|v| v.as_str()) else {
            continue;
        };
        let args: Vec<String> = def
            .get("args")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let env = def
            .get("env")
            .and_then(|v| v.as_object())
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        let mode = default_mode(name, command, &args);
        out.push(ServerConfig {
            name: name.clone(),
            command: command.to_string(),
            args,
            env,
            mode,
            idle_minutes: if mode == Mode::Shared { 10 } else { 5 },
            enabled: true,
            routed: false,
        });
    }
    Ok(out)
}

/// Browser automation keeps a browser per caller, so it must not be shared.
fn default_mode(name: &str, command: &str, args: &[String]) -> Mode {
    let all = format!("{name} {command} {}", args.join(" ")).to_lowercase();
    let stateful = ["playwright", "chrome-devtools", "browser", "puppeteer", "computer-use"];
    if stateful.iter().any(|k| all.contains(k)) { Mode::PerSession } else { Mode::Shared }
}

/// A short text that appears in the command line of this server's processes,
/// used to find them in the process list (`@scope/pkg@1.2` -> `@scope/pkg`).
pub fn signature(server: &ServerConfig) -> String {
    let skip = ["npx", "npx.cmd", "node", "cmd", "cmd.exe", "/c", "/d", "-y", "--yes", "mcp", "pnpm", "dlx", "bunx", "uvx"];
    let mut best = String::new();
    for arg in std::iter::once(&server.command).chain(server.args.iter()) {
        let a = arg.replace('\\', "/");
        if a.starts_with('-') || skip.contains(&a.to_lowercase().as_str()) {
            continue;
        }
        let file = a.rsplit('/').next().unwrap_or(&a).to_string();
        let candidate = if a.starts_with('@') { strip_version(&a) } else if a.contains('/') { file } else { strip_version(&a) };
        if candidate.len() > best.len() {
            best = candidate;
        }
    }
    best.to_lowercase()
}

fn strip_version(pkg: &str) -> String {
    // Keep a leading scope "@x/", drop a trailing "@version".
    let (scope, rest) = match pkg.strip_prefix('@') {
        Some(r) => match r.split_once('/') {
            Some((s, r)) => (format!("@{s}/"), r),
            None => (String::new(), pkg),
        },
        None => (String::new(), pkg),
    };
    let name = rest.split('@').next().unwrap_or(rest);
    format!("{scope}{name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(command: &str, args: &[&str]) -> ServerConfig {
        ServerConfig {
            name: "x".into(),
            command: command.into(),
            args: args.iter().map(|a| a.to_string()).collect(),
            env: BTreeMap::new(),
            mode: Mode::Shared,
            idle_minutes: 5,
            enabled: true,
            routed: false,
        }
    }

    #[test]
    fn signatures() {
        assert_eq!(signature(&s("npx", &["-y", "@masonator/coolify-mcp"])), "@masonator/coolify-mcp");
        assert_eq!(signature(&s("npx", &["chrome-devtools-mcp@latest"])), "chrome-devtools-mcp");
        assert_eq!(signature(&s("npx", &["@playwright/mcp@latest"])), "@playwright/mcp");
        assert_eq!(signature(&s("cmd.exe", &["/d", "/c", "npx", "-y", "rea-agents@6.8.0", "mcp"])), "rea-agents");
        assert_eq!(
            signature(&s("node", &["C:\\Users\\u\\AppData\\Local\\FleetAI\\backend\\agent-ssh-mcp.js"])),
            "agent-ssh-mcp.js"
        );
    }

    #[test]
    fn browsers_are_per_session() {
        assert_eq!(default_mode("playwright", "npx", &[]), Mode::PerSession);
        assert_eq!(default_mode("github", "npx", &["-y".into(), "@modelcontextprotocol/server-github".into()]), Mode::Shared);
    }
}
