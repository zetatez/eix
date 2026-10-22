//! Fetch OpenAI-compatible model catalogs for `/model`.
//!
//! Each configured provider is queried concurrently. A provider that is
//! offline or does not implement `GET /models` only contributes a warning; the
//! statically configured `[provider]` / `[[models]]` entries always remain
//! selectable.

use std::time::Duration;

use eix_core::{models_endpoint, Config, ModelInfo, ProviderSource};
use futures_util::future::join_all;
use serde_json::Value;

const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn fetch_all(config: &Config) -> (Vec<ModelInfo>, Vec<String>) {
    let client = match reqwest::Client::builder()
        .user_agent(format!("eix/{}", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(5))
        .timeout(FETCH_TIMEOUT)
        .build()
    {
        Ok(client) => client,
        Err(err) => return (Vec::new(), vec![format!("model catalog client: {err}")]),
    };

    let default_id = config.provider.model.trim().to_string();
    let results = join_all(config.provider_sources().into_iter().map(|source| {
        let client = client.clone();
        let default_id = default_id.clone();
        async move { fetch_source(&client, &source, &default_id).await }
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
) -> Result<Vec<ModelInfo>, String> {
    let url = models_endpoint(&source.base_url);
    let mut request = client
        .get(&url)
        .header(reqwest::header::ACCEPT, "application/json");
    if let Some(key) = &source.api_key {
        request = request.bearer_auth(key);
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
    let items = value
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| value.get("models").and_then(Value::as_array))
        .or_else(|| value.as_array());
    let Some(items) = items else {
        return Vec::new();
    };

    let mut models = Vec::new();
    for item in items {
        let Some(api_id) = item
            .as_str()
            .map(str::to_string)
            .or_else(|| item.get("id").and_then(Value::as_str).map(str::to_string))
        else {
            continue;
        };
        let api_id = api_id.trim();
        if api_id.is_empty() {
            continue;
        }
        // The statically configured default already appears first in the
        // catalog; do not duplicate it under the `default/` alias.
        if source.default && api_id == default_id {
            continue;
        }
        let id = if source.default {
            format!("default/{api_id}")
        } else {
            format!("{}/{api_id}", source.key)
        };
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(api_id)
            .to_string();
        models.push(ModelInfo {
            id,
            api_model: Some(api_id.to_string()),
            name,
            provider: source.name.clone(),
            default: false,
            variants: Vec::new(),
            input_cost: price(item, "input", "prompt"),
            output_cost: price(item, "output", "completion"),
            context_tokens: context_tokens(item),
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
        let models = parse_models(&value, &source("default", true), "current");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "default/openai/gpt-5");
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
    fn merge_keeps_configured_rows_first() {
        let configured = vec![ModelInfo {
            id: "default/current".into(),
            api_model: None,
            name: "current".into(),
            provider: "default".into(),
            default: true,
            variants: Vec::new(),
            input_cost: None,
            output_cost: None,
            context_tokens: None,
        }];
        let fetched = vec![
            ModelInfo {
                id: "default/current".into(),
                api_model: Some("current".into()),
                name: "duplicate".into(),
                provider: "default".into(),
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
        assert_eq!(merged[0].id, "default/current");
        assert_eq!(merged[1].id, "router/new");
    }
}
