use crate::paths;
use anyhow::{Context, Result};
use serde_json::{json, Value};

#[derive(Debug, Default, Clone)]
pub struct AuthState {
    pub has_tokens: bool,
    pub has_refresh_token: bool,
    pub api_key: Option<String>,
    pub account_id: Option<String>,
}

impl AuthState {
    pub fn describe(&self) -> String {
        match (self.has_tokens, &self.api_key) {
            (true, Some(_)) => "OAuth tokens + API Key（混合）".to_string(),
            (true, None) if !self.has_refresh_token => {
                "ChatGPT 登录态（缺少 refresh token，可能需要重新登录）".to_string()
            }
            (true, None) => match &self.account_id {
                Some(a) if a.len() > 8 => format!("ChatGPT 登录态（account {}…）", &a[..8]),
                _ => "ChatGPT 登录态".to_string(),
            },
            (false, Some(k)) => format!("仅 API Key（{}…）", &k[..k.len().min(6)]),
            (false, None) => "未登录".to_string(),
        }
    }
}

pub fn read_raw() -> Result<String> {
    let path = paths::auth_path();
    if !path.exists() {
        return Ok(String::new());
    }
    std::fs::read_to_string(&path).with_context(|| format!("读取 {}", path.display()))
}

pub fn read() -> Result<Value> {
    let text = read_raw()?;
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&text).with_context(|| "解析 auth.json 失败，可能已损坏")
}

pub fn inspect(v: &Value) -> AuthState {
    let tokens = v.get("tokens").filter(|t| !t.is_null());
    let refresh = tokens
        .and_then(|t| t.get("refresh_token"))
        .and_then(|t| t.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let account_id = tokens
        .and_then(|t| t.get("account_id"))
        .and_then(|t| t.as_str())
        .map(str::to_string);
    let api_key = v
        .get("OPENAI_API_KEY")
        .and_then(|k| k.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    AuthState {
        has_tokens: tokens.is_some(),
        has_refresh_token: refresh,
        api_key,
        account_id,
    }
}

/// auth.json for a profile that relies on the cached ChatGPT login.
/// `OPENAI_API_KEY` must be nulled, otherwise Codex may keep billing the key
/// even though the user asked for subscription auth.
pub fn render_for_chatgpt(existing: &Value) -> Value {
    let mut v = existing.clone();
    if !v.is_object() {
        v = json!({});
    }
    v["OPENAI_API_KEY"] = Value::Null;
    v
}

/// auth.json for a profile that carries its own API key.
/// Existing OAuth tokens are left untouched so official features keep working.
pub fn render_for_apikey(existing: &Value, key: &str) -> Value {
    let mut v = existing.clone();
    if !v.is_object() {
        v = json!({});
    }
    v["OPENAI_API_KEY"] = Value::String(key.to_string());
    v
}

/// Persist the current credential file before it gets overwritten, so a bad
/// switch never costs the user a browser round-trip.
pub fn stash(existing: &Value) -> Result<()> {
    let path = paths::stash_official_path();
    if existing.as_object().map(|o| o.is_empty()).unwrap_or(true) {
        return Ok(());
    }
    let st = inspect(existing);
    if !st.has_tokens && st.api_key.is_none() {
        return Ok(());
    }
    let text = serde_json::to_string_pretty(existing)?;
    paths::atomic_write(&path, text.as_bytes())
}

pub fn load_stashed() -> Option<Value> {
    let path = paths::stash_official_path();
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn write(v: &Value) -> Result<()> {
    let text = serde_json::to_string_pretty(v)?;
    paths::atomic_write(&paths::auth_path(), text.as_bytes())
}
