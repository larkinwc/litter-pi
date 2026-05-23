import XCTest
@testable import Litter

/// Verifies the UniFFI `connect_local_pi` + `connect_local_pi_byok`
/// surface is reachable from Swift, the returned session handle is
/// non-empty, and the BYOK overload accepts both an Anthropic-style
/// payload and an OpenAI-compatible base-URL payload (VAL-IOS-PI-007).
///
/// This test exercises the connect path itself; it does not drive a
/// real assistant turn (that lives in the user-testing-validator
/// flow VAL-IOS-PI-011/012, which provides real BYOK credentials).
/// Connecting is the per-build assertion: it proves the in-process
/// pi runtime boots, the BYOK profile is accepted by Rust, and the
/// session id is round-tripped back to Swift.
@MainActor
final class ConnectLocalPiEventsTests: XCTestCase {

    func testConnectLocalPiByokAnthropicReturnsHandle() async throws {
        let client = AppClient()

        // Fake API key — connect itself does not validate credentials;
        // only sending a turn does. The test exercises the BYOK code
        // path that the SwiftUI settings panel calls.
        let serverId = "pi-byok-anthropic-\(UUID().uuidString)"
        let returned = try await client.connectLocalPiByok(
            serverId: serverId,
            displayName: "Local pi (Anthropic BYOK test)",
            provider: "anthropic",
            apiKey: "sk-ant-fake-test-key",
            baseUrl: nil,
            model: nil
        )
        XCTAssertEqual(returned, serverId, "BYOK Anthropic connect must echo serverId")
    }

    func testConnectLocalPiByokOpenAICompatibleAcceptsBaseURL() async throws {
        let client = AppClient()

        let serverId = "pi-byok-openai-\(UUID().uuidString)"
        let customBaseURL = "https://example.test/v1"
        let returned = try await client.connectLocalPiByok(
            serverId: serverId,
            displayName: "Local pi (OpenAI-compatible BYOK test)",
            provider: "openai",
            apiKey: "sk-fake-openai-test-key",
            baseUrl: customBaseURL,
            model: "gpt-4o-mini"
        )
        XCTAssertEqual(
            returned,
            serverId,
            "BYOK OpenAI-compatible connect must echo serverId and accept the supplied base URL"
        )
    }

    func testConnectLocalPiNoByokStillBoots() async throws {
        let client = AppClient()
        let serverId = "pi-events-\(UUID().uuidString)"
        let returned = try await client.connectLocalPi(
            serverId: serverId,
            displayName: "Local pi (events test)"
        )
        XCTAssertEqual(returned, serverId)
    }
}
