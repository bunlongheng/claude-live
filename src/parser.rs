use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Typed event streamed to WebSocket clients
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum SessionEvent {
    #[allow(dead_code)]
    Ready    { session_id: String },
    Thinking { text: String, timestamp: String },
    Tool     { name: String, summary: String, timestamp: String },
    Text     { text: String, timestamp: String },
    UserMsg  { text: String, timestamp: String },
    Todos    { todos: Vec<TodoItem>, timestamp: String },
    Usage    { input_tokens: u64, output_tokens: u64, cache_read: u64,
               cache_creation: u64, model: String, timestamp: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    pub id:          String,
    pub subject:     String,
    pub status:      String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
}

/// Parse one JSONL line into zero or one SessionEvent.
pub fn parse_line(line: &str) -> Option<SessionEvent> {
    let obj: Value = serde_json::from_str(line).ok()?;
    let ts = obj["timestamp"].as_str().unwrap_or("").to_string();

    match obj["type"].as_str()? {
        "user" => parse_user(&obj, ts),
        "assistant" => parse_assistant(&obj, ts),
        _ => None,
    }
}

fn parse_user(obj: &Value, ts: String) -> Option<SessionEvent> {
    // Todos first
    if let Some(todos) = obj["todos"].as_array() {
        if !todos.is_empty() {
            let parsed: Vec<TodoItem> = todos
                .iter()
                .filter_map(|t| serde_json::from_value(t.clone()).ok())
                .collect();
            if !parsed.is_empty() {
                return Some(SessionEvent::Todos { todos: parsed, timestamp: ts });
            }
        }
    }

    // User message text
    let text = extract_text(&obj["message"]["content"])?;
    if text.is_empty() { return None; }
    Some(SessionEvent::UserMsg { text: truncate(text, 300), timestamp: ts })
}

fn parse_assistant(obj: &Value, ts: String) -> Option<SessionEvent> {
    let msg = &obj["message"];

    // Usage-only line
    if let Some(usage) = msg["usage"].as_object() {
        if msg["content"].is_null() || !msg["content"].is_array() {
            return Some(SessionEvent::Usage {
                input_tokens:    usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
                output_tokens:   usage.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
                cache_read:      usage.get("cache_read_input_tokens").and_then(Value::as_u64).unwrap_or(0),
                cache_creation:  usage.get("cache_creation_input_tokens").and_then(Value::as_u64).unwrap_or(0),
                model:           msg["model"].as_str().unwrap_or("").to_string(),
                timestamp:       ts,
            });
        }
    }

    let content = msg["content"].as_array()?;

    // If there's usage alongside content, emit usage first (caller only gets one event per line,
    // so we prefer the most user-visible block)
    for block in content {
        let kind = block["type"].as_str().unwrap_or("");

        if kind == "thinking" {
            let text = block["thinking"].as_str().unwrap_or("").to_string();
            if !text.is_empty() {
                return Some(SessionEvent::Thinking { text, timestamp: ts });
            }
        }

        if kind == "text" {
            let text = block["text"].as_str().unwrap_or("").to_string();
            if !text.is_empty() {
                // Also check for usage in this message
                if let Some(usage) = msg["usage"].as_object() {
                    let _ = SessionEvent::Usage {
                        input_tokens:   usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
                        output_tokens:  usage.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
                        cache_read:     usage.get("cache_read_input_tokens").and_then(Value::as_u64).unwrap_or(0),
                        cache_creation: usage.get("cache_creation_input_tokens").and_then(Value::as_u64).unwrap_or(0),
                        model:          msg["model"].as_str().unwrap_or("").to_string(),
                        timestamp:      ts.clone(),
                    };
                }
                return Some(SessionEvent::Text { text, timestamp: ts });
            }
        }

        if kind == "tool_use" {
            let name = block["name"].as_str().unwrap_or("").to_string();
            let summary = summarise_tool(&name, &block["input"]);
            if !name.is_empty() {
                return Some(SessionEvent::Tool { name, summary, timestamp: ts });
            }
        }
    }

    // Fallback: usage only
    if let Some(usage) = msg["usage"].as_object() {
        return Some(SessionEvent::Usage {
            input_tokens:   usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
            output_tokens:  usage.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
            cache_read:     usage.get("cache_read_input_tokens").and_then(Value::as_u64).unwrap_or(0),
            cache_creation: usage.get("cache_creation_input_tokens").and_then(Value::as_u64).unwrap_or(0),
            model:          msg["model"].as_str().unwrap_or("").to_string(),
            timestamp:      ts,
        });
    }

    None
}

fn summarise_tool(name: &str, input: &Value) -> String {
    match name {
        "Bash"      => input["command"].as_str().unwrap_or("").to_string(),
        "Read"      => input["file_path"].as_str().unwrap_or("").to_string(),
        "Edit"      => input["file_path"].as_str().unwrap_or("").to_string(),
        "Write"     => input["file_path"].as_str().unwrap_or("").to_string(),
        "Grep"      => format!("\"{}\"  in  {}", input["pattern"].as_str().unwrap_or(""), input["path"].as_str().unwrap_or(".")),
        "Glob"      => input["pattern"].as_str().unwrap_or("").to_string(),
        "Agent"     => input["description"].as_str()
                .or_else(|| input["prompt"].as_str())
                .unwrap_or("")
                .to_string(),
        _           => input.to_string(),
    }
}

fn extract_text(content: &Value) -> Option<String> {
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = content.as_array() {
        for item in arr {
            if item["type"].as_str() == Some("text") {
                if let Some(t) = item["text"].as_str() {
                    return Some(t.to_string());
                }
            }
        }
    }
    None
}

fn truncate(s: String, max: usize) -> String {
    if s.chars().count() <= max { s } else { s.chars().take(max).collect::<String>() + "…" }
}

fn tail_path(p: &str) -> String {
    p.split('/').rev().take(3).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("/")
}
