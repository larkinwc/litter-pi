package com.litter.android.ui.conversation

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import uniffi.codex_mobile_client.PiTurnState

/**
 * Android Compose counterpart of the iOS `RetryTurnView`.
 *
 * Per VAL-NFR-003, the retry button must carry the test tag
 * `pi.turn.retry` so host-side / instrumentation tests can locate it
 * when the active pi turn observes `PiTurnState.Errored(retryable =
 * true, ..)`.
 */
@Composable
fun RetryTurnRow(
    message: String,
    onRetry: () -> Unit,
    modifier: Modifier = Modifier,
) {
    Column(
        modifier = modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(8.dp))
            .background(Color.Black)
            .border(1.dp, Color(0xFFFF5C5C), RoundedCornerShape(8.dp))
            .padding(12.dp)
            .testTag("pi.turn.retry.container"),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Text(
            text = "Turn errored",
            color = Color(0xFFFF5C5C),
        )
        if (message.isNotEmpty()) {
            Text(
                text = message,
                color = Color(0xAAFFFFFF),
                modifier = Modifier.testTag("pi.turn.retry.message"),
            )
        }
        Button(
            onClick = onRetry,
            colors = ButtonDefaults.buttonColors(
                containerColor = Color(0x2E00FF9C),
                contentColor = Color(0xFF00FF9C),
            ),
            modifier = Modifier
                .testTag("pi.turn.retry")
                .semantics { contentDescription = "Retry pi turn" },
        ) {
            Text("Retry")
        }
    }
}

/**
 * Gate that renders [RetryTurnRow] only when [state] is a retryable
 * errored state. Non-retryable errors (401/403) intentionally
 * suppress the retry affordance per VAL-NFR-003.
 */
@Composable
fun PiRetryTurnGate(state: PiTurnState, onRetry: () -> Unit) {
    when (state) {
        is PiTurnState.Errored -> if (state.retryable) {
            RetryTurnRow(message = state.message, onRetry = onRetry)
        }
        else -> Unit
    }
}

/**
 * Pure-Kotlin decision helper extracted so host-side unit tests can
 * assert the retry-row visibility logic without needing the UniFFI
 * native library loaded (the Compose [PiRetryTurnGate] still owns the
 * actual render call). Returns the canonical test tag string when the
 * row would render, otherwise `null`.
 *
 * The retry surface MUST be visible only for retryable errored
 * states; everything else (Idle/Streaming/Completed/Errored with
 * `retryable=false`) returns `null` so the host UI shows nothing.
 */
object PiRetryTurnRowDecision {
    const val TEST_TAG: String = "pi.turn.retry"

    fun visibleTestTag(stateName: String, retryable: Boolean?): String? =
        if (stateName == "Errored" && retryable == true) TEST_TAG else null
}
