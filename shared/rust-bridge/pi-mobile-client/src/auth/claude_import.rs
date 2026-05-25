//! Import Anthropic OAuth credentials from a Claude Code-shaped JSON
//! file into pi's `auth.json`.
//!
//! Claude Code stores Anthropic OAuth tokens locally as
//! ```json
//! {
//!   "access_token": "sk-ant-oat01-...",
//!   "refresh_token": "sk-ant-ort01-...",
//!   "expired": "2026-05-26T05:16:09+08:00",
//!   "last_refresh": "...",
//!   "email": "...", "type": "claude"
//! }
//! ```
//!
//! This helper parses that JSON, converts the ISO 8601 `expired`
//! timestamp into Unix milliseconds, stamps pi's anthropic OAuth
//! `client_id` + `token_url` so `AuthStorage::refresh_expired_oauth_tokens`
//! can run self-contained, and writes the result under the
//! `anthropic` provider key in pi's `auth.json` via the shared
//! [`AuthStorage`] file-locking path. Existing non-anthropic entries
//! are preserved.

use std::path::PathBuf;

use pi::auth::{AuthCredential, AuthStorage};
use pi::config::Config;
use serde_json::Value;

use super::anthropic_oauth::{AuthEvent, AuthEventSource, ANTHROPIC_PROVIDER_ID};

/// Pi's stock Anthropic OAuth client id. Mirrors the constant in
/// `pi::auth` (kept private upstream) and the `PI_ANTHROPIC_OAUTH_CLIENT_ID`
/// env override. Stamped into the imported credential so refresh works
/// without requiring the env var to be set.
const PI_ANTHROPIC_OAUTH_CLIENT_ID_DEFAULT: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// Pi's stock Anthropic OAuth token endpoint. Mirrors the upstream
/// constant and `PI_ANTHROPIC_OAUTH_TOKEN_URL` env override.
const PI_ANTHROPIC_OAUTH_TOKEN_URL_DEFAULT: &str =
    "https://console.anthropic.com/v1/oauth/token";

/// Public OAuth client id used by pi's Anthropic flow. Reads the
/// `PI_ANTHROPIC_OAUTH_CLIENT_ID` env override first so callers that
/// run pi against a non-default OAuth app keep matching client ids.
pub fn anthropic_oauth_client_id() -> String {
    std::env::var("PI_ANTHROPIC_OAUTH_CLIENT_ID")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| PI_ANTHROPIC_OAUTH_CLIENT_ID_DEFAULT.to_string())
}

/// Token endpoint URL used by pi's Anthropic OAuth refresh path.
/// Honours `PI_ANTHROPIC_OAUTH_TOKEN_URL`.
pub fn anthropic_oauth_token_url() -> String {
    std::env::var("PI_ANTHROPIC_OAUTH_TOKEN_URL")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| PI_ANTHROPIC_OAUTH_TOKEN_URL_DEFAULT.to_string())
}

/// Inputs for the import helper. `auth_path` defaults to pi's
/// `Config::auth_path()` (honours `PI_CODING_AGENT_DIR`).
#[derive(Debug, Clone, Default)]
pub struct ClaudeImportConfig {
    pub auth_path: Option<PathBuf>,
}

impl ClaudeImportConfig {
    fn resolve_auth_path(&self) -> PathBuf {
        self.auth_path
            .clone()
            .unwrap_or_else(Config::auth_path)
    }
}

/// Wire shape of the Claude Code credentials JSON file. Unknown
/// fields (`type`, `last_refresh`, ...) are ignored. Parsed manually
/// off `serde_json::Value` so this crate does not need the `serde`
/// derive macros (and so we can give precise, token-free error
/// messages for missing/empty fields).
#[derive(Debug, Clone)]
struct ClaudeCredentialsFile {
    access_token: String,
    refresh_token: String,
    /// ISO 8601 timestamp like `2026-05-26T05:16:09+08:00`.
    expired: String,
    /// Optional email; only used for the redacted summary tag.
    email: Option<String>,
}

fn extract_string(value: &Value, key: &str) -> Result<String, String> {
    match value.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(other) => Err(format!("field `{key}` must be a string, got {other}")),
        None => Err(format!("missing field `{key}`")),
    }
}

fn extract_optional_string(value: &Value, key: &str) -> Option<String> {
    match value.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// Sidecar emitted on a successful import. Does NOT carry the raw
/// tokens — only the redacted, log-safe fields the caller should
/// surface in transcripts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeImportSummary {
    pub provider: String,
    pub email: Option<String>,
    pub expires_ms: i64,
    /// `expires_ms - now_ms`, clamped to 0 when already expired. The
    /// caller has no clock dependency this way.
    pub expires_in_ms: i64,
}

/// Result of `import_claude_credentials`: the typed [`AuthEvent`] for
/// the store reducer plus a redacted summary the CLI can log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeImportOutcome {
    pub event: AuthEvent,
    pub summary: Option<ClaudeImportSummary>,
}

/// Read JSON from `json_text`, convert it into pi's `AuthCredential::OAuth`,
/// and persist it under the `anthropic` provider key in pi's
/// `auth.json`. Existing entries for other providers are preserved.
///
/// On success, returns [`AuthEvent::Authorized`] with
/// [`AuthEventSource::Oauth`] + a redacted [`ClaudeImportSummary`].
/// On parse or IO failure, returns [`AuthEvent::Failed`] with a
/// human-readable, token-free reason.
pub fn import_claude_credentials(
    config: &ClaudeImportConfig,
    json_text: &str,
) -> ClaudeImportOutcome {
    let value: Value = match serde_json::from_str(json_text) {
        Ok(v) => v,
        Err(err) => {
            return failure(&format!("claude import: parse json: {err}"));
        }
    };
    let parsed = match (
        extract_string(&value, "access_token"),
        extract_string(&value, "refresh_token"),
        extract_string(&value, "expired"),
    ) {
        (Ok(at), Ok(rt), Ok(exp)) => ClaudeCredentialsFile {
            access_token: at,
            refresh_token: rt,
            expired: exp,
            email: extract_optional_string(&value, "email"),
        },
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => {
            return failure(&format!("claude import: {e}"));
        }
    };

    if parsed.access_token.trim().is_empty() {
        return failure("claude import: access_token was empty");
    }
    if parsed.refresh_token.trim().is_empty() {
        return failure("claude import: refresh_token was empty");
    }

    let expires_ms = match parse_iso8601_to_unix_ms(&parsed.expired) {
        Ok(ms) => ms,
        Err(err) => {
            return failure(&format!(
                "claude import: invalid ISO 8601 `expired` ({err})"
            ));
        }
    };

    let credential = AuthCredential::OAuth {
        access_token: parsed.access_token.clone(),
        refresh_token: parsed.refresh_token.clone(),
        expires: expires_ms,
        token_url: Some(anthropic_oauth_token_url()),
        client_id: Some(anthropic_oauth_client_id()),
    };

    let path = config.resolve_auth_path();
    let mut storage = match AuthStorage::load(path) {
        Ok(s) => s,
        Err(err) => {
            return failure(&format!("claude import: load auth.json: {err}"));
        }
    };

    storage.set(ANTHROPIC_PROVIDER_ID, credential);
    if let Err(err) = storage.save() {
        return failure(&format!("claude import: persist auth.json: {err}"));
    }

    let now_ms = current_unix_ms();
    let expires_in_ms = (expires_ms - now_ms).max(0);
    ClaudeImportOutcome {
        event: AuthEvent::Authorized {
            source: AuthEventSource::Oauth,
        },
        summary: Some(ClaudeImportSummary {
            provider: ANTHROPIC_PROVIDER_ID.to_string(),
            email: parsed.email,
            expires_ms,
            expires_in_ms,
        }),
    }
}

fn failure(reason: &str) -> ClaudeImportOutcome {
    ClaudeImportOutcome {
        event: AuthEvent::Failed {
            reason: reason.to_string(),
        },
        summary: None,
    }
}

fn current_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Parse an ISO 8601 timestamp of the shape `YYYY-MM-DDTHH:MM:SS[.fff]TZ`
/// (where `TZ` is `Z`, `+HH:MM`, `+HHMM`, or `-HH:MM`) into Unix
/// milliseconds. Implemented in-crate to keep `pi-mobile-client` free
/// of a `chrono`/`time` direct dependency at this layer (pi already
/// depends on `chrono` transitively, but exposing it here would widen
/// the public dependency surface).
pub(crate) fn parse_iso8601_to_unix_ms(text: &str) -> Result<i64, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("empty timestamp".to_string());
    }
    // Split date / time / tz
    let (date_part, rest) = trimmed
        .split_once('T')
        .ok_or_else(|| "missing 'T' separator".to_string())?;
    // Find timezone marker: trailing Z, or last '+' or '-' (but not the
    // one immediately following 'T' which is invalid here).
    let (time_part, tz_part) = split_tz(rest)?;

    let (y, m, d) = parse_date(date_part)?;
    let (hh, mm, ss, sub_ms) = parse_time(time_part)?;
    let tz_offset_secs = parse_tz(tz_part)?;

    // Compute Unix seconds for the wall-clock components, then apply
    // the timezone offset.
    let day_secs = days_from_civil(y, m as i32, d as i32) * 86_400;
    let wall_secs = day_secs
        + (hh as i64) * 3600
        + (mm as i64) * 60
        + (ss as i64);
    let utc_secs = wall_secs - tz_offset_secs;
    let ms = utc_secs
        .checked_mul(1000)
        .ok_or_else(|| "timestamp overflow".to_string())?
        .checked_add(sub_ms as i64)
        .ok_or_else(|| "timestamp overflow".to_string())?;
    Ok(ms)
}

fn split_tz(rest: &str) -> Result<(&str, &str), String> {
    if let Some(stripped) = rest.strip_suffix('Z') {
        return Ok((stripped, "Z"));
    }
    // Walk back from the end to find a '+' or '-'.
    let bytes = rest.as_bytes();
    for i in (0..bytes.len()).rev() {
        let c = bytes[i] as char;
        if c == '+' || c == '-' {
            return Ok((&rest[..i], &rest[i..]));
        }
        // Stop searching once we hit non-tz characters: digits, '.',
        // and ':' are valid in the time portion; anything else is an
        // error.
        if !(c.is_ascii_digit() || c == ':' || c == '.') {
            return Err(format!("unexpected char in timestamp: {c}"));
        }
    }
    Err("missing timezone designator".to_string())
}

fn parse_date(s: &str) -> Result<(i32, u32, u32), String> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 {
        return Err(format!("malformed date `{s}`"));
    }
    let y: i32 = parts[0]
        .parse()
        .map_err(|e| format!("year: {e}"))?;
    let m: u32 = parts[1]
        .parse()
        .map_err(|e| format!("month: {e}"))?;
    let d: u32 = parts[2]
        .parse()
        .map_err(|e| format!("day: {e}"))?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return Err(format!("date out of range: {y}-{m}-{d}"));
    }
    Ok((y, m, d))
}

fn parse_time(s: &str) -> Result<(u32, u32, u32, u32), String> {
    // Optional fractional seconds.
    let (main, frac) = match s.find('.') {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let parts: Vec<&str> = main.split(':').collect();
    if parts.len() != 3 {
        return Err(format!("malformed time `{s}`"));
    }
    let hh: u32 = parts[0]
        .parse()
        .map_err(|e| format!("hour: {e}"))?;
    let mm: u32 = parts[1]
        .parse()
        .map_err(|e| format!("minute: {e}"))?;
    let ss: u32 = parts[2]
        .parse()
        .map_err(|e| format!("second: {e}"))?;
    if hh >= 24 || mm >= 60 || ss >= 60 {
        return Err(format!("time out of range: {hh}:{mm}:{ss}"));
    }
    let sub_ms = if let Some(frac) = frac {
        if frac.is_empty() {
            return Err("trailing '.' with no fractional seconds".to_string());
        }
        // Take first 3 digits, zero-pad if fewer.
        let mut digits = frac.chars().take_while(|c| c.is_ascii_digit());
        let mut ms_str = String::new();
        for _ in 0..3 {
            ms_str.push(digits.next().unwrap_or('0'));
        }
        ms_str
            .parse::<u32>()
            .map_err(|e| format!("fractional seconds: {e}"))?
    } else {
        0
    };
    Ok((hh, mm, ss, sub_ms))
}

fn parse_tz(s: &str) -> Result<i64, String> {
    if s == "Z" {
        return Ok(0);
    }
    let (sign, rest) = match s.as_bytes().first() {
        Some(b'+') => (1i64, &s[1..]),
        Some(b'-') => (-1i64, &s[1..]),
        _ => return Err(format!("malformed timezone `{s}`")),
    };
    // Accept HH:MM or HHMM.
    let (h_str, m_str) = if let Some((h, m)) = rest.split_once(':') {
        (h, m)
    } else if rest.len() == 4 {
        (&rest[..2], &rest[2..])
    } else {
        return Err(format!("malformed timezone offset `{s}`"));
    };
    let hh: i64 = h_str
        .parse()
        .map_err(|e| format!("tz hour: {e}"))?;
    let mm: i64 = m_str
        .parse()
        .map_err(|e| format!("tz minute: {e}"))?;
    if !(0..=14).contains(&hh) || !(0..=59).contains(&mm) {
        return Err(format!("tz offset out of range `{s}`"));
    }
    Ok(sign * (hh * 3600 + mm * 60))
}

/// Days from civil date to 1970-01-01, using Howard Hinnant's
/// proleptic Gregorian algorithm (public domain). Avoids pulling in
/// chrono just to convert a fixed-format timestamp.
fn days_from_civil(y: i32, m: i32, d: i32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as i64; // [0, 399]
    let doy = ((153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5
        + d
        - 1) as i64; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    (era as i64) * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_json(expired: &str) -> String {
        // Clearly-fake values. NEVER use anything that resembles a
        // real-issued Anthropic token here.
        format!(
            r#"{{
                "access_token": "sk-ant-oat01-TEST-FAKE-ACCESS-TOKEN",
                "refresh_token": "sk-ant-ort01-TEST-FAKE-REFRESH-TOKEN",
                "expired": "{expired}",
                "last_refresh": "2026-05-01T00:00:00Z",
                "email": "tester@example.com",
                "type": "claude"
            }}"#
        )
    }

    #[test]
    fn iso8601_with_positive_tz_converts_to_unix_ms() {
        // 2026-05-26T05:16:09+08:00 == 2026-05-25T21:16:09Z
        // Day count from 1970-01-01 to 2026-05-25 is 20598; 86400*20598
        // + 21*3600 + 16*60 + 9 == 1_779_743_769 seconds.
        let ms = parse_iso8601_to_unix_ms("2026-05-26T05:16:09+08:00").expect("parse");
        assert_eq!(ms, 1_779_743_769_000);
    }

    #[test]
    fn iso8601_with_zulu_converts_to_unix_ms() {
        let ms = parse_iso8601_to_unix_ms("1970-01-01T00:00:00Z").expect("epoch");
        assert_eq!(ms, 0);
        let ms2 = parse_iso8601_to_unix_ms("2024-01-02T03:04:05Z").expect("parse");
        assert_eq!(ms2, 1_704_164_645_000);
    }

    #[test]
    fn iso8601_with_negative_tz_and_fraction() {
        // 2026-05-26T05:16:09.123-05:00 == 2026-05-26T10:16:09.123Z
        let ms = parse_iso8601_to_unix_ms("2026-05-26T05:16:09.123-05:00").expect("parse");
        assert_eq!(ms, 1_779_790_569_123);
    }

    #[test]
    fn iso8601_rejects_malformed_input() {
        assert!(parse_iso8601_to_unix_ms("not a date").is_err());
        assert!(parse_iso8601_to_unix_ms("2026-13-01T00:00:00Z").is_err());
        assert!(parse_iso8601_to_unix_ms("2026-05-26 05:16:09Z").is_err());
        assert!(parse_iso8601_to_unix_ms("2026-05-26T05:16:09").is_err());
    }

    #[test]
    fn import_round_trips_into_auth_storage() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let json = fake_json("2026-05-26T05:16:09+08:00");

        let outcome = import_claude_credentials(
            &ClaudeImportConfig {
                auth_path: Some(auth_path.clone()),
            },
            &json,
        );

        assert_eq!(
            outcome.event,
            AuthEvent::Authorized {
                source: AuthEventSource::Oauth
            }
        );
        let summary = outcome.summary.expect("summary present on success");
        assert_eq!(summary.provider, "anthropic");
        assert_eq!(summary.email.as_deref(), Some("tester@example.com"));
        assert_eq!(summary.expires_ms, 1_779_743_769_000);

        // Reload through AuthStorage and verify the OAuth variant
        // round-trips with stamped token_url/client_id.
        let storage = AuthStorage::load(auth_path).expect("reload");
        match storage.get(ANTHROPIC_PROVIDER_ID) {
            Some(AuthCredential::OAuth {
                access_token,
                refresh_token,
                expires,
                token_url,
                client_id,
            }) => {
                assert_eq!(access_token, "sk-ant-oat01-TEST-FAKE-ACCESS-TOKEN");
                assert_eq!(refresh_token, "sk-ant-ort01-TEST-FAKE-REFRESH-TOKEN");
                assert_eq!(*expires, 1_779_743_769_000);
                assert_eq!(token_url.as_deref(), Some(anthropic_oauth_token_url().as_str()));
                assert_eq!(client_id.as_deref(), Some(anthropic_oauth_client_id().as_str()));
            }
            other => panic!("expected OAuth credential, got {other:?}"),
        }
    }

    #[test]
    fn import_preserves_existing_non_anthropic_entries() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");

        // Seed an unrelated provider entry.
        let mut storage = AuthStorage::load(auth_path.clone()).expect("seed load");
        storage.set(
            "openai",
            AuthCredential::ApiKey {
                key: "sk-oa-untouched".to_string(),
            },
        );
        storage.save().expect("seed save");

        let outcome = import_claude_credentials(
            &ClaudeImportConfig {
                auth_path: Some(auth_path.clone()),
            },
            &fake_json("2026-05-26T05:16:09+00:00"),
        );
        assert!(matches!(
            outcome.event,
            AuthEvent::Authorized {
                source: AuthEventSource::Oauth
            }
        ));

        let storage = AuthStorage::load(auth_path).expect("reload");
        match storage.get("openai") {
            Some(AuthCredential::ApiKey { key }) => assert_eq!(key, "sk-oa-untouched"),
            other => panic!("openai entry must survive import, got {other:?}"),
        }
        assert!(storage.get(ANTHROPIC_PROVIDER_ID).is_some());
    }

    #[test]
    fn import_rejects_missing_tokens() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");

        let json = r#"{
            "access_token": "",
            "refresh_token": "sk-ant-ort01-TEST",
            "expired": "2026-05-26T05:16:09Z"
        }"#;
        let outcome = import_claude_credentials(
            &ClaudeImportConfig {
                auth_path: Some(auth_path),
            },
            json,
        );
        match outcome.event {
            AuthEvent::Failed { reason } => assert!(reason.contains("access_token")),
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(outcome.summary.is_none());
    }

    #[test]
    fn import_rejects_invalid_iso8601() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let outcome = import_claude_credentials(
            &ClaudeImportConfig {
                auth_path: Some(auth_path),
            },
            &fake_json("not-a-timestamp"),
        );
        match outcome.event {
            AuthEvent::Failed { reason } => assert!(reason.contains("ISO 8601")),
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}
