use crate::paths;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthKind {
    /// Browser OAuth login cached as tokens in auth.json.
    Chatgpt,
    /// Bearer API key, either stored in the provider table or in auth.json.
    Apikey,
}

impl AuthKind {
    pub fn label(self) -> &'static str {
        match self {
            AuthKind::Chatgpt => "ChatGPT 账号",
            AuthKind::Apikey => "API Key",
        }
    }
    /// Narrow form for table cells.
    pub fn short(self) -> &'static str {
        match self {
            AuthKind::Chatgpt => "ChatGPT",
            AuthKind::Apikey => "API Key",
        }
    }
}

fn default_wire_api() -> String {
    "responses".to_string()
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Profile {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    pub auth: AuthKind,
    /// Provider key used in `[model_providers.<provider>]`. Empty means the
    /// built-in official `openai` provider.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base_url: String,
    #[serde(default = "default_wire_api")]
    pub wire_api: String,
    /// Keep using the cached ChatGPT login against this endpoint instead of
    /// embedding an API key.
    #[serde(default)]
    pub requires_openai_auth: bool,
    #[serde(default)]
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reasoning_effort: String,
    /// Relays usually need this on; the official backend needs it off for
    /// cloud tasks. `None` leaves whatever the user already had.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_response_storage: Option<bool>,
}

impl Profile {
    pub fn is_official(&self) -> bool {
        self.provider.is_empty() || self.provider == "openai"
    }

    /// Provider key that sessions must be tagged with for this profile.
    pub fn history_provider(&self) -> &str {
        if self.provider.is_empty() {
            "openai"
        } else {
            &self.provider
        }
    }

    pub fn api_key(&self, keys: &Keys) -> Option<String> {
        keys.get(&self.id).cloned()
    }
}

#[derive(Serialize, Deserialize, Default)]
pub struct Profiles {
    #[serde(default)]
    pub active: String,
    #[serde(default)]
    pub profiles: Vec<Profile>,
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Keys(BTreeMap<String, String>);

impl Keys {
    pub fn get(&self, id: &str) -> Option<&String> {
        self.0.get(id)
    }
    pub fn set(&mut self, id: &str, value: &str) {
        self.0.insert(id.to_string(), value.to_string());
    }
    pub fn masked(&self, id: &str) -> String {
        match self.0.get(id) {
            None => "<未设置>".to_string(),
            Some(k) if k.len() <= 10 => "*".repeat(k.len()),
            Some(k) => format!("{}…{}", &k[..6], &k[k.len() - 4..]),
        }
    }
    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.0.iter()
    }
}

impl Profiles {
    pub fn get(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }
}

pub fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() {
        bail!("profile id 不能为空");
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        bail!("profile id 只能包含字母、数字、- _ . 。它同时会作为 TOML 表名使用");
    }
    Ok(())
}

pub fn load_profiles() -> Result<Profiles> {
    let path = paths::profiles_path();
    if !path.exists() {
        return Ok(Profiles::default());
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("读取 {}", path.display()))?;
    let mut parsed: Profiles =
        toml_edit::de::from_str(&text).with_context(|| format!("解析 {}", path.display()))?;
    if parsed.active.is_empty() {
        if let Some(first) = parsed.profiles.first() {
            parsed.active = first.id.clone();
        }
    }
    Ok(parsed)
}

pub fn save_profiles(p: &Profiles) -> Result<()> {
    let path = paths::profiles_path();
    let text = toml_edit::ser::to_string_pretty(p).context("序列化 profiles.toml")?;
    let header = "# codex-switch profiles —— 由 `codex-switch` 自动维护\n\
                  # 这里只放非敏感信息，API Key 单独存在 keys.json\n\n";
    paths::atomic_write(&path, format!("{header}{text}").as_bytes())
}

pub fn load_keys() -> Result<Keys> {
    let path = paths::keys_path();
    if !path.exists() {
        return Ok(Keys::default());
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("读取 {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("解析 {}", path.display()))
}

pub fn save_keys(keys: &Keys) -> Result<()> {
    let path = paths::keys_path();
    let text = serde_json::to_string_pretty(keys).context("序列化 keys.json")?;
    paths::atomic_write(&path, text.as_bytes())
}

/// Build a profile set from whatever the machine currently has configured.
/// Used on first run and by `import`.
pub fn import_current() -> Result<(Profiles, Keys)> {
    let cfg_path = paths::config_path();
    let auth_path = paths::auth_path();

    let mut model = String::new();
    let mut effort = String::new();
    let mut provider_key = String::new();
    let mut base_url = String::new();
    let mut wire_api = String::from("responses");
    let mut requires_auth = false;
    let mut disable_storage: Option<bool> = None;

    if cfg_path.exists() {
        let text = std::fs::read_to_string(&cfg_path)?;
        let doc: toml_edit::DocumentMut = text.parse().context("解析 config.toml")?;
        model = doc
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        effort = doc
            .get("model_reasoning_effort")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        disable_storage = doc
            .get("disable_response_storage")
            .and_then(|v| v.as_bool());
        provider_key = doc
            .get("model_provider")
            .and_then(|v| v.as_str())
            .unwrap_or("openai")
            .to_string();
        if let Some(table) = doc
            .get("model_providers")
            .and_then(|v| v.as_table())
            .and_then(|t| t.get(&provider_key))
            .and_then(|v| v.as_table())
        {
            base_url = table
                .get("base_url")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            wire_api = table
                .get("wire_api")
                .and_then(|v| v.as_str())
                .unwrap_or("responses")
                .to_string();
            requires_auth = table
                .get("requires_openai_auth")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            // A token embedded in the provider table wins over auth.json.
            if let Some(tok) = table
                .get("experimental_bearer_token")
                .and_then(|v| v.as_str())
            {
                if !tok.is_empty() {
                    let mut keys = Keys::default();
                    if provider_key != "openai" {
                        keys.set(&provider_key, tok);
                    }
                    let profiles = build_profiles(
                        &provider_key,
                        &base_url,
                        &wire_api,
                        requires_auth,
                        &model,
                        &effort,
                        disable_storage,
                    );
                    return Ok((profiles, keys));
                }
            }
        }
    }

    // Fall back to the credential file to decide the auth kind.
    let mut keys = Keys::default();
    if auth_path.exists() {
        if let Ok(text) = std::fs::read_to_string(&auth_path) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                let has_tokens = v.get("tokens").map(|t| !t.is_null()).unwrap_or(false);
                let key = v
                    .get("OPENAI_API_KEY")
                    .and_then(|k| k.as_str())
                    .unwrap_or_default()
                    .to_string();
                if !key.is_empty() && provider_key != "openai" {
                    keys.set(&provider_key, &key);
                }
                // "requires_openai_auth" only makes sense when there is an OAuth
                // login to reuse. With an API-key-only auth.json the effective
                // mode is apikey, whatever the flag says.
                let effective_requires = requires_auth && has_tokens;
                let profiles = build_profiles(
                    &provider_key,
                    &base_url,
                    &wire_api,
                    effective_requires,
                    &model,
                    &effort,
                    disable_storage,
                );
                return Ok((profiles, keys));
            }
        }
    }

    let profiles = build_profiles(
        &provider_key,
        &base_url,
        &wire_api,
        requires_auth,
        &model,
        &effort,
        disable_storage,
    );
    Ok((profiles, keys))
}

fn build_profiles(
    provider_key: &str,
    base_url: &str,
    wire_api: &str,
    requires_auth: bool,
    model: &str,
    effort: &str,
    disable_storage: Option<bool>,
) -> Profiles {
    let official = Profile {
        id: "official".to_string(),
        label: "ChatGPT 官方登录".to_string(),
        note: "订阅额度，支持 cloud / 插件 / 手机远程".to_string(),
        auth: AuthKind::Chatgpt,
        provider: String::new(),
        base_url: String::new(),
        wire_api: "responses".to_string(),
        requires_openai_auth: false,
        model: model.to_string(),
        reasoning_effort: effort.to_string(),
        disable_response_storage: Some(false),
    };

    let is_official = provider_key.is_empty() || provider_key == "openai";
    let mut list = vec![official];

    if !is_official {
        list.push(Profile {
            id: provider_key.to_string(),
            label: format!("{provider_key} 中转"),
            note: base_url.to_string(),
            auth: if requires_auth {
                AuthKind::Chatgpt
            } else {
                AuthKind::Apikey
            },
            provider: provider_key.to_string(),
            base_url: base_url.to_string(),
            wire_api: wire_api.to_string(),
            requires_openai_auth: requires_auth,
            model: model.to_string(),
            reasoning_effort: effort.to_string(),
            disable_response_storage: Some(disable_storage.unwrap_or(true)),
        });
    }

    let active = if is_official {
        "official".to_string()
    } else {
        provider_key.to_string()
    };

    Profiles {
        active,
        profiles: list,
    }
}

/// Write a starter profile set if none exists yet.
pub fn ensure_initialized() -> Result<Profiles> {
    let path = paths::profiles_path();
    if path.exists() {
        return load_profiles();
    }
    let (profiles, keys) = import_current()?;
    save_profiles(&profiles)?;
    save_keys(&keys)?;
    Ok(profiles)
}
