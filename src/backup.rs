use crate::paths;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct BackupInfo {
    pub dir: PathBuf,
    pub files: Vec<String>,
    pub bytes: u64,
}

/// Switching is cheap to undo but the history database is large, so only the
/// most recent snapshots are kept.
const KEEP: usize = 3;

/// Snapshot every file a switch may touch. Codex must be closed: the sqlite
/// side is copied as a plain file, so a live WAL would give an inconsistent
/// snapshot.
pub fn create(tag: &str) -> Result<BackupInfo> {
    let dir = paths::backup_root().join(format!("{}-{}", tag, paths::timestamp()));
    std::fs::create_dir_all(&dir).with_context(|| format!("创建备份目录 {}", dir.display()))?;

    let mut files = Vec::new();
    let mut bytes = 0u64;

    let mut candidates: Vec<PathBuf> = vec![paths::auth_path(), paths::config_path()];
    candidates.extend(paths::state_databases());
    candidates.extend(paths::thread_history_databases());

    for src in candidates {
        if !src.exists() {
            continue;
        }
        let name = src
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let dst = dir.join(&name);
        std::fs::copy(&src, &dst).with_context(|| format!("备份 {} 失败", src.display()))?;
        if let Ok(meta) = std::fs::metadata(&dst) {
            bytes += meta.len();
        }
        files.push(name.clone());

        // WAL sidecars keep the main file consistent; copy them when present.
        for suffix in ["-wal", "-shm"] {
            let side = PathBuf::from(format!("{}{}", src.to_string_lossy(), suffix));
            if side.exists() {
                let side_name = format!("{name}{suffix}");
                let _ = std::fs::copy(&side, dir.join(&side_name));
                files.push(side_name);
            }
        }
    }

    write_manifest(&dir, tag, &files)?;
    prune();
    Ok(BackupInfo { dir, files, bytes })
}

fn write_manifest(dir: &Path, tag: &str, files: &[String]) -> Result<()> {
    let body = format!(
        "created = \"{}\"\ntag = \"{}\"\nfiles = [{}]\n",
        paths::timestamp(),
        tag,
        files
            .iter()
            .map(|f| format!("\"{f}\""))
            .collect::<Vec<_>>()
            .join(", ")
    );
    paths::atomic_write(&dir.join("backup.toml"), body.as_bytes())
}

pub fn list() -> Result<Vec<PathBuf>> {
    let root = paths::backup_root();
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&root)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs.reverse();
    Ok(dirs)
}

pub fn latest() -> Option<PathBuf> {
    list().ok().and_then(|mut v| {
        v.retain(|p| p.join("backup.toml").exists());
        v.into_iter().next()
    })
}

/// Copy a snapshot back over the live files.
pub fn restore(dir: &Path) -> Result<Vec<String>> {
    let mut restored = Vec::new();
    for entry in std::fs::read_dir(dir)?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with("-wal") || name.ends_with("-shm") || name == "backup.toml" {
            continue;
        }
        // Only restore files that belong to Codex home.
        let target = match name.as_str() {
            "auth.json" => paths::auth_path(),
            "config.toml" => paths::config_path(),
            n if n.starts_with("state_") || n.starts_with("thread_history_") => {
                paths::codex_home().join(n)
            }
            _ => continue,
        };
        std::fs::copy(entry.path(), &target)
            .with_context(|| format!("还原 {} 失败", target.display()))?;
        restored.push(name);
    }
    Ok(restored)
}

fn prune() {
    let Ok(dirs) = list() else { return };
    // Only snapshot directories count against the quota; migration manifests
    // are tiny and are kept so any switch stays reversible.
    let snapshots: Vec<PathBuf> = dirs
        .into_iter()
        .filter(|d| {
            !d.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .starts_with("migrate-")
        })
        .collect();
    for old in snapshots.into_iter().skip(KEEP) {
        let _ = std::fs::remove_dir_all(&old);
    }
}

/// Total size of everything under the backup root, plus a per-kind count.
pub fn usage() -> (usize, u64) {
    fn dir_size(p: &Path) -> u64 {
        let mut total = 0;
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                let path = e.path();
                if path.is_dir() {
                    total += dir_size(&path);
                } else if let Ok(m) = e.metadata() {
                    total += m.len();
                }
            }
        }
        total
    }
    match list() {
        Ok(dirs) => {
            let n = dirs.len();
            let bytes = dirs.iter().map(|d| dir_size(d)).sum();
            (n, bytes)
        }
        Err(_) => (0, 0),
    }
}

/// Drop every snapshot but keep migration manifests.
pub fn clean() -> usize {
    let Ok(dirs) = list() else { return 0 };
    let mut n = 0;
    for d in dirs {
        if d.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .starts_with("migrate-")
        {
            continue;
        }
        if std::fs::remove_dir_all(&d).is_ok() {
            n += 1;
        }
    }
    n
}

pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{:.1}{}", v, UNITS[i])
}
