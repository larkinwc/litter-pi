//! Authentication drivers for the in-process pi runtime.
//!
//! Today only the Anthropic OAuth Authorization Code + PKCE driver
//! lives here. Additional drivers (e.g. BYOK key plumbing) land
//! alongside it in subsequent features.

pub mod anthropic_oauth;
pub mod blocking;
pub mod byok;
pub mod claude_import;

pub use anthropic_oauth::{
    AnthropicOAuthConfig, AnthropicOAuthDriver, AuthEvent, AuthEventSource, AuthorizeHandshake,
    CLIENT_ID_ENV, CLIENT_SECRET_ENV, ANTHROPIC_PROVIDER_ID,
};
pub use blocking::{
    complete_anthropic_oauth_paste, pi_byok_set_blocking, refresh_anthropic_oauth,
    snapshot_anthropic_oauth,
};
pub use byok::{ByokConfig, pi_byok_set};
pub use claude_import::{
    anthropic_oauth_client_id, anthropic_oauth_token_url, import_claude_credentials,
    ClaudeImportConfig, ClaudeImportOutcome, ClaudeImportSummary,
};
