package io.github.dushyantchetiwal.praxis.remote.ui

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Bundle
import android.speech.RecognitionListener
import android.speech.RecognizerIntent
import android.speech.SpeechRecognizer
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.Mic
import androidx.compose.material.icons.filled.Stop
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.FilledTonalIconButton
import androidx.compose.material3.FilterChip
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.IconButtonDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.core.content.ContextCompat
import io.github.dushyantchetiwal.praxis.remote.data.ComposerSuggestion
import io.github.dushyantchetiwal.praxis.remote.data.ComposerSuggestionsState
import io.github.dushyantchetiwal.praxis.remote.data.TriggerKind

@Composable
fun ComposerSuggestionsPopup(
    state: ComposerSuggestionsState,
    onSelect: (ComposerSuggestion) -> Unit,
    onDismiss: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val trigger = state.trigger ?: return
    if (!state.loading && state.items.isEmpty()) return

    val scheme = MaterialTheme.colorScheme
    Surface(
        modifier = modifier.fillMaxWidth(),
        shape = RoundedCornerShape(14.dp),
        tonalElevation = 3.dp,
        shadowElevation = 4.dp,
        color = scheme.surfaceContainerHigh,
    ) {
        Column {
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(start = 12.dp, end = 4.dp, top = 4.dp, bottom = 2.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(
                    text = if (trigger.kind == TriggerKind.Mention) "Mentions" else "Skills & commands",
                    style = MaterialTheme.typography.labelMedium,
                    color = scheme.onSurfaceVariant,
                    modifier = Modifier.weight(1f),
                )
                if (state.loading) {
                    CircularProgressIndicator(
                        modifier = Modifier.size(14.dp),
                        strokeWidth = 1.5.dp,
                    )
                    Spacer(Modifier.width(4.dp))
                }
                IconButton(
                    onClick = onDismiss,
                    modifier = Modifier.size(28.dp),
                ) {
                    Icon(
                        imageVector = Icons.Filled.Close,
                        contentDescription = "Dismiss suggestions",
                        modifier = Modifier.size(16.dp),
                    )
                }
            }
            if (state.items.isNotEmpty()) {
                HorizontalDivider(color = scheme.outlineVariant.copy(alpha = 0.5f))
                LazyColumn(
                    modifier = Modifier
                        .fillMaxWidth()
                        .heightIn(max = 220.dp),
                ) {
                    items(state.items, key = { it.id }) { suggestion ->
                        Row(
                            modifier = Modifier
                                .fillMaxWidth()
                                .clickable { onSelect(suggestion) }
                                .padding(horizontal = 12.dp, vertical = 9.dp),
                            verticalAlignment = Alignment.CenterVertically,
                            horizontalArrangement = Arrangement.spacedBy(8.dp),
                        ) {
                            Surface(
                                shape = RoundedCornerShape(6.dp),
                                color = scheme.secondaryContainer,
                                contentColor = scheme.onSecondaryContainer,
                            ) {
                                Text(
                                    text = suggestion.badge,
                                    style = MaterialTheme.typography.labelSmall,
                                    modifier = Modifier.padding(horizontal = 6.dp, vertical = 2.dp),
                                )
                            }
                            Column(modifier = Modifier.weight(1f)) {
                                Text(
                                    text = suggestion.label,
                                    style = MaterialTheme.typography.bodyMedium,
                                    fontWeight = FontWeight.Medium,
                                    maxLines = 1,
                                    overflow = TextOverflow.Ellipsis,
                                )
                                if (suggestion.subtitle.isNotBlank()) {
                                    Text(
                                        text = suggestion.subtitle,
                                        style = MaterialTheme.typography.bodySmall,
                                        color = scheme.onSurfaceVariant,
                                        maxLines = 1,
                                        overflow = TextOverflow.Ellipsis,
                                    )
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

enum class VoiceInputPhase {
    Idle,
    Listening,
    Error,
}

data class VoiceInputState(
    val phase: VoiceInputPhase = VoiceInputPhase.Idle,
    val partialText: String = "",
    val steerOnFinish: Boolean = false,
    val errorMessage: String? = null,
)

class VoiceInputController internal constructor(
    val state: VoiceInputState,
    val toggleListening: () -> Unit,
    val setSteerOnFinish: (Boolean) -> Unit,
)

@Composable
fun rememberVoiceInputController(
    isGenerating: Boolean,
    onTranscript: (text: String, steer: Boolean) -> Unit,
    onError: (String) -> Unit,
): VoiceInputController {
    val context = LocalContext.current
    val latestGenerating by rememberUpdatedState(isGenerating)
    val latestOnTranscript by rememberUpdatedState(onTranscript)
    val latestOnError by rememberUpdatedState(onError)

    var state by remember { mutableStateOf(VoiceInputState()) }
    var recognizer by remember(context) { mutableStateOf<SpeechRecognizer?>(null) }

    DisposableEffect(context) {
        onDispose {
            recognizer?.destroy()
            recognizer = null
        }
    }

    fun startListening() {
        if (!SpeechRecognizer.isRecognitionAvailable(context)) {
            val message = "Speech recognition is not available on this device."
            state = VoiceInputState(phase = VoiceInputPhase.Error, errorMessage = message)
            latestOnError(message)
            return
        }

        val activeRecognizer = recognizer ?: try {
            SpeechRecognizer.createSpeechRecognizer(context).also { created ->
                created.setRecognitionListener(
                    object : RecognitionListener {
                        override fun onReadyForSpeech(params: Bundle?) {}
                        override fun onBeginningOfSpeech() {}
                        override fun onRmsChanged(rmsdB: Float) {}
                        override fun onBufferReceived(buffer: ByteArray?) {}
                        override fun onEndOfSpeech() {}
                        override fun onEvent(eventType: Int, params: Bundle?) {}

                        override fun onPartialResults(partialResults: Bundle?) {
                            val partial = partialResults
                                ?.getStringArrayList(SpeechRecognizer.RESULTS_RECOGNITION)
                                ?.firstOrNull()
                                ?.trim()
                                .orEmpty()
                            if (partial.isNotEmpty()) {
                                state = state.copy(partialText = partial)
                            }
                        }

                        override fun onResults(results: Bundle?) {
                            val transcript = results
                                ?.getStringArrayList(SpeechRecognizer.RESULTS_RECOGNITION)
                                ?.firstOrNull()
                                ?.trim()
                                ?.takeIf { it.isNotEmpty() }
                                ?: state.partialText.trim()
                            val steer = state.steerOnFinish
                            state = VoiceInputState(phase = VoiceInputPhase.Idle)
                            if (transcript.isNotEmpty()) {
                                latestOnTranscript(transcript, steer)
                            }
                        }

                        override fun onError(error: Int) {
                            val message = speechErrorMessage(error)
                            state = VoiceInputState(
                                phase = VoiceInputPhase.Error,
                                errorMessage = message,
                            )
                            latestOnError(message)
                        }
                    },
                )
                recognizer = created
            }
        } catch (exception: Exception) {
            val message = exception.message?.takeIf { it.isNotBlank() }
                ?: "Unable to initialize speech recognition."
            state = VoiceInputState(phase = VoiceInputPhase.Error, errorMessage = message)
            latestOnError(message)
            return
        }

        val intent = Intent(RecognizerIntent.ACTION_RECOGNIZE_SPEECH).apply {
            putExtra(RecognizerIntent.EXTRA_LANGUAGE_MODEL, RecognizerIntent.LANGUAGE_MODEL_FREE_FORM)
            putExtra(RecognizerIntent.EXTRA_PARTIAL_RESULTS, true)
            putExtra(RecognizerIntent.EXTRA_MAX_RESULTS, 1)
        }

        state = VoiceInputState(
            phase = VoiceInputPhase.Listening,
            partialText = "",
            steerOnFinish = latestGenerating,
            errorMessage = null,
        )

        try {
            activeRecognizer.startListening(intent)
        } catch (exception: Exception) {
            val message = exception.message?.takeIf { it.isNotBlank() }
                ?: "Unable to start voice dictation."
            state = VoiceInputState(phase = VoiceInputPhase.Error, errorMessage = message)
            latestOnError(message)
        }
    }

    val permissionLauncher = rememberLauncherForActivityResult(
        contract = ActivityResultContracts.RequestPermission(),
    ) { granted ->
        if (granted) {
            startListening()
        } else {
            val message = "Microphone permission is required for voice input."
            state = VoiceInputState(phase = VoiceInputPhase.Error, errorMessage = message)
            latestOnError(message)
        }
    }

    val toggleListening = {
        if (state.phase == VoiceInputPhase.Listening) {
            try {
                recognizer?.stopListening()
            } catch (_: Exception) {
                state = VoiceInputState(phase = VoiceInputPhase.Idle)
            }
        } else {
            val hasPermission = ContextCompat.checkSelfPermission(
                context,
                Manifest.permission.RECORD_AUDIO,
            ) == PackageManager.PERMISSION_GRANTED
            if (hasPermission) {
                startListening()
            } else {
                permissionLauncher.launch(Manifest.permission.RECORD_AUDIO)
            }
        }
    }

    return VoiceInputController(
        state = state,
        toggleListening = toggleListening,
        setSteerOnFinish = { steer ->
            if (state.phase == VoiceInputPhase.Listening) {
                state = state.copy(steerOnFinish = steer)
            }
        },
    )
}

@Composable
fun VoiceInputButton(
    isGenerating: Boolean,
    onTranscript: (text: String, steer: Boolean) -> Unit,
    onError: (String) -> Unit,
    modifier: Modifier = Modifier,
) {
    val controller = rememberVoiceInputController(
        isGenerating = isGenerating,
        onTranscript = onTranscript,
        onError = onError,
    )
    val state = controller.state
    val scheme = MaterialTheme.colorScheme

    if (state.phase == VoiceInputPhase.Listening) {
        Row(
            modifier = modifier,
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            if (isGenerating) {
                FilterChip(
                    selected = state.steerOnFinish,
                    onClick = { controller.setSteerOnFinish(!state.steerOnFinish) },
                    label = { Text("Voice steer", maxLines = 1) },
                )
            }
            FilledTonalIconButton(
                onClick = controller.toggleListening,
                colors = IconButtonDefaults.filledTonalIconButtonColors(
                    containerColor = scheme.errorContainer,
                    contentColor = scheme.onErrorContainer,
                ),
            ) {
                Icon(
                    imageVector = Icons.Filled.Stop,
                    contentDescription = "Stop voice input",
                )
            }
        }
    } else {
        IconButton(
            onClick = controller.toggleListening,
            modifier = modifier,
        ) {
            Icon(
                imageVector = Icons.Filled.Mic,
                contentDescription = if (isGenerating) "Voice steer" else "Voice input",
            )
        }
    }
}

private fun speechErrorMessage(errorCode: Int): String = when (errorCode) {
    SpeechRecognizer.ERROR_AUDIO -> "Audio recording failed. Check microphone access."
    SpeechRecognizer.ERROR_INSUFFICIENT_PERMISSIONS -> "Microphone permission is required for voice input."
    SpeechRecognizer.ERROR_NETWORK,
    SpeechRecognizer.ERROR_NETWORK_TIMEOUT -> "Speech recognition network error. Check connection."
    SpeechRecognizer.ERROR_NO_MATCH -> "No speech detected. Tap the mic to try again."
    SpeechRecognizer.ERROR_RECOGNIZER_BUSY -> "Speech recognizer is busy. Try again in a moment."
    SpeechRecognizer.ERROR_SPEECH_TIMEOUT -> "Listening timed out. Tap the mic to try again."
    else -> "Voice dictation failed (code $errorCode)."
}
