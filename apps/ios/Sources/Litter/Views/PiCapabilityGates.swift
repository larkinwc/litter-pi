import SwiftUI

/// Centralized capability gates for the pi runtime.
///
/// Per `architecture.md`, voice/realtime and the planning UI are
/// out-of-scope for pi v1. UI surfaces consult these gates so they can
/// hide the unsupported affordances whenever the active server's
/// agent runtime is pi. Server-side flag enforcement lives in Rust
/// (`connection.rs`); these gates are presentation-only.
enum PiCapabilityGates {
    /// Returns `true` when the supplied `agentRuntimeKind` identifies a
    /// pi server. `kind` matches the canonical id used by
    /// `AgentRuntimeKind::Pi` (serde rename: `"pi"`).
    static func isPiRuntime(_ agentRuntimeKind: String?) -> Bool {
        agentRuntimeKind?.lowercased() == "pi"
    }

    /// Whether voice / realtime affordances should be visible for a
    /// server whose runtime is `agentRuntimeKind`. Returns `false` for
    /// pi (no native realtime surface today) and `true` otherwise.
    static func showsVoice(for agentRuntimeKind: String?) -> Bool {
        !isPiRuntime(agentRuntimeKind)
    }

    /// Whether the planning UI (plan card, plan list, etc.) should be
    /// visible for a server whose runtime is `agentRuntimeKind`. Returns
    /// `false` for pi (pi's planning model differs enough that we don't
    /// render it inside Litter's plan card for v1) and `true` otherwise.
    static func showsPlans(for agentRuntimeKind: String?) -> Bool {
        !isPiRuntime(agentRuntimeKind)
    }
}

/// Convenience view modifier that hides its content when the active
/// server runtime kind is pi and the gate for `feature` is `false`.
///
/// Usage:
/// ```
/// VoiceOrbButton().piCapabilityGate(.voice, agentRuntimeKind: kind)
/// PlanCard().piCapabilityGate(.plans, agentRuntimeKind: kind)
/// ```
extension View {
    func piCapabilityGate(
        _ feature: PiCapabilityFeature,
        agentRuntimeKind: String?
    ) -> some View {
        let visible: Bool = {
            switch feature {
            case .voice: return PiCapabilityGates.showsVoice(for: agentRuntimeKind)
            case .plans: return PiCapabilityGates.showsPlans(for: agentRuntimeKind)
            }
        }()
        return Group {
            if visible {
                self
            } else {
                EmptyView()
            }
        }
    }
}

/// Features the pi capability gate covers.
enum PiCapabilityFeature {
    case voice
    case plans
}
