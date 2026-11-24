//! Full-screen TUI: pick a profile, see exactly what will change, switch.
//!
//! Long operations block on purpose. They take a second or two, and letting the
//! event loop interleave keystrokes with sqlite writes would buy nothing but
//! bugs.

use crate::auth;
use crate::backup;
use crate::history;
use crate::paths;
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
}

enum Mode {
    Browse,
    Confirm(Confirm),
    Form(Form),
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
        };
        app.note("↑↓ 选择 profile，Enter 切换，? 查看全部按键");
        Ok(app)
    }

    fn reload(&mut self) -> Result<()> {
        let (profiles, keys) = switch::load()?;
        self.state = auth::inspect(&auth::read().unwrap_or_default());
        self.profiles = profiles;
        self.keys = keys;
        if self.selected >= self.profiles.profiles.len() {
            self.selected = self.profiles.profiles.len().saturating_sub(1);
        }
        Ok(())
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
            if !event::poll(Duration::from_millis(200))? {
                continue;
            }
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if let Some(action) = self.on_key(key) {
                self.execute(action, terminal)?;
                self.status = "就绪".to_string();
            }
        }
        Ok(())
    }

    fn on_key(&mut self, key: KeyEvent) -> Option<Action> {
        let mode = std::mem::replace(&mut self.mode, Mode::Browse);
        let (next, action) = self.handle(mode, key);
        self.mode = next;
        action
    }

    fn handle(&mut self, mode: Mode, key: KeyEvent) -> (Mode, Option<Action>) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return (Mode::Browse, None);
        }
        match mode {
            Mode::Browse => (Mode::Browse, self.browse_key(key)),
            Mode::Confirm(c) => self.confirm_key(c, key),
            Mode::Form(f) => self.form_key(f, key),
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
                self.note(
                    "↑↓/jk 选择 · Enter 切换 · e 设 Key · a 新增 · h 迁移历史 · v 校验 · b 备份 · u 回滚 · R 还原 · q 退出",
                );
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
        let procs = paths::running_codex_processes();
        if procs.has_critical() {
            body.push(format!(
                "注意: Codex 主进程仍在运行 ({})",
                procs.critical.join(", ")
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
                let procs = paths::running_codex_processes();
                let mut log = Vec::new();
                let result = switch::run(
                    &id,
                    Options {
                        force: procs.has_critical(),
                        ..Default::default()
                    },
                    |m| log.push(m),
                );
                match result {
                    Ok(()) => {
                        log.push(Msg::ok("完成。完全退出并重开 Codex 后生效"));
                    }
                    Err(e) => log.push(Msg::err(format!("{e:#}"))),
                }
                self.absorb(log);
                self.reload()?;
                if let Some(i) = self.profiles.profiles.iter().position(|p| p.id == id) {
                    self.selected = i;
                }
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

fn latest_manifest() -> Option<PathBuf> {
    backup::list().ok()?.into_iter().find_map(|d| {
        let m = d.join("manifest.json");
        m.exists().then_some(m)
    })
}

// ---------------------------------------------------------------- rendering

fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(7),
        Constraint::Length(7),
        Constraint::Length(1),
    ])
    .split(f.area());

    draw_header(f, app, chunks[0]);

    let body = Layout::horizontal([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(chunks[1]);
    draw_list(f, app, body[0]);
    draw_details(f, app, body[1]);
    draw_log(f, app, chunks[2]);
    draw_footer(f, app, chunks[3]);

    match &app.mode {
        Mode::Browse => {}
        Mode::Confirm(c) => draw_confirm(f, c),
        Mode::Form(form) => draw_form(f, form),
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let auth_desc = app.state.describe();
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(Line::from(vec![
            Span::styled(
                " codex-switch ",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("v{} ", env!("CARGO_PKG_VERSION")),
                Style::default().fg(Color::DarkGray),
            ),
        ]))
        .title_top(
            Line::from(vec![
                Span::styled("当前 ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    app.profiles.active.clone(),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
                Span::styled(auth_desc, Style::default().fg(Color::DarkGray)),
                Span::raw(" "),
            ])
            .right_aligned(),
        );

    let procs = paths::running_codex_processes();
    let (proc_text, proc_style) = if procs.has_critical() {
        (
            format!("Codex 运行中: {}", procs.critical.join(", ")),
            Style::default().fg(Color::Yellow),
        )
    } else {
        (
            "Codex 未运行".to_string(),
            Style::default().fg(Color::Green),
        )
    };

    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("  "),
            Span::styled(proc_text, proc_style),
            Span::styled("   ", Style::default()),
            Span::styled(app.status.clone(), Style::default().fg(Color::DarkGray)),
        ])),
        inner,
    );
}

fn draw_list(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .profiles
        .profiles
        .iter()
        .map(|p| {
            let active = p.id == app.profiles.active;
            let marker = if active { "● " } else { "○ " };
            let name = format!("{:<14}", truncate(&p.id, 14));
            let mut spans = vec![
                Span::styled(
                    marker.to_string(),
                    Style::default().fg(if active {
                        Color::Green
                    } else {
                        Color::DarkGray
                    }),
                ),
                Span::styled(
                    name,
                    Style::default()
                        .fg(if active { Color::White } else { Color::Gray })
                        .add_modifier(if active {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Span::styled(
                    format!("{:<10}", p.auth.short()),
                    Style::default().fg(Color::DarkGray),
                ),
            ];
            if p.auth == AuthKind::Apikey {
                let has = app.keys.get(&p.id).is_some();
                spans.push(Span::styled(
                    if has { "key ✓" } else { "key ✗" },
                    Style::default().fg(if has { Color::Green } else { Color::Red }),
                ));
            } else {
                spans.push(Span::styled(
                    if app.state.has_tokens {
                        "登录 ✓"
                    } else {
                        "需登录"
                    },
                    Style::default().fg(if app.state.has_tokens {
                        Color::Green
                    } else {
                        Color::Yellow
                    }),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(" profiles ".to_string()),
        )
        .highlight_style(Style::default().bg(Color::Rgb(38, 44, 58)))
        .highlight_symbol("▶ ");

    let mut state = ListState::default();
    state.select(Some(app.selected));
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_details(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(" 详情 ".to_string());
    let mut lines: Vec<Line> = Vec::new();

    match app.current() {
        None => lines.push(Line::from(Span::styled(
            "  没有 profile。按 a 新增一个。",
            Style::default().fg(Color::Yellow),
        ))),
        Some(p) => {
            kv(
                &mut lines,
                "id",
                p.id.clone(),
                Style::default().fg(Color::White),
            );
            kv(&mut lines, "名称", p.label.clone(), Style::default());
            kv(
                &mut lines,
                "认证",
                match p.auth {
                    AuthKind::Chatgpt => "ChatGPT 账号（OAuth）".to_string(),
                    AuthKind::Apikey => "API Key".to_string(),
                },
                Style::default().fg(Color::Cyan),
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
            );
            if !p.reasoning_effort.is_empty() {
                kv(
                    &mut lines,
                    "reasoning",
                    p.reasoning_effort.clone(),
                    Style::default(),
                );
            }
            if !p.is_official() {
                kv(
                    &mut lines,
                    "wire_api",
                    p.wire_api.clone(),
                    Style::default().fg(Color::DarkGray),
                );
            }
            let key_text = if p.auth == AuthKind::Apikey {
                app.keys.masked(&p.id)
            } else if app.state.has_tokens {
                "复用 auth.json 登录态".to_string()
            } else {
                "缺少登录态，需要 codex login".to_string()
            };
            kv(
                &mut lines,
                "凭据",
                key_text,
                Style::default().fg(Color::Gray),
            );
            kv(
                &mut lines,
                "历史标记",
                p.history_provider().to_string(),
                Style::default().fg(Color::DarkGray),
            );

            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  环境",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )));
            kv(
                &mut lines,
                "CODEX_HOME",
                paths::codex_home().display().to_string(),
                Style::default().fg(Color::DarkGray),
            );
            let (n, bytes) = backup::usage();
            kv(
                &mut lines,
                "备份",
                format!("{n} 个目录 · {}", backup::human(bytes)),
                Style::default().fg(Color::DarkGray),
            );
        }
    }

    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_log(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(" 日志 ".to_string());
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
                    m.text.clone(),
                    if m.level == Level::Note {
                        Style::default().fg(Color::Gray)
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
    let key = |k: &'static str| Span::styled(k.to_string(), Style::default().fg(Color::Cyan));
    let sep = || Span::styled("  ", Style::default());
    let text = match app.mode {
        Mode::Browse => Line::from(vec![
            Span::raw(" "),
            key("↑↓"),
            Span::raw(" 选择"),
            sep(),
            key("Enter"),
            Span::raw(" 切换"),
            sep(),
            key("e"),
            Span::raw(" 设 Key"),
            sep(),
            key("a"),
            Span::raw(" 新增"),
            sep(),
            key("h"),
            Span::raw(" 迁移历史"),
            sep(),
            key("v"),
            Span::raw(" 校验"),
            sep(),
            key("b"),
            Span::raw(" 备份"),
            sep(),
            key("u"),
            Span::raw(" 回滚"),
            sep(),
            key("?"),
            Span::raw(" 帮助"),
            sep(),
            key("q"),
            Span::raw(" 退出"),
        ]),
        Mode::Confirm(_) => Line::from(vec![
            Span::raw(" "),
            key("Y"),
            Span::raw(" 确认"),
            sep(),
            key("N"),
            Span::raw(" 取消"),
        ]),
        Mode::Form(_) => Line::from(vec![
            Span::raw(" "),
            key("Tab"),
            Span::raw(" 切换字段"),
            sep(),
            key("Enter"),
            Span::raw(" 保存"),
            sep(),
            key("Esc"),
            Span::raw(" 取消"),
        ]),
    };
    f.render_widget(Paragraph::new(text), area);
}

fn draw_confirm(f: &mut Frame, c: &Confirm) {
    let width = 66u16.min(f.area().width.saturating_sub(4));
    let height = (c.body.len() as u16 + 5).min(f.area().height.saturating_sub(2));
    let area = centered(f.area(), width, height);
    f.render_widget(Clear, area);

    let mut lines: Vec<Line> = Vec::new();
    for b in &c.body {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(b.clone(), Style::default().fg(Color::Gray)),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "Y",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" 确认    "),
        Span::styled(
            "N",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" 取消"),
    ]));

    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Yellow))
                .title(format!(" {} ", c.title)),
        ),
        area,
    );
}

fn draw_form(f: &mut Frame, form: &Form) {
    let width = 72u16.min(f.area().width.saturating_sub(4));
    let height = (form.fields.len() as u16 + 6).min(f.area().height.saturating_sub(2));
    let area = centered(f.area(), width, height);
    f.render_widget(Clear, area);

    let mut lines: Vec<Line> = Vec::new();
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
            Style::default().fg(Color::White).bg(Color::Rgb(38, 44, 58))
        } else {
            Style::default().fg(Color::Gray)
        };
        lines.push(Line::from(vec![
            Span::raw(if focused { " ▸ " } else { "   " }),
            Span::styled(
                format!("{:<10}", field.label),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(shown, style),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(form.hint.clone(), Style::default().fg(Color::DarkGray)),
    ]));

    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Cyan))
                .title(format!(" {} ", form.title)),
        ),
        area,
    );
}

fn level_style(level: Level) -> (&'static str, Style) {
    match level {
        Level::Step => ("→", Style::default().fg(Color::Cyan)),
        Level::Ok => ("✓", Style::default().fg(Color::Green)),
        Level::Warn => ("!", Style::default().fg(Color::Yellow)),
        Level::Err => ("✗", Style::default().fg(Color::Red)),
        Level::Note => ("·", Style::default().fg(Color::DarkGray)),
    }
}

fn kv(lines: &mut Vec<Line<'static>>, key: &str, value: String, style: Style) {
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(format!("{key:<18}"), Style::default().fg(Color::DarkGray)),
        Span::styled(value, style),
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

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
    }
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

    #[test]
    fn browse_view_lists_profiles_and_details() {
        let text = render(&demo_app(), 100, 32);
        assert!(has(&text, "codex-switch"), "标题没有渲染");
        assert!(has(&text, "official"), "缺少 official");
        assert!(has(&text, "relay.example.com"), "详情面板没有 base_url");
        assert!(has(&text, "迁移历史"), "底部按键提示缺失");
    }

    #[test]
    fn narrow_terminal_does_not_panic() {
        // 40x12 is roughly the smallest usable size; the layout must clamp.
        let _ = render(&demo_app(), 40, 12);
        let _ = render(&demo_app(), 20, 8);
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
