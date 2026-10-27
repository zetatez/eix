//! Provider credentials: API keys and OAuth access tokens.
//!
//! Credentials live in `<data_dir>/auth/<provider>.json` with owner-only
//! permissions. The config remains the first source (inline key, then
//! `api_key_env`); this store is consulted when neither is set. The daemon (and
//! `model.list`) read the files on demand, so `eix auth login` applies to a
//! running daemon without a restart.
//!
//! OAuth uses the authorization-code flow with PKCE and a loopback redirect.
//! Endpoints come from `[providers.<name>.oauth]`; tokens are refreshed
//! transparently before a request when a refresh token is available.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use eix_core::ProviderOAuthConfig;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;

use crate::oauth;

const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);
/// Refresh a little early so a token cannot expire while the request is in
/// flight.
const REFRESH_MARGIN_SECS: u64 = 60;

/// How a stored credential authorizes requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    ApiKey,
    OAuth,
}

/// One provider's persisted credential.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credential {
    pub kind: CredentialKind,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Unix seconds; absent when the token does not expire.
    #[serde(default)]
    pub expires_at: Option<u64>,
    #[serde(default)]
    pub token_endpoint: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub extra_token_params: BTreeMap<String, String>,
    #[serde(default)]
    pub created_at: u64,
}

impl Credential {
    fn expires_soon(&self) -> bool {
        self.expires_at
            .is_some_and(|at| at <= oauth::now_secs().saturating_add(REFRESH_MARGIN_SECS))
    }

    /// Whether the credential can be used without a network refresh.
    pub fn usable_cached(&self) -> bool {
        match self.kind {
            CredentialKind::ApiKey => self
                .api_key
                .as_deref()
                .is_some_and(|key| !key.trim().is_empty()),
            CredentialKind::OAuth => {
                self.access_token
                    .as_deref()
                    .is_some_and(|token| !token.trim().is_empty())
                    && !self.expires_soon()
            }
        }
    }

    /// Whether the credential can authorize a request now or after a refresh.
    pub fn available(&self) -> bool {
        self.usable_cached()
            || (self.kind == CredentialKind::OAuth
                && self
                    .refresh_token
                    .as_deref()
                    .is_some_and(|token| !token.trim().is_empty()))
    }

    /// One-line state for `eix auth list`; never includes the secret.
    pub fn summary(&self) -> String {
        match self.kind {
            CredentialKind::ApiKey => "api key".to_string(),
            CredentialKind::OAuth => match self.expires_at {
                Some(at) if at <= oauth::now_secs() => "oauth (expired)".to_string(),
                Some(at) => format!("oauth (expires in {}s)", at - oauth::now_secs()),
                None => "oauth".to_string(),
            },
        }
    }
}

/// Material to attach to a provider request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Secret {
    /// A static provider API key (`x-api-key` for Anthropic, otherwise Bearer).
    ApiKey(String),
    /// An OAuth access token, always sent as a Bearer token.
    Bearer(String),
}

impl Secret {
    pub fn expose(&self) -> &str {
        match self {
            Self::ApiKey(value) | Self::Bearer(value) => value,
        }
    }
}

/// Owner-only credential directory.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Default location under a daemon data directory.
    pub fn under_data_dir(data_dir: &Path) -> Self {
        Self::new(data_dir.join("auth"))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Provider names are config keys; sanitize before touching the filesystem.
    pub fn token_path(&self, provider: &str) -> PathBuf {
        let safe: String = provider
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                    ch
                } else {
                    '_'
                }
            })
            .collect();
        self.dir.join(format!("{safe}.json"))
    }

    pub fn load(&self, provider: &str) -> Result<Option<Credential>> {
        let path = self.token_path(provider);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("cannot read {}", path.display())),
        };
        let credential = serde_json::from_str(&text)
            .with_context(|| format!("invalid credential in {}", path.display()))?;
        Ok(Some(credential))
    }

    pub fn save(&self, provider: &str, credential: &Credential) -> Result<()> {
        oauth::create_private_dir(&self.dir)?;
        let path = self.token_path(provider);
        oauth::write_private(&path, serde_json::to_vec_pretty(credential)?)
            .with_context(|| format!("cannot save the credential to {}", path.display()))
    }

    pub fn remove(&self, provider: &str) -> Result<bool> {
        match std::fs::remove_file(self.token_path(provider)) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err).context("cannot remove the credential"),
        }
    }

    /// Stored provider names, sorted. Unreadable files are skipped.
    pub fn list(&self) -> Vec<(String, Credential)> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(_) => return Vec::new(),
        };
        let mut out: Vec<(String, Credential)> = entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
            .filter_map(|entry| {
                let name = entry.path().file_stem()?.to_string_lossy().to_string();
                let credential = self.load(&name).ok().flatten()?;
                Some((name, credential))
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    pub fn save_api_key(&self, provider: &str, api_key: &str) -> Result<()> {
        let api_key = api_key.trim();
        if api_key.is_empty() {
            bail!("the API key is empty");
        }
        self.save(
            provider,
            &Credential {
                kind: CredentialKind::ApiKey,
                api_key: Some(api_key.to_string()),
                access_token: None,
                refresh_token: None,
                expires_at: None,
                token_endpoint: String::new(),
                client_id: String::new(),
                client_secret: None,
                scope: None,
                extra_token_params: BTreeMap::new(),
                created_at: oauth::now_secs(),
            },
        )
    }

    /// Credential for a request, refreshing an expired OAuth token when
    /// possible. A refresh failure is reported on stderr and the stale token is
    /// returned so the provider's 401 reaches the caller with its own message.
    pub async fn secret(&self, provider: &str) -> Option<Secret> {
        let mut credential = match self.load(provider) {
            Ok(Some(credential)) => credential,
            Ok(None) => return None,
            Err(err) => {
                eprintln!("eixd: credential for {provider:?}: {err:#}");
                return None;
            }
        };
        if credential.kind == CredentialKind::OAuth && credential.expires_soon() {
            match refresh(&mut credential).await {
                Ok(()) => {
                    if let Err(err) = self.save(provider, &credential) {
                        eprintln!("eixd: credential for {provider:?}: {err:#}");
                    }
                }
                Err(err) => {
                    eprintln!("eixd: cannot refresh the OAuth token for {provider:?}: {err:#}")
                }
            }
        }
        match credential.kind {
            CredentialKind::ApiKey => credential.api_key.map(Secret::ApiKey),
            CredentialKind::OAuth => credential.access_token.map(Secret::Bearer),
        }
    }

    /// Cached credential without refreshing. Used by the model catalog, where a
    /// stale token only means the provider contributes a warning.
    pub fn cached_secret(&self, provider: &str) -> Option<Secret> {
        let credential = self.load(provider).ok().flatten()?;
        if !credential.usable_cached() {
            return None;
        }
        match credential.kind {
            CredentialKind::ApiKey => credential.api_key.map(Secret::ApiKey),
            CredentialKind::OAuth => credential.access_token.map(Secret::Bearer),
        }
    }

    /// Run the browser OAuth flow and persist the token.
    pub async fn authorize_oauth(
        &self,
        provider: &str,
        config: &ProviderOAuthConfig,
        no_browser: bool,
    ) -> Result<Credential> {
        let authorize_endpoint = Url::parse(&config.authorization_endpoint)
            .context("invalid providers.*.oauth.authorization_endpoint")?;

        // Bind before building the redirect so the URI is exact.
        let (listener, redirect_uri, callback_path) = match config
            .redirect_uri
            .as_deref()
            .map(str::trim)
            .filter(|uri| !uri.is_empty())
        {
            Some(uri) => {
                let parsed = Url::parse(uri).context("invalid providers.*.oauth.redirect_uri")?;
                let port = parsed
                    .port_or_known_default()
                    .context("redirect_uri must include a port")?;
                let host = parsed.host_str().unwrap_or_default();
                if !matches!(host, "127.0.0.1" | "localhost") {
                    bail!("redirect_uri must point at 127.0.0.1 or localhost");
                }
                let listener = TcpListener::bind(("127.0.0.1", port))
                    .await
                    .with_context(|| format!("cannot bind redirect_uri port {port}"))?;
                let path = if parsed.path().is_empty() {
                    "/callback".to_string()
                } else {
                    parsed.path().to_string()
                };
                (listener, uri.to_string(), path)
            }
            None => {
                let listener = TcpListener::bind(("127.0.0.1", 0))
                    .await
                    .context("cannot bind the OAuth callback listener")?;
                let port = listener.local_addr()?.port();
                (
                    listener,
                    format!("http://127.0.0.1:{port}/callback"),
                    "/callback".to_string(),
                )
            }
        };

        let verifier = oauth::random_verifier();
        let challenge = oauth::b64url(&Sha256::digest(verifier.as_bytes()));
        let state = oauth::random_state();
        let scope = (!config.scopes.is_empty()).then(|| config.scopes.join(" "));

        let mut authorize = authorize_endpoint;
        {
            let mut query = authorize.query_pairs_mut();
            query.append_pair("response_type", "code");
            query.append_pair("client_id", config.client_id.trim());
            query.append_pair("redirect_uri", &redirect_uri);
            query.append_pair("state", &state);
            query.append_pair("code_challenge", &challenge);
            query.append_pair("code_challenge_method", "S256");
            if let Some(scope) = scope.as_deref() {
                query.append_pair("scope", scope);
            }
            for (key, value) in &config.extra_authorize_params {
                query.append_pair(key, value);
            }
        }

        println!();
        println!("Authorize eix for provider {provider:?}:");
        println!();
        println!("  {authorize}");
        println!();
        if no_browser {
            println!("Open the URL above in a browser to continue.");
        } else {
            println!("Opening it in your browser; otherwise copy the URL above.");
            oauth::open_browser(authorize.as_str());
        }

        let code = tokio::time::timeout(
            CALLBACK_TIMEOUT,
            oauth::wait_for_callback(listener, &state, &callback_path),
        )
        .await
        .context("timed out waiting for the OAuth callback")??;

        let mut form: Vec<(&str, &str)> = vec![
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("client_id", config.client_id.trim()),
            ("code_verifier", verifier.as_str()),
        ];
        if let Some(scope) = scope.as_deref() {
            form.push(("scope", scope));
        }
        if let Some(secret) = config.client_secret.as_deref() {
            form.push(("client_secret", secret));
        }
        for (key, value) in &config.extra_token_params {
            form.push((key, value));
        }
        let payload = oauth::post_form(
            config.token_endpoint.trim(),
            &form,
            Duration::from_secs(30),
            "exchange the authorization code",
        )
        .await?;

        let mut credential = Credential {
            kind: CredentialKind::OAuth,
            api_key: None,
            access_token: None,
            refresh_token: None,
            expires_at: None,
            token_endpoint: config.token_endpoint.trim().to_string(),
            client_id: config.client_id.trim().to_string(),
            client_secret: config.client_secret.clone(),
            scope,
            extra_token_params: config.extra_token_params.clone(),
            created_at: oauth::now_secs(),
        };
        apply_token_response(&mut credential, &payload)?;
        self.save(provider, &credential)?;
        Ok(credential)
    }
}

fn apply_token_response(credential: &mut Credential, payload: &Value) -> Result<()> {
    let access = payload
        .get("access_token")
        .and_then(Value::as_str)
        .context("the token response has no access_token")?;
    credential.access_token = Some(access.to_string());
    if let Some(expires_in) = payload.get("expires_in").and_then(Value::as_u64) {
        credential.expires_at = Some(oauth::now_secs().saturating_add(expires_in));
    }
    if let Some(refresh) = payload.get("refresh_token").and_then(Value::as_str) {
        credential.refresh_token = Some(refresh.to_string());
    }
    if let Some(scope) = payload.get("scope").and_then(Value::as_str) {
        credential.scope = Some(scope.to_string());
    }
    Ok(())
}

async fn refresh(credential: &mut Credential) -> Result<()> {
    let refresh_token = credential
        .refresh_token
        .clone()
        .context("the credential has no refresh_token")?;
    if credential.token_endpoint.trim().is_empty() {
        bail!("the credential has no token_endpoint");
    }
    let mut form: Vec<(&str, &str)> = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token.as_str()),
        ("client_id", credential.client_id.as_str()),
    ];
    if let Some(scope) = credential.scope.as_deref() {
        form.push(("scope", scope));
    }
    if let Some(secret) = credential.client_secret.as_deref() {
        form.push(("client_secret", secret));
    }
    for (key, value) in &credential.extra_token_params {
        form.push((key, value));
    }
    let payload = oauth::post_form(
        credential.token_endpoint.trim(),
        &form,
        Duration::from_secs(30),
        "refresh the OAuth token",
    )
    .await?;
    apply_token_response(credential, &payload)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn credential() -> Credential {
        Credential {
            kind: CredentialKind::OAuth,
            api_key: None,
            access_token: Some("old".into()),
            refresh_token: Some("refresh".into()),
            expires_at: Some(0),
            token_endpoint: String::new(),
            client_id: "client".into(),
            client_secret: None,
            scope: None,
            extra_token_params: BTreeMap::new(),
            created_at: 0,
        }
    }

    #[test]
    fn paths_are_confined_to_the_store() {
        let store = Store::new("/tmp/eix-auth-test");
        assert_eq!(
            store.token_path("../evil/name").file_name().unwrap(),
            "___evil_name.json"
        );
        assert_eq!(
            store.token_path("openai"),
            Path::new("/tmp/eix-auth-test/openai.json")
        );
    }

    #[test]
    fn token_responses_preserve_refresh_tokens() {
        let mut credential = credential();
        apply_token_response(
            &mut credential,
            &json!({"access_token": "new", "expires_in": 3600}),
        )
        .unwrap();
        assert_eq!(credential.access_token.as_deref(), Some("new"));
        assert_eq!(credential.refresh_token.as_deref(), Some("refresh"));
        assert!(credential.expires_at.unwrap() > oauth::now_secs());
    }

    #[tokio::test]
    async fn refresh_posts_the_stored_client_and_updates_the_token() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0u8; 8192];
            let read = stream.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            assert!(request.contains("grant_type=refresh_token"), "{request}");
            assert!(request.contains("client_id=client"), "{request}");
            assert!(request.contains("refresh_token=old-refresh"), "{request}");
            let body = json!({"access_token": "new-access", "expires_in": 3600}).to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let mut credential = credential();
        credential.refresh_token = Some("old-refresh".into());
        credential.token_endpoint = format!("http://{addr}/token");
        refresh(&mut credential).await.unwrap();
        assert_eq!(credential.access_token.as_deref(), Some("new-access"));
        server.await.unwrap();
    }

    #[test]
    fn only_unexpired_oauth_tokens_are_cached() {
        let store = Store::new(
            std::env::temp_dir().join(format!("eix-auth-test-{}", uuid::Uuid::new_v4())),
        );
        store.save_api_key("openai", "sk-test").unwrap();
        assert_eq!(
            store.cached_secret("openai"),
            Some(Secret::ApiKey("sk-test".into()))
        );

        let mut oauth = credential();
        oauth.expires_at = Some(oauth::now_secs() + 3600);
        oauth.access_token = Some("access".into());
        store.save("google", &oauth).unwrap();
        assert_eq!(
            store.cached_secret("google"),
            Some(Secret::Bearer("access".into()))
        );

        oauth.expires_at = Some(0);
        store.save("stale", &oauth).unwrap();
        assert_eq!(store.cached_secret("stale"), None);
        // Stale but refreshable: unusable for an immediate request, yet still
        // counted as configured because the daemon will refresh it.
        assert!(store.load("stale").unwrap().unwrap().available());
        oauth.refresh_token = None;
        assert!(!oauth.available());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.token_path("openai"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(store.dir());
    }
}
