package com.litter.android.state

import uniffi.codex_mobile_client.AnthropicOAuthConfig
import uniffi.codex_mobile_client.AuthSource
import uniffi.codex_mobile_client.AuthState
import uniffi.codex_mobile_client.piAnthropicOauthComplete

/**
 * Token-exchange step for the Android Anthropic OAuth surface.
 *
 * The canonical PKCE handshake + token exchange lives in the shared
 * Rust driver (`pi_mobile_client::auth::anthropic_oauth`); this
 * interface exists so [com.litter.android.ui.AnthropicOAuthScreen]
 * can be unit-tested without crossing the UniFFI boundary. The
 * default implementation calls into the regenerated Kotlin binding
 * and extracts the raw refresh token off the
 * `AuthState.Authorized` variant produced by the Rust driver so the
 * caller can mirror it through
 * [AnthropicOAuthBridge.persistRefreshToken] (and therefore through
 * [androidx.security.crypto.EncryptedSharedPreferences]) per
 * VAL-AUTH-005.
 *
 * BYOK credentials never carry a refresh token across the FFI
 * boundary — the Rust driver pins `refresh_token = null` on the
 * `AuthSource.BYOK` arm — so this surface returns `null` on that
 * source and the bridge skips the keychain write entirely.
 */
fun interface AnthropicOAuthCompleter {
    /**
     * Drive the token exchange for `code` and return the refresh
     * token the Rust driver surfaced (or `null` when the credential
     * does not carry one — e.g. BYOK or non-`Authorized` states).
     */
    fun complete(code: String): String?
}

/** Production [AnthropicOAuthCompleter] backed by the UniFFI surface. */
object DefaultAnthropicOAuthCompleter : AnthropicOAuthCompleter {
    override fun complete(code: String): String? {
        val state = piAnthropicOauthComplete(
            config = AnthropicOAuthConfig(
                clientId = null,
                clientSecret = null,
                authPath = null,
            ),
            code = code,
        )
        return refreshTokenForPersistence(state)
    }
}

/**
 * Extract the OAuth refresh token from `state` when (and only when)
 * the source is OAuth. Exposed for unit tests so callers can verify
 * the BYOK / non-Authorized arms drop the token without going
 * through the live Rust driver.
 */
internal fun refreshTokenForPersistence(state: AuthState): String? = when (state) {
    is AuthState.Authorized -> when (state.source) {
        AuthSource.OAUTH -> state.refreshToken
        AuthSource.BYOK -> null
    }
    else -> null
}
