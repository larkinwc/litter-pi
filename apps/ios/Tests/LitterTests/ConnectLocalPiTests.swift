import XCTest
@testable import Litter

/// Verifies the UniFFI `connect_local_pi` surface is reachable from the
/// iOS Swift layer (VAL-IOS-PI-006). The test does not exercise pi's
/// agent loop — it only asserts that booting an in-process pi runtime
/// through `AppClient.connectLocalPi` returns a non-empty session id
/// without throwing, even when no API key is configured (the in-process
/// boot path itself does not require an Anthropic key; sending a turn
/// is what would).
@MainActor
final class ConnectLocalPiTests: XCTestCase {
    func testConnectLocalPiReturnsServerHandle() async throws {
        let client = AppClient()

        // Fake-API-key configuration: we deliberately do not set
        // ANTHROPIC_API_KEY (or any other credential) for the duration
        // of this test. `connect_local_pi` must succeed regardless,
        // because credential checks only happen when a turn is sent.
        let serverId = "pi-local-test-\(UUID().uuidString)"

        let returnedId = try await client.connectLocalPi(
            serverId: serverId,
            displayName: "Local pi (test)"
        )

        XCTAssertFalse(returnedId.isEmpty, "connectLocalPi must return a non-empty handle")
        XCTAssertEqual(returnedId, serverId, "connectLocalPi should echo the caller-supplied server id")
    }
}
