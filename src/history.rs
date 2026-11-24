use crate::paths;
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Change {
    pub path: PathBuf,
    pub thread_id: String,
    pub old_provider: String,
    /// Byte delta introduced in the first line. Every later offset shifts by it.
    pub delta: i64,
}

#[derive(Debug, Default)]
pub struct Plan {
    /// Files present in the sqlite index whose provider tag differs.
    pub changes: Vec<Change>,
    /// Files on disk that the index does not know about (crashed sessions).
    pub orphans: Vec<(PathBuf, String)>,
    pub already: usize,
    pub missing: usize,
    pub skipped_zst: usize,
    pub by_provider: BTreeMap<String, usize>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty() && self.orphans.is_empty()
    }
}

#[derive(Debug, Default)]
pub struct Report {
    pub files_rewritten: usize,
    pub threads_relabelled: usize,
    pub rows_shifted: usize,
    pub orphans_rewritten: usize,
    pub errors: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct ManifestEntry {
    path: String,
    thread_id: String,
    old_provider: String,
    delta: i64,
    /// Verbatim original first line, so rollback restores exact bytes instead
    /// of merely re-serialising the provider field.
    #[serde(default)]
    original_head: String,
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    created: String,
    target_provider: String,
    entries: Vec<ManifestEntry>,
}

fn open_state_db(path: &Path) -> Result<Connection> {
    let con = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("打开 {}", path.display()))?;
    con.busy_timeout(std::time::Duration::from_secs(10))?;
    Ok(con)
}

/// Read the `model_provider` out of the first line (`session_meta`) without
/// parsing the whole rollout.
fn read_head_provider(path: &Path) -> Result<(String, usize)> {
    let f = File::open(path).with_context(|| format!("打开 {}", path.display()))?;
    let mut reader = BufReader::new(f);
    let mut line = Vec::new();
    reader
        .read_until(b'\n', &mut line)
        .with_context(|| format!("读取首行 {}", path.display()))?;
    let len = line.len();
    let text = String::from_utf8_lossy(&line);
    let v: serde_json::Value = serde_json::from_str(text.trim_end())
        .with_context(|| format!("首行不是合法 JSON: {}", path.display()))?;
    let provider = v
        .get("payload")
        .and_then(|p| p.get("model_provider"))
        .and_then(|p| p.as_str())
        .unwrap_or("")
        .to_string();
    Ok((provider, len))
}

/// Swap the first line of a rollout, keeping the remaining bytes untouched.
/// Returns the byte delta and the original first line verbatim.
fn rewrite_first_line(path: &Path, new_line: Vec<u8>) -> Result<(i64, String)> {
    let f = File::open(path).with_context(|| format!("打开 {}", path.display()))?;
    let mut reader = BufReader::new(f);
    let mut old_line = Vec::new();
    reader.read_until(b'\n', &mut old_line)?;
    if old_line.is_empty() {
        anyhow::bail!("{} 是空文件", path.display());
    }
    let original = String::from_utf8_lossy(&old_line).to_string();
    let delta = new_line.len() as i64 - old_line.len() as i64;

    let parent = path.parent().context("文件没有父目录")?;
    let tmp = parent.join(format!(
        ".{}.migrating-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    {
        let out = File::create(&tmp).with_context(|| format!("创建 {}", tmp.display()))?;
        let mut w = BufWriter::with_capacity(1 << 20, out);
        w.write_all(&new_line)?;
        std::io::copy(&mut reader, &mut w)?;
        w.flush()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("替换 {}", path.display()))?;
    Ok((delta, original))
}

fn rewrite_head(path: &Path, target: &str) -> Result<(i64, String)> {
    let f = File::open(path).with_context(|| format!("打开 {}", path.display()))?;
    let mut reader = BufReader::new(f);
    let mut old_line = Vec::new();
    reader.read_until(b'\n', &mut old_line)?;
    if old_line.is_empty() {
        anyhow::bail!("{} 是空文件", path.display());
    }

    let trimmed = String::from_utf8_lossy(&old_line);
    let mut v: serde_json::Value =
        serde_json::from_str(trimmed.trim_end()).context("首行 JSON 解析失败")?;
    let payload = v
        .get_mut("payload")
        .context("首行缺少 payload 字段，可能是非 session_meta 文件")?;
    payload["model_provider"] = serde_json::Value::String(target.to_string());

    let mut new_line = serde_json::to_vec(&v).context("重新序列化首行失败")?;
    new_line.push(b'\n');
    drop(reader);
    rewrite_first_line(path, new_line)
}

/// Restore a verbatim first line captured in the manifest.
fn restore_head(path: &Path, original: &str) -> Result<(i64, String)> {
    let mut raw = original.as_bytes().to_vec();
    if !raw.ends_with(b"\n") {
        raw.push(b'\n');
    }
    rewrite_first_line(path, raw)
}

/// Every rollout referenced by the index, plus orphan files found on disk.
pub fn scan(target: &str) -> Result<Plan> {
    let mut plan = Plan::default();
    let mut known: std::collections::HashSet<String> = std::collections::HashSet::new();

    for db in paths::state_databases() {
        let con = match open_state_db(&db) {
            Ok(c) => c,
            Err(e) => {
                plan.missing += 1;
                eprintln!("  ! 跳过数据库 {}: {e}", db.display());
                continue;
            }
        };
        let has_threads: bool = con
            .query_row(
                "select count(*) from sqlite_master where type='table' and name='threads'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if !has_threads {
            continue;
        }
        let mut stmt = con.prepare(
            "select id, rollout_path, model_provider from threads where rollout_path is not null and rollout_path <> ''",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        for row in rows {
            let (id, rollout_path, provider) = row?;
            let path = PathBuf::from(&rollout_path);
            // Compare by file name: sqlite stores one separator style, the
            // directory walk may produce the other. rollout names carry a UUID
            // so collisions are not a practical concern.
            if let Some(name) = path.file_name() {
                known.insert(name.to_string_lossy().to_string());
            }
            if provider.as_deref() == Some(target) {
                plan.already += 1;
                continue;
            }
            if !path.exists() {
                plan.missing += 1;
                continue;
            }
            let current = read_head_provider(&path).unwrap_or_else(|_| ("".to_string(), 0));
            if current.0 == target {
                plan.already += 1;
                continue;
            }
            *plan
                .by_provider
                .entry(if current.0.is_empty() {
                    "<无标记>".to_string()
                } else {
                    current.0.clone()
                })
                .or_insert(0) += 1;
            plan.changes.push(Change {
                path,
                thread_id: id,
                old_provider: current.0,
                delta: 0,
            });
        }
    }

    // Sessions written but never indexed (crash, or Codex closed mid-write).
    if let Ok(rd) = std::fs::read_dir(paths::sessions_dir()) {
        for entry in walkdir(rd) {
            let p = entry;
            let name = p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            if !name.starts_with("rollout-") {
                continue;
            }
            if name.ends_with(".zst") {
                plan.skipped_zst += 1;
                continue;
            }
            if !name.ends_with(".jsonl") {
                continue;
            }
            if known.contains(&name) {
                continue;
            }
            if let Ok((provider, _)) = read_head_provider(&p) {
                if provider != target {
                    plan.orphans.push((p, provider));
                }
            }
        }
    }

    Ok(plan)
}

fn walkdir(rd: std::fs::ReadDir) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    while let Some(p) = stack.pop() {
        if p.is_dir() {
            if let Ok(inner) = std::fs::read_dir(&p) {
                stack.extend(inner.flatten().map(|e| e.path()));
            }
        } else {
            out.push(p);
        }
    }
    out
}

/// Execute the plan: rewrite heads, then fix the indexes in one transaction each.
pub fn apply(plan: &mut Plan, target: &str) -> Result<Report> {
    let mut report = Report::default();

    for orphan in &plan.orphans {
        match rewrite_head(&orphan.0, target) {
            Ok(_) => report.orphans_rewritten += 1,
            Err(e) => report.errors.push(format!("{}: {e}", orphan.0.display())),
        }
    }

    let mut entries = Vec::new();
    for c in plan.changes.iter_mut() {
        match rewrite_head(&c.path, target) {
            Ok((delta, head)) => {
                c.delta = delta;
                report.files_rewritten += 1;
                entries.push(ManifestEntry {
                    path: c.path.to_string_lossy().to_string(),
                    thread_id: c.thread_id.clone(),
                    old_provider: c.old_provider.clone(),
                    delta,
                    original_head: head,
                });
            }
            Err(e) => report.errors.push(format!("{}: {e}", c.path.display())),
        }
    }

    // state_*.sqlite: the provider tag that drives the history list.
    for db in paths::state_databases() {
        if !db.exists() {
            continue;
        }
        let mut con = open_state_db(&db)?;
        let has_threads: bool = con
            .query_row(
                "select count(*) from sqlite_master where type='table' and name='threads'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if !has_threads {
            continue;
        }
        let tx = con.transaction()?;
        for e in &entries {
            let n = tx.execute(
                "update threads set model_provider = ?1 where id = ?2",
                params![target, e.thread_id],
            )?;
            report.threads_relabelled += n;
        }
        tx.commit()?;
    }

    // thread_history_*.sqlite: byte offsets into the rollout files we just grew.
    for db in paths::thread_history_databases() {
        if !db.exists() {
            continue;
        }
        let mut con = open_state_db(&db)?;
        let has_turns: bool = con
            .query_row(
                "select count(*) from sqlite_master where type='table' and name='thread_turns'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if !has_turns {
            continue;
        }
        let tx = con.transaction()?;
        for e in &entries {
            if e.delta == 0 {
                continue;
            }
            report.rows_shifted += tx.execute(
                "update thread_turns set rollout_byte_offset = rollout_byte_offset + ?1
                 where thread_id = ?2 and rollout_byte_offset > 0",
                params![e.delta, e.thread_id],
            )?;
            report.rows_shifted += tx.execute(
                "update thread_turns set rollout_end_byte_offset = rollout_end_byte_offset + ?1
                 where thread_id = ?2 and rollout_end_byte_offset > 0",
                params![e.delta, e.thread_id],
            )?;
            report.rows_shifted += tx.execute(
                "update thread_history_projection_state set next_rollout_byte_offset = next_rollout_byte_offset + ?1
                 where thread_id = ?2 and next_rollout_byte_offset > 0",
                params![e.delta, e.thread_id],
            )?;
        }
        tx.commit()?;
    }

    let manifest = Manifest {
        created: paths::timestamp(),
        target_provider: target.to_string(),
        entries,
    };
    let dir = paths::backup_root().join(format!("migrate-{}", paths::timestamp()));
    std::fs::create_dir_all(&dir)?;
    let text = serde_json::to_string_pretty(&manifest)?;
    paths::atomic_write(&dir.join("manifest.json"), text.as_bytes())?;
    Ok(report)
}

/// Put every rewritten session back where it came from.
pub fn rollback(manifest_path: &Path) -> Result<Report> {
    let text = std::fs::read_to_string(manifest_path)
        .with_context(|| format!("读取 {}", manifest_path.display()))?;
    let manifest: Manifest = serde_json::from_str(&text).context("解析 manifest")?;
    let mut report = Report::default();

    for e in &manifest.entries {
        let p = PathBuf::from(&e.path);
        if !p.exists() {
            report.errors.push(format!("{} 不存在", e.path));
            continue;
        }
        let restored = if e.original_head.is_empty() {
            rewrite_head(&p, &e.old_provider)
        } else {
            restore_head(&p, &e.original_head)
        };
        match restored {
            Ok((delta, _)) => {
                report.files_rewritten += 1;
                // Undo the offset shift using the reverse delta we just measured.
                if delta != 0 {
                    for db in paths::thread_history_databases() {
                        if let Ok(mut con) = open_state_db(&db) {
                            let tx = con.transaction()?;
                            let _ = tx.execute(
                                "update thread_turns set rollout_byte_offset = rollout_byte_offset + ?1
                                 where thread_id = ?2 and rollout_byte_offset > 0",
                                params![delta, e.thread_id],
                            );
                            let _ = tx.execute(
                                "update thread_turns set rollout_end_byte_offset = rollout_end_byte_offset + ?1
                                 where thread_id = ?2 and rollout_end_byte_offset > 0",
                                params![delta, e.thread_id],
                            );
                            let _ = tx.execute(
                                "update thread_history_projection_state set next_rollout_byte_offset = next_rollout_byte_offset + ?1
                                 where thread_id = ?2 and next_rollout_byte_offset > 0",
                                params![delta, e.thread_id],
                            );
                            tx.commit()?;
                        }
                    }
                }
            }
            Err(err) => report.errors.push(format!("{}: {err}", e.path)),
        }
        for db in paths::state_databases() {
            if let Ok(mut con) = open_state_db(&db) {
                let tx = con.transaction()?;
                let _ = tx.execute(
                    "update threads set model_provider = ?1 where id = ?2",
                    params![e.old_provider, e.thread_id],
                );
                tx.commit()?;
            }
        }
    }
    Ok(report)
}

/// Spot-check that every offset in the index still lands on a line boundary.
pub fn verify(target: &str) -> Result<(usize, usize, Vec<String>)> {
    let mut ok = 0usize;
    let mut bad = 0usize;
    let mut samples = Vec::new();

    let state = match paths::state_databases().into_iter().next() {
        Some(p) => p,
        None => return Ok((0, 0, vec!["找不到 state_*.sqlite".to_string()])),
    };
    let history = paths::thread_history_databases().into_iter().next();

    let con = Connection::open_with_flags(&state, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = con.prepare("select id, rollout_path from threads where model_provider = ?1")?;
    let rows: Vec<(String, String)> = stmt
        .query_map(params![target], |r| Ok((r.get(0)?, r.get(1)?)))?
        .flatten()
        .collect();

    let Some(history) = history else {
        return Ok((
            rows.len(),
            0,
            vec!["找不到 thread_history_*.sqlite".to_string()],
        ));
    };
    let hcon = Connection::open_with_flags(&history, OpenFlags::SQLITE_OPEN_READ_ONLY)?;

    for (id, path) in rows.iter().take(60) {
        let p = PathBuf::from(path);
        if !p.exists() {
            continue;
        }
        let offsets: Vec<i64> = hcon
            .prepare("select rollout_byte_offset from thread_turns where thread_id = ?1 and rollout_byte_offset > 0 order by rollout_ordinal limit 3")
            .and_then(|mut s| {
                let v = s
                    .query_map(params![id], |r| r.get::<_, i64>(0))?
                    .flatten()
                    .collect::<Vec<_>>();
                Ok(v)
            })
            .unwrap_or_default();
        if offsets.is_empty() {
            continue;
        }
        let mut f = File::open(&p)?;
        let mut buf = vec![0u8; 1];
        for off in offsets {
            if off <= 0 {
                continue;
            }
            // An offset is valid when the byte right before it is a newline:
            // that is exactly what "start of a rollout line" means. off equal
            // to the head length is the first line after session_meta.
            use std::io::{Seek, SeekFrom};
            f.seek(SeekFrom::Start(off as u64 - 1))?;
            f.read_exact(&mut buf)?;
            if buf[0] == b'\n' {
                ok += 1;
            } else {
                bad += 1;
                if samples.len() < 5 {
                    samples.push(format!("{id} offset {off} 不是行首"));
                }
            }
        }
    }
    Ok((ok, bad, samples))
}
