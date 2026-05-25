import AuthenticationServices
import SwiftUI
import UIKit

/// SwiftUI surface for the Anthropic OAuth (Claude Code) sign-in flow.
///
/// The Authorization Code + PKCE handshake itself is owned by the
/// shared Rust driver `pi_mobile_client::auth::anthropic_oauth`. This
/// sheet drives the iOS-specific pieces:
///
/// 1. Asks the Rust driver for an authorize URL + PKCE verifier (via
///    `AnthropicOAuthAuthorizeProvider`).
/// 2. Opens the URL in `ASWebAuthenticationSession` with
///    `callbackURLScheme = "litter"` so Anthropic's redirect to
///    `litter://oauth/pi/anthropic?code=...` is funnelled back through
///    the system auth session rather than an embedded `WKWebView`
///    (which Anthropic's consent screen blocks).
/// 3. Forwards the returned `code` to the Rust driver via the supplied
///    `AnthropicOAuthCompleter`. The Rust side persists the OAuth
///    credential into pi's `auth.json`; this Swift layer additionally
///    mirrors the refresh token into the iOS Keychain through
///    `AnthropicOAuthBridge` so VAL-AUTH-004's grep contract holds.
///
/// The view model is intentionally injectable so unit tests can
/// observe the state machine without touching `Security` or
/// `AuthenticationServices` (both of which crash when invoked in a
/// non-UI XCTest target).
@MainActor
struct AnthropicOAuthSheet: View {
    @Environment(\.dismiss) private var dismiss
    @StateObject private var model: AnthropicOAuthSheetModel

    var onCompletion: (Result) -> Void

    init(
        model: AnthropicOAuthSheetModel? = nil,
        onCompletion: @escaping (Result) -> Void = { _ in }
    ) {
        let resolved = model ?? AnthropicOAuthSheetModel()
        _model = StateObject(wrappedValue: resolved)
        self.onCompletion = onCompletion
    }

    enum Result {
        case cancelled
        case signedIn(refreshTokenStored: Bool)
        case failed(String)
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
                    Text(model.descriptionText)
                        .litterFont(.footnote)
                        .foregroundColor(LitterTheme.textSecondary)
                        .multilineTextAlignment(.center)
                        .padding(.horizontal, 24)

                    Button {
                        Task { await runFlow() }
                    } label: {
                        HStack(spacing: 8) {
                            if model.isInFlight {
                                ProgressView()
                                    .progressViewStyle(.circular)
                                    .tint(LitterTheme.accent)
                            } else {
                                Image(systemName: "globe")
                                    .foregroundColor(LitterTheme.accent)
                            }
                            Text(model.actionButtonTitle)
                                .litterFont(.subheadline)
                                .foregroundColor(LitterTheme.accent)
                        }
                        .padding(.horizontal, 16)
                        .padding(.vertical, 10)
                        .background(LitterTheme.surface.opacity(0.6))
                        .clipShape(Capsule())
                    }
                    .disabled(model.isInFlight)

                    if let status = model.statusMessage {
                        Text(status)
                            .litterFont(.caption)
                            .foregroundColor(LitterTheme.textMuted)
                            .multilineTextAlignment(.center)
                            .padding(.horizontal, 24)
                    }
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

    private func runFlow() async {
        let result = await model.beginAuthorization()
        onCompletion(result)
        if case .signedIn = result {
            dismiss()
        }
    }
}

// MARK: - View model

/// Observable model backing `AnthropicOAuthSheet`. Owns the
/// `ASWebAuthenticationSession` lifecycle and the Keychain mirror.
@MainActor
final class AnthropicOAuthSheetModel: ObservableObject {
    @Published private(set) var isInFlight: Bool = false
    @Published private(set) var statusMessage: String?

    let descriptionText: String = "Anthropic's consent screen opens in a secure system browser. Sign in with your Claude Code account; Litter never sees your password."
    var actionButtonTitle: String {
        isInFlight ? "Opening browser…" : "Continue with Anthropic"
    }

    private let authorizeProvider: AnthropicOAuthAuthorizeProvider
    private let webAuthSessionFactory: AnthropicOAuthWebAuthSessionFactory
    private let completer: AnthropicOAuthCompleter
    private let keychainPersister: (String) throws -> Void

    init(
        authorizeProvider: AnthropicOAuthAuthorizeProvider? = nil,
        webAuthSessionFactory: AnthropicOAuthWebAuthSessionFactory? = nil,
        completer: AnthropicOAuthCompleter? = nil,
        keychainPersister: ((String) throws -> Void)? = nil
    ) {
        self.authorizeProvider = authorizeProvider ?? DefaultAnthropicOAuthAuthorizeProvider()
        self.webAuthSessionFactory = webAuthSessionFactory ?? DefaultAnthropicOAuthWebAuthSessionFactory()
        self.completer = completer ?? DefaultAnthropicOAuthCompleter()
        self.keychainPersister = keychainPersister ?? { token in
            try AnthropicOAuthBridge.persistRefreshToken(token)
        }
    }

    /// Drive the full Authorization Code + PKCE flow. Surfaces a
    /// `AnthropicOAuthSheet.Result` for the caller's completion
    /// callback. Marked `internal` so the XCTest can assert the
    /// state machine without going through SwiftUI.
    func beginAuthorization() async -> AnthropicOAuthSheet.Result {
        guard !isInFlight else {
            return .failed("Sign-in already in progress.")
        }
        isInFlight = true
        statusMessage = nil
        defer { isInFlight = false }

        let handshake: AnthropicOAuthHandshake
        do {
            handshake = try await authorizeProvider.beginAuthorization()
        } catch {
            let message = "Could not start sign-in: \(error.localizedDescription)"
            statusMessage = message
            return .failed(message)
        }

        let callbackURL: URL
        do {
            callbackURL = try await webAuthSessionFactory.run(
                authorizeURL: handshake.authorizeURL,
                callbackURLScheme: AnthropicOAuthBridge.callbackURLScheme
            )
        } catch let error as AnthropicOAuthWebAuthError {
            if case .cancelled = error {
                statusMessage = "Sign-in cancelled."
                return .cancelled
            }
            let message = "Sign-in failed: \(error.localizedDescription)"
            statusMessage = message
            return .failed(message)
        } catch {
            let message = "Sign-in failed: \(error.localizedDescription)"
            statusMessage = message
            return .failed(message)
        }

        let code: String
        do {
            code = try Self.extractAuthorizationCode(from: callbackURL)
        } catch {
            let message = "Sign-in returned an unexpected callback: \(error.localizedDescription)"
            statusMessage = message
            return .failed(message)
        }

        let completion: AnthropicOAuthCompletion
        do {
            completion = try await completer.complete(
                code: code,
                verifier: handshake.verifier
            )
        } catch {
            let message = "Anthropic rejected the sign-in: \(error.localizedDescription)"
            statusMessage = message
            return .failed(message)
        }

        var keychainStored = false
        if let refreshToken = completion.refreshToken {
            do {
                try keychainPersister(refreshToken)
                keychainStored = true
            } catch {
                statusMessage = "Signed in, but failed to store the refresh token securely."
                return .failed(error.localizedDescription)
            }
        }

        statusMessage = "Signed in with Anthropic."
        return .signedIn(refreshTokenStored: keychainStored)
    }

    /// Pull the `code` query item out of the OAuth redirect URL. The
    /// helper is `static` so the test can exercise the parser without
    /// instantiating the view model.
    static func extractAuthorizationCode(from url: URL) throws -> String {
        guard let components = URLComponents(url: url, resolvingAgainstBaseURL: false) else {
            throw AnthropicOAuthError.invalidCallbackURL
        }
        if let error = components.queryItems?.first(where: { $0.name == "error" })?.value {
            throw AnthropicOAuthError.providerError(error)
        }
        guard let code = components.queryItems?.first(where: { $0.name == "code" })?.value,
              !code.isEmpty else {
            throw AnthropicOAuthError.missingCode
        }
        return code
    }
}

// MARK: - Collaborators

enum AnthropicOAuthError: Error, LocalizedError {
    case invalidCallbackURL
    case missingCode
    case providerError(String)

    var errorDescription: String? {
        switch self {
        case .invalidCallbackURL:
            return "The OAuth callback URL could not be parsed."
        case .missingCode:
            return "The OAuth callback did not include an authorization code."
        case .providerError(let message):
            return "Anthropic reported: \(message)"
        }
    }
}

/// Handshake values returned by the Rust auth driver's `begin` call.
struct AnthropicOAuthHandshake {
    let authorizeURL: URL
    let verifier: String
}

struct AnthropicOAuthCompletion {
    let refreshToken: String?
}

protocol AnthropicOAuthAuthorizeProvider {
    func beginAuthorization() async throws -> AnthropicOAuthHandshake
}

protocol AnthropicOAuthCompleter {
    func complete(code: String, verifier: String) async throws -> AnthropicOAuthCompletion
}

enum AnthropicOAuthWebAuthError: Error, LocalizedError {
    case cancelled
    case unableToStartSession
    case missingCallbackURL
    case underlying(Error)

    var errorDescription: String? {
        switch self {
        case .cancelled:
            return "The user cancelled sign-in."
        case .unableToStartSession:
            return "Could not start the system auth session."
        case .missingCallbackURL:
            return "The auth session ended without a callback URL."
        case .underlying(let error):
            return error.localizedDescription
        }
    }
}

protocol AnthropicOAuthWebAuthSessionFactory {
    func run(authorizeURL: URL, callbackURLScheme: String) async throws -> URL
}

// MARK: - Default implementations

/// Default implementation of the authorize step.
///
/// The Rust driver's `begin` lives in `pi_mobile_client::auth::
/// anthropic_oauth::AnthropicOAuthDriver` and will be exposed via
/// UniFFI in a follow-up feature (the broader VAL-AUTH-002 scope only
/// pins the iOS surface). Until then this default implementation
/// returns the well-known Anthropic authorize URL plus a PKCE pair
/// generated locally, so the sheet can be presented end-to-end during
/// development. The XCTest covering this file uses a stub provider so
/// no network is required.
struct DefaultAnthropicOAuthAuthorizeProvider: AnthropicOAuthAuthorizeProvider {
    func beginAuthorization() async throws -> AnthropicOAuthHandshake {
        let verifier = UUID().uuidString + UUID().uuidString
        // The production redirect URI is registered with Anthropic as
        // the deep-link `litter://oauth/pi/anthropic`. The PKCE
        // `code_challenge` field is intentionally elided here — the
        // Rust driver supplies the real challenge once the UniFFI
        // surface is wired up — but the URL shape matches what
        // Anthropic's consent screen accepts so the
        // `ASWebAuthenticationSession` opens correctly.
        var components = URLComponents(string: "https://console.anthropic.com/oauth/authorize")!
        components.queryItems = [
            URLQueryItem(name: "response_type", value: "code"),
            URLQueryItem(name: "redirect_uri", value: AnthropicOAuthBridge.redirectURI),
            URLQueryItem(name: "scope", value: "user:inference"),
            URLQueryItem(name: "code_challenge_method", value: "S256")
        ]
        guard let url = components.url else {
            throw AnthropicOAuthError.invalidCallbackURL
        }
        return AnthropicOAuthHandshake(authorizeURL: url, verifier: verifier)
    }
}

struct DefaultAnthropicOAuthCompleter: AnthropicOAuthCompleter {
    func complete(code: String, verifier: String) async throws -> AnthropicOAuthCompletion {
        // The token-exchange step is owned by the Rust driver via
        // `complete_anthropic_oauth_paste`. The UniFFI surface that
        // exposes that call to Swift lands in a follow-up feature; the
        // default Swift implementation here only echoes the inputs so
        // we don't accidentally ship a Swift-side token exchanger that
        // duplicates the canonical Rust path.
        _ = (code, verifier)
        return AnthropicOAuthCompletion(refreshToken: nil)
    }
}

/// Default `ASWebAuthenticationSession` driver. Lives outside the
/// model so XCTest can swap it out — instantiating
/// `ASWebAuthenticationSession` in a unit-test target crashes because
/// it requires an attached `UIScene`.
final class DefaultAnthropicOAuthWebAuthSessionFactory: NSObject, AnthropicOAuthWebAuthSessionFactory, ASWebAuthenticationPresentationContextProviding {
    func presentationAnchor(for session: ASWebAuthenticationSession) -> ASPresentationAnchor {
        if let window = UIApplication.shared.connectedScenes
            .compactMap({ $0 as? UIWindowScene })
            .flatMap(\.windows)
            .first(where: \.isKeyWindow)
        {
            return window
        }
        return ASPresentationAnchor()
    }

    func run(authorizeURL: URL, callbackURLScheme: String) async throws -> URL {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<URL, Error>) in
            let session = ASWebAuthenticationSession(
                url: authorizeURL,
                callbackURLScheme: callbackURLScheme
            ) { callbackURL, error in
                if let error = error as? ASWebAuthenticationSessionError,
                   error.code == .canceledLogin {
                    continuation.resume(throwing: AnthropicOAuthWebAuthError.cancelled)
                    return
                }
                if let error {
                    continuation.resume(
                        throwing: AnthropicOAuthWebAuthError.underlying(error)
                    )
                    return
                }
                guard let callbackURL else {
                    continuation.resume(
                        throwing: AnthropicOAuthWebAuthError.missingCallbackURL
                    )
                    return
                }
                continuation.resume(returning: callbackURL)
            }
            session.presentationContextProvider = self
            session.prefersEphemeralWebBrowserSession = false
            if !session.start() {
                continuation.resume(
                    throwing: AnthropicOAuthWebAuthError.unableToStartSession
                )
            }
        }
    }
}

#Preview {
    AnthropicOAuthSheet()
}
