use crate::profile::{AuthKind, Profile};
use anyhow::{Context, Result};
use toml_edit::{value, DocumentMut, Item, Table};

/// Render the new config.toml contents without touching the file.
///
/// Only the keys that describe "which provider / which model / which auth" are
/// rewritten. `[projects]`, `[mcp_servers]`, `notify`, comments and any custom
/// provider tables the user added are preserved byte-for-byte.
pub fn render(existing: &str, profile: &Profile, api_key: Option<&str>) -> Result<String> {
    let mut doc: DocumentMut = if existing.trim().is_empty() {
        DocumentMut::new()
    } else {
        existing.parse().context("解析 config.toml 失败")?
    };

    let provider_key = if profile.provider.is_empty() {
        "openai"
    } else {
        profile.provider.as_str()
    };

    doc["model_provider"] = value(provider_key);

    if !profile.model.is_empty() {
        doc["model"] = value(&profile.model);
    }
    if !profile.reasoning_effort.is_empty() {
        doc["model_reasoning_effort"] = value(&profile.reasoning_effort);
    }

    match profile.auth {
        AuthKind::Chatgpt => {
            doc["preferred_auth_method"] = value("chatgpt");
        }
        AuthKind::Apikey => {
            doc["preferred_auth_method"] = value("apikey");
        }
    }

    if let Some(flag) = profile.disable_response_storage {
        doc["disable_response_storage"] = value(flag);
    }

    // Official provider: nothing to declare, Codex already knows `openai`.
    if !profile.is_official() {
        if !doc.contains_key("model_providers") {
            let mut t = Table::new();
            t.set_implicit(true);
            doc.insert("model_providers", Item::Table(t));
        }
        let providers = doc["model_providers"]
            .as_table_mut()
            .context("config.toml 里的 model_providers 不是表")?;

        if !providers.contains_key(&profile.provider) {
            providers.insert(&profile.provider, Item::Table(Table::new()));
        }
        let table = providers[&profile.provider]
            .as_table_mut()
            .with_context(|| format!("[model_providers.{}] 不是表", profile.provider))?;

        table["name"] = value(&profile.provider);
        table["base_url"] = value(&profile.base_url);
        table["wire_api"] = value(&profile.wire_api);

        match profile.auth {
            // Reuse the cached ChatGPT login against this endpoint.
            AuthKind::Chatgpt => {
                table["requires_openai_auth"] = value(true);
                table.remove("experimental_bearer_token");
                table.remove("env_key");
            }
            // Never let a third-party key leak into auth.json: keep it scoped
            // to this provider table.
            AuthKind::Apikey => {
                table["requires_openai_auth"] = value(false);
                table.remove("env_key");
                match api_key {
                    Some(key) if !key.is_empty() => {
                        table["experimental_bearer_token"] = value(key);
                    }
                    _ => {
                        // Without a key the provider would 401; surface it loudly.
                        table.remove("experimental_bearer_token");
                    }
                }
            }
        }
    }

    Ok(doc.to_string())
}

/// Keys worth showing the user before writing.
pub fn summarize(profile: &Profile, api_key: Option<&str>) -> Vec<(String, String)> {
    let provider_key = if profile.provider.is_empty() {
        "openai (内置)"
    } else {
        profile.provider.as_str()
    };
    let mut rows = vec![
        ("model_provider".to_string(), provider_key.to_string()),
        (
            "preferred_auth_method".to_string(),
            patch_auth(profile.auth),
        ),
    ];
    if !profile.model.is_empty() {
        rows.push(("model".to_string(), profile.model.clone()));
    }
    if !profile.reasoning_effort.is_empty() {
        rows.push((
            "model_reasoning_effort".to_string(),
            profile.reasoning_effort.clone(),
        ));
    }
    if !profile.is_official() {
        rows.push(("base_url".to_string(), profile.base_url.clone()));
        rows.push((
            "requires_openai_auth".to_string(),
            profile.requires_openai_auth.to_string(),
        ));
        let token_state = match (profile.auth, api_key) {
            (AuthKind::Apikey, Some(k)) if !k.is_empty() => {
                format!("experimental_bearer_token = {}…", &k[..k.len().min(6)])
            }
            (AuthKind::Apikey, _) => "缺少 API Key，需先执行 `codex-switch key <id>`".to_string(),
            (AuthKind::Chatgpt, _) => "复用 auth.json 里的登录态".to_string(),
        };
        rows.push(("凭据".to_string(), token_state));
    }
    rows
}

fn patch_auth(a: AuthKind) -> String {
    match a {
        AuthKind::Chatgpt => "chatgpt".to_string(),
        AuthKind::Apikey => "apikey".to_string(),
    }
}
