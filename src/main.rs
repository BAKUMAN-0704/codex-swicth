mod auth;
mod backup;
mod config_edit;
mod font;
mod history;
mod paths;
mod profile;
mod switch;
mod tui;
mod ui;

use anyhow::{bail, Context, Result};
use profile::{AuthKind, Keys, Profile, Profiles};
use std::io::{self, Write};
use std::path::PathBuf;

#[derive(Default, Clone, Copy)]
struct Opts {
    dry_run: bool,
    yes: bool,
    no_history: bool,
    force: bool,
}

fn main() {
    ui::enable_ansi();
    if let Err(e) = run() {
        eprintln!("\n{} {:#}", ui::red("错误:"), e);
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    // `font --face <name>` carries its own flags. Route it before the global
    // option parser, which would reject the leading dashes.
    if argv.first().map(|s| s.as_str()) == Some("font") {
        return cmd_font(&argv[1..]);
    }

    let mut opts = Opts::default();
    let mut positional: Vec<String> = Vec::new();

    for a in &argv {
        match a.as_str() {
            "--dry-run" | "-n" => opts.dry_run = true,
            "--yes" | "-y" => opts.yes = true,
            "--no-history" => opts.no_history = true,
            "--force" | "-f" => opts.force = true,
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            "--version" | "-V" => {
                println!("codex-switch {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            s if s.starts_with('-') => bail!("未知参数 {s}，用 --help 查看用法"),
            s => positional.push(s.to_string()),
        }
    }

    let cmd = positional.first().map(|s| s.as_str()).unwrap_or("");
    let rest: Vec<&str> = positional.iter().skip(1).map(|s| s.as_str()).collect();

    match cmd {
        "" | "tui" => tui::run(),
        "menu" => interactive(opts),
        "list" | "ls" => cmd_list(),
        "status" => cmd_status(),
        "use" | "switch" => {
            let id = rest
                .first()
                .context("用法: codex-switch use <profile-id>")?;
            do_switch(id, opts)
        }
        "add" => cmd_add(&rest),
        "key" => cmd_key(&rest),
        "history" => cmd_history(rest.first().copied(), opts),
        "rollback" => cmd_rollback(rest.first().copied(), opts),
        "backup" => cmd_backup(),
        "clean" => {
            let n = backup::clean();
            println!("{} 已清理 {n} 个旧备份快照（迁移记录保留）", ui::green("✓"));
            Ok(())
        }
        "restore" => cmd_restore(rest.first().copied(), opts),
        "verify" => cmd_verify(),
        "doctor" | "check" => cmd_doctor(),
        "import" => cmd_import(opts),
        other => bail!("未知命令 {other}，用 --help 查看用法"),
    }
}

fn print_help() {
    println!(
        "{}\n\n{}",
        ui::bold("codex-switch —— Codex 登录方式与供应商一键切换"),
        ui::bold("用法:")
    );
    for line in [
        "  codex-switch                     打开 TUI（可视界面）",
        "  codex-switch tui                 同上",
        "  codex-switch menu                纯文本交互菜单（不支持 TUI 的终端用）",
        "  codex-switch list                列出所有 profile",
        "  codex-switch status              当前配置与凭据状态",
        "  codex-switch use <id>            切换到某个 profile",
        "  codex-switch history [id]        只迁移历史会话到目标 provider",
        "  codex-switch rollback [dir]      回滚最近一次历史迁移",
        "  codex-switch add <id> --base-url <url> [--model <m>] [--auth apikey|chatgpt]",
        "  codex-switch key <id>            设置某个 profile 的 API Key",
        "  codex-switch backup              手动备份 auth/config/sqlite",
        "  codex-switch clean               清理旧备份快照（保留迁移记录）",
        "  codex-switch restore [dir]       从备份还原",
        "  codex-switch verify              校验会话偏移是否仍然对齐",
        "  codex-switch doctor              环境自检",
        "  codex-switch import              从当前 config.toml / auth.json 重新导入 profile",
    ] {
        println!("{line}");
    }

    println!("\n{}", ui::bold("字体"));
    for line in [
        "  codex-switch font                查看当前终端字体与配置文件位置",
        "  codex-switch font --list         列出系统已安装的字体",
        "  codex-switch font --face <名称>  切换 Windows Terminal 的字体",
        "  codex-switch font --restore      还原上一次字体修改",
    ] {
        println!("{line}");
    }

    println!("\n{}", ui::bold("选项:"));
    for line in [
        "  -n, --dry-run    只显示计划，不写任何文件",
        "  -y, --yes        跳过确认",
        "      --no-history 切换时不迁移历史会话",
        "  -f, --force      Codex 正在运行时也继续（风险自负）",
    ] {
        println!("{line}");
    }
}

fn load() -> Result<(Profiles, Keys)> {
    switch::load()
}

/// Render one execution step coming back from `switch::run`.
fn print_msg(m: switch::Msg) {
    use switch::Level::*;
    let tag = match m.level {
        Step => ui::cyan("→"),
        Ok => ui::green("✓"),
        Warn => ui::yellow("!"),
        Err => ui::red("✗"),
        Note => ui::dim("·"),
    };
    println!("  {tag} {}", m.text);
}

fn cmd_list() -> Result<()> {
    let (profiles, keys) = load()?;
    let auth = auth::read().unwrap_or_default();
    let state = auth::inspect(&auth);
    print_table(&profiles, &keys, &state);
    Ok(())
}

fn print_table(profiles: &Profiles, keys: &Keys, state: &auth::AuthState) {
    println!("\n{}", ui::bold("profiles"));
    println!(
        "  {:<3} {:<14} {:<22} {:<12} {}",
        "",
        ui::dim("id"),
        ui::dim("名称"),
        ui::dim("认证"),
        ui::dim("API Key")
    );
    for (i, p) in profiles.profiles.iter().enumerate() {
        let mark = if p.id == profiles.active {
            ui::green("*")
        } else {
            " ".to_string()
        };
        let key_state = if p.is_official() && p.auth == AuthKind::Chatgpt {
            if state.has_tokens {
                ui::green("已登录")
            } else {
                ui::yellow("需 codex login")
            }
        } else if p.auth == AuthKind::Chatgpt {
            if state.has_tokens {
                ui::green("复用登录态")
            } else {
                ui::yellow("需 codex login")
            }
        } else {
            keys.masked(&p.id)
        };
        let label = if p.label.chars().count() > 20 {
            format!("{}…", p.label.chars().take(19).collect::<String>())
        } else {
            p.label.clone()
        };
        println!(
            "  {:<3} {:<14} {:<22} {:<12} {}",
            mark,
            format!("{}. {}", i + 1, p.id),
            label,
            p.auth.label(),
            key_state
        );
    }
    println!();
}

fn cmd_status() -> Result<()> {
    let (profiles, keys) = load()?;
    let cfg = std::fs::read_to_string(paths::config_path()).unwrap_or_default();
    let doc: toml_edit::DocumentMut = cfg.parse().unwrap_or_default();
    let provider = doc
        .get("model_provider")
        .and_then(|v| v.as_str())
        .unwrap_or("openai");
    let model = doc
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("<未设置>");
    let auth_kind = doc
        .get("preferred_auth_method")
        .and_then(|v| v.as_str())
        .unwrap_or("<未设置>");

    let auth = auth::read().unwrap_or_default();
    let state = auth::inspect(&auth);

    println!("\n{}", ui::bold("当前状态"));
    println!("  Codex 目录     {}", paths::codex_home().display());
    println!("  活动 profile   {}", ui::cyan(&profiles.active));
    println!("  model_provider {provider}");
    println!("  model          {model}");
    println!("  auth 方式      {auth_kind}");
    println!("  auth.json      {}", state.describe());
    println!("  API Key 记录   {} 个", {
        let mut n = 0;
        for p in &profiles.profiles {
            if keys.get(&p.id).is_some() {
                n += 1;
            }
        }
        n
    });

    let procs = paths::running_codex_processes();
    if procs.critical.is_empty() {
        println!(
            "  Codex 进程     {} {}",
            ui::green("主进程未运行"),
            if procs.helper.is_empty() {
                String::new()
            } else {
                ui::dim(&procs.describe())
            }
        );
    } else {
        println!("  Codex 进程     {}", ui::yellow(&procs.describe()));
    }
    println!();
    Ok(())
}

fn interactive(opts: Opts) -> Result<()> {
    let (profiles, keys) = load()?;
    let auth = auth::read().unwrap_or_default();
    let state = auth::inspect(&auth);

    loop {
        println!(
            "\n{}  {}",
            ui::bold("codex-switch"),
            ui::dim(&format!("v{}", env!("CARGO_PKG_VERSION")))
        );
        println!(
            "当前: {}  ·  {}\n",
            ui::cyan(&profiles.active),
            state.describe()
        );
        print_table(&profiles, &keys, &state);
        print!(
            "{} ",
            ui::bold("输入编号切换，或 [s]状态 [v]校验 [b]备份 [q]退出 >")
        );
        io::stdout().flush()?;
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            return Ok(());
        }
        let input = line.trim().to_string();
        match input.as_str() {
            "" => continue,
            "q" | "quit" | "exit" => return Ok(()),
            "s" | "status" => {
                cmd_status()?;
                continue;
            }
            "v" | "verify" => {
                cmd_verify()?;
                continue;
            }
            "b" | "backup" => {
                cmd_backup()?;
                continue;
            }
            _ => {}
        }
        let idx: usize = match input.parse::<usize>() {
            Ok(n) if n >= 1 && n <= profiles.profiles.len() => n - 1,
            _ => {
                println!("{}", ui::yellow("无效输入"));
                continue;
            }
        };
        let target = profiles.profiles[idx].id.clone();
        if target == profiles.active {
            println!("{}", ui::dim("已经是当前 profile"));
            continue;
        }
        do_switch(&target, opts)?;
        return Ok(());
    }
}

/// Everything that has to happen for one switch, in a safe order.
fn do_switch(id: &str, opts: Opts) -> Result<()> {
    let (profiles, keys) = load()?;
    let p = switch::preview(&profiles, &keys, id)?;

    println!(
        "\n{} {}  {}  {}",
        ui::bold("切换:"),
        p.previous
            .as_ref()
            .map(|x| x.id.clone())
            .unwrap_or_else(|| "-".into()),
        ui::dim("→"),
        ui::cyan(&p.target.id)
    );
    for (k, v) in &p.rows {
        println!("  {:<22} {}", ui::dim(k), v);
    }
    if p.needs_login {
        println!(
            "\n{} auth.json 里没有可用的 ChatGPT 登录态。切换后请在终端执行一次 {} 完成浏览器登录。",
            ui::yellow("提示:"),
            ui::cyan("codex login")
        );
    }

    if opts.dry_run {
        switch::run(
            id,
            switch::Options {
                dry_run: true,
                ..Default::default()
            },
            print_msg,
        )?;
        return Ok(());
    }

    let procs = paths::running_codex_processes();
    let mut force = opts.force;
    if procs.has_critical() && !force {
        let msg = format!(
            "检测到 Codex 主进程正在运行: {}\n配置在启动时读取，运行中写入可能被覆盖或与数据库锁冲突。",
            procs.critical.join(", ")
        );
        if opts.yes {
            bail!("{msg}\n请先完全退出 Codex，或加 --force 强制执行。");
        }
        println!("\n{} {}", ui::yellow("警告:"), msg);
        if !ui::confirm("仍然继续？", false)? {
            return Ok(());
        }
        force = true;
    }

    if !opts.yes && !ui::confirm("\n确认写入？", true)? {
        println!("{}", ui::dim("已取消"));
        return Ok(());
    }

    switch::run(
        id,
        switch::Options {
            no_history: opts.no_history,
            force,
            ..Default::default()
        },
        print_msg,
    )?;
    Ok(())
}

fn print_plan(plan: &history::Plan, target: &str) {
    println!("\n{}", ui::bold(&format!("历史会话迁移 → `{target}`")));
    for m in switch::plan_messages(plan, target) {
        print_msg(m);
    }
}

fn cmd_history(id: Option<&str>, opts: Opts) -> Result<()> {
    let (profiles, _) = load()?;
    let target_id = id.unwrap_or(profiles.active.as_str());
    let p = profiles
        .get(target_id)
        .with_context(|| format!("找不到 profile `{target_id}`"))?;
    let provider = p.history_provider().to_string();

    let mut plan = history::scan(&provider)?;
    print_plan(&plan, &provider);
    if plan.is_empty() {
        return Ok(());
    }
    if opts.dry_run {
        println!("\n{}", ui::yellow("--dry-run：未写入"));
        return Ok(());
    }
    let procs = paths::running_codex_processes();
    if procs.has_critical() && !opts.force {
        bail!(
            "Codex 主进程正在运行 ({})，请先完全退出",
            procs.critical.join(", ")
        );
    }
    if !opts.yes && !ui::confirm("\n开始迁移？", true)? {
        println!("{}", ui::dim("已取消"));
        return Ok(());
    }
    let bk = backup::create("history")?;
    println!(
        "  {} 备份 {} 个文件 ({})",
        ui::green("✓"),
        bk.files.len(),
        backup::human(bk.bytes)
    );
    let rep = history::apply(&mut plan, &provider)?;
    println!(
        "  {} 改写 {} 个文件，更新 {} 条索引，修正 {} 条偏移",
        ui::green("✓"),
        rep.files_rewritten,
        rep.threads_relabelled,
        rep.rows_shifted
    );
    for e in rep.errors.iter().take(10) {
        println!("  {} {}", ui::yellow("!"), e);
    }
    Ok(())
}

fn cmd_rollback(dir: Option<&str>, opts: Opts) -> Result<()> {
    let manifest = match dir {
        Some(d) => PathBuf::from(d).join("manifest.json"),
        None => {
            let mut found = None;
            for d in backup::list()? {
                if d.join("manifest.json").exists() {
                    found = Some(d.join("manifest.json"));
                    break;
                }
            }
            found.context("找不到任何迁移记录（backups/migrate-*/manifest.json）")?
        }
    };
    println!("使用迁移记录: {}", manifest.display());
    if opts.dry_run {
        println!("{}", ui::yellow("--dry-run：未写入"));
        return Ok(());
    }
    if !opts.yes && !ui::confirm("把所有会话还原成迁移前的 provider？", true)? {
        return Ok(());
    }
    let rep = history::rollback(&manifest)?;
    println!(
        "  {} 还原 {} 个文件，{} 个错误",
        ui::green("✓"),
        rep.files_rewritten,
        rep.errors.len()
    );
    for e in rep.errors.iter().take(5) {
        println!("    {}", ui::dim(e));
    }
    Ok(())
}

fn cmd_verify() -> Result<()> {
    let (profiles, _) = load()?;
    let provider = profiles
        .get(&profiles.active)
        .map(|p| p.history_provider().to_string())
        .unwrap_or_else(|| "openai".to_string());
    println!("校验 provider = {provider} 的会话偏移…");
    let (ok, bad, samples) = history::verify(&provider)?;
    if bad == 0 {
        println!("  {} {ok} 个偏移全部落在行首", ui::green("✓"));
    } else {
        println!("  {} {ok} 个正常，{bad} 个异常", ui::red("✗"));
        for s in samples {
            println!("    {}", ui::dim(&s));
        }
    }
    Ok(())
}

fn cmd_backup() -> Result<()> {
    let bk = backup::create("manual")?;
    println!(
        "{} 备份 {} 个文件 ({}) → {}",
        ui::green("✓"),
        bk.files.len(),
        backup::human(bk.bytes),
        bk.dir.display()
    );
    Ok(())
}

fn cmd_restore(dir: Option<&str>, opts: Opts) -> Result<()> {
    let target = match dir {
        Some(d) => PathBuf::from(d),
        None => backup::latest().context("没有可用备份")?,
    };
    println!("从 {} 还原", target.display());
    if opts.dry_run {
        println!("{}", ui::yellow("--dry-run：未写入"));
        return Ok(());
    }
    let procs = paths::running_codex_processes();
    if procs.has_critical() {
        bail!(
            "Codex 主进程正在运行 ({})，请先完全退出",
            procs.critical.join(", ")
        );
    }
    if !opts.yes && !ui::confirm("覆盖当前 auth.json / config.toml / sqlite？", false)? {
        return Ok(());
    }
    let done = backup::restore(&target)?;
    println!("{} 已还原: {}", ui::green("✓"), done.join(", "));
    Ok(())
}

fn cmd_doctor() -> Result<()> {
    println!("\n{}", ui::bold("环境自检"));
    let ch = paths::codex_home();
    println!(
        "  Codex 目录      {} {}",
        ch.display(),
        if ch.exists() {
            ui::green("✓")
        } else {
            ui::red("✗")
        }
    );
    let cfg = paths::config_path();
    println!(
        "  config.toml     {}",
        if cfg.exists() {
            ui::green("✓")
        } else {
            ui::yellow("不存在")
        }
    );
    let au = paths::auth_path();
    println!(
        "  auth.json       {}",
        if au.exists() {
            ui::green("✓")
        } else {
            ui::yellow("不存在")
        }
    );
    println!("  sessions        {} 个会话文件", count_sessions());
    println!("  state 数据库    {} 个", paths::state_databases().len());
    println!(
        "  history 数据库  {} 个",
        paths::thread_history_databases().len()
    );
    let procs = paths::running_codex_processes();
    println!(
        "  Codex 进程      {}",
        if procs.critical.is_empty() {
            ui::green(&procs.describe())
        } else {
            ui::yellow(&procs.describe())
        }
    );
    println!("  switch 目录     {}", paths::switch_home().display());
    let (n, bytes) = backup::usage();
    println!("  备份占用        {} 个目录, {}", n, backup::human(bytes));
    println!();
    Ok(())
}

fn count_sessions() -> usize {
    fn walk(p: &std::path::Path, n: &mut usize) {
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                let path = e.path();
                if path.is_dir() {
                    walk(&path, n);
                } else if path
                    .file_name()
                    .map(|f| f.to_string_lossy().starts_with("rollout-"))
                    .unwrap_or(false)
                {
                    *n += 1;
                }
            }
        }
    }
    let mut n = 0;
    walk(&paths::sessions_dir(), &mut n);
    n
}

fn cmd_add(rest: &[&str]) -> Result<()> {
    let id = rest.first().context(
        "用法: codex-switch add <id> --base-url <url> [--model <m>] [--auth apikey|chatgpt]",
    )?;
    profile::validate_id(id)?;

    let mut base_url = String::new();
    let mut model = String::new();
    let mut wire = "responses".to_string();
    let mut kind = AuthKind::Apikey;
    let mut label = String::new();

    let mut i = 1;
    while i < rest.len() {
        match rest[i] {
            "--base-url" | "-b" => {
                base_url = rest.get(i + 1).context("--base-url 缺少值")?.to_string();
                i += 2;
            }
            "--model" | "-m" => {
                model = rest.get(i + 1).context("--model 缺少值")?.to_string();
                i += 2;
            }
            "--wire" => {
                wire = rest.get(i + 1).context("--wire 缺少值")?.to_string();
                i += 2;
            }
            "--auth" => {
                let v = rest.get(i + 1).context("--auth 缺少值")?;
                kind = match *v {
                    "apikey" | "api-key" => AuthKind::Apikey,
                    "chatgpt" | "oauth" => AuthKind::Chatgpt,
                    other => bail!("--auth 只能是 apikey 或 chatgpt，收到 {other}"),
                };
                i += 2;
            }
            "--label" => {
                label = rest.get(i + 1).context("--label 缺少值")?.to_string();
                i += 2;
            }
            other => bail!("未知参数 {other}"),
        }
    }
    if base_url.is_empty() {
        bail!("必须提供 --base-url，例如 https://api.example.com/v1");
    }

    let (mut profiles, mut keys) = load()?;
    if profiles.get(id).is_some() {
        bail!("profile `{id}` 已存在，用 codex-switch list 查看");
    }
    profiles.profiles.push(Profile {
        id: id.to_string(),
        label: if label.is_empty() {
            format!("{id} 供应商")
        } else {
            label
        },
        note: base_url.clone(),
        auth: kind,
        provider: id.to_string(),
        base_url,
        wire_api: wire,
        requires_openai_auth: kind == AuthKind::Chatgpt,
        model,
        reasoning_effort: String::new(),
        disable_response_storage: Some(true),
    });
    profile::save_profiles(&profiles)?;

    if kind == AuthKind::Apikey {
        println!("已添加 `{id}`。现在设置它的 API Key：");
        let key = ui::prompt("API Key")?;
        if !key.trim().is_empty() {
            keys.set(id, key.trim());
            profile::save_keys(&keys)?;
            println!("{} Key 已保存", ui::green("✓"));
        }
    }
    Ok(())
}

fn cmd_key(rest: &[&str]) -> Result<()> {
    let (profiles, mut keys) = load()?;
    let id = match rest.first() {
        Some(v) => v.to_string(),
        None => profiles.active.clone(),
    };
    if profiles.get(&id).is_none() {
        bail!("找不到 profile `{id}`");
    }
    if let Some(v) = rest.get(1) {
        keys.set(&id, v);
        profile::save_keys(&keys)?;
        println!(
            "{} `{id}` 的 Key 已更新为 {}",
            ui::green("✓"),
            keys.masked(&id)
        );
        return Ok(());
    }
    let key = ui::prompt(&format!("`{id}` 的 API Key"))?;
    if key.trim().is_empty() {
        bail!("Key 为空，未修改");
    }
    keys.set(&id, key.trim());
    profile::save_keys(&keys)?;
    println!(
        "{} `{id}` 的 Key 已保存为 {}",
        ui::green("✓"),
        keys.masked(&id)
    );
    Ok(())
}

fn cmd_import(opts: Opts) -> Result<()> {
    let (mut imported, imported_keys) = profile::import_current()?;
    let (mut existing, mut keys) = load()?;
    if opts.dry_run {
        println!("会从当前 config.toml / auth.json 导入:");
        for p in &imported.profiles {
            println!("  {} ({}) {}", p.id, p.auth.label(), ui::dim(&p.label));
        }
        return Ok(());
    }
    for p in imported.profiles.drain(..) {
        if let Some(slot) = existing.profiles.iter_mut().find(|x| x.id == p.id) {
            *slot = p;
        } else {
            existing.profiles.push(p);
        }
    }
    for (id, key) in imported_keys.iter() {
        keys.set(id, key);
    }
    profile::save_profiles(&existing)?;
    profile::save_keys(&keys)?;
    println!(
        "{} 已从当前配置导入，共 {} 个 profile",
        ui::green("✓"),
        existing.profiles.len()
    );
    Ok(())
}

/// `codex-switch font`: the one terminal setting a TUI cannot make for itself.
fn cmd_font(args: &[String]) -> Result<()> {
    let mut face: Option<String> = None;
    let mut restore = false;
    let mut dry_run = false;
    let mut yes = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--face" | "-f" => {
                face = Some(args.get(i + 1).context("--face 缺少字体名")?.clone());
                i += 2;
            }
            "--restore" => {
                restore = true;
                i += 1;
            }
            "--dry-run" | "-n" => {
                dry_run = true;
                i += 1;
            }
            "--yes" | "-y" => {
                yes = true;
                i += 1;
            }
            "--list" => {
                let Some(list) = font::installed_faces() else {
                    bail!("读取字体列表仅支持 Windows");
                };
                println!("\n{}", ui::bold("已安装的字体"));
                for name in list {
                    println!("  {name}");
                }
                println!();
                return Ok(());
            }
            other => bail!(
                "font: 未知参数 {other}\n用法: codex-switch font [--face <名称>] [--list] [--restore]"
            ),
        }
    }

    let Some(path) = font::locate() else {
        println!(
            "\n{} 没找到 Windows Terminal 的 settings.json。",
            ui::yellow("提示:")
        );
        println!(
            "  手动设置：设置 → 配置文件 → 默认值 → 外观 → 字体 → {}",
            font::DEFAULT_FACE
        );
        println!("  其他终端（WezTerm / Alacritty / iTerm2）请改各自的配置文件。\n");
        return Ok(());
    };

    let src = std::fs::read_to_string(&path).with_context(|| format!("读取 {}", path.display()))?;
    let current = font::current_face(&src)?.unwrap_or_else(|| "<未设置>".to_string());

    if restore {
        let Some(file) = latest_wt_backup() else {
            bail!("没有字体设置的备份（backups/wt-settings-*.json）");
        };
        if dry_run {
            println!("会把 {} 还原到 {}", file.display(), path.display());
            return Ok(());
        }
        if !yes
            && !ui::confirm(
                &format!("用 {} 覆盖当前 settings.json？", file.display()),
                true,
            )?
        {
            return Ok(());
        }
        std::fs::copy(&file, &path).with_context(|| format!("还原 {}", path.display()))?;
        println!(
            "{} 字体设置已还原，Windows Terminal 会自动重载",
            ui::green("✓")
        );
        println!("  {}", ui::dim(&file.display().to_string()));
        return Ok(());
    }

    println!("\n{}", ui::bold("终端字体"));
    println!("  配置文件   {}", path.display());
    println!("  当前字体   {}", ui::cyan(&current));
    if let Some(t) = &face {
        println!("  目标字体   {}", ui::cyan(t));
    }
    if std::env::var("WT_SESSION")
        .map(|v| v.is_empty())
        .unwrap_or(true)
    {
        println!(
            "\n  {} 当前不在 Windows Terminal 会话里；双击 exe 打开的系统控制台不读这份配置。",
            ui::yellow("注意:")
        );
    }

    let Some(target) = face else {
        println!(
            "\n{}\n  codex-switch font --face \"{}\"\n  codex-switch font --list\n",
            ui::dim("切换字体:"),
            font::DEFAULT_FACE
        );
        return Ok(());
    };

    if target == current {
        println!("\n{} 已经是这个字体了", ui::green("✓"));
        return Ok(());
    }

    // Pure: validates that the setting exists and that the splice round-trips.
    let updated = font::set_face(&src, &target)?;

    if font::face_installed(&target) == Some(false) {
        println!(
            "\n  {} 系统里没找到字体 `{}`，Windows Terminal 会静默回退到默认字体。",
            ui::yellow("警告:"),
            target
        );
    }

    if dry_run {
        println!(
            "\n{} 只演练，未写入。settings.json 里的这一行会变成：",
            ui::yellow("--dry-run：")
        );
        println!("  \"face\": \"{target}\"");
        return Ok(());
    }

    if !yes && !ui::confirm(&format!("\n把终端字体改成 `{target}`？"), true)? {
        println!("{}", ui::dim("已取消"));
        return Ok(());
    }

    let dir = paths::backup_root();
    std::fs::create_dir_all(&dir).with_context(|| format!("创建 {}", dir.display()))?;
    let backup = dir.join(format!("wt-settings-{}.json", paths::timestamp()));
    std::fs::copy(&path, &backup).with_context(|| format!("备份 {}", path.display()))?;
    paths::atomic_write(&path, updated.as_bytes())?;

    println!("\n{} 字体已切换为 {}", ui::green("✓"), ui::cyan(&target));
    println!("  {} {}", ui::dim("备份"), backup.display());
    println!(
        "  {}",
        ui::dim("Windows Terminal 会自动重载；已打开的标签页可能要新开一个才用上新字体。")
    );
    if is_proportional(&target) {
        println!(
            "\n  {} 这不是等宽字体，字符宽度不一致，表格列与边框可能对不齐。",
            ui::yellow("提醒:")
        );
        println!(
            "  {}",
            ui::dim("终端里更稳妥的是等宽 CJK 字体，例如 Sarasa Mono SC / Maple Mono NF CN。")
        );
    }
    println!();
    Ok(())
}

/// Known proportional CJK families. Anything with `Mono` in the name is left
/// alone; the list only exists to warn, never to block.
fn is_proportional(face: &str) -> bool {
    let f = face.to_ascii_lowercase();
    (f.starts_with("harmonyos sans")
        || f.starts_with("source han sans")
        || f.starts_with("noto sans cjk"))
        && !f.contains("mono")
}

fn latest_wt_backup() -> Option<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(paths::backup_root())
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().starts_with("wt-settings-"))
                .unwrap_or(false)
        })
        .collect();
    files.sort();
    files.pop()
}
