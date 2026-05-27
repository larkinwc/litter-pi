import XCTest
@testable import Litter

/// Verifies VAL-AUTH-002 / VAL-AUTH-004 wiring on the iOS side:
/// the OAuth sheet's view model loads without crashing, the
/// Authorization Code flow is driven through injectable
/// collaborators (so the underlying `ASWebAuthenticationSession`
/// stays the production path), and a successful sign-in mirrors
/// the refresh token into the Keychain wrapper.
@MainActor
final class AnthropicOAuthSheetTests: XCTestCase {

    func testViewModelLoadsWithDefaultCollaborators() {
        // Construct with the production defaults to ensure none of
        // the default initializers crash at load time. The model
        // doesn't open `ASWebAuthenticationSession` until
        // `beginAuthorization` is invoked, so this stays safe in a
        // unit-test target.
        let model = AnthropicOAuthSheetModel()
        XCTAssertFalse(model.isInFlight)
        XCTAssertNil(model.statusMessage)
        XCTAssertFalse(model.descriptionText.isEmpty)
    }

    func testRedirectURIMatchesContractedScheme() {
        XCTAssertEqual(
            AnthropicOAuthBridge.redirectURI,
            "litter://oauth/pi/anthropic"
        )
        XCTAssertEqual(
            AnthropicOAuthBridge.callbackURLScheme,
            "litter"
        )
    }

    func testExtractAuthorizationCodeReturnsCodeQueryItem() throws {
        let url = try XCTUnwrap(
            URL(string: "litter://oauth/pi/anthropic?code=abc123&state=xyz")
        )
        let code = try AnthropicOAuthSheetModel.extractAuthorizationCode(from: url)
        XCTAssertEqual(code, "abc123")
    }

    func testExtractAuthorizationCodeSurfacesProviderError() {
        let url = URL(string: "litter://oauth/pi/anthropic?error=access_denied")!
        XCTAssertThrowsError(
            try AnthropicOAuthSheetModel.extractAuthorizationCode(from: url)
        ) { error in
            guard case AnthropicOAuthError.providerError(let message) = error else {
                XCTFail("Expected providerError, got \(error)")
                return
            }
            XCTAssertEqual(message, "access_denied")
        }
    }

    func testExtractAuthorizationCodeFailsWithoutCode() {
        let url = URL(string: "litter://oauth/pi/anthropic?state=xyz")!
        XCTAssertThrowsError(
            try AnthropicOAuthSheetModel.extractAuthorizationCode(from: url)
        ) { error in
            guard case AnthropicOAuthError.missingCode = error else {
                XCTFail("Expected missingCode, got \(error)")
                return
            }
        }
    }

    func testBeginAuthorizationPersistsRefreshTokenThroughKeychainShim() async throws {
        let stubAuthorize = StubAuthorizeProvider(
            handshake: AnthropicOAuthHandshake(
                authorizeURL: URL(string: "https://console.anthropic.com/oauth/authorize?response_type=code")!,
                verifier: "verifier-token"
            )
        )
        let stubWebAuth = StubWebAuthSessionFactory(
            callbackURL: URL(string: "litter://oauth/pi/anthropic?code=auth-code-123")!
        )
        let stubCompleter = StubCompleter(
            completion: AnthropicOAuthCompletion(refreshToken: "refresh-token-xyz")
        )

        actor PersistedTokens {
            private(set) var tokens: [String] = []
            func append(_ token: String) { tokens.append(token) }
        }
        let persisted = PersistedTokens()

        let model = AnthropicOAuthSheetModel(
            authorizeProvider: stubAuthorize,
            webAuthSessionFactory: stubWebAuth,
            completer: stubCompleter,
            keychainPersister: { token in
                Task { await persisted.append(token) }
            }
        )
        let result = await model.beginAuthorization()

        guard case let .signedIn(refreshTokenStored) = result else {
            XCTFail("Expected signedIn, got \(result)")
            return
        }
        XCTAssertTrue(refreshTokenStored)
        XCTAssertEqual(stubCompleter.lastCode, "auth-code-123")
        XCTAssertEqual(stubCompleter.lastVerifier, "verifier-token")
        XCTAssertEqual(stubWebAuth.lastCallbackURLScheme, "litter")

        // Allow the persister Task to flush.
        try await Task.sleep(nanoseconds: 50_000_000)
        let observed = await persisted.tokens
        XCTAssertEqual(observed, ["refresh-token-xyz"])
    }

    func testBeginAuthorizationReportsCancellation() async {
        let stubAuthorize = StubAuthorizeProvider(
            handshake: AnthropicOAuthHandshake(
                authorizeURL: URL(string: "https://console.anthropic.com/oauth/authorize")!,
                verifier: "v"
            )
        )
        let stubWebAuth = StubWebAuthSessionFactory(error: AnthropicOAuthWebAuthError.cancelled)
        let model = AnthropicOAuthSheetModel(
            authorizeProvider: stubAuthorize,
            webAuthSessionFactory: stubWebAuth,
            completer: StubCompleter(completion: AnthropicOAuthCompletion(refreshToken: nil)),
            keychainPersister: { _ in XCTFail("Keychain must not be touched on cancel.") }
        )
        let result = await model.beginAuthorization()
        guard case .cancelled = result else {
            XCTFail("Expected cancelled, got \(result)")
            return
        }
    }

    func testBeginAuthorizationSkipsKeychainWhenCompleterReturnsNilRefresh() async {
        // Simulates the BYOK / non-Oauth arm: the Rust driver
        // surfaces `Authorized{source: .byok}` with `refresh_token =
        // nil`, so `DefaultAnthropicOAuthCompleter` maps it to a
        // `nil` `AnthropicOAuthCompletion.refreshToken`. The model
        // must not call the Keychain persister in that case.
        let stubAuthorize = StubAuthorizeProvider(
            handshake: AnthropicOAuthHandshake(
                authorizeURL: URL(string: "https://console.anthropic.com/oauth/authorize?byok")!,
                verifier: "verifier-token"
            )
        )
        let stubWebAuth = StubWebAuthSessionFactory(
            callbackURL: URL(string: "litter://oauth/pi/anthropic?code=byok-code")!
        )
        let stubCompleter = StubCompleter(
            completion: AnthropicOAuthCompletion(refreshToken: nil)
        )
        let model = AnthropicOAuthSheetModel(
            authorizeProvider: stubAuthorize,
            webAuthSessionFactory: stubWebAuth,
            completer: stubCompleter,
            keychainPersister: { _ in
                XCTFail("Keychain persister must not run on BYOK/refreshToken=nil path")
            }
        )
        let result = await model.beginAuthorization()
        guard case let .signedIn(refreshTokenStored) = result else {
            XCTFail("Expected signedIn, got \(result)")
            return
        }
        XCTAssertFalse(refreshTokenStored)
    }

    func testKeychainWrapperUsesPiOauthService() {
        // Smoke-test the wrapper's plumbing without touching the
        // system Keychain: instantiating it with a custom service is
        // enough to confirm the API surface compiles. The actual
        // SecItemAdd / SecItemCopyMatching calls are exercised in
        // device runs (and tracked via the VAL-AUTH-004 grep).
        let storage = AnthropicKeychainStorage(
            service: "com.larkinwc.pilitter.pi.oauth.tests",
            account: "anthropic-oauth-tests"
        )
        // `loadRefreshToken` may legitimately error on a sandboxed
        // simulator without an associated provisioning profile;
        // the test only asserts that the wrapper does not crash on
        // construction.
        _ = try? storage.loadRefreshToken()
        XCTAssertEqual(
            AnthropicKeychainStorage.defaultService,
            "com.larkinwc.pilitter.pi.oauth"
        )
    }
}

// MARK: - Stubs

private struct StubAuthorizeProvider: AnthropicOAuthAuthorizeProvider {
    let handshake: AnthropicOAuthHandshake
    func beginAuthorization() async throws -> AnthropicOAuthHandshake { handshake }
}

private final class StubWebAuthSessionFactory: AnthropicOAuthWebAuthSessionFactory {
    private let callbackURL: URL?
    private let error: Error?
    private(set) var lastCallbackURLScheme: String?

    init(callbackURL: URL) {
        self.callbackURL = callbackURL
        self.error = nil
    }

    init(error: Error) {
        self.callbackURL = nil
        self.error = error
    }

    func run(authorizeURL: URL, callbackURLScheme: String) async throws -> URL {
        lastCallbackURLScheme = callbackURLScheme
        if let error { throw error }
        return callbackURL!
    }
}

private final class StubCompleter: AnthropicOAuthCompleter {
    let completion: AnthropicOAuthCompletion
    private(set) var lastCode: String?
    private(set) var lastVerifier: String?

    init(completion: AnthropicOAuthCompletion) {
        self.completion = completion
    }

    func complete(code: String, verifier: String) async throws -> AnthropicOAuthCompletion {
        lastCode = code
        lastVerifier = verifier
        return completion
    }
}
