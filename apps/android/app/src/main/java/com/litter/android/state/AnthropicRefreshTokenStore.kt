package com.litter.android.state

import android.content.Context
import androidx.security.crypto.EncryptedSharedPreferences
import androidx.security.crypto.MasterKey

/**
 * Encrypted storage for the Anthropic OAuth refresh token used by the
 * in-process pi runtime.
 *
 * VAL-AUTH-005 requires the Android bridge to persist the refresh
 * token in [EncryptedSharedPreferences] keyed by an Android Keystore
 * [MasterKey] (rather than plain [android.content.SharedPreferences]
 * or a file in the app sandbox). This wrapper is the single Kotlin
 * surface that touches the `androidx.security.crypto` APIs for the
 * Anthropic OAuth credential and is invoked from
 * [AnthropicOAuthBridge] whenever the Rust `AuthStorage` would
 * otherwise need a platform-supplied refresh token (e.g. after a
 * successful PKCE handshake or on refresh).
 *
 * The prefs file name and master-key alias are pinned constants so
 * grep-based contract validators can locate them; keep them in sync
 * with the iOS [`AnthropicKeychainStorage`] service id documented in
 * `architecture.md` so cross-platform debugging can correlate stored
 * credentials.
 */
class AnthropicRefreshTokenStore(context: Context) {
    private val appContext = context.applicationContext
    private val prefs = openEncryptedPrefsOrReset(appContext, PREFS_NAME)

    /** Persist `refreshToken`, replacing any previously stored value. */
    fun saveRefreshToken(refreshToken: String) {
        prefs.edit().putString(KEY_REFRESH_TOKEN, refreshToken).apply()
    }

    /** Fetch the stored refresh token, or `null` when no entry exists. */
    fun loadRefreshToken(): String? =
        prefs.getString(KEY_REFRESH_TOKEN, null)?.takeIf { it.isNotBlank() }

    /** Remove the stored refresh token. Idempotent. */
    fun clearRefreshToken() {
        prefs.edit().remove(KEY_REFRESH_TOKEN).apply()
    }

    companion object {
        /**
         * Encrypted prefs file name. Pinned for VAL-AUTH-005 evidence:
         * `EncryptedSharedPreferences` + `MasterKey` references both
         * live in this file and grep against this constant.
         */
        const val PREFS_NAME = "anthropic_oauth_credentials"

        private const val KEY_REFRESH_TOKEN = "refresh_token"
    }
}
