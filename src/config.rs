use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub claude_dir: PathBuf,
    /// Sessions idle longer than this (seconds) are marked stale
    pub active_threshold_secs: u64,
}

impl Config {
    pub fn from_env() -> Self {
        let _ = dotenv::dotenv();

        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let claude_dir = PathBuf::from(&home).join(".claude").join("projects");

        Self {
            port: std::env::var("PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(7878),
            claude_dir,
            active_threshold_secs: std::env::var("ACTIVE_THRESHOLD_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(600), // 10 min
        }
    }
}
