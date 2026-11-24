//! The one code path that actually performs a switch.
//!
//! Both front ends (the line-oriented CLI and the TUI) call into here. Nothing
//! in this module writes to stdout: every step is reported through a callback so
//! the TUI can render it in a pane instead of corrupting the screen.

use crate::auth;
use crate::backup;
use crate::config_edit;
use crate::history;
use crate::paths;
use crate::profile::{self, AuthKind, Keys, Profile, Profiles};
use anyhow::{bail, Context, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Step,
    Ok,
    Warn,
    Err,
    Note,
}

#[derive(Clone, Debug)]
pub struct Msg {
    pub level: Level,
    pub text: String,
}

impl Msg {
    pub fn new(level: Level, text: impl Into<String>) -> Self {
        Self {
            level,
            text: text.into(),
        }
    }
    pub fn step(t: impl Into<String>) -> Self {
        Self::new(Level::Step, t)
    }
    pub fn ok(t: impl Into<String>) -> Self {
        Self::new(Level::Ok, t)
    }
    pub fn warn(t: impl Into<String>) -> Self {
        Self::new(Level::Warn, t)
    }
    pub fn err(t: impl Into<String>) -> Self {
        Self::new(Level::Err, t)
    }
    pub fn note(t: impl Into<String>) -> Self {
        Self::new(Level::Note, t)
    }
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Options {
    pub dry_run: bool,
    pub no_history: bool,
    /// Proceed even when the Codex main process is running. The caller is
    /// expected to have asked the user first.
    pub force: bool,
}

pub fn load() -> Result<(Profiles, Keys)> {
    let p = profile::ensure_initialized()?;
    let k = profile::load_keys()?;
    Ok((p, k))
}

pub fn resolve(profiles: &Profiles, id: &str) -> Result<Profile> {
    profiles
        .get(id)
        .cloned()
        .with_context(|| format!("找不到 profile `{id}`，先跑 codex-switch list"))
}

/// What the user is about to change. Built without touching any file.
pub struct Preview {
    pub target: Profile,
    pub previous: Option<Profile>,
    pub rows: Vec<(String, String)>,
    /// The target needs a browser login before it can be used.
    pub needs_login: bool,
    /// Set when this switch would also relabel history.
    pub history_target: Option<String>,
}

pub fn preview(profiles: &Profiles, keys: &Keys, id: &str) -> Result<Preview> {
    let target = resolve(profiles, id)?;
    let api_key = target.api_key(keys);
    let rows = config_edit::summarize(&target, api_key.as_deref());

    let auth_now = auth::read().unwrap_or_default();
    let state = auth::inspect(&auth_now);
    let stashed_ok = if target.auth == AuthKind::Chatgpt && !state.has_tokens {
        auth::load_stashed()
            .map(|v| auth::inspect(&v).has_tokens)
            .unwrap_or(false)
    } else {
        true
    };

    let previous_provider = profiles
        .get(&profiles.active)
        .map(|p| p.history_provider().to_string());
    let target_provider = target.history_provider().to_string();
    let history_target = if previous_provider.as_deref() == Some(target_provider.as_str()) {
        None
    } else {
        Some(target_provider)
    };

    Ok(Preview {
        needs_login: target.auth == AuthKind::Chatgpt && !stashed_ok,
        history_target,
        target,
        previous: profiles.get(&profiles.active).cloned(),
        rows,
    })
}

/// Bail when Codex is running and the caller did not clear it.
pub fn guard_processes(force: bool) -> Result<()> {
    let procs = paths::running_codex_processes();
    if procs.has_critical() && !force {
        bail!(
            "Codex 主进程正在运行 ({})\n配置只在启动时读取，运行中写入会被覆盖或与数据库锁冲突。请先完全退出 Codex。",
            procs.critical.join(", ")
        );
    }
    Ok(())
}

/// Relabel every saved conversation to `provider`.
pub fn migrate_history<F: FnMut(Msg)>(provider: &str, mut emit: F) -> Result<()> {
    let mut plan = history::scan(provider)?;
    for m in plan_messages(&plan, provider) {
        emit(m);
    }
    if plan.is_empty() {
        return Ok(());
    }
    match history::apply(&mut plan, provider) {
        Ok(rep) => {
            emit(Msg::ok(format!(
                "迁移完成: {} 个会话文件, {} 条索引, {} 条偏移",
                rep.files_rewritten, rep.threads_relabelled, rep.rows_shifted
            )));
            if !rep.errors.is_empty() {
                emit(Msg::warn(format!(
                    "{} 个文件迁移失败（可重跑）",
                    rep.errors.len()
                )));
                for e in rep.errors.iter().take(5) {
                    emit(Msg::note(e.clone()));
                }
            }
        }
        Err(e) => emit(Msg::err(format!("历史迁移失败: {e}"))),
    }
    Ok(())
}

pub fn plan_messages(plan: &history::Plan, target: &str) -> Vec<Msg> {
    let mut out = Vec::new();
    if plan.is_empty() {
        out.push(Msg::ok(format!("历史会话已全部在 `{target}` 下，无需迁移")));
        return out;
    }
    let detail = plan
        .by_provider
        .iter()
        .map(|(k, v)| format!("{k}: {v}"))
        .collect::<Vec<_>>()
        .join(", ");
    out.push(Msg::step(format!(
        "历史会话 → `{target}`：待改 {} 个{}",
        plan.changes.len(),
        if detail.is_empty() {
            String::new()
        } else {
            format!("（{detail}）")
        }
    )));
    if !plan.orphans.is_empty() {
        out.push(Msg::note(format!(
            "未入库的会话文件 {} 个",
            plan.orphans.len()
        )));
    }
    if plan.already > 0 {
        out.push(Msg::note(format!(
            "{} 个已在目标 provider 下",
            plan.already
        )));
    }
    if plan.missing > 0 {
        out.push(Msg::warn(format!(
            "{} 条索引指向的文件已不存在",
            plan.missing
        )));
    }
    if plan.skipped_zst > 0 {
        out.push(Msg::note(format!(
            "{} 个 .zst 归档（不在索引内，跳过）",
            plan.skipped_zst
        )));
    }
    out
}

/// Execute a switch.
///
/// Order matters and is load-bearing: back up first, write config before auth,
/// and only relabel history once both files are on disk. Anything that fails
/// after the backup leaves a restorable snapshot behind.
pub fn run<F: FnMut(Msg)>(id: &str, opts: Options, mut emit: F) -> Result<()> {
    let (mut profiles, keys) = load()?;
    let target = resolve(&profiles, id)?;
    let api_key = target.api_key(&keys);

    if target.auth == AuthKind::Apikey && api_key.is_none() {
        bail!(
            "profile `{}` 使用 API Key 但没有设置。先运行: codex-switch key {}",
            target.id,
            target.id
        );
    }

    if !opts.dry_run {
        guard_processes(opts.force)?;
    }

    let cfg_text = std::fs::read_to_string(paths::config_path()).unwrap_or_default();
    let new_cfg = config_edit::render(&cfg_text, &target, api_key.as_deref())?;

    let mut auth_now = auth::read()?;
    let auth_state = auth::inspect(&auth_now);
    // A previous switch may have stashed a valid ChatGPT login. Recover it
    // instead of forcing another browser round-trip.
    if target.auth == AuthKind::Chatgpt && !auth_state.has_tokens {
        if let Some(stashed) = auth::load_stashed() {
            let st = auth::inspect(&stashed);
            if st.has_tokens {
                auth_now = stashed;
                emit(Msg::note("从登录 stash 恢复了 ChatGPT 凭证"));
            }
        }
    }
    let new_auth = match target.auth {
        AuthKind::Chatgpt => auth::render_for_chatgpt(&auth_now),
        AuthKind::Apikey => {
            auth::render_for_apikey(&auth_now, api_key.as_deref().unwrap_or_default())
        }
    };

    // The plan itself is rendered by `preview`, so a caller that already showed
    // it does not have to look at the same rows twice.
    if opts.dry_run {
        let current_provider = profiles
            .get(&profiles.active)
            .map(|p| p.history_provider().to_string());
        if current_provider.as_deref() != Some(target.history_provider()) {
            let plan = history::scan(target.history_provider())?;
            for m in plan_messages(&plan, target.history_provider()) {
                emit(m);
            }
        }
        emit(Msg::warn("--dry-run：以上变更未写入"));
        return Ok(());
    }

    let bk = backup::create("switch")?;
    emit(Msg::ok(format!(
        "备份 {} 个文件（{}）→ {}",
        bk.files.len(),
        backup::human(bk.bytes),
        bk.dir.display()
    )));

    paths::atomic_write(&paths::config_path(), new_cfg.as_bytes())?;
    emit(Msg::ok("config.toml 已更新"));

    auth::stash(&auth_now)?;
    auth::write(&new_auth)?;
    emit(Msg::ok("auth.json 已更新"));

    if !opts.no_history {
        migrate_history(target.history_provider(), &mut emit)?;
    }

    profiles.active = target.id.clone();
    profile::save_profiles(&profiles)?;

    emit(Msg::ok(format!("已切换到 {}", target.label)));
    emit(Msg::note("请完全退出并重新打开 Codex 让配置生效"));
    Ok(())
}
