/// Optional Supabase sync — only runs when SUPABASE_URL + SUPABASE_SERVICE_KEY are set.
/// Upserts session metadata to `claude_sessions` table on every full scan.
use anyhow::Result;
use reqwest::Client;
use serde_json::{json, Value};

use crate::{config::Config, sessions::SessionInfo};

pub struct SupabaseClient {
    client:  Client,
    url:     String,
    api_key: String,
}

impl SupabaseClient {
    pub fn new(cfg: &Config) -> Option<Self> {
        let url = cfg.supabase_url.clone()?;
        let api_key = cfg.supabase_key.clone()?;
        Some(Self { client: Client::new(), url, api_key })
    }

    /// Upsert a batch of sessions.
    /// Table DDL (run once in Supabase SQL editor):
    /// ```sql
    /// create table if not exists claude_sessions (
    ///   session_id     text primary key,
    ///   project_name   text,
    ///   cwd            text,
    ///   git_branch     text,
    ///   version        text,
    ///   active         boolean,
    ///   last_modified  timestamptz,
    ///   created_at     timestamptz,
    ///   first_message  text,
    ///   custom_title   text,
    ///   message_count  int,
    ///   input_tokens   bigint default 0,
    ///   output_tokens  bigint default 0,
    ///   cache_read     bigint default 0,
    ///   cache_creation bigint default 0,
    ///   model          text,
    ///   synced_at      timestamptz default now()
    /// );
    /// ```
    pub async fn upsert_sessions(&self, sessions: &[SessionInfo]) -> Result<()> {
        if sessions.is_empty() { return Ok(()); }

        let rows: Vec<Value> = sessions.iter().map(|s| {
            let (it, ot, cr, cc, model) = s.last_usage.as_ref()
                .map(|u| (u.input_tokens, u.output_tokens, u.cache_read, u.cache_creation, u.model.clone()))
                .unwrap_or((0, 0, 0, 0, String::new()));

            json!({
                "session_id":    s.session_id,
                "project_name":  s.project_name,
                "cwd":           s.cwd,
                "git_branch":    s.git_branch,
                "version":       s.version,
                "active":        s.active,
                "last_modified": s.last_modified,
                "created_at":    s.created_at,
                "first_message": s.first_message,
                "custom_title":  s.custom_title,
                "message_count": s.message_count,
                "input_tokens":  it,
                "output_tokens": ot,
                "cache_read":    cr,
                "cache_creation": cc,
                "model":         model,
                "synced_at":     chrono::Utc::now().to_rfc3339(),
            })
        }).collect();

        let endpoint = format!("{}/rest/v1/claude_sessions", self.url);
        let resp = self.client
            .post(&endpoint)
            .header("apikey", &self.api_key)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .header("Prefer", "resolution=merge-duplicates,return=minimal")
            .json(&rows)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Supabase upsert failed {status}: {body}");
        }

        Ok(())
    }
}
