//! Typed UniFFI surface for the Anthropic authentication state that
//! `pi-mobile-client::auth::anthropic_oauth` produces.
//!
//! Keeping this enum here (under `codex-mobile-client::store`) instead
//! of in `pi-mobile-client` follows the project rule that the single
//! public UniFFI surface lives in `codex-mobile-client`. Platform
//! bridges observe this enum directly; reducer/store code can fold it
//! into snapshots without re-parsing wire strings.
//!
//! The enum mirrors the driver's internal [`AuthEvent`] exactly. The
//! `From` impl below is the only translation point and lives in this
//! crate so `pi-mobile-client` does not need to know about UniFFI.

use pi_mobile_client::auth::{AuthEvent, AuthEventSource};

/// Which credential source produced the currently-authorized session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum AuthSource {
    /// Token obtained via Anthropic OAuth (Authorization Code + PKCE).
    Oauth,
    /// User-supplied API key plumbed through `pi_byok_set`.
    Byok,
}

/// Typed authentication state surfaced to the platform layer.
///
/// Reducer code converts the driver's [`AuthEvent`] into this enum via
/// [`From`] and emits it as part of the store snapshot stream so that
/// iOS / Android bridges can update UI without re-parsing wire strings.
///
/// The `refresh_token` field on `Authorized` carries the raw OAuth
/// refresh token only on the same call that minted (or imported) it
/// via PKCE — platform completers mirror it into the iOS Keychain /
/// Android EncryptedSharedPreferences. Snapshots and BYOK sources
/// surface `None` so the token never leaves the Rust `auth.json`
/// boundary except on the explicit completion call. The `Debug`
/// implementation below redacts the field; never replace it with a
/// `#[derive(Debug)]` without preserving that redaction.
#[derive(Clone, PartialEq, Eq, uniffi::Enum)]
pub enum AuthState {
    /// No credential present.
    Unauthenticated,
    /// PKCE handshake is in flight (authorize URL surfaced to the
    /// platform; waiting for the user to complete the browser flow).
    Authorizing,
    /// A live credential is present.
    Authorized {
        source: AuthSource,
        /// Raw OAuth refresh token. `Some` only on the in-process
        /// `Authorized{Oauth}` event produced by
        /// `pi_anthropic_oauth_complete` or claude-credentials import;
        /// all other paths (snapshot reads, BYOK, refresh) surface
        /// `None` so platform code never re-exfiltrates a persisted
        /// token from `auth.json` across the FFI boundary.
        refresh_token: Option<String>,
    },
    /// The most recent auth attempt failed. `reason` is a redacted,
    /// human-readable explanation. Carrying the reason inline keeps
    /// platforms from having to drill into a separate error channel.
    Failed { reason: String },
}

impl std::fmt::Debug for AuthState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthState::Unauthenticated => f.write_str("Unauthenticated"),
            AuthState::Authorizing => f.write_str("Authorizing"),
            AuthState::Authorized {
                source,
                refresh_token,
            } => f
                .debug_struct("Authorized")
                .field("source", source)
                .field(
                    "refresh_token",
                    &refresh_token.as_ref().map(|_| "<redacted>"),
                )
                .finish(),
            AuthState::Failed { reason } => {
                f.debug_struct("Failed").field("reason", reason).finish()
            }
        }
    }
}

impl From<AuthEventSource> for AuthSource {
    fn from(value: AuthEventSource) -> Self {
        match value {
            AuthEventSource::Oauth => AuthSource::Oauth,
            AuthEventSource::Byok => AuthSource::Byok,
        }
    }
}

impl From<AuthEvent> for AuthState {
    fn from(value: AuthEvent) -> Self {
        match value {
            AuthEvent::Unauthenticated => AuthState::Unauthenticated,
            AuthEvent::Authorizing => AuthState::Authorizing,
            AuthEvent::Authorized {
                source,
                refresh_token,
            } => AuthState::Authorized {
                source: source.into(),
                refresh_token,
            },
            AuthEvent::Failed { reason } => AuthState::Failed { reason },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_each_variant() {
        assert_eq!(
            AuthState::from(AuthEvent::Unauthenticated),
            AuthState::Unauthenticated
        );
        assert_eq!(
            AuthState::from(AuthEvent::Authorizing),
            AuthState::Authorizing
        );
        assert_eq!(
            AuthState::from(AuthEvent::Authorized {
                source: AuthEventSource::Oauth,
                refresh_token: Some("rt-abc".to_string()),
            }),
            AuthState::Authorized {
                source: AuthSource::Oauth,
                refresh_token: Some("rt-abc".to_string()),
            }
        );
        assert_eq!(
            AuthState::from(AuthEvent::Authorized {
                source: AuthEventSource::Byok,
                refresh_token: None,
            }),
            AuthState::Authorized {
                source: AuthSource::Byok,
                refresh_token: None,
            }
        );
    }

    #[test]
    fn debug_redacts_refresh_token() {
        let state = AuthState::Authorized {
            source: AuthSource::Oauth,
            refresh_token: Some("super-secret-refresh-token-value".to_string()),
        };
        let rendered = format!("{state:?}");
        assert!(
            !rendered.contains("super-secret-refresh-token-value"),
            "Debug must not leak refresh token: {rendered}"
        );
        assert!(
            rendered.contains("<redacted>"),
            "Debug should mark the refresh token as redacted: {rendered}"
        );
        assert_eq!(
            AuthState::from(AuthEvent::Failed {
                reason: "bad refresh".to_string(),
            }),
            AuthState::Failed {
                reason: "bad refresh".to_string(),
            }
        );
    }
}
