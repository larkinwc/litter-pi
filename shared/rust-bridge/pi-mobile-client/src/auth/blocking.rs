//! Thread-backed blocking wrappers around the async auth driver
//! surface.
//!
//! The auth drivers in this module are inherently `async` because pi's
//! `AuthStorage` and HTTP client run on the `asupersync` runtime.
//! Callers that live on a tokio runtime (or no runtime at all, like
//! `pi-server-runner`) would otherwise have to depend on `asupersync`
//! directly to drive these futures, leaking an implementation detail
//! that the crate intentionally keeps internal.
//!
//! Each helper here spins up a short-lived `asupersync` current-thread
//! runtime on a fresh OS thread, drives the requested future to
//! completion, and returns the result on the caller's thread. Threads
//! are joined before the helper returns so there is no background work
//! after the call site sees the value.

use std::path::PathBuf;
use std::thread;

use super::anthropic_oauth::{AnthropicOAuthConfig, AnthropicOAuthDriver, AuthEvent};
use super::byok::{ByokConfig, pi_byok_set};

fn run_on_asupersync<F, T>(future: F) -> T
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    thread::Builder::new()
        .name("pi-mobile-client-auth".to_string())
        .spawn(move || {
            let reactor = asupersync::runtime::reactor::create_reactor()
                .expect("create asupersync reactor for auth driver");
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .with_reactor(reactor)
                .build()
                .expect("build asupersync runtime for auth driver");
            runtime.block_on(future)
        })
        .expect("spawn pi-mobile-client-auth thread")
        .join()
        .expect("pi-mobile-client-auth thread joined cleanly")
}

/// Synchronously run [`AnthropicOAuthDriver::snapshot`].
pub fn snapshot_anthropic_oauth(config: AnthropicOAuthConfig) -> AuthEvent {
    run_on_asupersync(async move {
        let driver = AnthropicOAuthDriver::new(config);
        driver.snapshot().await
    })
}

/// Synchronously run [`AnthropicOAuthDriver::complete`] for a pasted
/// authorization code + verifier pair.
pub fn complete_anthropic_oauth_paste(
    config: AnthropicOAuthConfig,
    code_input: String,
    verifier: String,
) -> AuthEvent {
    run_on_asupersync(async move {
        let driver = AnthropicOAuthDriver::new(config);
        driver.complete(&code_input, &verifier).await
    })
}

/// Synchronously run [`AnthropicOAuthDriver::refresh`].
pub fn refresh_anthropic_oauth(config: AnthropicOAuthConfig) -> AuthEvent {
    run_on_asupersync(async move {
        let driver = AnthropicOAuthDriver::new(config);
        driver.refresh().await
    })
}

/// Synchronously run [`pi_byok_set`] for the supplied provider id.
pub fn pi_byok_set_blocking(
    auth_path: Option<PathBuf>,
    provider: String,
    api_key: String,
) -> AuthEvent {
    run_on_asupersync(async move {
        let cfg = ByokConfig { auth_path };
        pi_byok_set(&cfg, provider, api_key).await
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi::auth::{AuthCredential, AuthStorage};

    #[test]
    fn snapshot_returns_unauthenticated_for_empty_store() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let event = snapshot_anthropic_oauth(AnthropicOAuthConfig {
            client_id: Some("test-client".to_string()),
            client_secret: None,
            auth_path: Some(dir.path().join("auth.json")),
        });
        assert_eq!(event, AuthEvent::Unauthenticated);
    }

    #[test]
    fn pi_byok_set_blocking_persists_and_snapshots_authorized() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let set_event = pi_byok_set_blocking(
            Some(auth_path.clone()),
            "anthropic".to_string(),
            "sk-ant-blocking-test".to_string(),
        );
        assert!(matches!(set_event, AuthEvent::Authorized { .. }));

        // Persisted on disk.
        let storage = AuthStorage::load(auth_path.clone()).expect("reload");
        match storage.get("anthropic") {
            Some(AuthCredential::ApiKey { key }) => {
                assert_eq!(key, "sk-ant-blocking-test");
            }
            other => panic!("unexpected credential: {other:?}"),
        }

        // Snapshot observes the BYOK credential.
        let snapshot = snapshot_anthropic_oauth(AnthropicOAuthConfig {
            client_id: Some("test-client".to_string()),
            client_secret: None,
            auth_path: Some(auth_path),
        });
        assert!(matches!(snapshot, AuthEvent::Authorized { .. }));
    }
}
