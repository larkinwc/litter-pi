import SwiftUI

/// SwiftUI stub for the Anthropic OAuth (Claude Code) sign-in sheet.
///
/// The full Authorization-Code + PKCE flow (ASWebAuthenticationSession,
/// Keychain-backed refresh-token storage, `pi_oauth_begin`/`pi_oauth_finish`
/// UniFFI calls) lands in the auth milestone. This stub exists so other
/// surfaces (the BYOK Pi settings panel below) can already reference and
/// present it. Per the mission contract, the sheet must not perform a
/// real OAuth handshake until the auth milestone wires it up.
struct AnthropicOAuthSheet: View {
    @Environment(\.dismiss) private var dismiss

    /// Caller-supplied completion callback. Today only ever invoked
    /// with `.cancelled`; the auth milestone will add `.signedIn(...)`.
    var onCompletion: (Result) -> Void = { _ in }

    enum Result {
        case cancelled
        // case signedIn(refreshTokenStored: Bool)   // auth milestone
    }

    var body: some View {
        NavigationStack {
            ZStack {
                LitterTheme.backgroundGradient.ignoresSafeArea()
                VStack(spacing: 16) {
                    Image(systemName: "key.horizontal.fill")
                        .font(.system(size: 36))
                        .foregroundColor(LitterTheme.accent)
                    Text("Sign in with Anthropic")
                        .litterFont(.title3)
                        .foregroundColor(LitterTheme.textPrimary)
                    Text("Anthropic OAuth (Claude Code) sign-in arrives in the next milestone. Until then, use the BYOK Anthropic key or BYOK OpenAI-compatible flow to start a pi session.")
                        .litterFont(.footnote)
                        .foregroundColor(LitterTheme.textSecondary)
                        .multilineTextAlignment(.center)
                        .padding(.horizontal, 24)
                    Spacer()
                }
                .padding(.top, 36)
            }
            .navigationTitle("Anthropic OAuth")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .topBarTrailing) {
                    Button("Close") {
                        onCompletion(.cancelled)
                        dismiss()
                    }
                    .foregroundColor(LitterTheme.accent)
                }
            }
        }
    }
}

#Preview {
    AnthropicOAuthSheet()
}
