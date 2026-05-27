//! VAL-NFR-006: the pi runtime must never emit Codex-style `PlanUpdate`
//! events on its outbound stream. Pi's capability manifest sets
//! `plans = false`, and the in-process server's `PiEvent` enum has no
//! plan-update variant — the cross-runtime planning UI is intentionally
//! a codex-only affordance.
//!
//! This integration test pins both invariants:
//!
//!   1. Source-level: the `PiEvent` enum has no variant whose name
//!      contains `Plan`, so no future contributor can add one without
//!      breaking this gate. The check inspects the crate's `server.rs`
//!      so the test fails closed if the enum grows a plan variant.
//!
//!   2. Runtime: a scripted in-process pi session (no network, echo
//!      bridge) produces a transcript whose `event` field never reads
//!      `plan_update` / `turn_plan_updated` and whose decoded
//!      `PiEvent` stream contains no `Debug` representation matching
//!      a plan event.
//!
//! Run with: `cargo test -p pi-server-runner --test capability_plans -- --nocapture`.

use pi_mobile_client::{Command, InProcessStartArgs, PiEvent, start_in_process};
use std::path::PathBuf;
use std::time::Duration;

/// Source-level gate: the `PiEvent` enum in
/// `shared/rust-bridge/pi-mobile-client/src/server.rs` must not
/// declare a variant whose name contains `Plan`. We grep the source
/// file directly so this fails closed if a future contributor adds a
/// `PlanUpdate` / `TurnPlanUpdated` / `PlanCard` variant.
#[test]
fn pi_event_enum_has_no_plan_variant_in_source() {
    // The integration test runs from the crate root
    // (`shared/rust-bridge/pi-server-runner`), so the relative path
    // points back up at the sibling crate.
    let server_rs: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("pi-mobile-client")
        .join("src")
        .join("server.rs");
    assert!(
        server_rs.exists(),
        "expected to find pi-mobile-client server.rs at {}",
        server_rs.display()
    );
    let source =
        std::fs::read_to_string(&server_rs).expect("read pi-mobile-client server.rs");

    // Find the `pub enum PiEvent { ... }` block and inspect just the
    // variant names. Looking for `Plan` anywhere in `server.rs` would
    // match doc-comments referring to codex's plan UI; we want the
    // variants only.
    let enum_start = source
        .find("pub enum PiEvent")
        .expect("PiEvent enum block must exist in server.rs");
    let after_open = source[enum_start..]
        .find('{')
        .expect("PiEvent enum must have an opening brace");
    let body_start = enum_start + after_open + 1;

    // Walk braces to find the matching closing brace for the enum.
    let mut depth = 1usize;
    let mut end = body_start;
    for (offset, ch) in source[body_start..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = body_start + offset;
                    break;
                }
            }
            _ => {}
        }
    }
    let body = &source[body_start..end];

    // Strip comments (line and block) and inspect what remains. If a
    // variant name appears it must not contain `Plan`.
    let mut stripped = String::with_capacity(body.len());
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if in_line_comment {
            if c == '\n' {
                in_line_comment = false;
                stripped.push(c);
            }
            continue;
        }
        if in_block_comment {
            if c == '*' && chars.peek() == Some(&'/') {
                chars.next();
                in_block_comment = false;
            }
            continue;
        }
        if c == '/' {
            match chars.peek() {
                Some('/') => {
                    in_line_comment = true;
                    chars.next();
                    continue;
                }
                Some('*') => {
                    in_block_comment = true;
                    chars.next();
                    continue;
                }
                _ => {}
            }
        }
        stripped.push(c);
    }

    assert!(
        !stripped.contains("Plan"),
        "PiEvent enum body must not declare a Plan-named variant, but server.rs body contains:\n{stripped}"
    );
}

/// Runtime gate: drive a scripted in-process pi session through the
/// echo-mode bridge (no `PiSessionConfig` -> the bridge replies with a
/// single `PromptReceived` plus the shutdown event) and assert no event
/// in the stream is plan-shaped.
///
/// We do not need a real Anthropic/OpenAI key for this gate — the
/// contract is that *no* plan event ever flows on the pi outbound
/// stream, which is structurally true regardless of provider. Echo
/// mode is the deterministic, no-network path that always emits at
/// least one event so the assertion has something to look at.
#[tokio::test]
async fn scripted_pi_run_emits_no_plan_events() {
    let handle = start_in_process(InProcessStartArgs {
        command_buffer: Some(8),
        event_buffer: Some(64),
        session: None, // echo-mode bridge: no network, deterministic
    });
    let mut events = handle.subscribe();

    handle
        .send(Command::Prompt("ping".to_string()))
        .expect("send prompt");

    let mut observed = Vec::new();
    // Collect events for up to 2 seconds, then look for a terminal
    // event. The echo bridge emits `PromptReceived { text: "ping" }`
    // immediately; the rest of the stream stays empty until the
    // handle drops.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Ok(event)) => {
                observed.push(event);
                // For echo mode we only expect a single PromptReceived;
                // break early once we have it so the test runs fast.
                if matches!(observed.last(), Some(PiEvent::PromptReceived { .. })) {
                    break;
                }
            }
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }

    assert!(
        !observed.is_empty(),
        "expected at least one PiEvent from echo-mode bridge, got none"
    );

    for event in &observed {
        let debug = format!("{event:?}");
        assert!(
            !debug.to_ascii_lowercase().contains("plan"),
            "pi outbound stream produced an event whose debug repr mentions plan: {debug}"
        );
        // Pattern-level guard: ensure the variant is one of the
        // known non-plan variants. This is structural — adding a new
        // PiEvent variant in the future is fine, but it must not be
        // plan-shaped.
        match event {
            PiEvent::PromptReceived { .. }
            | PiEvent::AssistantTextDelta { .. }
            | PiEvent::AssistantText { .. }
            | PiEvent::ToolExecStart { .. }
            | PiEvent::ToolExecEnd { .. }
            | PiEvent::TurnComplete
            | PiEvent::TurnError { .. }
            | PiEvent::TurnStateChanged { .. }
            | PiEvent::ShuttingDown => {}
        }
    }
    drop(handle);
}

/// Belt-and-suspenders: a real provider-config session would still go
/// through the same `runtime_bridge::drive`, which forwards only the
/// subset of `pi-agent-rust` events the bridge knows how to map. None
/// of those forwarded mappings produces a plan event, but to lock that
/// down at the test boundary, we also confirm the `PiSessionConfig`
/// itself carries no plan-enabled toggle.
#[test]
fn pi_session_config_has_no_plans_toggle() {
    // The mere fact this compiles + the struct deserializes is enough
    // — there is no `plans` field on `PiSessionConfig`. We assert at
    // the source level so a future contributor adding such a field
    // trips this gate.
    let server_rs: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("pi-mobile-client")
        .join("src")
        .join("server.rs");
    let source =
        std::fs::read_to_string(&server_rs).expect("read pi-mobile-client server.rs");
    // Quick structural check: PiSessionConfig must not declare a
    // `plans` field. Comments are allowed to mention plans (they
    // discuss the capability manifest); we look for an actual struct
    // field declaration.
    let cfg_start = source
        .find("pub struct PiSessionConfig")
        .expect("PiSessionConfig struct must exist");
    let after_open = source[cfg_start..]
        .find('{')
        .expect("PiSessionConfig must have an opening brace");
    let body_start = cfg_start + after_open + 1;
    let mut depth = 1usize;
    let mut end = body_start;
    for (offset, ch) in source[body_start..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = body_start + offset;
                    break;
                }
            }
            _ => {}
        }
    }
    let body = &source[body_start..end];
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("//") || trimmed.starts_with("///") || trimmed.is_empty() {
            continue;
        }
        // Drop trailing comments before the field check.
        let code = trimmed.split("//").next().unwrap_or("").trim();
        assert!(
            !code.starts_with("pub plans") && !code.starts_with("plans:"),
            "PiSessionConfig must not carry a `plans` field; offending line: {line}"
        );
    }
}
