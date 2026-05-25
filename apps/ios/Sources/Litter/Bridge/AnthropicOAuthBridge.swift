import Foundation

/// Glue between the iOS Anthropic OAuth surface and the shared Rust
/// `AuthStorage` driver.
///
/// The Rust driver (`pi_mobile_client::auth::anthropic_oauth`) is the
/// canonical owner of token state — it persists OAuth credentials into
/// pi's `auth.json` and refreshes them via `AuthStorage::
/// refresh_expired_oauth_tokens`. The Swift bridge layer is
/// responsible for two additional things on iOS:
///
/// 1. Driving the system browser for the Authorization Code + PKCE
///    handshake (see `AnthropicOAuthSheet` /
///    `AnthropicOAuthSessionRunner`), and
/// 2. Mirroring the OAuth **refresh token** into the iOS Keychain so a
///    future "wipe app sandbox" (or restoring onto a new device via an
///    encrypted backup) preserves the long-lived credential under a
///    secure store rather than alongside the pi sandbox JSON file.
///
/// This bridge file is the seam that satisfies VAL-AUTH-004: every
/// platform-side read/write of the Anthropic refresh token goes
/// through `AnthropicKeychainStorage` (and therefore through
/// `SecItemAdd` / `SecItemCopyMatching` / `SecItemDelete`).
enum AnthropicOAuthBridge {
    /// Static redirect URI Anthropic must call back to after the user
    /// finishes the consent screen. Matches the `callbackURLScheme`
    /// configured on the `ASWebAuthenticationSession` inside
    /// `AnthropicOAuthSessionRunner`.
    static let redirectURI = "litter://oauth/pi/anthropic"

    /// `callbackURLScheme` argument for `ASWebAuthenticationSession`.
    /// Must match `redirectURI`'s scheme verbatim or the system will
    /// silently refuse to surface the redirect.
    static let callbackURLScheme = "litter"

    /// Persist a freshly-issued refresh token through the iOS
    /// Keychain. Called by `AnthropicOAuthSheet` after the Rust driver
    /// returns `AuthState::Authorized` from
    /// `complete_anthropic_oauth_paste`. The Rust side has already
    /// written the credential into `auth.json`; storing it here gives
    /// the Swift layer a Keychain-backed copy that survives sandbox
    /// resets and (per VAL-AUTH-004) keeps the refresh token out of
    /// `UserDefaults`.
    static func persistRefreshToken(_ refreshToken: String) throws {
        try AnthropicKeychainStorage.shared.saveRefreshToken(refreshToken)
    }

    /// Load the Keychain-backed refresh token if any. Used at app
    /// launch to hand the Rust driver a credential to refresh when
    /// the pi sandbox `auth.json` is missing (e.g. after a clean
    /// install but before the user re-signs in).
    static func loadRefreshToken() throws -> String? {
        try AnthropicKeychainStorage.shared.loadRefreshToken()
    }

    /// Remove the Keychain-backed refresh token. Called from sign-out
    /// flows after `pi_byok_set` or after the Rust driver invalidates
    /// the credential via `refresh()` returning `Failed`.
    static func clearRefreshToken() throws {
        try AnthropicKeychainStorage.shared.deleteRefreshToken()
    }
}
