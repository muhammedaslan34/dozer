# Dozer

Dozer shows what is using your RAM and CPU and closes what is safe to close in one click. It also puts idle agent sessions and MCP servers to sleep, and wakes MCP servers only when an agent actually calls a tool.

It is written in Rust with egui. It ships as a single `.exe` and runs in the system tray.

## Why

Every Claude Code / Codex session starts its own copy of every MCP server you configured. With 10 sessions open and 10 servers, that is about 100 `node` processes (plus `cmd` and `conhost` wrappers), many gigabytes, even when the agents are idle. Leftover processes pile up as well: `git` scans whose parent closed, dev servers you forgot about, and sessions nobody has touched for hours.

## Features

**Top usage** lists everything sorted by memory. Each row says whether it is safe to close:

| Badge | Meaning | Examples |
|---|---|---|
| 🟢 Safe to close | Nothing is lost. | Stuck `git` scans, MCP servers, Windows parts that restart themselves |
| 🟠 Save work first | Fine to close, but you may lose unsaved work. | Browsers, apps, dev servers, agent sessions |
| 🔴 Keep running | Part of Windows (or Dozer). Buttons are disabled. | `svchost`, `explorer`, `dwm`, `csrss`… |

- **Free X now** (top right) ends everything safe in one click.
- **Close** asks an app to quit, like clicking ✕. **End task** forces it.
- WebView2 is shown per owning app, for example `WebView2 · WhatsApp`, so you know which app to close.
- Gauges, a RAM/CPU history chart and a "what fills your RAM" bar.

**Agent sessions** lists every running Claude Code, Codex and OpenCode session with:
- the project folder it works in
- what it is working on (its first prompt)
- last activity, and the state: Working, Waiting or Idle
- the memory of the session and everything it started

Closing a session takes one click. You can also turn on *Close idle sessions automatically after N minutes*. A session counts as idle when it has not written to its conversation or used real CPU for that long. Closing a Claude desktop session only stops its background process; open the session again in Claude to resume it.

**MCP on demand** puts Dozer between your agents and their MCP servers:
- Agents connect to `http://127.0.0.1:47800/mcp/<name>` instead of starting a server each.
- Dozer answers the handshake and the tool list from a cache, so an open session costs nothing.
- The server starts only when an agent calls a tool. It stops after its idle minutes (default: 10, or 5 for browsers).
- **Shared** mode runs one process for all sessions (API servers such as GitHub or Coolify). **Per session** mode gives each session its own process (browser automation such as Playwright or chrome-devtools).
- Every server runs inside a Windows job object, so nothing is left behind, even if Dozer is killed.

**Floating widget** (tray menu → *Show / hide floating widget*) is a small always-on-top panel with RAM and CPU gauges, history, the RAM breakdown, *Free*, *Close idle* and the biggest consumers. Drag it anywhere; its position is remembered.

## Set up MCP on demand

1. Open Dozer → **MCP on demand**. On first start it imports the stdio servers from `~/.claude.json`.
2. Tick the servers to switch. FleetAI servers are left unticked, because they identify the calling agent by how they were started.
3. Click **Use Dozer in Claude**. Dozer backs up `~/.claude.json` to `%APPDATA%\Dozer\backups\`, caches each server's tools, and runs `claude mcp add --transport http` for each server.
4. Tick **Start Dozer with Windows**. Agents cannot reach their servers while Dozer is closed.
5. Restart open Claude sessions so they drop their old server processes.

To undo, click **Restore Claude's original setup**.

## Build

Requires Rust (MSVC toolchain).

```bash
cargo build --release
```

The exe is `target/release/dozer.exe`. It needs no installer and no VC++ runtime (the CRT is linked statically).

Command-line flags: `--hidden` (start in the tray), `--widget` (show the widget), `--tab=sessions|type|mcp|activity`.

## Files

- `%APPDATA%\Dozer\config.json` holds the servers, modes, idle times and widget settings. It can contain API keys copied from the Claude config; it stays in your profile.
- `%APPDATA%\Dozer\cache\` holds the cached tool lists.
- `%APPDATA%\Dozer\backups\` holds the `~/.claude.json` backups made before each change.

## Limits

- Dozer has no server-to-agent stream, so servers that push notifications or ask the agent for input (sampling, elicitation) only get basic answers. Tool calls, resources and prompts work.
- Shared servers run in your home folder, not the agent's project folder. Put servers that work on project files in *Per session* mode, or leave them direct.
- Codex and other agents keep their own MCP config. Only Claude Code is switched automatically.
