//! Tests for `PiReconnectTransport` session-config reuse on forced
//! reconnect (VAL-REM-003).
//!
//! `PiReconnectTransport` lives in `session::connection` next to the
//! other transport adapters; this module exists so the validation
//! contract's `cargo test -p codex-mobile-client ssh::pi_reconnect`
//! filter matches the reuse-on-reconnect assertion.

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use codex_app_server_client::{AppServerClient, RemoteAppServerConnectArgs};

    use crate::session::connection::{PiBootstrapFn, PiReconnectTransport};
    use crate::session::remote_transport::RemoteTransport;
    use crate::ssh::pi_bootstrap::SshSessionConfig;
    use crate::transport::TransportError;

    struct RecordingBootstrap {
        observed: Arc<Mutex<Vec<SshSessionConfig>>>,
    }

    #[async_trait]
    impl PiBootstrapFn for RecordingBootstrap {
        async fn run(
            &self,
            config: &SshSessionConfig,
        ) -> Result<AppServerClient, TransportError> {
            self.observed
                .lock()
                .expect("recording bootstrap mutex")
                .push(config.clone());
            // Production bootstrap returns a real `AppServerClient`;
            // the reuse assertion only inspects the recorded configs,
            // so simulate a benign failure that still lets `connect()`
            // and `reconnect()` complete their bookkeeping.
            Err(TransportError::ConnectionFailed(
                "stub bootstrap: no real ssh stack".to_string(),
            ))
        }
    }

    fn args() -> RemoteAppServerConnectArgs {
        use codex_app_server_client::RemoteAppServerEndpoint;
        RemoteAppServerConnectArgs {
            endpoint: RemoteAppServerEndpoint::WebSocket {
                websocket_url: "ws://pi-acp-proxy.localhost/rpc".to_string(),
                auth_token: None,
            },
            client_name: "LitterTest".into(),
            client_version: "0".into(),
            experimental_api: true,
            opt_out_notification_methods: Vec::new(),
            channel_capacity: 16,
        }
    }

    fn sample_config() -> SshSessionConfig {
        SshSessionConfig {
            host: "pi.local".to_string(),
            port: 2222,
            username: "pi".to_string(),
            key_fingerprint: Some("SHA256:reused".to_string()),
            keepalive_interval: Duration::from_secs(15),
        }
    }

    #[tokio::test]
    async fn reuses_session_config() {
        let observed: Arc<Mutex<Vec<SshSessionConfig>>> = Arc::new(Mutex::new(Vec::new()));
        let bootstrap: Arc<dyn PiBootstrapFn> = Arc::new(RecordingBootstrap {
            observed: Arc::clone(&observed),
        });

        let host_config = sample_config();
        // Initial connect: bootstrap is invoked once with the original
        // session config. The stub bootstrap returns an error so we
        // build the transport manually around the recording arc; this
        // keeps the test free of a real `AppServerClient`.
        let initial = PiReconnectTransport::connect_with(host_config.clone(), Arc::clone(&bootstrap))
            .await;
        assert!(
            initial.is_err(),
            "stub bootstrap should surface its error on initial connect"
        );
        drop(initial);

        let transport = PiReconnectTransport {
            host_config: host_config.clone(),
            bootstrap: Arc::clone(&bootstrap),
        };

        // Forced drop / reconnect: the transport must reuse the same
        // `SshSessionConfig` it was constructed with.
        let _ = transport
            .reconnect(&args(), "ws://pi-acp-proxy.localhost/rpc")
            .await;

        let recorded = observed.lock().expect("recording bootstrap mutex").clone();
        println!("pi_reconnect recorded {} bootstrap calls", recorded.len());
        for (idx, cfg) in recorded.iter().enumerate() {
            println!(
                "  [{idx}] host={} port={} user={} fp={:?} keepalive={:?}",
                cfg.host, cfg.port, cfg.username, cfg.key_fingerprint, cfg.keepalive_interval,
            );
        }
        assert_eq!(
            recorded.len(),
            2,
            "expected exactly two bootstrap invocations (initial + reconnect)"
        );
        assert_eq!(
            recorded[0], host_config,
            "initial connect must use the original SshSessionConfig"
        );
        assert_eq!(
            recorded[1], host_config,
            "reconnect must reuse the original SshSessionConfig verbatim"
        );
        assert_eq!(
            recorded[0], recorded[1],
            "initial and reconnect SshSessionConfig must be value-equal"
        );
    }
}
