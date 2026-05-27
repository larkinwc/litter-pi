//! Bring-Your-Own-Key (BYOK) credential plumbing.
//!
//! The `pi_byok_set` entry point writes a caller-supplied API key into
//! pi's `auth.json` for a given provider. Subsequent runs of the in-
//! process pi runtime (and of `pi-server-runner` in default mode) then
//! pick the key up via pi's normal credential resolution path, so
//! callers do not have to keep re-passing the key on every prompt.
//!
//! This module is the BYOK counterpart of [`super::anthropic_oauth`]
//! and intentionally surfaces the same typed [`AuthEvent`] enum so the
//! `codex-mobile-client` store reducer can fold both sources into the
//! single [`AuthState`](codex_mobile_client::store::AuthState) UniFFI
//! enum.

use std::path::PathBuf;

use pi::auth::{AuthCredential, AuthStorage};
use pi::config::Config;

use super::anthropic_oauth::{AuthEvent, AuthEventSource};

/// Where the persisted credential should land. `None` resolves to pi's
/// default `Config::auth_path()` (which on mobile honours
/// `PI_CODING_AGENT_DIR`).
#[derive(Debug, Clone, Default)]
pub struct ByokConfig {
    pub auth_path: Option<PathBuf>,
}

impl ByokConfig {
    fn resolve_auth_path(&self) -> PathBuf {
        self.auth_path
            .clone()
            .unwrap_or_else(Config::auth_path)
    }
}

/// Persist a BYOK API key for the supplied pi provider id.
///
/// Returns [`AuthEvent::Authorized`] with [`AuthEventSource::Byok`] on
/// success and [`AuthEvent::Failed`] with a redacted reason on a load
/// or persistence failure. The key itself is never echoed in the
/// failure reason.
pub async fn pi_byok_set(
    config: &ByokConfig,
    provider: impl Into<String>,
    api_key: impl Into<String>,
) -> AuthEvent {
    let provider = provider.into();
    let api_key = api_key.into();
    let trimmed_provider = provider.trim();
    let trimmed_key = api_key.trim();
    if trimmed_provider.is_empty() {
        return AuthEvent::Failed {
            reason: "byok: provider id was empty".to_string(),
        };
    }
    if trimmed_key.is_empty() {
        return AuthEvent::Failed {
            reason: "byok: api key was empty".to_string(),
        };
    }

    let path = config.resolve_auth_path();
    let mut storage = match AuthStorage::load_async(path).await {
        Ok(storage) => storage,
        Err(err) => {
            return AuthEvent::Failed {
                reason: format!("byok load auth.json: {}", redact_error(&err.to_string())),
            };
        }
    };

    storage.set(
        trimmed_provider.to_string(),
        AuthCredential::ApiKey {
            key: trimmed_key.to_string(),
        },
    );

    if let Err(err) = storage.save_async().await {
        return AuthEvent::Failed {
            reason: format!("byok persist auth.json: {}", redact_error(&err.to_string())),
        };
    }

    AuthEvent::Authorized {
        source: AuthEventSource::Byok,
        refresh_token: None,
    }
}

/// Belt-and-suspenders redaction for failure strings. Mirrors the
/// helper used by the OAuth driver so neither path leaks long base64
/// runs (which would typically be tokens or keys) into transcripts.
fn redact_error(text: &str) -> String {
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

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        let reactor = asupersync::runtime::reactor::create_reactor().expect("reactor");
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .with_reactor(reactor)
            .build()
            .expect("runtime");
        rt.block_on(future)
    }

    #[test]
    fn pi_byok_set_persists_api_key_for_provider() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("auth.json");
        let cfg = ByokConfig {
            auth_path: Some(path.clone()),
        };

        let event = block_on(pi_byok_set(&cfg, "anthropic", "sk-ant-test-key-value"));
        assert_eq!(
            event,
            AuthEvent::Authorized {
                source: AuthEventSource::Byok,
                refresh_token: None,
            }
        );

        let storage = AuthStorage::load(path).expect("reload auth.json");
        match storage.get("anthropic") {
            Some(AuthCredential::ApiKey { key }) => {
                assert_eq!(key, "sk-ant-test-key-value");
            }
            other => panic!("expected ApiKey credential, got {other:?}"),
        }
    }

    #[test]
    fn pi_byok_set_overwrites_existing_provider_entry() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("auth.json");
        let cfg = ByokConfig {
            auth_path: Some(path.clone()),
        };
        let _ = block_on(pi_byok_set(&cfg, "openai", "sk-first"));
        let _ = block_on(pi_byok_set(&cfg, "openai", "sk-second"));

        let storage = AuthStorage::load(path).expect("reload auth.json");
        match storage.get("openai") {
            Some(AuthCredential::ApiKey { key }) => assert_eq!(key, "sk-second"),
            other => panic!("expected overwritten ApiKey credential, got {other:?}"),
        }
    }

    #[test]
    fn pi_byok_set_rejects_empty_inputs() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let cfg = ByokConfig {
            auth_path: Some(dir.path().join("auth.json")),
        };
        match block_on(pi_byok_set(&cfg, "  ", "key")) {
            AuthEvent::Failed { reason } => assert!(reason.contains("provider")),
            other => panic!("expected Failed, got {other:?}"),
        }
        match block_on(pi_byok_set(&cfg, "anthropic", "  ")) {
            AuthEvent::Failed { reason } => assert!(reason.contains("api key")),
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}
