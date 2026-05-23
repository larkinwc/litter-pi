import Foundation

/// Thin Swift facade over the Rust `AppClient` pi-runtime UniFFI surface
/// (`subscribePiEvents` / `sendPiPrompt`).
///
/// Keeps Swift code free of any wire-format parsing per AGENTS.md
/// "Drift Guardrails" — every event arriving from Rust is already the
/// typed `PiEvent` UniFFI enum.
final class RustPiRuntimeBridge: @unchecked Sendable {
    /// Errors surfaced to UI from the pi-runtime surface. These map
    /// 1:1 to the Rust `PiRuntimeError` UniFFI variants.
    enum BridgeError: Error, LocalizedError {
        case noPiRuntime(serverId: String)
        case channelFull
        case closed
        case underlying(String)

        var errorDescription: String? {
            switch self {
            case .noPiRuntime(let id):
                return "No pi runtime registered for server \(id)"
            case .channelFull:
                return "Pi runtime command channel is full"
            case .closed:
                return "Pi runtime is shutting down"
            case .underlying(let message):
                return message
            }
        }
    }

    private let client: AppClient

    init(client: AppClient = AppClient()) {
        self.client = client
    }

    /// Forward `text` to the pi runtime backing `serverId`. The Rust
    /// side validates that the session was started via
    /// `connectLocalPi*` and surfaces a typed `PiRuntimeError` if not.
    func sendPrompt(serverId: String, text: String) throws {
        do {
            try client.sendPiPrompt(serverId: serverId, text: text)
        } catch let error as PiRuntimeError {
            throw Self.mapError(error)
        } catch {
            throw BridgeError.underlying(error.localizedDescription)
        }
    }

    /// Subscribe to the typed `PiEvent` stream for `serverId`. The
    /// caller retains the returned `PiEventSubscription` for the
    /// lifetime of the observation; dropping it (or calling `cancel`)
    /// tears the underlying Rust pump down.
    func subscribe(
        serverId: String,
        onEvent: @Sendable @escaping (PiEvent) -> Void
    ) throws -> PiEventSubscription {
        let listener = PiEventListenerAdapter(onEvent: onEvent)
        do {
            return try client.subscribePiEvents(serverId: serverId, listener: listener)
        } catch let error as PiRuntimeError {
            throw Self.mapError(error)
        } catch {
            throw BridgeError.underlying(error.localizedDescription)
        }
    }

    private static func mapError(_ error: PiRuntimeError) -> BridgeError {
        switch error {
        case .NoPiRuntime(let serverId):
            return .noPiRuntime(serverId: serverId)
        case .ChannelFull:
            return .channelFull
        case .Closed:
            return .closed
        }
    }
}

/// UniFFI callback adapter that forwards each typed `PiEvent` to a
/// Swift closure on the same dispatch context the listener is
/// invoked on (the Rust pump task). Callers that need to bounce onto
/// the main actor should hop inside `onEvent`.
private final class PiEventListenerAdapter: PiEventListener, @unchecked Sendable {
    private let handler: @Sendable (PiEvent) -> Void

    init(onEvent: @Sendable @escaping (PiEvent) -> Void) {
        self.handler = onEvent
    }

    func onEvent(event: PiEvent) {
        handler(event)
    }
}
