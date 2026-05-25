package com.litter.android.state

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import uniffi.codex_mobile_client.AuthSource
import uniffi.codex_mobile_client.AuthState

/**
 * Unit tests for the Anthropic OAuth completer's policy of mirroring
 * the refresh token into [AnthropicRefreshTokenStore] only on the
 * `AuthState.Authorized{source = OAUTH}` path.
 *
 * Lives in plain JUnit (no Robolectric) because the policy is a
 * pure-Kotlin mapping from the UniFFI [AuthState] enum to either a
 * raw refresh token (which the caller persists via
 * [AnthropicOAuthBridge.persistRefreshToken]) or `null` (which
 * intentionally suppresses the keychain write). The contract the
 * test pins down is the same one VAL-AUTH-005 enforces by grep:
 * `EncryptedSharedPreferences` writes happen iff
 * `refreshTokenForPersistence(state)` returns non-null.
 */
class AnthropicOAuthCompleterTest {

    @Test
    fun `oauth authorized with refresh token is mirrored`() {
        val state = AuthState.Authorized(
            source = AuthSource.OAUTH,
            refreshToken = "refresh-token-xyz",
        )
        assertEquals("refresh-token-xyz", refreshTokenForPersistence(state))
    }

    @Test
    fun `oauth authorized without refresh token returns null`() {
        // Snapshot reads pin refresh_token = null even on the OAuth
        // arm. The completer must not invent one.
        val state = AuthState.Authorized(
            source = AuthSource.OAUTH,
            refreshToken = null,
        )
        assertNull(refreshTokenForPersistence(state))
    }

    @Test
    fun `byok authorized never carries a refresh token`() {
        val state = AuthState.Authorized(
            source = AuthSource.BYOK,
            refreshToken = null,
        )
        assertNull(refreshTokenForPersistence(state))
    }

    @Test
    fun `byok authorized with a stray refresh token is dropped`() {
        // Defensive: even if the Rust side ever regressed and surfaced
        // a refresh_token on the BYOK arm, the Kotlin policy must
        // still drop it. The pi_byok_set driver pins refresh_token =
        // null today; this test guards against future drift.
        val state = AuthState.Authorized(
            source = AuthSource.BYOK,
            refreshToken = "should-not-leak",
        )
        assertNull(refreshTokenForPersistence(state))
    }

    @Test
    fun `unauthenticated and authorizing never persist a token`() {
        assertNull(refreshTokenForPersistence(AuthState.Unauthenticated))
        assertNull(refreshTokenForPersistence(AuthState.Authorizing))
    }

    @Test
    fun `failed state never persists a token`() {
        assertNull(
            refreshTokenForPersistence(AuthState.Failed(reason = "boom"))
        )
    }
}
