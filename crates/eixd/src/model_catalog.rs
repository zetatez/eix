//! Fetch OpenAI-compatible model catalogs for `/model`.
//!
//! Each configured provider is queried concurrently. A provider that is
//! offline or does not implement `GET /models` only contributes a warning; the
//! statically configured default provider / `[[models]]` entries always remain
//! selectable.

use std::time::Duration;

use eix_core::{Config, ModelInfo, ProviderKind, ProviderSource};
use futures_util::future::join_all;
use serde_json::Value;

use crate::auth::{Secret, Store};

const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn fetch_all(config: &Config, credentials: &Store) -> (Vec<ModelInfo>, Vec<String>) {
    let client = match reqwest::Client::builder()
        .user_agent(format!("eix/{}", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(5))
        .timeout(FETCH_TIMEOUT)
        .build()
    {
        Ok(client) => client,
        Err(err) => return (Vec::new(), vec![format!("model catalog client: {err}")]),
    };

    let default_id = config.default_model().to_string();
    let results = join_all(config.provider_sources().into_iter().map(|source| {
        let client = client.clone();
        let default_id = default_id.clone();
        let credentials = credentials.clone();
        async move { fetch_source(&client, &source, &default_id, &credentials).await }
    }))
    .await;

    let mut models = Vec::new();
    let mut warnings = Vec::new();
    for result in results {
        match result {
            Ok(mut fetched) => models.append(&mut fetched),
            Err(warning) => warnings.push(warning),
        }
    }
    (models, warnings)
}

async fn fetch_source(
    client: &reqwest::Client,
    source: &ProviderSource,
    default_id: &str,
    credentials: &Store,
) -> Result<Vec<ModelInfo>, String> {
    let secret = source
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(|key| Secret::ApiKey(key.to_string()))
        .or_else(|| credentials.cached_secret(&source.key));
    let mut url = source.kind.models_endpoint(&source.base_url);
    if source.kind == ProviderKind::Gemini && !url.contains("pageSize=") {
        url.push_str(if url.contains('?') {
            "&pageSize=1000"
        } else {
            "?pageSize=1000"
        });
    }
    let mut request = client
        .get(&url)
        .header(reqwest::header::ACCEPT, "application/json");
    match (source.kind, secret) {
        (ProviderKind::Anthropic, Some(Secret::ApiKey(key))) => {
            request = request
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01");
        }
        (ProviderKind::Gemini, Some(Secret::ApiKey(key))) => {
            request = request.header("x-goog-api-key", key);
        }
        (_, Some(secret)) => request = request.bearer_auth(secret.expose()),
        (_, None) => {}
    }
    let response = request
        .send()
        .await
        .map_err(|err| format!("{}: {err}", source.key))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("{}: HTTP {}", source.key, status.as_u16()));
    }
    let value: Value = response
        .json()
        .await
        .map_err(|err| format!("{}: invalid model list: {err}", source.key))?;
    Ok(parse_models(&value, source, default_id))
}

fn parse_models(value: &Value, source: &ProviderSource, default_id: &str) -> Vec<ModelInfo> {
    let items = match source.kind {
        ProviderKind::Anthropic => value.get("data").and_then(Value::as_array),
        ProviderKind::Gemini => value.get("models").and_then(Value::as_array),
        ProviderKind::OpenAi => value
            .get("data")
            .and_then(Value::as_array)
            .or_else(|| value.get("models").and_then(Value::as_array))
            .or_else(|| value.as_array()),
    };
    let Some(items) = items else {
        return Vec::new();
    };

    let mut models = Vec::new();
    for item in items {
        if source.kind == ProviderKind::Gemini
            && item
                .get("supportedGenerationMethods")
                .and_then(Value::as_array)
                .is_some_and(|methods| {
                    !methods
                        .iter()
                        .any(|method| method.as_str() == Some("generateContent"))
                })
        {
            continue;
        }
        let Some(api_id) = item
            .as_str()
            .map(str::to_string)
            .or_else(|| item.get("id").and_then(Value::as_str).map(str::to_string))
            .or_else(|| {
                item.get("name")
                    .and_then(Value::as_str)
                    .map(|name| name.strip_prefix("models/").unwrap_or(name).to_string())
            })
        else {
            continue;
        };
        let api_id = api_id.trim();
        if api_id.is_empty() {
            continue;
        }
        // The statically configured default already appears first in the
        // catalog; do not duplicate it.
        if source.default && api_id == default_id {
            continue;
        }
        let id = format!("{}/{api_id}", source.key);
        let name = match source.kind {
            ProviderKind::Anthropic => item.get("display_name").and_then(Value::as_str),
            ProviderKind::Gemini => item.get("displayName").and_then(Value::as_str),
            ProviderKind::OpenAi => item.get("name").and_then(Value::as_str),
        }
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(api_id)
        .to_string();
        let context = match source.kind {
            ProviderKind::Gemini => item
                .get("inputTokenLimit")
                .and_then(Value::as_u64)
                .map(|tokens| tokens as usize),
            _ => context_tokens(item),
        };
        models.push(ModelInfo {
            id,
            api_model: Some(api_id.to_string()),
            name,
            provider: source.name.clone(),
            default: false,
            variants: Vec::new(),
            input_cost: price(item, "input", "prompt"),
            output_cost: price(item, "output", "completion"),
            context_tokens: context,
        });
    }
    models
}

fn price(item: &Value, direct: &str, nested: &str) -> Option<f64> {
    if let Some(value) = item.get(direct).and_then(number) {
        return Some(value);
    }
    let value = item.get("pricing")?.get(nested)?;
    // OpenRouter reports USD per token as a string.
    number(value).map(|value| value * 1_000_000.0)
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}

fn context_tokens(item: &Value) -> Option<usize> {
    ["context_length", "context_window", "max_context_length"]
        .iter()
        .find_map(|key| item.get(*key).and_then(Value::as_u64))
        .or_else(|| {
            item.get("top_provider")
                .and_then(|provider| provider.get("context_length"))
                .and_then(Value::as_u64)
        })
        .map(|tokens| tokens as usize)
}

/// Keep configured entries first and append fetched models that are not
/// already present under the same selection id.
pub fn merge(configured: Vec<ModelInfo>, fetched: Vec<ModelInfo>) -> Vec<ModelInfo> {
    let mut out = configured;
    for model in fetched {
        if !out.iter().any(|existing| existing.id == model.id) {
            out.push(model);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(key: &str, default: bool) -> ProviderSource {
        ProviderSource {
            key: key.to_string(),
            name: key.to_string(),
            kind: ProviderKind::OpenAi,
            base_url: "http://127.0.0.1:9/v1".into(),
            api_key: None,
            default,
        }
    }

    #[test]
    fn parses_openai_and_openrouter_metadata() {
        let value = serde_json::json!({
            "data": [
                {"id": "current", "name": "Current"},
                {"id": "openai/gpt-5", "name": "GPT-5",
                 "context_length": 400000,
                 "pricing": {"prompt": "0.00000125", "completion": "0.00001"}},
                {"id": ""}
            ]
        });
        let models = parse_models(&value, &source("openai", true), "current");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "openai/openai/gpt-5");
        assert_eq!(models[0].api_model.as_deref(), Some("openai/gpt-5"));
        assert_eq!(models[0].name, "GPT-5");
        assert_eq!(models[0].context_tokens, Some(400_000));
        assert_eq!(models[0].input_cost, Some(1.25));
        assert_eq!(models[0].output_cost, Some(10.0));

        let named = parse_models(&value, &source("router", false), "current");
        assert_eq!(named.len(), 2);
        assert_eq!(named[0].id, "router/current");
        assert_eq!(named[0].api_model.as_deref(), Some("current"));
        assert_eq!(named[1].id, "router/openai/gpt-5");
        assert_eq!(named[1].api_model.as_deref(), Some("openai/gpt-5"));
    }

    #[test]
    fn parses_anthropic_and_gemini_catalogs() {
        let mut anthropic = source("claude", false);
        anthropic.kind = ProviderKind::Anthropic;
        let value = serde_json::json!({
            "data": [
                {"id": "claude-sonnet-4", "display_name": "Claude Sonnet 4"},
                {"id": ""}
            ]
        });
        let models = parse_models(&value, &anthropic, "unused");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "claude/claude-sonnet-4");
        assert_eq!(models[0].api_model.as_deref(), Some("claude-sonnet-4"));
        assert_eq!(models[0].name, "Claude Sonnet 4");

        let mut gemini = source("google", false);
        gemini.kind = ProviderKind::Gemini;
        let value = serde_json::json!({
            "models": [
                {"name": "models/gemini-2.5-pro", "displayName": "Gemini 2.5 Pro",
                 "inputTokenLimit": 1048576,
                 "supportedGenerationMethods": ["generateContent", "countTokens"]},
                {"name": "models/text-embedding-004",
                 "supportedGenerationMethods": ["embedContent"]}
            ]
        });
        let models = parse_models(&value, &gemini, "unused");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "google/gemini-2.5-pro");
        assert_eq!(models[0].api_model.as_deref(), Some("gemini-2.5-pro"));
        assert_eq!(models[0].name, "Gemini 2.5 Pro");
        assert_eq!(models[0].context_tokens, Some(1_048_576));
    }

    #[test]
    fn merge_keeps_configured_rows_first() {
        let configured = vec![ModelInfo {
            id: "openai/current".into(),
            api_model: None,
            name: "current".into(),
            provider: "openai".into(),
            default: true,
            variants: Vec::new(),
            input_cost: None,
            output_cost: None,
            context_tokens: None,
        }];
        let fetched = vec![
            ModelInfo {
                id: "openai/current".into(),
                api_model: Some("current".into()),
                name: "duplicate".into(),
                provider: "openai".into(),
                default: false,
                variants: Vec::new(),
                input_cost: None,
                output_cost: None,
                context_tokens: None,
            },
            ModelInfo {
                id: "router/new".into(),
                api_model: Some("new".into()),
                name: "new".into(),
                provider: "router".into(),
                default: false,
                variants: Vec::new(),
                input_cost: None,
                output_cost: None,
                context_tokens: None,
            },
        ];
        let merged = merge(configured, fetched);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].id, "openai/current");
        assert_eq!(merged[1].id, "router/new");
    }
}
