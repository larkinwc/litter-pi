package com.litter.android.state

import android.content.Context
import android.net.Uri
import kotlinx.coroutines.channels.BufferOverflow
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.asSharedFlow

/**
 * Glue between the Android Anthropic OAuth surface and the shared Rust
 * `AuthStorage` driver.
 *
 * The Rust driver (`pi_mobile_client::auth::anthropic_oauth`) is the
 * canonical owner of token state — it persists OAuth credentials into
 * pi's `auth.json` and refreshes them via
 * `AuthStorage::refresh_expired_oauth_tokens`. The Kotlin bridge layer
 * is responsible for two additional things on Android:
 *
 * 1. Driving the system browser for the Authorization Code + PKCE
 *    handshake via [androidx.browser.customtabs.CustomTabsIntent]
 *    (see `AnthropicOAuthScreen`), and
 * 2. Mirroring the OAuth refresh token into
 *    [EncryptedSharedPreferences] (via [AnthropicRefreshTokenStore])
 *    so a future app data wipe or reinstall preserves the long-lived
 *    credential under a Keystore-backed store rather than alongside
 *    the pi sandbox JSON file.
 *
 * This bridge satisfies VAL-AUTH-005: every platform-side read/write
 * of the Anthropic refresh token goes through
 * [AnthropicRefreshTokenStore] (and therefore through
 * `androidx.security.crypto.EncryptedSharedPreferences` keyed by a
 * `MasterKey`).
 */
object AnthropicOAuthBridge {
    /**
     * Static redirect URI Anthropic must call back to after the user
     * finishes the consent screen. Matches the deep-link intent filter
     * declared on `MainActivity` in `AndroidManifest.xml`.
     */
    const val REDIRECT_URI = "litter://oauth/pi/anthropic"

    /** Scheme component of [REDIRECT_URI] for deep-link matching. */
    const val REDIRECT_SCHEME = "litter"

    /** Host component of [REDIRECT_URI] for deep-link matching. */
    const val REDIRECT_HOST = "oauth"

    /** Path prefix component of [REDIRECT_URI] for deep-link matching. */
    const val REDIRECT_PATH_PREFIX = "/pi/anthropic"

    /**
     * Return true when `uri` matches the registered OAuth redirect
     * (scheme + host + path prefix). Used by `MainActivity` to decide
     * whether to forward the intent to [AnthropicOAuthCallbackBus].
     */
    fun isOAuthRedirect(uri: Uri?): Boolean {
        uri ?: return false
        if (!REDIRECT_SCHEME.equals(uri.scheme, ignoreCase = true)) return false
        if (!REDIRECT_HOST.equals(uri.host, ignoreCase = true)) return false
        val path = uri.path.orEmpty()
        return path == REDIRECT_PATH_PREFIX || path.startsWith("$REDIRECT_PATH_PREFIX/")
    }

    /**
     * Persist a freshly-issued refresh token through the Android
     * EncryptedSharedPreferences store. Called by
     * `AnthropicOAuthScreen` after the Rust driver returns
     * `AuthState::Authorized` from `complete_anthropic_oauth_paste`.
     * The Rust side has already written the credential into
     * `auth.json`; storing it here gives the Kotlin layer a
     * Keystore-backed copy that survives sandbox resets and (per
     * VAL-AUTH-005) keeps the refresh token out of plain
     * `SharedPreferences`.
     */
    fun persistRefreshToken(context: Context, refreshToken: String) {
        AnthropicRefreshTokenStore(context).saveRefreshToken(refreshToken)
    }

    /**
     * Load the EncryptedSharedPreferences-backed refresh token if any.
     * Used at app launch to hand the Rust driver a credential to
     * refresh when the pi sandbox `auth.json` is missing (e.g. after a
     * clean install but before the user re-signs in).
     */
    fun loadRefreshToken(context: Context): String? =
        AnthropicRefreshTokenStore(context).loadRefreshToken()

    /**
     * Remove the EncryptedSharedPreferences-backed refresh token.
     * Called from sign-out flows after `pi_byok_set` or after the Rust
     * driver invalidates the credential via `refresh()` returning
     * `Failed`.
     */
    fun clearRefreshToken(context: Context) {
        AnthropicRefreshTokenStore(context).clearRefreshToken()
    }
}

/**
 * Process-wide bus that delivers the parsed `code` (and any error
 * payload) from the Anthropic OAuth deep-link redirect to whichever
 * Compose surface is awaiting it. `MainActivity.onNewIntent` parses
 * the redirect URI and emits onto this flow; the screen collects it.
 *
 * Replay = 1 so a redirect that lands while the Compose surface is
 * being recomposed is still delivered.
 */
object AnthropicOAuthCallbackBus {
    private val _events = MutableSharedFlow<AnthropicOAuthCallback>(
        replay = 1,
        extraBufferCapacity = 1,
        onBufferOverflow = BufferOverflow.DROP_OLDEST,
    )

    /** Events stream consumed by `AnthropicOAuthScreen`. */
    val events: SharedFlow<AnthropicOAuthCallback> = _events.asSharedFlow()

    /** Push a parsed callback onto the bus. Returns true on success. */
    fun emit(callback: AnthropicOAuthCallback): Boolean = _events.tryEmit(callback)

    /**
     * Parse the deep-link URI into a typed [AnthropicOAuthCallback].
     * Returns null when the URI is not an Anthropic OAuth redirect.
     */
    fun parse(uri: Uri?): AnthropicOAuthCallback? {
        if (!AnthropicOAuthBridge.isOAuthRedirect(uri)) return null
        uri ?: return null
        val error = uri.getQueryParameter("error")
        if (!error.isNullOrBlank()) {
            return AnthropicOAuthCallback.Error(error)
        }
        val code = uri.getQueryParameter("code")?.takeIf { it.isNotBlank() }
        val state = uri.getQueryParameter("state")
        return if (code != null) {
            AnthropicOAuthCallback.Success(code = code, state = state)
        } else {
            AnthropicOAuthCallback.Error("missing_code")
        }
    }
}

/** Typed outcome of an Anthropic OAuth deep-link redirect. */
sealed interface AnthropicOAuthCallback {
    data class Success(val code: String, val state: String?) : AnthropicOAuthCallback
    data class Error(val reason: String) : AnthropicOAuthCallback
}
