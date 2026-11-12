//! Points Claude Code's user-scope MCP servers at Dozer, and back.
//!
//! Uses the `claude mcp` command instead of editing `~/.claude.json` directly,
//! because running sessions rewrite that file all the time.

use std::path::PathBuf;

use serde_json::json;

use crate::config::ServerConfig;
use crate::winutil::run_hidden;

fn claude_exe() -> String {
    let local = dirs::home_dir().unwrap_or_default().join(".local").join("bin").join("claude.exe");
    if local.is_file() {
        return local.display().to_string();
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            for ext in ["exe", "cmd"] {
                let p: PathBuf = dir.join(format!("claude.{ext}"));
                if p.is_file() {
                    return p.display().to_string();
                }
            }
        }
    }
    "claude".into()
}

/// Copies `~/.claude.json` next to Dozer's config before the first change of a day.
pub fn backup_claude_config() -> anyhow::Result<PathBuf> {
    let src = crate::config::claude_config_path();
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let dir = crate::config::data_dir().join("backups");
    std::fs::create_dir_all(&dir)?;
    let dst = dir.join(format!("claude.json.{stamp}.bak"));
    std::fs::copy(&src, &dst)?;
    Ok(dst)
}

/// Replaces the stdio entry with an HTTP entry that points at Dozer.
pub fn route_to_hub(server: &ServerConfig, url: &str) -> anyhow::Result<()> {
    let exe = claude_exe();
    let _ = run_hidden(&exe, &["mcp", "remove", &server.name, "-s", "user"]);
    let (ok, out) = run_hidden(&exe, &["mcp", "add", "--transport", "http", "-s", "user", &server.name, url]);
    if !ok {
        // Put the original back so the agent is never left without the server.
        restore_direct(server).ok();
        anyhow::bail!("claude mcp add failed: {out}");
    }
    Ok(())
}

/// Puts the original command back, so Claude starts the server itself again.
pub fn restore_direct(server: &ServerConfig) -> anyhow::Result<()> {
    let exe = claude_exe();
    let def = json!({
        "type": "stdio",
        "command": server.command,
        "args": server.args,
        "env": server.env,
    });
    let _ = run_hidden(&exe, &["mcp", "remove", &server.name, "-s", "user"]);
    let (ok, out) = run_hidden(&exe, &["mcp", "add-json", "-s", "user", &server.name, &def.to_string()]);
    if !ok {
        anyhow::bail!("claude mcp add-json failed: {out}");
    }
    Ok(())
}
