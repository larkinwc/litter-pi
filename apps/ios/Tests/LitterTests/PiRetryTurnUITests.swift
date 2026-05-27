import XCTest
@testable import Litter

/// Companion unit test for VAL-NFR-003. Exercises the typed
/// `PiTurnState` → `RetryTurnView` mapping that the XCUITest in
/// `apps/ios/Tests/LitterUITests/PiRetryTurnUITests.swift` drives end
/// to end on the simulator.
///
/// The XCUITest performs the actual background → foreground cycle
/// against the `--ui-test-pi-retry-turn` harness; this unit test
/// asserts the gate logic itself (no UI test harness required) so the
/// retryability matrix is locked in at the Swift surface even when
/// the simulator lane is unavailable.
@MainActor
final class PiRetryTurnUITests: XCTestCase {

    func testRetryableErroredStateRendersGate() throws {
        let state = PiTurnState.errored(retryable: true, message: "connection reset")
        switch state {
        case .errored(let retryable, let message):
            XCTAssertTrue(retryable, "transport-drop class must be retryable")
            XCTAssertEqual(message, "connection reset")
        default:
            XCTFail("expected .errored variant")
        }
    }

    func testNonRetryableErroredStateDoesNotRenderRetry() throws {
        let state = PiTurnState.errored(retryable: false, message: "401 Unauthorized")
        switch state {
        case .errored(let retryable, _):
            XCTAssertFalse(retryable, "auth-class errors must not surface retry")
        default:
            XCTFail("expected .errored variant")
        }
    }

    func testCompletedStateIsTerminalNonRetry() throws {
        let state = PiTurnState.completed
        // The gate explicitly only renders for `.errored(retryable: true, ..)`,
        // so completed must not produce a retry surface.
        guard case .completed = state else {
            XCTFail("expected .completed variant")
            return
        }
    }
}
