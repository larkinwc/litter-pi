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
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum AuthState {
    /// No credential present.
    Unauthenticated,
    /// PKCE handshake is in flight (authorize URL surfaced to the
    /// platform; waiting for the user to complete the browser flow).
    Authorizing,
    /// A live credential is present.
    Authorized { source: AuthSource },
    /// The most recent auth attempt failed. `reason` is a redacted,
    /// human-readable explanation. Carrying the reason inline keeps
    /// platforms from having to drill into a separate error channel.
    Failed { reason: String },
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
            AuthEvent::Authorized { source } => AuthState::Authorized {
                source: source.into(),
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
            }),
            AuthState::Authorized {
                source: AuthSource::Oauth,
            }
        );
        assert_eq!(
            AuthState::from(AuthEvent::Authorized {
                source: AuthEventSource::Byok,
            }),
            AuthState::Authorized {
                source: AuthSource::Byok,
            }
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
