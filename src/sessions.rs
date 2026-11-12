//! Finds running AI agent sessions, the project each works in, and whether it is idle.
//!
//! Claude Code keeps a transcript per session in `~/.claude/projects/<folder>/<id>.jsonl`.
//! Its first prompt tells what the session is about, and its modification time tells
//! when the session last did something. CPU use of the session's process tree covers
//! the rest (long builds, tests, Codex).

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::monitor::Proc;

#[derive(Clone, Debug)]
pub struct AgentSession {
    pub pid: u32,
    pub app: &'static str,
    pub project_dir: String,
    pub project: String,
    /// First prompt of the session, shortened.
    pub task: Option<String>,
    pub transcript: Option<PathBuf>,
    /// Time since the session last wrote to its transcript or used CPU.
    pub idle: Duration,
    /// Memory of the session and everything it started (MCP servers, dev servers…).
    pub memory: u64,
    pub children: usize,
}

impl AgentSession {
    pub fn is_idle(&self, limit: Duration) -> bool {
        self.idle >= limit
    }
}

/// Remembers CPU readings between scans, and transcript titles.
#[derive(Default)]
pub struct Tracker {
    cpu: HashMap<u32, CpuState>,
    tasks: HashMap<PathBuf, Option<String>>,
}

struct CpuState {
    cpu_ms: u64,
    scanned: Instant,
    last_busy: Option<Instant>,
    first_seen: Instant,
}

pub fn detect(procs: &HashMap<u32, Proc>, tracker: &mut Tracker) -> Vec<AgentSession> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for p in procs.values() {
        if let Some(pp) = p.parent {
            children.entry(pp).or_default().push(p.pid);
        }
    }
    let tree = |pid: u32| -> Vec<u32> {
        let mut out = vec![pid];
        let mut i = 0;
        while i < out.len() {
            for &c in children.get(&out[i]).into_iter().flatten() {
                if !out.contains(&c) {
                    out.push(c);
                }
            }
            i += 1;
        }
        out
    };

    let roots: Vec<(&Proc, &'static str)> = procs
        .values()
        .filter_map(|p| {
            let app = session_app(p)?;
            // Skip a session process started by another session process of the same app.
            let parent_same = p.parent.and_then(|pp| procs.get(&pp)).is_some_and(|pp| session_app(pp) == Some(app));
            (!parent_same).then_some((p, app))
        })
        .collect();

    let now = Instant::now();
    // Transcripts named by --resume belong to that session; others are matched by time.
    let mut claimed: HashSet<PathBuf> = HashSet::new();
    let mut found = Vec::new();
    let mut sorted = roots;
    sorted.sort_by_key(|(p, _)| std::cmp::Reverse(p.start_time));
    let resumed: Vec<(u32, PathBuf)> = sorted
        .iter()
        .filter_map(|(p, _)| {
            let id = resume_id(&p.cmd)?;
            let dir = p.cwd.as_deref().map(claude_project_dir)?;
            Some((p.pid, dir.join(format!("{id}.jsonl"))))
        })
        .collect();
    for (_, path) in &resumed {
        claimed.insert(path.clone());
    }

    for (p, app) in sorted {
        let members = tree(p.pid);
        let memory = members.iter().filter_map(|m| procs.get(m)).map(|m| m.memory).sum();
        let cpu: u64 = members.iter().filter_map(|m| procs.get(m)).map(|m| m.cpu_ms).sum();

        let cwd = p.cwd.clone().unwrap_or_default();
        let transcript = if app.starts_with("Claude") && !cwd.is_empty() {
            resumed.iter().find(|(pid, _)| *pid == p.pid).map(|(_, path)| path.clone()).filter(|p| p.is_file()).or_else(|| {
                let path = newest_transcript(&claude_project_dir(&cwd), p.start_time, &claimed)?;
                claimed.insert(path.clone());
                Some(path)
            })
        } else {
            None
        };
        // Busy when the whole tree used a real share of a CPU core since the last scan.
        // Background timers in MCP servers use ~1%; a build or test run uses far more.
        // With a transcript, its writes are the main signal, so CPU must be clearly busy.
        let threshold = if transcript.is_some() { 0.15 } else { 0.05 };
        let st = tracker.cpu.entry(p.pid).or_insert(CpuState { cpu_ms: cpu, scanned: now, last_busy: None, first_seen: now });
        let elapsed = now.duration_since(st.scanned).as_millis() as f64;
        if elapsed > 0.0 && cpu.saturating_sub(st.cpu_ms) as f64 / elapsed >= threshold {
            st.last_busy = Some(now);
        }
        st.cpu_ms = cpu;
        st.scanned = now;
        let (last_busy, first_seen) = (st.last_busy, st.first_seen);

        let written = transcript.as_ref().and_then(|t| std::fs::metadata(t).ok()?.modified().ok());
        let since_write = written.and_then(|w| SystemTime::now().duration_since(w).ok());
        let since_cpu = last_busy.map(|t| now.duration_since(t));
        let idle = match (since_write, since_cpu) {
            (Some(w), Some(c)) => w.min(c),
            (Some(w), None) => w,
            (None, Some(c)) => c,
            (None, None) => now.duration_since(first_seen),
        };
        let task = transcript.as_ref().and_then(|t| {
            tracker.tasks.entry(t.clone()).or_insert_with(|| first_prompt(t)).clone()
        });

        found.push(AgentSession {
            pid: p.pid,
            app,
            project: Path::new(&cwd).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "?".into()),
            project_dir: cwd,
            task,
            transcript,
            idle,
            memory,
            children: members.len() - 1,
        });
    }
    let alive: HashSet<u32> = found.iter().map(|s| s.pid).collect();
    tracker.cpu.retain(|pid, _| alive.contains(pid));
    found.sort_by(|a, b| a.idle.cmp(&b.idle));
    found
}

fn session_app(p: &Proc) -> Option<&'static str> {
    let name = p.name.to_lowercase();
    let cmd = p.cmd.to_lowercase();
    if name == "claude.exe" && !cmd.contains("windowsapps") {
        return Some(if cmd.contains("stream-json") { "Claude (desktop app)" } else { "Claude (terminal)" });
    }
    if (name == "node.exe" && cmd.contains("codex.js")) || name == "codex.exe" {
        return Some("Codex");
    }
    if name.starts_with("opencode") {
        return Some("OpenCode");
    }
    None
}

fn resume_id(cmd: &str) -> Option<String> {
    let i = cmd.find("--resume")?;
    let rest = cmd[i + "--resume".len()..].trim_start_matches(['=', ' ', '"']);
    let id: String = rest.chars().take_while(|c| c.is_ascii_hexdigit() || *c == '-').collect();
    (id.len() >= 32).then_some(id)
}

/// `C:\Users\me\app` -> `~/.claude/projects/C--Users-me-app`.
pub fn claude_project_dir(cwd: &str) -> PathBuf {
    let enc: String = cwd.trim_end_matches(['\\', '/']).chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    dirs::home_dir().unwrap_or_default().join(".claude").join("projects").join(enc)
}

/// The newest transcript changed after the session process started.
fn newest_transcript(dir: &Path, started_unix: u64, claimed: &HashSet<PathBuf>) -> Option<PathBuf> {
    let start = SystemTime::UNIX_EPOCH + Duration::from_secs(started_unix.saturating_sub(5));
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl") && !claimed.contains(p))
        .filter_map(|p| Some((std::fs::metadata(&p).ok()?.modified().ok()?, p)))
        .filter(|(m, _)| *m >= start)
        .max_by_key(|(m, _)| *m)
        .map(|(_, p)| p)
}

/// The first real prompt the user typed in the session.
fn first_prompt(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    for line in BufReader::new(file).lines().take(400).map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        if v["type"] == "summary"
            && let Some(s) = v["summary"].as_str()
        {
            return Some(shorten(s));
        }
        if v["type"] != "user" || v["isMeta"] == true {
            continue;
        }
        let content = &v["message"]["content"];
        let text = match content {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Array(parts) => parts.iter().find_map(|p| (p["type"] == "text").then(|| p["text"].as_str().map(String::from)).flatten()),
            _ => None,
        };
        let Some(text) = text else { continue };
        let t = text.trim();
        // Skip injected context and command wrappers.
        if t.is_empty() || t.starts_with('<') || t.starts_with("Caveat:") || t.starts_with("[Request interrupted") {
            continue;
        }
        return Some(shorten(t));
    }
    None
}

fn shorten(s: &str) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > 90 { format!("{}…", one_line.chars().take(90).collect::<String>()) } else { one_line }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_project_dir() {
        let d = claude_project_dir("C:\\Users\\dev\\Projects\\my-app");
        assert!(d.ends_with("C--Users-dev-Projects-my-app"));
    }

    #[test]
    fn reads_resume_id() {
        assert_eq!(
            resume_id("claude.exe --model=x --resume=c9ffb086-ed23-4289-9ca9-8d92c45f3afa --allowedTools=a").as_deref(),
            Some("c9ffb086-ed23-4289-9ca9-8d92c45f3afa")
        );
        assert_eq!(resume_id("claude.exe --model=x"), None);
    }
}
