use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use serde::Serialize;
use serde_json::Value;

use crate::config::Config;

#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub session_id:    String,
    pub project_folder: String,
    pub project_name:  String,
    pub file_path:     String,
    pub active:        bool,
    pub last_modified: String,
    pub created_at:    String,
    pub first_message: String,
    pub custom_title:  Option<String>,
    pub cwd:           String,
    pub git_branch:    String,
    pub version:       String,
    pub message_count: usize,
    pub last_usage:    Option<UsageSnapshot>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageSnapshot {
    pub input_tokens:   u64,
    pub output_tokens:  u64,
    pub cache_read:     u64,
    pub cache_creation: u64,
    pub model:          String,
}

static PROJECT_MAP: &[(&str, &str)] = &[
    ("-Users-hengb01-Sites-3pi",        "3pi"),
    ("-Users-hengb01-Sites-bheng",      "bheng"),
    ("-Users-hengb01-Sites-stickies",   "stickies"),
    ("-Users-hengb01-Sites-mermaid",    "mermaid"),
    ("-Users-hengb01-Sites-bheng2026",  "bheng2026"),
    ("-Users-hengb01-Sites-score-card", "score-card"),
];

fn project_name(folder: &str) -> String {
    PROJECT_MAP
        .iter()
        .find(|(k, _)| *k == folder)
        .map(|(_, v)| v.to_string())
        .unwrap_or_else(|| folder.to_string())
}

pub fn find_session_file(claude_dir: &Path, session_id: &str) -> Option<PathBuf> {
    let rd = std::fs::read_dir(claude_dir).ok()?;
    for entry in rd.flatten() {
        let fp = entry.path().join(format!("{}.jsonl", session_id));
        if fp.exists() { return Some(fp); }
    }
    None
}

pub fn list_sessions(cfg: &Config) -> Vec<SessionInfo> {
    let mut sessions = Vec::new();
    let now = SystemTime::now();

    let folders = match std::fs::read_dir(&cfg.claude_dir) {
        Ok(rd) => rd,
        Err(_) => return sessions,
    };

    for folder_entry in folders.flatten() {
        let folder_path = folder_entry.path();
        if !folder_path.is_dir() { continue; }
        let folder_name = folder_entry.file_name().to_string_lossy().into_owned();

        let files = match std::fs::read_dir(&folder_path) {
            Ok(rd) => rd,
            Err(_) => continue,
        };

        for file_entry in files.flatten() {
            let fp = file_entry.path();
            if fp.extension().and_then(|e| e.to_str()) != Some("jsonl") { continue; }

            let session_id = match fp.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };

            let stat = match std::fs::metadata(&fp) {
                Ok(m) => m,
                Err(_) => continue,
            };

            let mtime = stat.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let age = now.duration_since(mtime).unwrap_or(Duration::from_secs(u64::MAX));
            let active = age.as_secs() < cfg.active_threshold_secs;

            let meta = parse_session_meta(&fp);

            sessions.push(SessionInfo {
                session_id,
                project_folder: folder_name.clone(),
                project_name: project_name(&folder_name),
                file_path: fp.to_string_lossy().into_owned(),
                active,
                last_modified: fmt_time(mtime),
                created_at: meta.created_at.unwrap_or_else(|| fmt_time(stat.created().unwrap_or(mtime))),
                first_message: meta.first_message,
                custom_title: meta.custom_title,
                cwd: meta.cwd,
                git_branch: meta.git_branch,
                version: meta.version,
                message_count: meta.message_count,
                last_usage: meta.last_usage,
            });
        }
    }

    // Sort: active first, then most recent
    sessions.sort_by(|a, b| {
        b.active.cmp(&a.active)
            .then(b.last_modified.cmp(&a.last_modified))
    });

    sessions
}

struct RawMeta {
    created_at:    Option<String>,
    first_message: String,
    custom_title:  Option<String>,
    cwd:           String,
    git_branch:    String,
    version:       String,
    message_count: usize,
    last_usage:    Option<UsageSnapshot>,
}

fn parse_session_meta(fp: &PathBuf) -> RawMeta {
    let mut meta = RawMeta {
        created_at: None,
        first_message: String::new(),
        custom_title: None,
        cwd: String::new(),
        git_branch: String::new(),
        version: String::new(),
        message_count: 0,
        last_usage: None,
    };

    let content = match std::fs::read_to_string(fp) {
        Ok(c) => c,
        Err(_) => return meta,
    };

    for line in content.lines().filter(|l| !l.is_empty()) {
        meta.message_count += 1;

        let obj: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if meta.created_at.is_none() {
            if let Some(ts) = obj["timestamp"].as_str() {
                meta.created_at = Some(ts.to_string());
            }
        }

        if let Some("custom-title") = obj["type"].as_str() {
            if let Some(t) = obj["customTitle"].as_str() {
                meta.custom_title = Some(t.to_string());
            }
        }

        if let Some("user") = obj["type"].as_str() {
            if meta.cwd.is_empty() {
                meta.cwd      = obj["cwd"].as_str().unwrap_or("").to_string();
                meta.git_branch = obj["gitBranch"].as_str().unwrap_or("").to_string();
                meta.version  = obj["version"].as_str().unwrap_or("").to_string();
            }
            if meta.first_message.is_empty() {
                let c = &obj["message"]["content"];
                let text = if let Some(s) = c.as_str() {
                    s.to_string()
                } else if let Some(arr) = c.as_array() {
                    arr.iter()
                        .find(|b| b["type"].as_str() == Some("text"))
                        .and_then(|b| b["text"].as_str())
                        .unwrap_or("")
                        .to_string()
                } else { String::new() };

                if !text.trim().is_empty() {
                    meta.first_message = text.chars().take(200).collect();
                }
            }
        }

        // Track last usage
        if let Some(usage) = obj["message"]["usage"].as_object() {
            if usage.contains_key("input_tokens") || usage.contains_key("output_tokens") {
                meta.last_usage = Some(UsageSnapshot {
                    input_tokens:   usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
                    output_tokens:  usage.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
                    cache_read:     usage.get("cache_read_input_tokens").and_then(Value::as_u64).unwrap_or(0),
                    cache_creation: usage.get("cache_creation_input_tokens").and_then(Value::as_u64).unwrap_or(0),
                    model:          obj["message"]["model"].as_str().unwrap_or("").to_string(),
                });
            }
        }
    }

    meta
}

fn fmt_time(t: SystemTime) -> String {
    let secs = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
    // ISO 8601 manually (no chrono needed for this path)
    let dt = chrono::DateTime::from_timestamp(secs as i64, 0)
        .unwrap_or_default();
    dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}
