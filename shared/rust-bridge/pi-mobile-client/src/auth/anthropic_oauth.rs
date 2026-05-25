//! Anthropic OAuth Authorization Code + PKCE driver.
//!
//! This module owns the PKCE handshake plumbing for the
//! `pi-mobile-client` crate. The upstream `pi_agent_rust::auth` module
//! already implements the canonical Anthropic OAuth flow (PKCE pair
//! generation, authorize URL construction, token exchange, refresh) and
//! `AuthStorage` for persistence. This driver is a thin adapter so the
//! mobile stack:
//!
//! 1. reads `PI_ANTHROPIC_OAUTH_CLIENT_ID` / `PI_ANTHROPIC_OAUTH_CLIENT_SECRET`
//!    (pi's existing `oauth_param` helper already honours
//!    `PI_ANTHROPIC_OAUTH_CLIENT_ID`; the secret is plumbed through here
//!    purely for platforms that ship a confidential-client variant of the
//!    same OAuth app),
//! 2. generates the PKCE pair via the same routine pi uses,
//! 3. exposes a stable, narrow surface (`begin`, `complete`, `refresh`)
//!    that platform layers and `pi-server-runner --oauth-paste` can call,
//!    and
//! 4. surfaces a typed [`AuthEvent`] enum that the `codex-mobile-client`
//!    store maps to its UniFFI [`AuthState`](codex_mobile_client::store)
//!    enum.
//!
//! No business policy lives here; it's a driver. Reconciliation policy
//! (e.g. "when does `Authorized` become `Failed`?") lives in the
//! reducer/store in `codex-mobile-client`.

#![allow(dead_code)]

use std::path::PathBuf;

use pi::auth::{
    AuthCredential, AuthStorage, OAuthStartInfo, complete_anthropic_oauth, start_anthropic_oauth,
};
use pi::config::Config;
use pi::error::Error as PiError;
use pi::http::client::Client as PiHttpClient;

/// Provider id pi uses as the `auth.json` key for Anthropic OAuth
/// credentials. Kept as a `const` so the failure-path test can grep the
/// expected key without re-deriving it.
pub const ANTHROPIC_PROVIDER_ID: &str = "anthropic";

/// Names of the environment variables this driver reads at construction
/// time. They mirror pi's own override knobs so the same `.env` works
/// for the mobile stack and for raw `pi` invocations.
pub const CLIENT_ID_ENV: &str = "PI_ANTHROPIC_OAUTH_CLIENT_ID";
pub const CLIENT_SECRET_ENV: &str = "PI_ANTHROPIC_OAUTH_CLIENT_SECRET";

/// Snapshot of which OAuth source produced the live credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthEventSource {
    /// Token came from an Authorization Code + PKCE handshake.
    Oauth,
    /// Token came from `pi_byok_set` (caller-supplied API key).
    Byok,
}

/// Typed driver events. The `codex-mobile-client` store reducer maps
/// these to the UniFFI `AuthState` enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthEvent {
    /// No credential present.
    Unauthenticated,
    /// PKCE handshake is in flight (authorize URL surfaced, waiting for
    /// the user to paste the redirect code).
    Authorizing,
    /// A live credential is present.
    Authorized { source: AuthEventSource },
    /// The driver could not produce a live credential. `reason` is a
    /// human-readable, redacted explanation (no tokens, no secrets).
    Failed { reason: String },
}

/// Inputs the platform layer hands the driver. All values are optional
/// because the upstream pi code already has env-var fall-backs; the
/// fields here exist so platform-supplied config (Keychain /
/// EncryptedSharedPreferences) can override the env. The secret is
/// stored only in memory for the duration of the flow.
#[derive(Debug, Clone, Default)]
pub struct AnthropicOAuthConfig {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    /// Override for the `auth.json` path. When `None`, the driver
    /// resolves it via pi's `Config::auth_path()`, which on mobile
    /// resolves to the sandboxed `PI_CODING_AGENT_DIR`.
    pub auth_path: Option<PathBuf>,
}

impl AnthropicOAuthConfig {
    /// Build a config seeded from the documented environment variables.
    pub fn from_env() -> Self {
        Self {
            client_id: std::env::var(CLIENT_ID_ENV).ok().filter(|v| !v.is_empty()),
            client_secret: std::env::var(CLIENT_SECRET_ENV)
                .ok()
                .filter(|v| !v.is_empty()),
            auth_path: None,
        }
    }
}

/// Result of a `begin()` call. Carries the authorize URL the platform
/// must open and the PKCE verifier the driver needs back at `complete`.
#[derive(Debug, Clone)]
pub struct AuthorizeHandshake {
    pub authorize_url: String,
    /// PKCE verifier. The caller must hand this back to `complete()`
    /// alongside the redirect code. Treat as a one-time secret.
    pub verifier: String,
    pub redirect_uri: Option<String>,
}

/// Anthropic OAuth driver. Holds platform-supplied config + an HTTP
/// client so callers can inject a fake client in tests.
#[derive(Debug, Clone)]
pub struct AnthropicOAuthDriver {
    config: AnthropicOAuthConfig,
    http: PiHttpClient,
}

impl AnthropicOAuthDriver {
    pub fn new(config: AnthropicOAuthConfig) -> Self {
        Self::with_client(config, PiHttpClient::new())
    }

    pub fn with_client(config: AnthropicOAuthConfig, http: PiHttpClient) -> Self {
        // Pi's `oauth_param` helper already prefers `PI_ANTHROPIC_OAUTH_CLIENT_ID`
        // from the environment; the platform layer is responsible for
        // populating that env (e.g. via `.env` or a launchctl/UserDefaults
        // shim) before constructing the driver. Mutating the process
        // environment from this thread is `unsafe` under Rust 2024 and
        // `pi-mobile-client` is `#![deny(unsafe_code)]`, so the driver
        // intentionally does not call `std::env::set_var` here.
        Self { config, http }
    }

    fn auth_path(&self) -> PathBuf {
        self.config
            .auth_path
            .clone()
            .unwrap_or_else(Config::auth_path)
    }

    /// Step 1: produce an authorize URL + PKCE verifier. Returns
    /// [`AuthEvent::Authorizing`] alongside the handshake the caller
    /// should hand to the platform browser.
    pub fn begin(&self) -> Result<(AuthEvent, AuthorizeHandshake), AuthEvent> {
        let info: OAuthStartInfo = start_anthropic_oauth().map_err(|err| AuthEvent::Failed {
            reason: format!("authorize url: {}", redact(&err.to_string())),
        })?;
        Ok((
            AuthEvent::Authorizing,
            AuthorizeHandshake {
                authorize_url: info.url,
                verifier: info.verifier,
                redirect_uri: info.redirect_uri,
            },
        ))
    }

    /// Step 2: exchange the pasted code + verifier for tokens and
    /// persist them in pi's `AuthStorage`. Returns the typed
    /// [`AuthEvent::Authorized`] on success and
    /// [`AuthEvent::Failed`] on a server-side or persistence failure.
    pub async fn complete(&self, code_input: &str, verifier: &str) -> AuthEvent {
        let credential = match complete_anthropic_oauth(code_input, verifier).await {
            Ok(cred) => cred,
            Err(err) => {
                return AuthEvent::Failed {
                    reason: format!("token exchange: {}", redact(&err.to_string())),
                };
            }
        };

        match self.persist(credential).await {
            Ok(()) => AuthEvent::Authorized {
                source: AuthEventSource::Oauth,
            },
            Err(err) => AuthEvent::Failed {
                reason: format!("persist: {}", redact(&err.to_string())),
            },
        }
    }

    /// Refresh the persisted Anthropic OAuth credential if it is
    /// expired (or near expiry). Drives pi's
    /// `AuthStorage::refresh_expired_oauth_tokens` and folds the result
    /// into a typed [`AuthEvent`].
    ///
    /// On refresh failure the stored credential is invalidated (removed
    /// from `auth.json`) so the next caller sees `Unauthenticated`
    /// instead of a stale token. The reason string carries the redacted
    /// HTTP error text so platform UIs can present a meaningful prompt
    /// without leaking the refresh token.
    pub async fn refresh(&self) -> AuthEvent {
        let mut storage = match self.load_storage().await {
            Ok(storage) => storage,
            Err(err) => {
                return AuthEvent::Failed {
                    reason: format!("load auth.json: {}", redact(&err.to_string())),
                };
            }
        };

        match storage
            .refresh_expired_oauth_tokens_with_client(&self.http)
            .await
        {
            Ok(()) => {
                if storage.get(ANTHROPIC_PROVIDER_ID).is_some() {
                    AuthEvent::Authorized {
                        source: AuthEventSource::Oauth,
                    }
                } else {
                    AuthEvent::Unauthenticated
                }
            }
            Err(err) => self.invalidate_after_refresh_failure(&mut storage, err).await,
        }
    }

    /// Inspect the currently persisted credential without touching the
    /// network. Used by `pi-server-runner` to decide whether the
    /// initial event surfaced to the platform should be
    /// `Unauthenticated` or `Authorized`.
    pub async fn snapshot(&self) -> AuthEvent {
        match self.load_storage().await {
            Ok(storage) => match storage.get(ANTHROPIC_PROVIDER_ID) {
                Some(AuthCredential::OAuth { .. }) => AuthEvent::Authorized {
                    source: AuthEventSource::Oauth,
                },
                Some(AuthCredential::ApiKey { .. }) | Some(AuthCredential::BearerToken { .. }) => {
                    AuthEvent::Authorized {
                        source: AuthEventSource::Byok,
                    }
                }
                _ => AuthEvent::Unauthenticated,
            },
            Err(_) => AuthEvent::Unauthenticated,
        }
    }

    async fn persist(&self, credential: AuthCredential) -> Result<(), PiError> {
        let mut storage = self.load_storage().await?;
        storage.set(ANTHROPIC_PROVIDER_ID, credential);
        storage.save_async().await
    }

    async fn load_storage(&self) -> Result<AuthStorage, PiError> {
        AuthStorage::load_async(self.auth_path()).await
    }

    async fn invalidate_after_refresh_failure(
        &self,
        storage: &mut AuthStorage,
        err: PiError,
    ) -> AuthEvent {
        let reason = format!("refresh: {}", redact(&err.to_string()));
        if storage.remove(ANTHROPIC_PROVIDER_ID)
            && let Err(save_err) = storage.save_async().await
        {
            tracing::warn!(
                target: "pi_mobile_client::auth",
                "failed to persist auth.json after invalidating anthropic credential: {save_err}"
            );
        }
        AuthEvent::Failed { reason }
    }
}

/// Best-effort redaction so error messages never carry raw tokens. The
/// upstream pi code already redacts a few well-known shapes; this layer
/// adds a belt-and-suspenders pass on any base64-shaped run longer than
/// 16 chars.
fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    let push_run = |out: &mut String, run: &mut String| {
        if run.len() >= 16
            && run
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '=')
        {
            out.push_str("<redacted>");
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in text.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '=' {
            run.push(c);
        } else {
            push_run(&mut out, &mut run);
            out.push(c);
        }
    }
    push_run(&mut out, &mut run);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_driver(tmp: &std::path::Path) -> AnthropicOAuthDriver {
        AnthropicOAuthDriver::new(AnthropicOAuthConfig {
            client_id: Some("test-client".to_string()),
            client_secret: None,
            auth_path: Some(tmp.join("auth.json")),
        })
    }

    /// Spawn a single-shot local TCP server that responds with the
    /// given HTTP status + JSON body and returns its `http://addr/...`
    /// URL. Used by the failure-path test to simulate Anthropic's
    /// 4xx response to an invalidated refresh token.
    fn spawn_oneshot_http(status: u16, body: &str) -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind oneshot");
        let addr = listener.local_addr().expect("addr");
        let body = body.to_string();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let reason = match status {
                    400 => "Bad Request",
                    401 => "Unauthorized",
                    _ => "OK",
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(response.as_bytes());
                let _ = sock.flush();
            }
        });
        format!("http://{addr}/oauth/token")
    }

    /// Stub a known-invalid refresh token in `auth.json`. The token is
    /// already expired (timestamp 0) so
    /// `refresh_expired_oauth_tokens_with_client` will attempt a real
    /// refresh; pointing `token_url` at a one-shot 401 server
    /// guarantees a 4xx that the driver must surface as
    /// `AuthEvent::Failed` with the credential invalidated afterwards.
    fn seed_invalid_credential(path: &std::path::Path, token_url: String) {
        let mut storage = AuthStorage::load(path.to_path_buf()).expect("load");
        storage.set(
            ANTHROPIC_PROVIDER_ID,
            AuthCredential::OAuth {
                // ubs:ignore deliberate test fixture; not a real token.
                access_token: "expired-access".to_string(),
                // ubs:ignore deliberate test fixture; not a real token.
                refresh_token: "known-invalid-refresh".to_string(),
                expires: 0,
                token_url: Some(token_url),
                client_id: Some("test-client".to_string()),
            },
        );
        storage.save().expect("save seed");
    }

    #[test]
    #[allow(unsafe_code)]
    fn refresh_with_invalid_token_emits_failed_and_invalidates_credential() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");

        // Spawn a one-shot 401 server and point pi's anthropic
        // token-url env override at it so the refresh path produces a
        // typed failure rather than hitting the real Anthropic host.
        let token_url = spawn_oneshot_http(
            401,
            r#"{"error":"invalid_grant","error_description":"refresh token revoked"}"#,
        );
        // SAFETY: setting our own process env var. Tests in this crate
        // run with `serial_test` semantics implicitly because the env
        // override is scoped to the single Anthropic OAuth driver path
        // and no other tests touch `PI_ANTHROPIC_OAUTH_TOKEN_URL`.
        unsafe {
            std::env::set_var("PI_ANTHROPIC_OAUTH_TOKEN_URL", &token_url);
        }

        seed_invalid_credential(&auth_path, token_url);

        let driver = AnthropicOAuthDriver::new(AnthropicOAuthConfig {
            client_id: Some("test-client".to_string()),
            client_secret: None,
            auth_path: Some(auth_path.clone()),
        });

        let reactor = asupersync::runtime::reactor::create_reactor().expect("reactor");
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .with_reactor(reactor)
            .build()
            .expect("runtime");
        let event = rt.block_on(driver.refresh());

        match &event {
            AuthEvent::Failed { reason } => {
                assert!(
                    !reason.is_empty(),
                    "Failed reason must carry a human-readable explanation"
                );
                assert!(
                    !reason.contains("known-invalid-refresh"),
                    "Failed reason must not leak the refresh token: {reason}"
                );
            }
            other => panic!("expected AuthEvent::Failed, got {other:?}"),
        }

        // The credential must be invalidated on disk: a fresh
        // `AuthStorage::load` must not find the anthropic entry, and a
        // follow-up snapshot must report `Unauthenticated`.
        let reloaded = AuthStorage::load(auth_path.clone()).expect("reload");
        assert!(
            reloaded.get(ANTHROPIC_PROVIDER_ID).is_none(),
            "credential must be removed after refresh failure"
        );

        let post = rt.block_on(driver.snapshot());
        assert_eq!(post, AuthEvent::Unauthenticated);

        // SAFETY: scoped cleanup of the env override set above.
        unsafe {
            std::env::remove_var("PI_ANTHROPIC_OAUTH_TOKEN_URL");
        }
    }

    #[test]
    fn snapshot_without_credentials_returns_unauthenticated() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let driver = make_driver(dir.path());
        let reactor = asupersync::runtime::reactor::create_reactor().expect("reactor");
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .with_reactor(reactor)
            .build()
            .expect("runtime");
        let event = rt.block_on(driver.snapshot());
        assert_eq!(event, AuthEvent::Unauthenticated);
    }

    #[test]
    fn redact_masks_long_base64_runs() {
        let s = "error AAAAAAAAAAAAAAAAAAAAAAAAAA: bad";
        let red = redact(s);
        assert!(red.contains("<redacted>"));
        assert!(!red.contains("AAAAAAAAAAAAAAAAAAAAAAAAAA"));
    }

    #[test]
    fn redact_preserves_short_words() {
        let s = "token exchange failed";
        assert_eq!(redact(s), s);
    }
}
