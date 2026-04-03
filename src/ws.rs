use std::sync::Arc;

use axum::{
    extract::{ws::{Message, WebSocket, WebSocketUpgrade}, Path, State},
    response::IntoResponse,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tracing::{info, warn};

use crate::{parser, sessions, AppState};

/// Find the tty device path for the Claude process running in `cwd`.
fn find_tty_for_cwd(cwd: &str) -> Option<String> {
    let script = r#"
for pid in $(/usr/bin/pgrep -f "claude" 2>/dev/null); do
    proc_cwd=$(/usr/sbin/lsof -a -p "$pid" -d cwd -Fn 2>/dev/null | /usr/bin/grep '^n' | /usr/bin/sed 's/^n//')
    if [ "$proc_cwd" = "$INJECT_CWD" ]; then
        tty=$(/bin/ps -o tty= -p "$pid" 2>/dev/null | /usr/bin/tr -d ' ')
        if [ -n "$tty" ] && [ "$tty" != "??" ]; then
            echo "/dev/$tty"
            exit 0
        fi
    fi
done
exit 1
"#;
    let out = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .env("INJECT_CWD", cwd)
        .output()
        .ok()?;
    if out.status.success() {
        let tty = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !tty.is_empty() { Some(tty) } else { None }
    } else {
        None
    }
}

/// Try iTerm2 AppleScript injection (works for sessions owned by iTerm2).
/// Strategy (all in one osascript call so ordering is guaranteed):
///   1. write text inputText newline NO  → bracketed paste wraps it; text lands in readline buffer
///   2. shell: printf '\e[?2004l' > tty  → iTerm2 reads this from master, disables bracketed paste
///   3. delay 0.05                        → let iTerm2 process the mode change
///   4. write text CR newline NO         → now sent WITHOUT paste wrapping → readline submits ✓
///   5. shell: printf '\e[?2004h' > tty  → re-enable bracketed paste
fn inject_via_iterm2(tty_path: &str, text: &str) -> bool {
    let script = r#"
set ttyTarget to (system attribute "INJECT_TTY")
set inputText to (system attribute "INJECT_TEXT")
tell application "iTerm2"
    repeat with w in windows
        repeat with t in tabs of w
            repeat with s in sessions of t
                try
                    if (tty of s) = ttyTarget then
                        -- 1. send text (bracketed-paste wraps it; text lands in readline buffer)
                        tell s to write text inputText newline NO
                        -- 2. disable bracketed paste via slave tty output
                        do shell script "printf '\\033[?2004l' > " & quoted form of ttyTarget
                        -- 3. let iTerm2 process the mode change
                        delay 0.05
                        -- 4. send CR — now outside paste wrapping → readline submits
                        tell s to write text (ASCII character 13) newline NO
                        -- 5. restore bracketed paste
                        do shell script "printf '\\033[?2004h' > " & quoted form of ttyTarget
                        return "ok"
                    end if
                end try
            end repeat
        end repeat
    end repeat
    return "not_found"
end tell
"#;
    let out = std::process::Command::new("osascript")
        .arg("-e").arg(script)
        .env("INJECT_TTY", tty_path)
        .env("INJECT_TEXT", text)
        .output();

    match out {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout).trim() == "ok"
        }
        _ => false,
    }
}

/// Try VS Code injection via System Events keystroke (brings VS Code to front).
/// Works when the Claude terminal is in VS Code's integrated terminal.
fn inject_via_vscode(text: &str) -> bool {
    // Ctrl+` focuses VS Code terminal, then we type the text + Enter
    let script = r#"
set inputText to (system attribute "INJECT_TEXT")
tell application "Visual Studio Code" to activate
delay 0.25
tell application "System Events"
    tell process "Electron"
        -- Focus terminal panel with Ctrl+`
        key code 50 using control down
        delay 0.2
        keystroke inputText
        delay 0.1
        key code 36
    end tell
end tell
return "ok"
"#;
    let out = std::process::Command::new("osascript")
        .arg("-e").arg(script)
        .env("INJECT_TEXT", text)
        .output();

    match out {
        Ok(o) => {
            if !o.status.success() {
                let stderr = String::from_utf8_lossy(&o.stderr);
                warn!("inject_via_vscode: osascript error: {stderr}");
            }
            o.status.success()
        }
        Err(e) => {
            warn!("inject_via_vscode: failed to run osascript: {e}");
            false
        }
    }
}

/// Detect if the tty is owned by a VS Code terminal (parent process chain includes Electron).
fn tty_is_vscode(tty_path: &str) -> bool {
    // tty_path like "/dev/ttys007" → device "ttys007"
    let dev = tty_path.trim_start_matches("/dev/");
    // Find the shell/process on this tty and check its parent chain
    let script = r#"
dev="$INJECT_DEV"
# find a pid on this tty that has a parent
for pid in $(ps -e -o pid,tty | awk -v d="$dev" '$2==d {print $1}'); do
    ppid=$(ps -o ppid= -p "$pid" 2>/dev/null | tr -d ' ')
    if [ -n "$ppid" ]; then
        cmd=$(ps -o command= -p "$ppid" 2>/dev/null)
        echo "$cmd"
        exit 0
    fi
done
"#;
    let out = std::process::Command::new("/bin/sh")
        .arg("-c").arg(script)
        .env("INJECT_DEV", dev)
        .output();
    let cmd = out.map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase().to_string()).unwrap_or_default();
    cmd.contains("electron") || cmd.contains("visual studio code") || cmd.contains("code helper")
}

/// Inject text into the Claude process in `cwd`.
/// Strategy:
///   1. Find the tty for the Claude process in this cwd
///   2. Try iTerm2 AppleScript (no focus steal, reliable)
///   3. Fall back to VS Code System Events (brings VS Code to front)
pub fn inject_to_tty(cwd: &str, text: &str) -> bool {
    let tty_path = match find_tty_for_cwd(cwd) {
        Some(p) => p,
        None => {
            warn!("inject_to_tty: no tty found for cwd={cwd}");
            return false;
        }
    };

    info!("inject_to_tty: targeting {tty_path}");

    // Try iTerm2 first
    if inject_via_iterm2(&tty_path, text) {
        info!("inject_to_tty: ok via iTerm2");
        return true;
    }

    // Fall back to VS Code if tty is owned by Electron
    if tty_is_vscode(&tty_path) {
        info!("inject_to_tty: detected VS Code terminal, trying System Events");
        let ok = inject_via_vscode(text);
        if ok { info!("inject_to_tty: ok via VS Code System Events"); }
        else   { warn!("inject_to_tty: VS Code injection failed"); }
        return ok;
    }

    warn!("inject_to_tty: no injection method worked for {tty_path}");
    false
}

/// WebSocket upgrade handler — one connection per session
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    Path(session_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, session_id, state))
}

async fn handle_socket(mut socket: WebSocket, session_id: String, state: Arc<AppState>) {
    // Find the JSONL file
    let fp = match sessions::find_session_file(&state.cfg.claude_dir, &session_id) {
        Some(p) => p,
        None => {
            let _ = socket.send(Message::Text(
                r#"{"event":"error","data":{"message":"session not found"}}"#.into()
            )).await;
            return;
        }
    };

    // Look up the cwd once for input injection
    let session_cwd = sessions::get_session_cwd(&state.cfg.claude_dir, &session_id)
        .unwrap_or_default();

    info!("WS connected: {session_id}");

    // Open file and read existing content first
    let mut file = match tokio::fs::File::open(&fp).await {
        Ok(f) => f,
        Err(e) => {
            warn!("Cannot open {fp:?}: {e}");
            return;
        }
    };

    let mut partial = String::new();

    // ── Catch-up: stream last 100 parsed events from existing content ─────
    let mut offset: u64;
    {
        let mut buf = String::new();
        let _ = file.read_to_string(&mut buf).await;
        offset = file.stream_position().await.unwrap_or(0);

        // Collect all parseable lines, then send only the last 100
        let events: Vec<String> = buf.lines()
            .filter_map(|line| parser::parse_line(line))
            .filter_map(|evt| serde_json::to_string(&evt).ok())
            .collect();

        for json in events.iter().rev().take(100).collect::<Vec<_>>().into_iter().rev() {
            if socket.send(Message::Text(json.clone().into())).await.is_err() {
                return;
            }
        }
    }

    // Send ready
    let _ = socket.send(Message::Text(
        serde_json::json!({ "event": "ready", "data": { "session_id": &session_id } })
            .to_string().into()
    )).await;

    // ── Live tail: poll every 1 second for new bytes ────────────────────────
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));

    loop {
        tokio::select! {
            _ = interval.tick() => {
                // Check for new bytes
                match file.metadata().await {
                    Ok(m) if m.len() > offset => {
                        let mut buf = vec![0u8; (m.len() - offset) as usize];
                        if let Ok(_) = file.seek(tokio::io::SeekFrom::Start(offset)).await {
                            if let Ok(n) = file.read(&mut buf).await {
                                offset += n as u64;
                                partial.push_str(&String::from_utf8_lossy(&buf[..n]));

                                let mut lines: Vec<&str> = partial.lines().collect();
                                // If partial doesn't end with newline, last token is incomplete
                                let keep_last = !partial.ends_with('\n');
                                let last = if keep_last { lines.pop() } else { None };

                                for line in lines {
                                    if let Some(evt) = parser::parse_line(line) {
                                        if let Ok(json) = serde_json::to_string(&evt) {
                                            if socket.send(Message::Text(json.into())).await.is_err() {
                                                info!("WS disconnected: {session_id}");
                                                return;
                                            }
                                        }
                                    }
                                }

                                partial = last.map(|s| s.to_string()).unwrap_or_default();
                            }
                        }
                    }
                    Err(e) => {
                        warn!("stat error for {session_id}: {e}");
                        return;
                    }
                    _ => {} // no new bytes
                }
            }
            // Handle incoming messages (input injection, pings, close)
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => {
                        info!("WS closed: {session_id}");
                        return;
                    }
                    Some(Ok(Message::Ping(p))) => {
                        let _ = socket.send(Message::Pong(p)).await;
                    }
                    Some(Ok(Message::Text(txt))) => {
                        // Expect {"type":"input","text":"..."}
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&txt) {
                            if val["type"].as_str() == Some("input") {
                                if let Some(text) = val["text"].as_str() {
                                    let ok = inject_to_tty(&session_cwd, text);
                                    let ack = serde_json::json!({
                                        "event": "input_ack",
                                        "data": { "ok": ok }
                                    });
                                    let _ = socket.send(Message::Text(ack.to_string().into())).await;
                                    if ok {
                                        info!("Input injected into session {session_id}");
                                    } else {
                                        warn!("Failed to inject input for session {session_id}");
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}
