package com.litter.android.ui

import androidx.compose.runtime.Composable
import uniffi.codex_mobile_client.AlleycatAgentRuntimeKind

/**
 * Capability gates for the Pi runtime on Android.
 *
 * Mirrors the iOS `PiCapabilityGates` surface: server-side enforcement
 * (network, plans, voice) lives in shared Rust on the connection layer;
 * this object exists purely as a render-time gate so Android UI hides
 * surfaces that do not exist for `AlleycatAgentRuntimeKind.Pi` runtimes.
 *
 * Today Pi runs in-process with no realtime voice transport and no app
 * plans flow, so both surfaces are hidden whenever the active runtime
 * is `AlleycatAgentRuntimeKind.Pi`. All other runtimes keep their
 * existing surfaces.
 */
object PiCapabilityGates {
    /** Whether the realtime voice launcher is visible for [runtime]. */
    fun isVoiceVisible(runtime: AlleycatAgentRuntimeKind): Boolean =
        when (runtime) {
            AlleycatAgentRuntimeKind.PI -> false
            else -> true
        }

    /** Whether the app/plan surfaces are visible for [runtime]. */
    fun isPlansVisible(runtime: AlleycatAgentRuntimeKind): Boolean =
        when (runtime) {
            AlleycatAgentRuntimeKind.PI -> false
            else -> true
        }
}

/**
 * Convenience Composable wrapper that only emits [content] when the
 * current [runtime] permits the voice surface. Equivalent to checking
 * [PiCapabilityGates.isVoiceVisible] at the call site; provided so call
 * sites stay declarative.
 */
@Composable
fun PiVoiceGate(runtime: AlleycatAgentRuntimeKind, content: @Composable () -> Unit) {
    if (PiCapabilityGates.isVoiceVisible(runtime)) {
        content()
    }
}

/**
 * Convenience Composable wrapper that only emits [content] when the
 * current [runtime] permits the plans surface.
 */
@Composable
fun PiPlansGate(runtime: AlleycatAgentRuntimeKind, content: @Composable () -> Unit) {
    if (PiCapabilityGates.isPlansVisible(runtime)) {
        content()
    }
}
