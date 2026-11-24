//! Full-screen TUI: pick a profile, see exactly what will change, switch.
//!
//! Two things shape this file:
//!
//! * A terminal programme cannot choose its own font. That belongs to the
//!   terminal emulator, so the layout never assumes a light or dark palette and
//!   uses `REVERSED` for emphasis — it stays readable either way. `codex-switch
//!   font` flips the Windows Terminal setting if you want a different face.
//! * Probing for running processes forks a helper on Windows. Doing that per
//!   frame makes the UI feel dead, so the result is cached and refreshed only
//!   when something actually happens.

use crate::auth;
use crate::backup;
use crate::history;
use crate::paths::{self, RunningProcs};
use crate::profile::{self, AuthKind, Keys, Profile, Profiles};
use crate::switch::{self, Level, Msg, Options};
use anyhow::{bail, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;
use unicode_width::UnicodeWidthStr;

pub fn run() -> Result<()> {
    // Piped stdin/stdout has no raw mode. Fail with a usable message instead of
    // letting the terminal backend panic halfway through setup.
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("当前不是交互式终端，TUI 无法启动。可改用: codex-switch menu");
    }
    let mut app = App::new()?;
    let mut terminal = ratatui::init();
    let result = app.event_loop(&mut terminal);
    ratatui::restore();
    result
}

struct App {
    profiles: Profiles,
    keys: Keys,
    state: auth::AuthState,
    selected: usize,
    log: Vec<Msg>,
    status: String,
    mode: Mode,
    quit: bool,
    /// Cached: spawning a probe helper per frame would stall the event loop.
    procs: RunningProcs,
    /// Cached backup footprint (directory count, bytes).
    backups: (usize, u64),
}

enum Mode {
    Browse,
    Confirm(Confirm),
    Form(Form),
    Help,
}

struct Confirm {
    title: String,
    body: Vec<String>,
    action: Action,
}

struct Form {
    title: String,
    hint: String,
    fields: Vec<Field>,
    focus: usize,
    action: FormAction,
}

struct Field {
    label: &'static str,
    value: String,
    secret: bool,
}

#[derive(Clone)]
enum Action {
    Switch(String),
    Migrate(String),
    Verify,
    Backup,
    Rollback(PathBuf),
    Restore(PathBuf),
}

#[derive(Clone)]
enum FormAction {
    AddProfile,
    SetKey(String),
}

impl App {
    fn new() -> Result<Self> {
        let (profiles, keys) = switch::load()?;
        let state = auth::inspect(&auth::read().unwrap_or_default());
        let selected = profiles
            .profiles
            .iter()
            .position(|p| p.id == profiles.active)
            .unwrap_or(0);
        let mut app = App {
            profiles,
            keys,
            state,
            selected,
            log: Vec::new(),
            status: "就绪".to_string(),
            mode: Mode::Browse,
            quit: false,
            procs: RunningProcs::default(),
            backups: (0, 0),
        };
        app.refresh_meta();
        app.note("↑↓ 选择 · Enter 切换 · ? 看全部按键");
        Ok(app)
    }

    /// Re-read everything from disk. Called at start and after any write.
    fn reload(&mut self) -> Result<()> {
        let (profiles, keys) = switch::load()?;
        self.state = auth::inspect(&auth::read().unwrap_or_default());
        self.profiles = profiles;
        self.keys = keys;
        if self.selected >= self.profiles.profiles.len() {
            self.selected = self.profiles.profiles.len().saturating_sub(1);
        }
        self.refresh_meta();
        Ok(())
    }

    fn refresh_meta(&mut self) {
        self.procs = paths::running_codex_processes();
        self.backups = backup::usage();
    }

    fn note(&mut self, text: impl Into<String>) {
        self.log.push(Msg::note(text));
    }

    fn absorb(&mut self, msgs: Vec<Msg>) {
        for m in msgs {
            self.log.push(m);
        }
        // Keep the buffer bounded; the log pane only shows the tail anyway.
        if self.log.len() > 400 {
            let cut = self.log.len() - 400;
            self.log.drain(..cut);
        }
    }

    fn current(&self) -> Option<&Profile> {
        self.profiles.profiles.get(self.selected)
    }

    fn busy(&mut self, terminal: &mut DefaultTerminal, text: impl Into<String>) {
        self.status = text.into();
        let _ = terminal.draw(|f| draw(f, self));
    }

    fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            terminal.draw(|f| draw(f, self))?;
            if !event::poll(Duration::from_millis(250))? {
                continue;
            }
            match event::read()? {
                // Windows delivers Release events too; acting on them would
                // double every keystroke.
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if let Some(action) = self.on_key(key) {
                        self.execute(action, terminal)?;
                        self.status = "就绪".to_string();
                    }
                }
                Event::Resize(..) => {}
                _ => {}
            }
        }
        Ok(())
    }

    /// Route one keystroke through the current mode.
    ///
    /// The mode is taken out first so handlers can mutate `self` freely. What a
    /// handler puts back must survive: assigning a default afterwards is what
    /// used to swallow every popup the moment it opened.
    fn on_key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        {
            self.quit = true;
            return None;
        }

        let mode = std::mem::replace(&mut self.mode, Mode::Browse);
        match mode {
            Mode::Browse => self.browse_key(key),
            Mode::Help => {
                let close = matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Enter
                );
                if !close {
                    self.mode = Mode::Help;
                }
                None
            }
            Mode::Confirm(c) => {
                let (mode, action) = self.confirm_key(c, key);
                self.mode = mode;
                action
            }
            Mode::Form(f) => {
                let (mode, action) = self.form_key(f, key);
                self.mode = mode;
                action
            }
        }
    }

    /// Returns an action to run when the key implies one immediately.
    fn browse_key(&mut self, key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.quit = true;
                None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.profiles.profiles.len() {
                    self.selected += 1;
                }
                None
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.selected = 0;
                None
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.selected = self.profiles.profiles.len().saturating_sub(1);
                None
            }
            KeyCode::Enter => {
                self.begin_switch();
                None
            }
            KeyCode::Char('e') => {
                if let Some(p) = self.current() {
                    let id = p.id.clone();
                    self.mode = Mode::Form(Form {
                        title: format!("设置 `{id}` 的 API Key"),
                        hint: "Enter 保存 · Esc 取消".to_string(),
                        fields: vec![Field {
                            label: "API Key",
                            value: String::new(),
                            secret: true,
                        }],
                        focus: 0,
                        action: FormAction::SetKey(id),
                    });
                }
                None
            }
            KeyCode::Char('a') => {
                self.mode = Mode::Form(Form {
                    title: "新增 profile".to_string(),
                    hint: "Tab 切换字段 · Enter 保存 · Esc 取消".to_string(),
                    fields: vec![
                        Field {
                            label: "id",
                            value: String::new(),
                            secret: false,
                        },
                        Field {
                            label: "base_url",
                            value: "https://".to_string(),
                            secret: false,
                        },
                        Field {
                            label: "model",
                            value: String::new(),
                            secret: false,
                        },
                        Field {
                            label: "auth",
                            value: "apikey".to_string(),
                            secret: false,
                        },
                        Field {
                            label: "API Key",
                            value: String::new(),
                            secret: true,
                        },
                    ],
                    focus: 0,
                    action: FormAction::AddProfile,
                });
                None
            }
            KeyCode::Char('h') => {
                if let Some(p) = self.current() {
                    let provider = p.history_provider().to_string();
                    let mut body = vec![
                        format!("把所有已保存的会话打上 `{provider}` 标记。"),
                        "同时改写 rollout 文件首行、threads 表和字节偏移。".to_string(),
                        "写前自动备份，可用 u 回滚。".to_string(),
                    ];
                    if provider == self.active_provider() {
                        body.push("目标与当前 provider 相同，可能无需迁移。".to_string());
                    }
                    self.mode = Mode::Confirm(Confirm {
                        title: format!("迁移历史会话 → {provider}"),
                        body,
                        action: Action::Migrate(provider),
                    });
                }
                None
            }
            KeyCode::Char('v') => Some(Action::Verify),
            KeyCode::Char('b') => Some(Action::Backup),
            KeyCode::Char('r') => {
                match self.reload() {
                    Ok(()) => self.note("已重新读取 profiles / auth / 进程状态"),
                    Err(e) => self.log.push(Msg::err(format!("{e:#}"))),
                }
                None
            }
            KeyCode::Char('u') => match latest_manifest() {
                Some(m) => {
                    self.mode = Mode::Confirm(Confirm {
                        title: "回滚上一次历史迁移".to_string(),
                        body: vec![
                            format!("记录: {}", m.display()),
                            "把每个会话恢复成迁移前的首行字节，偏移量同步还原。".to_string(),
                        ],
                        action: Action::Rollback(m),
                    });
                    None
                }
                None => {
                    self.note("没有可用的迁移记录");
                    None
                }
            },
            KeyCode::Char('R') => match backup::latest() {
                Some(d) => {
                    self.mode = Mode::Confirm(Confirm {
                        title: "从备份还原".to_string(),
                        body: vec![
                            format!("快照: {}", d.display()),
                            "会用快照覆盖当前 auth.json / config.toml / sqlite。".to_string(),
                            "这一步不回滚会话文件本身。".to_string(),
                        ],
                        action: Action::Restore(d),
                    });
                    None
                }
                None => {
                    self.note("没有可用备份");
                    None
                }
            },
            KeyCode::Char('?') => {
                self.mode = Mode::Help;
                None
            }
            _ => None,
        }
    }

    fn begin_switch(&mut self) {
        let Some(p) = self.current().cloned() else {
            return;
        };
        if p.id == self.profiles.active {
            self.note(format!("`{}` 已经是当前 profile", p.id));
            return;
        }
        let (profiles, keys) = match switch::load() {
            Ok(v) => v,
            Err(e) => {
                self.log.push(Msg::err(format!("{e:#}")));
                return;
            }
        };
        let preview = match switch::preview(&profiles, &keys, &p.id) {
            Ok(v) => v,
            Err(e) => {
                self.log.push(Msg::err(format!("{e:#}")));
                return;
            }
        };
        let mut body: Vec<String> = preview
            .rows
            .iter()
            .map(|(k, v)| format!("{k} = {v}"))
            .collect();
        if let Some(h) = &preview.history_target {
            body.push(format!("历史会话将迁移到 `{h}`"));
        }
        if preview.needs_login {
            body.push("auth.json 里没有登录态，切换后需要跑一次 codex login".to_string());
        }
        if self.procs.has_critical() {
            body.push(format!(
                "注意: Codex 主进程仍在运行 ({})",
                self.procs.critical.join(", ")
            ));
        }
        self.mode = Mode::Confirm(Confirm {
            title: format!("切换到 {}?", p.label),
            body,
            action: Action::Switch(p.id),
        });
    }

    fn confirm_key(&mut self, c: Confirm, key: KeyEvent) -> (Mode, Option<Action>) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                (Mode::Browse, Some(c.action))
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => {
                self.note("已取消");
                (Mode::Browse, None)
            }
            _ => (Mode::Confirm(c), None),
        }
    }

    fn form_key(&mut self, mut f: Form, key: KeyEvent) -> (Mode, Option<Action>) {
        match key.code {
            KeyCode::Esc => {
                self.note("已取消");
                (Mode::Browse, None)
            }
            KeyCode::Tab | KeyCode::Down => {
                f.focus = (f.focus + 1) % f.fields.len();
                (Mode::Form(f), None)
            }
            KeyCode::BackTab | KeyCode::Up => {
                f.focus = (f.focus + f.fields.len() - 1) % f.fields.len();
                (Mode::Form(f), None)
            }
            KeyCode::Backspace => {
                f.fields[f.focus].value.pop();
                (Mode::Form(f), None)
            }
            KeyCode::Enter => match self.submit(&f) {
                Ok(Some(action)) => (Mode::Browse, Some(action)),
                Ok(None) => (Mode::Browse, None),
                Err(e) => {
                    f.hint = format!("{e}");
                    (Mode::Form(f), None)
                }
            },
            KeyCode::Char(ch) if !ch.is_control() => {
                f.fields[f.focus].value.push(ch);
                (Mode::Form(f), None)
            }
            _ => (Mode::Form(f), None),
        }
    }

    fn submit(&mut self, f: &Form) -> Result<Option<Action>> {
        match &f.action {
            FormAction::SetKey(id) => {
                let key = f.fields[0].value.trim();
                if key.is_empty() {
                    bail!("Key 为空，未修改");
                }
                let mut keys = profile::load_keys()?;
                keys.set(id, key);
                profile::save_keys(&keys)?;
                self.log.push(Msg::ok(format!("`{id}` 的 Key 已保存")));
                self.reload()?;
                Ok(None)
            }
            FormAction::AddProfile => {
                let id = f.fields[0].value.trim().to_string();
                let base_url = f.fields[1].value.trim().to_string();
                let model = f.fields[2].value.trim().to_string();
                let auth_raw = f.fields[3].value.trim().to_ascii_lowercase();
                let api_key = f.fields[4].value.trim().to_string();

                profile::validate_id(&id)?;
                if base_url.is_empty() || base_url == "https://" {
                    bail!("base_url 不能为空");
                }
                let kind = match auth_raw.as_str() {
                    "apikey" | "api-key" | "" => AuthKind::Apikey,
                    "chatgpt" | "oauth" => AuthKind::Chatgpt,
                    other => bail!("auth 只能是 apikey 或 chatgpt，收到 {other}"),
                };

                let (mut profiles, mut keys) = switch::load()?;
                if profiles.get(&id).is_some() {
                    bail!("profile `{id}` 已存在");
                }
                profiles.profiles.push(Profile {
                    id: id.clone(),
                    label: format!("{id} 中转"),
                    note: base_url.clone(),
                    auth: kind,
                    provider: id.clone(),
                    base_url,
                    wire_api: "responses".to_string(),
                    requires_openai_auth: kind == AuthKind::Chatgpt,
                    model,
                    reasoning_effort: String::new(),
                    disable_response_storage: Some(true),
                });
                profile::save_profiles(&profiles)?;
                if kind == AuthKind::Apikey && !api_key.is_empty() {
                    keys.set(&id, &api_key);
                    profile::save_keys(&keys)?;
                }
                self.log.push(Msg::ok(format!("已新增 profile `{id}`")));
                self.reload()?;
                if let Some(i) = self.profiles.profiles.iter().position(|p| p.id == id) {
                    self.selected = i;
                }
                Ok(None)
            }
        }
    }

    fn execute(&mut self, action: Action, terminal: &mut DefaultTerminal) -> Result<()> {
        match action {
            Action::Switch(id) => {
                self.busy(terminal, format!("正在切换到 {id}…"));
                let force = self.procs.has_critical();
                let mut log = Vec::new();
                let result = switch::run(
                    &id,
                    Options {
                        force,
                        ..Default::default()
                    },
                    |m| log.push(m),
                );
                match result {
                    Ok(()) => log.push(Msg::ok("完成。完全退出并重开 Codex 后生效")),
                    Err(e) => log.push(Msg::err(format!("{e:#}"))),
                }
                self.absorb(log);
                self.reload()?;
                if let Some(i) = self.profiles.profiles.iter().position(|p| p.id == id) {
                    self.selected = i;
                }
                drain_input();
            }
            Action::Migrate(provider) => {
                self.busy(terminal, format!("正在迁移到 {provider}…"));
                let mut log = Vec::new();
                match backup::create("history") {
                    Ok(bk) => log.push(Msg::ok(format!(
                        "备份 {} 个文件（{}）",
                        bk.files.len(),
                        backup::human(bk.bytes)
                    ))),
                    Err(e) => log.push(Msg::err(format!("备份失败: {e:#}"))),
                }
                let _ = switch::migrate_history(&provider, |m| log.push(m));
                self.absorb(log);
                self.reload()?;
                drain_input();
            }
            Action::Verify => {
                let provider = self.active_provider();
                let mut log = Vec::new();
                log.push(Msg::step(format!("校验 provider = {provider} 的会话偏移")));
                match history::verify(&provider) {
                    Ok((ok, 0, _)) => log.push(Msg::ok(format!("{ok} 个偏移全部落在行首"))),
                    Ok((ok, bad, samples)) => {
                        log.push(Msg::err(format!("{ok} 个正常，{bad} 个异常")));
                        for s in samples.into_iter().take(3) {
                            log.push(Msg::note(s));
                        }
                    }
                    Err(e) => log.push(Msg::err(format!("{e:#}"))),
                }
                self.absorb(log);
            }
            Action::Backup => {
                self.busy(terminal, "正在备份…");
                let mut log = Vec::new();
                match backup::create("manual") {
                    Ok(bk) => log.push(Msg::ok(format!(
                        "备份 {} 个文件（{}）→ {}",
                        bk.files.len(),
                        backup::human(bk.bytes),
                        bk.dir.display()
                    ))),
                    Err(e) => log.push(Msg::err(format!("{e:#}"))),
                }
                self.absorb(log);
                self.reload()?;
            }
            Action::Rollback(manifest) => {
                self.busy(terminal, "正在回滚…");
                let mut log = Vec::new();
                match history::rollback(&manifest) {
                    Ok(rep) => log.push(Msg::ok(format!(
                        "还原 {} 个文件，{} 个错误",
                        rep.files_rewritten,
                        rep.errors.len()
                    ))),
                    Err(e) => log.push(Msg::err(format!("{e:#}"))),
                }
                self.absorb(log);
                self.reload()?;
                drain_input();
            }
            Action::Restore(dir) => {
                self.busy(terminal, "正在还原备份…");
                let mut log = Vec::new();
                match backup::restore(&dir) {
                    Ok(done) => log.push(Msg::ok(format!("已还原: {}", done.join(", ")))),
                    Err(e) => log.push(Msg::err(format!("{e:#}"))),
                }
                self.absorb(log);
                self.reload()?;
                drain_input();
            }
        }
        Ok(())
    }

    fn active_provider(&self) -> String {
        self.profiles
            .get(&self.profiles.active)
            .map(|p| p.history_provider().to_string())
            .unwrap_or_else(|| "openai".to_string())
    }
}

/// Drop keystrokes buffered while a long write held the loop.
fn drain_input() {
    while event::poll(Duration::from_millis(0)).unwrap_or(false) {
        if event::read().is_err() {
            break;
        }
    }
}

fn latest_manifest() -> Option<PathBuf> {
    backup::list().ok()?.into_iter().find_map(|d| {
        let m = d.join("manifest.json");
        m.exists().then_some(m)
    })
}

// ---------------------------------------------------------------- styling

fn s_dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn s_bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

fn s_dim_bold() -> Style {
    Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::BOLD)
}

/// Selection and tags use reverse video rather than a fixed background, so the
/// result is legible on light and dark schemes alike.
fn s_pill() -> Style {
    Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
}

fn s_ok() -> Style {
    Style::default().fg(Color::Green)
}

fn s_warn() -> Style {
    Style::default().fg(Color::Yellow)
}

fn s_err() -> Style {
    Style::default().fg(Color::Red)
}

fn s_accent() -> Style {
    Style::default().fg(Color::Cyan)
}

// ---------------------------------------------------------------- rendering

fn draw(f: &mut Frame, app: &App) {
    let log_h = (f.area().height / 4).clamp(4, 9);
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(6),
        Constraint::Length(log_h),
        Constraint::Length(1),
    ])
    .split(f.area());

    draw_header(f, app, chunks[0]);
    draw_rule(f, chunks[1]);

    if f.area().width < 72 {
        let body = Layout::vertical([Constraint::Min(4), Constraint::Min(6)]).split(chunks[2]);
        draw_list(f, app, body[0]);
        draw_details(f, app, body[1]);
    } else {
        let body = Layout::horizontal([Constraint::Percentage(38), Constraint::Percentage(62)])
            .split(chunks[2]);
        draw_list(f, app, body[0]);
        draw_details(f, app, body[1]);
    }

    draw_log(f, app, chunks[3]);
    draw_footer(f, app, chunks[4]);

    match &app.mode {
        Mode::Browse => {}
        Mode::Confirm(c) => draw_confirm(f, c),
        Mode::Form(form) => draw_form(f, form),
        Mode::Help => draw_help(f),
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let mut left: Vec<Span> = vec![
        Span::styled(" codex-switch ", s_bold()),
        Span::styled(format!("{} ", env!("CARGO_PKG_VERSION")), s_dim()),
        Span::raw(" "),
        Span::styled(format!(" {} ", app.profiles.active), s_pill()),
    ];
    if area.width >= 96 {
        left.push(Span::styled(" · ", s_dim()));
        left.push(Span::styled(fit(&app.state.describe(), 34), s_dim()));
    }

    let (badge, badge_style) = if app.procs.has_critical() {
        (
            format!("● Codex 运行中 · {}", app.procs.critical.join(", ")),
            s_warn(),
        )
    } else {
        ("● Codex 未运行".to_string(), s_ok())
    };
    let mut right: Vec<Span> = vec![];
    if app.status != "就绪" {
        right.push(Span::styled(format!("{} ", app.status), s_dim()));
    }
    right.push(Span::styled(badge, badge_style));
    right.push(Span::raw(" "));

    let right_w = (line_width(&right) as u16).min(area.width.saturating_sub(12));
    let cols = Layout::horizontal([Constraint::Min(0), Constraint::Length(right_w)]).split(area);
    f.render_widget(Paragraph::new(Line::from(left)), cols[0]);
    f.render_widget(Paragraph::new(Line::from(right)), cols[1]);
}

fn draw_rule(f: &mut Frame, area: Rect) {
    let rule = "─".repeat(area.width as usize);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(rule, s_dim()))),
        area,
    );
}

fn draw_list(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(s_dim())
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled("profiles", s_bold()),
            Span::raw(" "),
        ]));

    if app.profiles.profiles.is_empty() {
        f.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled("   还没有 profile", s_warn())),
                Line::from(Span::styled("   按 a 新增一个", s_dim())),
            ])
            .block(block),
            area,
        );
        return;
    }

    // Text is padded by display width: a CJK glyph is two cells wide, and
    // padding by char count leaves the columns jagged.
    let inner = area.width.saturating_sub(2) as usize;
    let row_w = inner.saturating_sub(2); // highlight symbol
                                         // marker + name + auth(8) + gap(2) + status(6): reserve the status column so
                                         // the badge never collides with the auth label. On a narrow pane the auth
                                         // word is dropped instead — the details pane repeats it anyway.
    let show_auth = row_w >= 34;
    let name_w = if show_auth {
        row_w.saturating_sub(19)
    } else {
        row_w.saturating_sub(11)
    }
    .clamp(6, 26);

    let items: Vec<ListItem> = app
        .profiles
        .profiles
        .iter()
        .map(|p| {
            let active = p.id == app.profiles.active;
            let mut spans = vec![
                Span::styled(
                    if active { "● " } else { "○ " },
                    if active { s_ok() } else { s_dim() },
                ),
                Span::styled(
                    pad(&fit(&p.id, name_w), name_w),
                    if active { s_bold() } else { Style::default() },
                ),
            ];
            if show_auth {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(pad(p.auth.short(), 8), s_dim()));
            }

            let (status, style) = match p.auth {
                AuthKind::Apikey => {
                    if app.keys.get(&p.id).is_some() {
                        ("key ✓", s_ok())
                    } else {
                        ("key ✗", s_err())
                    }
                }
                AuthKind::Chatgpt => {
                    if app.state.has_tokens {
                        ("登录 ✓", s_ok())
                    } else {
                        ("需登录", s_warn())
                    }
                }
            };
            let used: usize = spans
                .iter()
                .map(|s| UnicodeWidthStr::width(&*s.content))
                .sum();
            let gap = row_w
                .saturating_sub(used + UnicodeWidthStr::width(status))
                .max(2);
            spans.push(Span::raw(" ".repeat(gap)));
            spans.push(Span::styled(status, style));
            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▌ ");

    let mut state = ListState::default();
    state.select(Some(app.selected.min(app.profiles.profiles.len() - 1)));
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_details(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(s_dim())
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled("详情", s_bold()),
            Span::raw(" "),
        ]));

    let inner = area.width.saturating_sub(2) as usize;
    let label_w = 10usize;
    let value_w = inner.saturating_sub(label_w + 2).max(8);
    let mut lines: Vec<Line> = Vec::new();

    match app.current() {
        None => {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("  没有可显示的 profile", s_dim())));
        }
        Some(p) => {
            kv(&mut lines, "id", p.id.clone(), s_bold(), label_w, value_w);
            kv(
                &mut lines,
                "名称",
                p.label.clone(),
                Style::default(),
                label_w,
                value_w,
            );
            kv(
                &mut lines,
                "认证",
                match p.auth {
                    AuthKind::Chatgpt => "ChatGPT 账号（OAuth）".to_string(),
                    AuthKind::Apikey => "API Key".to_string(),
                },
                s_accent(),
                label_w,
                value_w,
            );
            kv(
                &mut lines,
                "provider",
                if p.is_official() {
                    "openai（内置）".to_string()
                } else {
                    p.provider.clone()
                },
                Style::default(),
                label_w,
                value_w,
            );
            kv(
                &mut lines,
                "base_url",
                if p.base_url.is_empty() {
                    "—".to_string()
                } else {
                    p.base_url.clone()
                },
                Style::default(),
                label_w,
                value_w,
            );
            kv(
                &mut lines,
                "model",
                if p.model.is_empty() {
                    "（沿用 config.toml）".to_string()
                } else {
                    p.model.clone()
                },
                Style::default(),
                label_w,
                value_w,
            );
            if !p.reasoning_effort.is_empty() {
                kv(
                    &mut lines,
                    "reasoning",
                    p.reasoning_effort.clone(),
                    Style::default(),
                    label_w,
                    value_w,
                );
            }
            let key_text = if p.auth == AuthKind::Apikey {
                app.keys.masked(&p.id)
            } else if app.state.has_tokens {
                "复用 auth.json 登录态".to_string()
            } else {
                "缺少登录态，需要 codex login".to_string()
            };
            kv(&mut lines, "凭据", key_text, s_dim(), label_w, value_w);
            kv(
                &mut lines,
                "历史标记",
                p.history_provider().to_string(),
                s_dim(),
                label_w,
                value_w,
            );

            section(&mut lines, "环境");
            kv(
                &mut lines,
                "CODEX_HOME",
                paths::codex_home().display().to_string(),
                s_dim(),
                label_w,
                value_w,
            );
            kv(
                &mut lines,
                "备份",
                format!(
                    "{} 个快照 · {}",
                    app.backups.0,
                    backup::human(app.backups.1)
                ),
                s_dim(),
                label_w,
                value_w,
            );
            kv(
                &mut lines,
                "进程",
                app.procs.describe(),
                s_dim(),
                label_w,
                value_w,
            );
        }
    }

    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_log(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(s_dim())
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled("日志", s_bold()),
            Span::raw(" "),
        ]));
    let inner_w = area.width.saturating_sub(4) as usize;
    let height = area.height.saturating_sub(2) as usize;
    let start = app.log.len().saturating_sub(height);
    let lines: Vec<Line> = app.log[start..]
        .iter()
        .map(|m| {
            let (tag, style) = level_style(m.level);
            Line::from(vec![
                Span::raw(" "),
                Span::styled(tag, style),
                Span::raw(" "),
                Span::styled(
                    fit(&m.text, inner_w),
                    if m.level == Level::Note {
                        s_dim()
                    } else {
                        Style::default()
                    },
                ),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let hint = |k: &'static str, label: &'static str| {
        vec![
            Span::styled(k, s_accent()),
            Span::raw(" "),
            Span::styled(label, s_dim()),
        ]
    };

    let mut spans: Vec<Span> = vec![Span::raw(" ")];
    let seq: Vec<(&'static str, &'static str)> = match app.mode {
        Mode::Browse if area.width >= 92 => vec![
            ("↑↓", "选择"),
            ("Enter", "切换"),
            ("e", "设 Key"),
            ("a", "新增"),
            ("h", "迁移历史"),
            ("v", "校验"),
            ("b", "备份"),
            ("u", "回滚"),
            ("?", "帮助"),
            ("q", "退出"),
        ],
        Mode::Browse => vec![
            ("↑↓", "选择"),
            ("Enter", "切换"),
            ("a", "新增"),
            ("h", "迁移"),
            ("?", "帮助"),
            ("q", "退出"),
        ],
        Mode::Confirm(_) => vec![("Y", "确认"), ("N", "取消")],
        Mode::Form(_) => vec![("Tab", "切换字段"), ("Enter", "保存"), ("Esc", "取消")],
        Mode::Help => vec![("Esc", "关闭帮助")],
    };
    for (i, (k, label)) in seq.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("   ", s_dim()));
        }
        spans.extend(hint(k, label));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_confirm(f: &mut Frame, c: &Confirm) {
    let width = 68u16.min(f.area().width.saturating_sub(4));
    let height = (c.body.len() as u16 + 7).min(f.area().height.saturating_sub(2));
    let area = centered(f.area(), width, height);
    f.render_widget(Clear, area);

    let inner_w = area.width.saturating_sub(4) as usize;
    let mut lines: Vec<Line> = vec![Line::from("")];
    for b in &c.body {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(fit(b, inner_w), Style::default()),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(" Y ", s_pill().fg(Color::Green)),
        Span::raw("  确认"),
        Span::raw("        "),
        Span::styled(" N ", s_pill().fg(Color::Red)),
        Span::raw("  取消"),
    ]));

    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(s_warn())
                .title(Line::from(vec![
                    Span::raw(" "),
                    Span::styled(c.title.clone(), s_bold()),
                    Span::raw(" "),
                ])),
        ),
        area,
    );
}

fn draw_form(f: &mut Frame, form: &Form) {
    let width = 72u16.min(f.area().width.saturating_sub(4));
    let height = (form.fields.len() as u16 + 7).min(f.area().height.saturating_sub(2));
    let area = centered(f.area(), width, height);
    f.render_widget(Clear, area);

    let inner_w = area.width.saturating_sub(6) as usize;
    let mut lines: Vec<Line> = vec![Line::from("")];
    for (i, field) in form.fields.iter().enumerate() {
        let focused = i == form.focus;
        let shown = if field.secret && !field.value.is_empty() {
            "•".repeat(field.value.chars().count())
        } else if field.value.is_empty() {
            "<空>".to_string()
        } else {
            field.value.clone()
        };
        let style = if focused {
            s_pill()
        } else if field.value.is_empty() {
            s_dim()
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::raw(if focused { " ▸ " } else { "   " }),
            Span::styled(pad(field.label, 10), s_dim()),
            Span::styled(fit(&shown, inner_w.saturating_sub(13)), style),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(fit(&form.hint, inner_w), s_dim()),
    ]));

    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(s_accent())
                .title(Line::from(vec![
                    Span::raw(" "),
                    Span::styled(form.title.clone(), s_bold()),
                    Span::raw(" "),
                ])),
        ),
        area,
    );
}

fn draw_help(f: &mut Frame) {
    let width = 74u16.min(f.area().width.saturating_sub(4));
    let rows: Vec<(&str, &str)> = vec![
        ("↑ ↓ / k j", "上下选择（g / G 跳到首尾）"),
        ("Enter", "切换到选中的 profile"),
        ("e", "设置当前 profile 的 API Key"),
        ("a", "新增一个 profile"),
        ("h", "把历史会话迁移到当前 profile 的 provider"),
        ("v", "校验会话字节偏移是否仍然对齐"),
        ("b", "手动备份 auth / config / sqlite"),
        ("u", "回滚上一次历史迁移"),
        ("R", "从最近一次备份整体还原"),
        ("r", "重新读取配置与进程状态"),
        ("?", "打开 / 关闭这份帮助"),
        ("q / Esc", "退出"),
    ];
    let height = (rows.len() as u16 + 7).min(f.area().height.saturating_sub(2));
    let area = centered(f.area(), width, height);
    f.render_widget(Clear, area);

    let mut lines: Vec<Line> = vec![Line::from("")];
    for (key, desc) in rows {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(pad(key, 12), s_accent()),
            Span::styled(desc, Style::default()),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "字体由终端决定：codex-switch font --face \"<字体名>\"（--list 看装了什么）",
            s_dim(),
        ),
    ]));

    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(s_accent())
                .title(Line::from(vec![
                    Span::raw(" "),
                    Span::styled("按键", s_bold()),
                    Span::raw(" "),
                ])),
        ),
        area,
    );
}

fn level_style(level: Level) -> (&'static str, Style) {
    match level {
        Level::Step => ("→", s_accent()),
        Level::Ok => ("✓", s_ok()),
        Level::Warn => ("!", s_warn()),
        Level::Err => ("✗", s_err()),
        Level::Note => ("·", s_dim()),
    }
}

fn kv(
    lines: &mut Vec<Line<'static>>,
    key: &'static str,
    value: String,
    style: Style,
    label_w: usize,
    value_w: usize,
) {
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled(pad(key, label_w), s_dim()),
        Span::raw("  "),
        Span::styled(fit(&value, value_w), style),
    ]));
}

fn section(lines: &mut Vec<Line<'static>>, title: &'static str) {
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled(title, s_dim_bold()),
    ]));
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width.saturating_sub(2));
    let h = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

/// Pad to a display width, counting a CJK glyph as two cells.
fn pad(s: &str, w: usize) -> String {
    let cur = UnicodeWidthStr::width(s);
    if cur >= w {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(w - cur))
    }
}

/// Truncate to a display width, appending an ellipsis.
fn fit(s: &str, w: usize) -> String {
    if UnicodeWidthStr::width(s) <= w {
        return s.to_string();
    }
    let mut out = String::new();
    let mut cur = 0;
    for ch in s.chars() {
        let mut buf = [0u8; 4];
        let cw = UnicodeWidthStr::width(ch.encode_utf8(&mut buf));
        if cur + cw + 1 > w {
            break;
        }
        out.push(ch);
        cur += cw;
    }
    out.push('…');
    out
}

fn line_width(spans: &[Span]) -> usize {
    spans
        .iter()
        .map(|s| UnicodeWidthStr::width(&*s.content))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn profile(id: &str, label: &str, auth: AuthKind, provider: &str, base_url: &str) -> Profile {
        Profile {
            id: id.to_string(),
            label: label.to_string(),
            note: String::new(),
            auth,
            provider: provider.to_string(),
            base_url: base_url.to_string(),
            wire_api: "responses".to_string(),
            requires_openai_auth: false,
            model: "gpt-5".to_string(),
            reasoning_effort: String::new(),
            disable_response_storage: Some(true),
        }
    }

    fn demo_app() -> App {
        App {
            profiles: Profiles {
                active: "relay".to_string(),
                profiles: vec![
                    profile("official", "ChatGPT 官方登录", AuthKind::Chatgpt, "", ""),
                    profile(
                        "relay",
                        "relay 中转",
                        AuthKind::Apikey,
                        "relay",
                        "https://relay.example.com/v1",
                    ),
                ],
            },
            keys: Keys::default(),
            state: auth::AuthState::default(),
            selected: 1,
            log: vec![Msg::ok("测试日志")],
            status: "就绪".to_string(),
            mode: Mode::Browse,
            quit: false,
            procs: RunningProcs::default(),
            backups: (2, 1024 * 1024),
        }
    }

    /// Wide (CJK) glyphs occupy two cells, and ratatui blanks the second one,
    /// so the flattened buffer has spaces woven through Chinese text.
    fn has(haystack: &str, needle: &str) -> bool {
        let squash = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
        squash(haystack).contains(&squash(needle))
    }

    fn render(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    #[test]
    fn browse_view_lists_profiles_and_details() {
        let text = render(&demo_app(), 100, 32);
        assert!(has(&text, "codex-switch"), "标题没有渲染");
        assert!(has(&text, "official"), "缺少 official");
        assert!(has(&text, "relay.example.com"), "详情面板没有 base_url");
        assert!(has(&text, "切换"), "底部按键提示缺失");
    }

    #[test]
    fn narrow_terminal_does_not_panic() {
        let _ = render(&demo_app(), 40, 12);
        let _ = render(&demo_app(), 20, 8);
    }

    // -- input routing -----------------------------------------------------
    // Regression: the mode a handler set used to be overwritten by the caller,
    // so Enter / a / e / h / u / R looked like dead keys.

    #[test]
    fn enter_on_inactive_profile_opens_confirm() {
        let mut app = demo_app();
        app.selected = 0; // official, not active
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Confirm(_)), "Enter 没有弹出确认框");
    }

    #[test]
    fn letter_a_opens_form() {
        let mut app = demo_app();
        press(&mut app, KeyCode::Char('a'));
        assert!(matches!(app.mode, Mode::Form(_)), "a 没有打开新增表单");
    }

    #[test]
    fn letter_e_opens_key_form() {
        let mut app = demo_app();
        press(&mut app, KeyCode::Char('e'));
        assert!(matches!(app.mode, Mode::Form(_)), "e 没有打开 Key 表单");
    }

    #[test]
    fn letter_q_quits_and_esc_closes_popup() {
        let mut app = demo_app();
        press(&mut app, KeyCode::Char('e'));
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Browse), "Esc 没有关闭弹窗");
        assert!(!app.quit, "Esc 在弹窗里不该退出程序");

        let mut app = demo_app();
        press(&mut app, KeyCode::Char('q'));
        assert!(app.quit, "q 没有退出");
    }

    #[test]
    fn help_toggles() {
        let mut app = demo_app();
        press(&mut app, KeyCode::Char('?'));
        assert!(matches!(app.mode, Mode::Help));
        assert!(has(&render(&app, 100, 32), "按键"), "帮助面板没有渲染");
        press(&mut app, KeyCode::Char('?'));
        assert!(matches!(app.mode, Mode::Browse));
    }

    #[test]
    fn form_accepts_typing() {
        let mut app = demo_app();
        press(&mut app, KeyCode::Char('e'));
        for ch in "sk-abc".chars() {
            press(&mut app, KeyCode::Char(ch));
        }
        match &app.mode {
            Mode::Form(f) => assert_eq!(f.fields[0].value, "sk-abc"),
            _ => panic!("表单被意外关闭"),
        }
    }

    #[test]
    fn arrows_move_selection_within_bounds() {
        let mut app = demo_app();
        press(&mut app, KeyCode::Up);
        assert_eq!(app.selected, 0);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.selected, 0, "上边界越界");
        press(&mut app, KeyCode::End);
        assert_eq!(app.selected, 1);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.selected, 1, "下边界越界");
    }

    // -- layout ------------------------------------------------------------

    #[test]
    fn wide_text_pads_by_display_width() {
        assert_eq!(UnicodeWidthStr::width(pad("认证", 10).as_str()), 10);
        assert_eq!(UnicodeWidthStr::width(pad("provider", 10).as_str()), 10);
        assert_eq!(fit("中文内容很长很长", 6), "中文…");
        assert_eq!(UnicodeWidthStr::width(fit(&"中".repeat(20), 9).as_str()), 9);
    }

    #[test]
    fn confirm_and_form_popups_render() {
        let mut app = demo_app();
        app.mode = Mode::Confirm(Confirm {
            title: "切换到 relay?".to_string(),
            body: vec!["model_provider = relay".to_string()],
            action: Action::Switch("relay".to_string()),
        });
        assert!(has(&render(&app, 90, 28), "确认"));

        app.mode = Mode::Form(Form {
            title: "新增 profile".to_string(),
            hint: "Enter 保存".to_string(),
            fields: vec![Field {
                label: "id",
                value: String::new(),
                secret: false,
            }],
            focus: 0,
            action: FormAction::AddProfile,
        });
        let text = render(&app, 90, 28);
        assert!(has(&text, "新增 profile"));
        assert!(has(&text, "Enter 保存"));
    }

    #[test]
    fn empty_profile_list_renders_hint() {
        let mut app = demo_app();
        app.profiles = Profiles::default();
        app.selected = 0;
        let text = render(&app, 80, 24);
        assert!(has(&text, "按 a 新增"), "空列表应给出提示");
    }
}
