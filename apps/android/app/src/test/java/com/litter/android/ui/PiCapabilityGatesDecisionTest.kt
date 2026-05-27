package com.litter.android.ui

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Host-side parity test for the production capability gate.
 *
 * iOS production composers/launchers gate their voice surfaces with
 * `PiCapabilityGates.showsVoice(for:)`. The Android composer call site
 * in `ui/conversation/ComposerBar.kt` uses the same string-based
 * helper here so that, when the active thread's runtime is `"pi"`
 * (i.e. `capabilities.voice = false`), the inline voice mic, the
 * `homeVoiceLauncher` overlay, and the handoff banner are all
 * suppressed in the production app — not just in the XCUITest
 * harness.
 *
 * The full Compose instrumentation test remains deferred per the
 * Android UI deferral documented in `apps/android/docs/qa-matrix.md`.
 * This unit test fills the same role
 * `RetryTurnRowDecisionTest.kt` plays for VAL-NFR-003: it pins the
 * filter logic so regressions surface in `./gradlew :app:testDebugUnitTest`
 * even without an Android emulator.
 */
class PiCapabilityGatesDecisionTest {
    @Test
    fun piRuntimeHidesVoiceSurface() {
        assertFalse(PiCapabilityGates.showsVoice("pi"))
    }

    @Test
    fun piRuntimeIsCaseInsensitive() {
        assertFalse(PiCapabilityGates.showsVoice("PI"))
        assertFalse(PiCapabilityGates.showsVoice(" Pi "))
    }

    @Test
    fun nonPiRuntimesShowVoiceSurface() {
        assertTrue(PiCapabilityGates.showsVoice("codex"))
        assertTrue(PiCapabilityGates.showsVoice("claude"))
        assertTrue(PiCapabilityGates.showsVoice("droid"))
        assertTrue(PiCapabilityGates.showsVoice("amp"))
    }

    @Test
    fun unknownAndMissingRuntimesDefaultToShowingVoice() {
        // Cold-start state (snapshot has no active thread / runtime
        // metadata not yet populated). The gate must be *permissive*
        // so codex/claude users do not see a flicker.
        assertTrue(PiCapabilityGates.showsVoice(null))
        assertTrue(PiCapabilityGates.showsVoice(""))
        assertTrue(PiCapabilityGates.showsVoice("   "))
        assertTrue(PiCapabilityGates.showsVoice("brand-new-agent"))
    }

    @Test
    fun isPiRuntimeMatchesIosCanonicalId() {
        assertTrue(PiCapabilityGates.isPiRuntime("pi"))
        assertFalse(PiCapabilityGates.isPiRuntime("codex"))
        assertFalse(PiCapabilityGates.isPiRuntime(null))
    }

    @Test
    fun showsPlansMirrorsShowsVoice() {
        assertFalse(PiCapabilityGates.showsPlans("pi"))
        assertTrue(PiCapabilityGates.showsPlans("codex"))
        assertTrue(PiCapabilityGates.showsPlans(null))
    }
}
