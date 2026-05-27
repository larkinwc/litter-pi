package com.litter.android.ui.conversation

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/**
 * Host-side parity test for VAL-NFR-003. Asserts the `pi.turn.retry`
 * test tag is present when the Rust runtime emits an
 * `Errored { retryable: true, .. }` state and absent otherwise.
 *
 * The XCUI side of this assertion lives in
 * `apps/ios/Tests/LitterUITests/PiRetryTurnUITests.swift`; this
 * unit test is the Android host-side parity check. The full Compose
 * instrumentation test remains deferred per the Android UI deferral
 * documented in `apps/android/docs/qa-matrix.md`.
 */
class RetryTurnRowDecisionTest {
    @Test
    fun erroredRetryableStateExposesTestTag() {
        assertEquals(
            "pi.turn.retry",
            PiRetryTurnRowDecision.visibleTestTag("Errored", retryable = true),
        )
    }

    @Test
    fun erroredNonRetryableStateHidesTestTag() {
        assertNull(PiRetryTurnRowDecision.visibleTestTag("Errored", retryable = false))
    }

    @Test
    fun nonErroredStatesHideTestTag() {
        assertNull(PiRetryTurnRowDecision.visibleTestTag("Idle", retryable = null))
        assertNull(PiRetryTurnRowDecision.visibleTestTag("Streaming", retryable = null))
        assertNull(PiRetryTurnRowDecision.visibleTestTag("Completed", retryable = null))
    }
}
