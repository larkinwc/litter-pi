//! UniFFI wrapper around `pi_mobile_client::auth::anthropic_oauth`.
//!
//! Exposes three free functions to Swift/Kotlin so the iOS
//! `AnthropicOAuthSheet` and the Android `AnthropicOAuthScreen` can
//! drive the canonical Rust OAuth driver instead of falling back to
//! stub Default*Provider implementations.
//!
//! The wrapper layer keeps the Rust-owned PKCE verifier behind the
//! single UniFFI surface in `codex-mobile-client`: `pi-mobile-client`
//! does not depend on UniFFI directly. Reconciliation policy (token
//! refresh cadence, BYOK vs OAuth precedence, etc.) still lives in
//! the reducer/store; this module is intentionally thin.
//!
//! Because the underlying driver methods are `async` and run on the
//! `asupersync` reactor (not on tokio), each UniFFI free function
//! dispatches through the existing `blocking::*` helpers in
//! `pi-mobile-client::auth`. The PKCE verifier produced by `begin`
//! is held in a process-global `Mutex<Option<String>>` so `complete`
//! can recover it without round-tripping the secret through Swift /
//! Kotlin.

use std::path::PathBuf;
use std::sync::Mutex;

use pi_mobile_client::auth::{
    AnthropicOAuthConfig as PiAnthropicOAuthConfig, AnthropicOAuthDriver, AuthEvent,
    complete_anthropic_oauth_paste, snapshot_anthropic_oauth,
};

use crate::store::AuthState;

/// Platform-supplied configuration for the Anthropic OAuth driver.
///
/// All fields are optional because the underlying driver already
/// honours pi's env-var fall-backs (`PI_ANTHROPIC_OAUTH_CLIENT_ID`,
/// etc.). Crossing the FFI as a `Record` keeps the surface narrow:
/// platforms supply only what they own (Keychain / EncryptedSharedPrefs
/// values) and let Rust resolve everything else.
#[derive(Debug, Clone, Default, uniffi::Record)]
pub struct AnthropicOAuthConfig {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    /// Override for the `auth.json` path. When `None`, the driver
    /// resolves it via pi's `Config::auth_path()`.
    pub auth_path: Option<String>,
}

impl AnthropicOAuthConfig {
    fn into_driver_config(self) -> PiAnthropicOAuthConfig {
        PiAnthropicOAuthConfig {
            client_id: self.client_id,
            client_secret: self.client_secret,
            auth_path: self.auth_path.map(PathBuf::from),
        }
    }
}

/// Errors surfaced from the Anthropic OAuth UniFFI wrappers. The
/// driver itself only emits a typed [`AuthEvent::Failed { reason }`]
/// terminal state; this enum wraps it for the `begin`/`complete`
/// entry points so Swift/Kotlin observe a typed error rather than
/// a `String`.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum AuthError {
    /// The underlying driver could not produce an authorize URL or
    /// could not exchange the pasted code for tokens. `reason` is
    /// already redacted (no tokens, no secrets).
    #[error("anthropic oauth: {reason}")]
    Driver { reason: String },
    /// `pi_anthropic_oauth_complete` was called before `begin`. The
    /// platform must drive the flow in order.
    #[error("anthropic oauth: no in-flight PKCE handshake")]
    MissingVerifier,
}

/// Process-global slot for the PKCE verifier produced by the most
/// recent `pi_anthropic_oauth_begin`. The verifier is only used to
/// complete the same handshake; it is one-shot and is cleared as soon
/// as `complete` consumes it (success or failure).
static PENDING_VERIFIER: Mutex<Option<String>> = Mutex::new(None);

fn store_verifier(verifier: String) {
    if let Ok(mut guard) = PENDING_VERIFIER.lock() {
        *guard = Some(verifier);
    }
}

fn take_verifier() -> Option<String> {
    PENDING_VERIFIER.lock().ok().and_then(|mut g| g.take())
}

/// Begin an Anthropic OAuth Authorization Code + PKCE handshake.
///
/// Returns the authorize URL the platform must open in a system
/// browser (`ASWebAuthenticationSession` on iOS, Chrome Custom Tabs
/// on Android). The PKCE verifier is held inside the Rust crate so
/// platform code never has to ferry the secret back and forth.
#[uniffi::export]
pub fn pi_anthropic_oauth_begin(config: AnthropicOAuthConfig) -> Result<String, AuthError> {
    let driver_config = config.into_driver_config();
    let driver = AnthropicOAuthDriver::new(driver_config);
    match driver.begin() {
        Ok((_event, handshake)) => {
            store_verifier(handshake.verifier);
            Ok(handshake.authorize_url)
        }
        Err(AuthEvent::Failed { reason }) => Err(AuthError::Driver { reason }),
        Err(other) => Err(AuthError::Driver {
            reason: format!("unexpected driver event: {other:?}"),
        }),
    }
}

/// Complete an in-flight handshake by exchanging the redirected
/// authorization `code` for tokens. Persists the OAuth credential into
/// pi's `auth.json` via `AuthStorage` and clears the in-memory PKCE
/// verifier regardless of success.
#[uniffi::export]
pub fn pi_anthropic_oauth_complete(
    config: AnthropicOAuthConfig,
    code: String,
) -> Result<AuthState, AuthError> {
    let verifier = take_verifier().ok_or(AuthError::MissingVerifier)?;
    let driver_config = config.into_driver_config();
    let event = complete_anthropic_oauth_paste(driver_config, code, verifier);
    match event {
        AuthEvent::Failed { reason } => Err(AuthError::Driver { reason }),
        other => Ok(AuthState::from(other)),
    }
}

/// Inspect the persisted Anthropic credential without touching the
/// network. Returns [`AuthState::Unauthenticated`] when no credential
/// is present or the `auth.json` is missing.
#[uniffi::export]
pub fn pi_anthropic_oauth_snapshot(config: AnthropicOAuthConfig) -> AuthState {
    let driver_config = config.into_driver_config();
    let event = snapshot_anthropic_oauth(driver_config);
    AuthState::from(event)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clear_verifier() {
        if let Ok(mut g) = PENDING_VERIFIER.lock() {
            *g = None;
        }
    }

    #[test]
    fn snapshot_returns_unauthenticated_for_empty_store() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let state = pi_anthropic_oauth_snapshot(AnthropicOAuthConfig {
            client_id: Some("test-client".to_string()),
            client_secret: None,
            auth_path: Some(dir.path().join("auth.json").to_string_lossy().to_string()),
        });
        assert_eq!(state, AuthState::Unauthenticated);
    }

    #[test]
    fn begin_stores_verifier_and_returns_url() {
        clear_verifier();
        let dir = tempfile::tempdir().expect("tmpdir");
        let url = pi_anthropic_oauth_begin(AnthropicOAuthConfig {
            client_id: Some("test-client".to_string()),
            client_secret: None,
            auth_path: Some(dir.path().join("auth.json").to_string_lossy().to_string()),
        })
        .expect("begin succeeds");
        assert!(
            url.contains("oauth/authorize") || url.contains("authorize"),
            "authorize URL should be an Anthropic-style authorize endpoint: {url}"
        );
        // The verifier should now be present in the in-process slot.
        let stashed = PENDING_VERIFIER.lock().unwrap().clone();
        assert!(
            stashed.is_some(),
            "begin() must stash the PKCE verifier for the matching complete()"
        );
        clear_verifier();
    }

    #[test]
    fn complete_without_begin_returns_missing_verifier() {
        clear_verifier();
        let dir = tempfile::tempdir().expect("tmpdir");
        let err = pi_anthropic_oauth_complete(
            AnthropicOAuthConfig {
                client_id: Some("test-client".to_string()),
                client_secret: None,
                auth_path: Some(dir.path().join("auth.json").to_string_lossy().to_string()),
            },
            "code-from-redirect".to_string(),
        )
        .expect_err("complete without begin must fail");
        assert!(
            matches!(err, AuthError::MissingVerifier),
            "expected MissingVerifier, got {err:?}"
        );
    }
}
