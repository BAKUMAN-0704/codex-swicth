use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

/// Resolve the user home directory in a shell-agnostic way.
pub fn home() -> PathBuf {
    if let Ok(v) = std::env::var("USERPROFILE") {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    if let Ok(v) = std::env::var("HOME") {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    PathBuf::from(".")
}

/// Codex honours CODEX_HOME; fall back to ~/.codex.
pub fn codex_home() -> PathBuf {
    if let Ok(v) = std::env::var("CODEX_HOME") {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    home().join(".codex")
}

/// Our own state directory. Deliberately outside ~/.codex so Codex never
/// validates or rewrites it. `CODEX_SWITCH_HOME` overrides it (used by tests).
pub fn switch_home() -> PathBuf {
    if let Ok(v) = std::env::var("CODEX_SWITCH_HOME") {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    home().join(".codex-switch")
}

pub fn profiles_path() -> PathBuf {
    switch_home().join("profiles.toml")
}

pub fn keys_path() -> PathBuf {
    switch_home().join("keys.json")
}

pub fn stash_official_path() -> PathBuf {
    switch_home().join("stash").join("official-auth.json")
}

pub fn backup_root() -> PathBuf {
    switch_home().join("backups")
}

pub fn auth_path() -> PathBuf {
    codex_home().join("auth.json")
}

pub fn config_path() -> PathBuf {
    codex_home().join("config.toml")
}

pub fn sessions_dir() -> PathBuf {
    codex_home().join("sessions")
}

#[derive(Debug, Default)]
pub struct RunningProcs {
    /// Codex itself. Holds the sqlite index and rewrites config on exit.
    pub critical: Vec<String>,
    /// Notify hooks, computer-use bridges, MCP helpers. Safe to leave running.
    pub helper: Vec<String>,
}

impl RunningProcs {
    pub fn has_critical(&self) -> bool {
        !self.critical.is_empty()
    }
    /// One-line description for status output.
    pub fn describe(&self) -> String {
        if self.critical.is_empty() && self.helper.is_empty() {
            return "未运行".to_string();
        }
        let mut parts = Vec::new();
        if !self.critical.is_empty() {
            parts.push(format!("主进程 {}", self.critical.join(", ")));
        }
        if !self.helper.is_empty() {
            parts.push(format!("辅助进程 {}", self.helper.join(", ")));
        }
        parts.join(" · ")
    }
}

fn classify(name: &str) -> Option<bool> {
    let lower = name.to_ascii_lowercase();
    if lower.starts_with("codex-switch") {
        return None;
    }
    if !lower.contains("codex") {
        return None;
    }
    let is_helper = [
        "notify",
        "computer-use",
        "computer_use",
        "mcp",
        "helper",
        "bridge",
    ]
    .iter()
    .any(|k| lower.contains(k));
    Some(!is_helper)
}

/// Processes that would hold `~/.codex` open, split by how much they matter.
pub fn running_codex_processes() -> RunningProcs {
    let mut procs = RunningProcs::default();
    let mut names: Vec<String> = Vec::new();

    #[cfg(windows)]
    {
        if let Ok(out) = Command::new("tasklist")
            .args(["/FO", "CSV", "/NH"])
            .output()
        {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                let name = line
                    .split(',')
                    .next()
                    .unwrap_or("")
                    .trim_matches('"')
                    .to_string();
                if !name.is_empty() {
                    names.push(name);
                }
            }
        }
    }

    #[cfg(not(windows))]
    {
        if let Ok(out) = Command::new("ps").args(["-A", "-o", "comm="]).output() {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                names.push(line.trim().to_string());
            }
        }
    }

    for name in names {
        match classify(&name) {
            Some(true) => {
                if !procs.critical.contains(&name) {
                    procs.critical.push(name);
                }
            }
            Some(false) => {
                if !procs.helper.contains(&name) {
                    procs.helper.push(name);
                }
            }
            None => {}
        }
    }
    procs
}

/// Every `state_*.sqlite` in the Codex home, newest schema number first.
pub fn state_databases() -> Vec<PathBuf> {
    let mut out: Vec<(u32, PathBuf)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(codex_home()) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(num) = name
                .strip_prefix("state_")
                .and_then(|s| s.strip_suffix(".sqlite"))
                .and_then(|s| s.parse::<u32>().ok())
            {
                out.push((num, entry.path()));
            }
        }
    }
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.into_iter().map(|(_, p)| p).collect()
}

/// Every `thread_history_*.sqlite`.
pub fn thread_history_databases() -> Vec<PathBuf> {
    let mut out: Vec<(u32, PathBuf)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(codex_home()) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(num) = name
                .strip_prefix("thread_history_")
                .and_then(|s| s.strip_suffix(".sqlite"))
                .and_then(|s| s.parse::<u32>().ok())
            {
                out.push((num, entry.path()));
            }
        }
    }
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.into_iter().map(|(_, p)| p).collect()
}

/// Atomic write: temp file in the same directory, then rename over the target.
pub fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("no parent directory for {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create directory {}", parent.display()))?;
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    // Windows can refuse the rename if the destination is briefly locked.
    let mut last_err = None;
    for _ in 0..5 {
        match std::fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(60));
            }
        }
    }
    let _ = std::fs::remove_file(&tmp);
    Err(anyhow::anyhow!(
        "replace {} failed: {}",
        path.display(),
        last_err.map(|e| e.to_string()).unwrap_or_default()
    ))
}

pub fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // 2024-01-01T00:00:00Z baseline; simple civil-time conversion, no chrono needed.
    let days = now / 86_400;
    let secs = now % 86_400;
    let (y, m, d) = civil_from_days(days as i64);
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        y,
        m,
        d,
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Howard Hinnant's days-to-civil algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
