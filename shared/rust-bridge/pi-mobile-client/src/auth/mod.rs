//! Authentication drivers for the in-process pi runtime.
//!
//! Today only the Anthropic OAuth Authorization Code + PKCE driver
//! lives here. Additional drivers (e.g. BYOK key plumbing) land
//! alongside it in subsequent features.

pub mod anthropic_oauth;

pub use anthropic_oauth::{
    AnthropicOAuthConfig, AnthropicOAuthDriver, AuthEvent, AuthEventSource, AuthorizeHandshake,
    CLIENT_ID_ENV, CLIENT_SECRET_ENV, ANTHROPIC_PROVIDER_ID,
};
