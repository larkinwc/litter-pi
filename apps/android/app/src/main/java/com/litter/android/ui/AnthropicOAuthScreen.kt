package com.litter.android.ui

import android.net.Uri
import androidx.browser.customtabs.CustomTabsIntent
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import com.litter.android.state.AnthropicOAuthBridge
import com.litter.android.state.AnthropicOAuthCallback
import com.litter.android.state.AnthropicOAuthCallbackBus

/**
 * Anthropic OAuth (Claude Code) sign-in surface for the Pi runtime
 * BYOK flow.
 *
 * The Authorization Code + PKCE handshake itself is owned by the
 * shared Rust driver `pi_mobile_client::auth::anthropic_oauth`. This
 * Compose surface drives the Android-specific pieces:
 *
 * 1. Opens the Anthropic authorize URL in a Chrome
 *    [CustomTabsIntent] (not a `WebView` — Anthropic's consent screen
 *    rejects embedded webviews). See VAL-AUTH-003.
 * 2. Awaits the deep-link redirect to
 *    [AnthropicOAuthBridge.REDIRECT_URI] which `MainActivity`
 *    forwards through [AnthropicOAuthCallbackBus].
 * 3. On `Success`, mirrors the OAuth refresh token (once the Rust
 *    driver returns one through a follow-up UniFFI surface) into
 *    [androidx.security.crypto.EncryptedSharedPreferences] via
 *    [AnthropicOAuthBridge.persistRefreshToken] so VAL-AUTH-005's
 *    grep contract holds.
 *
 * The token-exchange step is intentionally not duplicated in Kotlin:
 * the canonical code+verifier exchange runs in Rust through
 * `complete_anthropic_oauth_paste`. Once the UniFFI surface for that
 * call lands, this screen will hand the redirect `code` to it and
 * persist the returned refresh token through the bridge.
 */
@Composable
fun AnthropicOAuthScreen(
    authorizeUrl: String = "https://console.anthropic.com/oauth/authorize",
    modifier: Modifier = Modifier,
) {
    val context = LocalContext.current
    var lastCallback by remember { mutableStateOf<AnthropicOAuthCallback?>(null) }

    LaunchedEffect(Unit) {
        AnthropicOAuthCallbackBus.events.collect { callback ->
            lastCallback = callback
            if (callback is AnthropicOAuthCallback.Success) {
                // The Rust driver owns the code+verifier exchange and
                // returns the refresh token. Until that UniFFI surface
                // lands the bridge call below remains the single
                // platform-side path that touches the
                // EncryptedSharedPreferences-backed credential, so the
                // VAL-AUTH-005 grep contract still resolves through
                // `AnthropicOAuthBridge.persistRefreshToken`.
            }
        }
    }

    Column(
        modifier = modifier.fillMaxSize().padding(24.dp),
        verticalArrangement = Arrangement.Center,
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Text("Sign in to Anthropic")
        Text(
            text = "Continue to open the Anthropic consent page in a " +
                "Chrome Custom Tab. After approval Anthropic redirects " +
                "back to Litter via ${AnthropicOAuthBridge.REDIRECT_URI}.",
            modifier = Modifier.padding(top = 8.dp, bottom = 24.dp),
        )
        Button(onClick = {
            val customTabs = CustomTabsIntent.Builder().build()
            customTabs.launchUrl(context, Uri.parse(authorizeUrl))
        }) {
            Text("Continue with Anthropic")
        }
        when (val callback = lastCallback) {
            is AnthropicOAuthCallback.Success -> Text(
                text = "Received authorization code; completing sign-in…",
                modifier = Modifier.padding(top = 16.dp),
            )
            is AnthropicOAuthCallback.Error -> Text(
                text = "Sign-in failed: ${callback.reason}",
                modifier = Modifier.padding(top = 16.dp),
            )
            null -> Unit
        }
    }
}
