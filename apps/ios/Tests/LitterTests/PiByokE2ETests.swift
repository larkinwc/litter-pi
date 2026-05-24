#if PI_TEST_INJECTION
import XCTest
@testable import Litter

/// End-to-end BYOK XCTest harness backing VAL-IOS-PI-011 and
/// VAL-IOS-PI-012.
///
/// Each test case wires the in-process pi runtime through a stub
/// `PiIshExec` (so no real iSH kernel boot is required), subscribes
/// to the typed `PiEvent` stream, drives a single user turn, and
/// asserts:
///
/// * an `assistantText` event was received,
/// * a `turnComplete` event was received,
/// * (VAL-IOS-PI-011) at least one shell tool call routed through
///   the stub `PiIshExec`,
/// * (VAL-IOS-PI-012) the configured base URL is the one returned
///   by `AppClient.pi_active_base_url(server_id)`.
///
/// The test-injection surface (`connectLocalPiByokWithIshExec`,
/// `piActiveBaseUrl`, `PiIshExec`) is exposed only when the
/// `test-injection` cargo feature is enabled. Build the simulator
/// staticlib with `make rust-ios-sim-test-injection` before running
/// these tests; production lanes (`make rust-ios-sim-fast`) compile
/// without those symbols.
///
/// Credentials are read from the process environment (typically
/// sourced from `.env` and forwarded by the orchestrator); the tests
/// `XCTSkip` cleanly when the relevant credentials are absent. See
/// `library/environment.md` for the canonical credential matrix.
@MainActor
final class PiByokE2ETests: XCTestCase {

    /// VAL-IOS-PI-011: BYOK Anthropic end-to-end. Sends a single turn
    /// that should produce assistant text and a shell tool call; the
    /// stub `PiIshExec` records the call so we can assert the tool
    /// exec was routed through it (rather than the real iSH kernel).
    func testBYOKAnthropicTurnRoundTrip() async throws {
        let apiKey = try Self.requireEnv(
            "ANTHROPIC_API_KEY",
            assertion: "VAL-IOS-PI-011"
        )
        let baseURL = ProcessInfo.processInfo.environment["ANTHROPIC_BASE_URL"]

        let stub = RecordingPiIshExec(
            stdout: Data("bin\netc\nhome\nroot\nusr\n".utf8),
            exitCode: 0
        )
        let serverId = "pi-byok-e2e-anthropic-\(UUID().uuidString)"
        let outcome = try await Self.runTurn(
            serverId: serverId,
            provider: "anthropic",
            apiKey: apiKey,
            baseURL: baseURL,
            model: nil,
            stub: stub,
            prompt: "say hi and run ls /root"
        )

        XCTAssertTrue(
            outcome.sawAssistantText,
            "BYOK Anthropic turn must produce at least one PiEvent.assistantText"
        )
        XCTAssertTrue(
            outcome.sawTurnComplete,
            "BYOK Anthropic turn must terminate with PiEvent.turnComplete"
        )
        XCTAssertGreaterThan(
            stub.calls.count, 0,
            "BYOK Anthropic turn must route at least one tool exec through the stub PiIshExec (VAL-IOS-PI-011)"
        )
        XCTAssertFalse(
            ishIsKernelBooted(),
            "Stub PiIshExec must absorb tool exec; the real iSH kernel must remain unbooted (VAL-IOS-PI-011)"
        )
    }

    /// VAL-IOS-PI-012: BYOK OpenAI-compatible end-to-end against a
    /// custom `OPENAI_BASE_URL`. Asserts the configured base URL is
    /// the one observable via `AppClient.pi_active_base_url`.
    func testBYOKOpenAICompatibleTurnRoundTrip() async throws {
        let apiKey = try Self.requireEnv(
            "OPENAI_API_KEY",
            assertion: "VAL-IOS-PI-012"
        )
        let baseURL = try Self.requireEnv(
            "OPENAI_BASE_URL",
            assertion: "VAL-IOS-PI-012"
        )
        // Many OpenAI-compatible hosts only expose a subset of model
        // names; allow the test environment to override the default.
        let model = ProcessInfo.processInfo.environment["OPENAI_MODEL"]

        let stub = RecordingPiIshExec(
            stdout: Data("bin\netc\nhome\nroot\nusr\n".utf8),
            exitCode: 0
        )
        let serverId = "pi-byok-e2e-openai-\(UUID().uuidString)"
        let outcome = try await Self.runTurn(
            serverId: serverId,
            provider: "openai",
            apiKey: apiKey,
            baseURL: baseURL,
            model: model,
            stub: stub,
            prompt: "say hi and run ls /root"
        )

        XCTAssertTrue(
            outcome.sawAssistantText,
            "BYOK OpenAI-compatible turn must produce at least one PiEvent.assistantText"
        )
        XCTAssertTrue(
            outcome.sawTurnComplete,
            "BYOK OpenAI-compatible turn must terminate with PiEvent.turnComplete"
        )

        let active = AppClient().piActiveBaseUrl(serverId: serverId)
        XCTAssertEqual(
            active, baseURL,
            "pi_active_base_url must return the configured OPENAI_BASE_URL (VAL-IOS-PI-012)"
        )
        XCTAssertFalse(
            ishIsKernelBooted(),
            "Stub PiIshExec must absorb tool exec; the real iSH kernel must remain unbooted (VAL-IOS-PI-012)"
        )
    }

    // MARK: - Helpers

    /// Aggregate of what a single turn observed via the typed
    /// `PiEvent` stream. We only care about reachability of the two
    /// terminal-ish event types here; finer-grained transcript
    /// validation lives in the user-testing-validator flow.
    private struct TurnOutcome {
        var sawAssistantText: Bool = false
        var sawTurnComplete: Bool = false
    }

    /// Skip-with-pointer helper for missing credentials.
    private static func requireEnv(
        _ name: String,
        assertion: String
    ) throws -> String {
        let env = ProcessInfo.processInfo.environment
        if let value = env[name], !value.isEmpty {
            return value
        }
        throw XCTSkip(
            "\(assertion): \(name) is not set in the test environment. " +
            "Populate it from the repo .env and forward it to xcodebuild. " +
            "See library/environment.md for the canonical credential matrix."
        )
    }

    /// Drive a single BYOK pi turn through the test-injection
    /// `connect_local_pi_byok_with_ish_exec` path and wait (≤60s) for
    /// the terminal `turnComplete`/`error` event.
    private static func runTurn(
        serverId: String,
        provider: String,
        apiKey: String,
        baseURL: String?,
        model: String?,
        stub: RecordingPiIshExec,
        prompt: String
    ) async throws -> TurnOutcome {
        let client = AppClient()

        // Wire the stub IshExec via the test-injection constructor.
        let returnedId = try await client.connectLocalPiByokWithIshExec(
            serverId: serverId,
            displayName: "pi BYOK e2e (\(provider))",
            provider: provider,
            apiKey: apiKey,
            baseUrl: baseURL,
            model: model,
            ishExec: stub
        )
        XCTAssertEqual(returnedId, serverId, "connect must echo serverId")

        // Subscribe before sending the prompt so no events are lost.
        let collector = PiEventCollector()
        let subscription = try client.subscribePiEvents(
            serverId: serverId,
            listener: collector
        )
        defer { subscription.cancel() }

        try client.sendPiPrompt(serverId: serverId, text: prompt)

        // Wait up to ~60s for the turn to terminate.
        let deadline = Date().addingTimeInterval(60)
        while Date() < deadline {
            if collector.isTerminal {
                break
            }
            try await Task.sleep(nanoseconds: 200_000_000)
        }

        var outcome = TurnOutcome()
        let observed = collector.snapshot()
        for event in observed {
            switch event {
            case .assistantText, .assistantTextDelta:
                outcome.sawAssistantText = true
            case .turnComplete:
                outcome.sawTurnComplete = true
            case .error(let message):
                XCTFail("Pi runtime emitted PiEvent.error: \(message)")
            default:
                break
            }
        }
        return outcome
    }
}

/// Capturing stub `PiIshExec` that records every `(command, cwd,
/// timeoutMs)` triple the runtime forwards and returns canned
/// output. Thread-safe via an internal lock because the UniFFI
/// callback can fire from a Rust worker thread.
private final class RecordingPiIshExec: PiIshExec, @unchecked Sendable {
    struct Call: Equatable {
        let command: String
        let cwd: String
        let timeoutMs: UInt64?
    }

    private let lock = NSLock()
    private var _calls: [Call] = []
    private let stdout: Data
    private let exitCode: Int32

    init(stdout: Data, exitCode: Int32) {
        self.stdout = stdout
        self.exitCode = exitCode
    }

    var calls: [Call] {
        lock.lock(); defer { lock.unlock() }
        return _calls
    }

    func exec(command: String, cwd: String, timeoutMs: UInt64?) -> PiIshExecOutput {
        lock.lock()
        _calls.append(Call(command: command, cwd: cwd, timeoutMs: timeoutMs))
        lock.unlock()
        return PiIshExecOutput(stdout: stdout, exitCode: exitCode)
    }
}

/// Thread-safe `PiEventListener` collector. Captures every event
/// the Rust pump forwards plus a flag for the terminal events
/// (`turnComplete` or `error`) so the test loop knows when to stop
/// polling.
private final class PiEventCollector: PiEventListener, @unchecked Sendable {
    private let lock = NSLock()
    private var events: [PiEvent] = []
    private var terminal: Bool = false

    func onEvent(event: PiEvent) {
        lock.lock()
        events.append(event)
        switch event {
        case .turnComplete, .error:
            terminal = true
        default:
            break
        }
        lock.unlock()
    }

    var isTerminal: Bool {
        lock.lock(); defer { lock.unlock() }
        return terminal
    }

    func snapshot() -> [PiEvent] {
        lock.lock(); defer { lock.unlock() }
        return events
    }
}

#endif // PI_TEST_INJECTION

