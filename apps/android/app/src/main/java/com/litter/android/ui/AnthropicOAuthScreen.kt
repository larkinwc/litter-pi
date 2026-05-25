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
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp

/**
 * Stub Anthropic OAuth screen for the Pi runtime BYOK flow.
 *
 * The full Authorization Code + PKCE handshake — including listening for
 * the redirect, exchanging the code + verifier, and persisting the
 * resulting tokens through the shared Rust auth store — lands in the
 * Anthropic OAuth + BYOK milestone (see `validation-contract.md`
 * `VAL-AUTH-*`).
 *
 * This stub keeps the Android UI tree compiling now so the Pi capability
 * gates (see [PiCapabilityGates]) and BYOK settings can reference the
 * route. It opens the Anthropic authorize URL in a Chrome
 * [CustomTabsIntent] when the user taps "Continue with Anthropic". Token
 * exchange + state machine wiring are deliberately deferred.
 */
@Composable
fun AnthropicOAuthScreen(
    authorizeUrl: String = "https://console.anthropic.com/oauth/authorize",
    modifier: Modifier = Modifier,
) {
    val context = LocalContext.current
    Column(
        modifier = modifier.fillMaxSize().padding(24.dp),
        verticalArrangement = Arrangement.Center,
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Text("Sign in to Anthropic")
        Text(
            text = "OAuth flow is being wired up. Continue to open the " +
                "Anthropic authorize page in a Chrome Custom Tab.",
            modifier = Modifier.padding(top = 8.dp, bottom = 24.dp),
        )
        Button(onClick = {
            val customTabs = CustomTabsIntent.Builder().build()
            customTabs.launchUrl(context, Uri.parse(authorizeUrl))
        }) {
            Text("Continue with Anthropic")
        }
    }
}
