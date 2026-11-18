package io.github.dushyantchetiwal.praxis.remote

import android.Manifest
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.asSharedFlow

enum class RunAlertKind { Running, NeedsPermission, NeedsQuestion, Completed, Failed }

data class RunNotificationState(
    val channel: String,
    val deviceName: String,
    val windowId: Long?,
    val sessionId: String?,
    val title: String,
    val detail: String,
    val kind: RunAlertKind,
    val permissionToolCallId: String? = null,
    val approveOptionId: String? = null,
    val questionId: String? = null,
)

data class RunActionEvent(
    val action: String,
    val channel: String?,
    val windowId: Long?,
    val sessionId: String?,
    val toolCallId: String?,
    val optionId: String?,
    val questionId: String? = null,
)

class ActiveRunService : Service() {
    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val action = intent?.action
        when (action) {
            ACTION_APPROVE, ACTION_RETRY, ACTION_STOP_RUN -> {
                _actions.tryEmit(
                    RunActionEvent(
                        action = action,
                        channel = intent.getStringExtra(EXTRA_CHANNEL),
                        windowId = if (intent.hasExtra(EXTRA_WINDOW_ID)) {
                            intent.getLongExtra(EXTRA_WINDOW_ID, 0L)
                        } else {
                            null
                        },
                        sessionId = intent.getStringExtra(EXTRA_SESSION_ID),
                        toolCallId = intent.getStringExtra(EXTRA_TOOL_CALL_ID),
                        optionId = intent.getStringExtra(EXTRA_OPTION_ID),
                        questionId = intent.getStringExtra(EXTRA_QUESTION_ID),
                    ),
                )
                return START_NOT_STICKY
            }

            ACTION_STOP_SERVICE -> {
                stopForegroundCompat()
                stopSelf()
                return START_NOT_STICKY
            }
        }

        val state = latestState
        if (state == null || !state.kind.isOngoing) {
            stopForegroundCompat()
            stopSelf()
            return START_NOT_STICKY
        }

        ensureChannels(this)
        val notification = buildNotification(this, state, ongoing = true)
        val foregroundType = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC
        } else {
            0
        }
        val started = runCatching {
            ServiceCompat.startForeground(this, NOTIFICATION_ID_FOREGROUND, notification, foregroundType)
        }.isSuccess
        if (!started) {
            stopSelf()
        }
        return START_NOT_STICKY
    }

    private fun stopForegroundCompat() {
        runCatching {
            ServiceCompat.stopForeground(this, ServiceCompat.STOP_FOREGROUND_REMOVE)
        }
    }

    companion object {
        const val ACTION_APPROVE = "io.github.dushyantchetiwal.praxis.remote.action.APPROVE"
        const val ACTION_RETRY = "io.github.dushyantchetiwal.praxis.remote.action.RETRY"
        const val ACTION_STOP_RUN = "io.github.dushyantchetiwal.praxis.remote.action.STOP_RUN"
        const val ACTION_OPEN = "io.github.dushyantchetiwal.praxis.remote.action.OPEN"

        private const val ACTION_UPDATE = "io.github.dushyantchetiwal.praxis.remote.action.UPDATE"
        private const val ACTION_STOP_SERVICE = "io.github.dushyantchetiwal.praxis.remote.action.STOP_SERVICE"

        const val EXTRA_CHANNEL = "channel"
        const val EXTRA_WINDOW_ID = "window_id"
        const val EXTRA_SESSION_ID = "session_id"
        const val EXTRA_TOOL_CALL_ID = "tool_call_id"
        const val EXTRA_OPTION_ID = "option_id"
        const val EXTRA_QUESTION_ID = "question_id"

        const val CHANNEL_ACTIVE_RUN = "praxis_active_run"
        const val CHANNEL_RUN_ALERTS = "praxis_run_alerts"

        private const val NOTIFICATION_ID_FOREGROUND = 1001
        private const val NOTIFICATION_ID_ALERT = 1002

        private const val REQUEST_OPEN = 0
        private const val REQUEST_APPROVE = 1
        private const val REQUEST_RETRY = 2
        private const val REQUEST_STOP_RUN = 3

        @Volatile
        private var latestState: RunNotificationState? = null

        private val _actions = MutableSharedFlow<RunActionEvent>(extraBufferCapacity = 16)
        val actions: SharedFlow<RunActionEvent> = _actions.asSharedFlow()

        private val RunAlertKind.isOngoing: Boolean
            get() = this == RunAlertKind.Running ||
                this == RunAlertKind.NeedsPermission ||
                this == RunAlertKind.NeedsQuestion

        fun canPostNotifications(context: Context): Boolean {
            if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return true
            return ContextCompat.checkSelfPermission(
                context,
                Manifest.permission.POST_NOTIFICATIONS,
            ) == PackageManager.PERMISSION_GRANTED
        }

        fun ensureChannels(context: Context) {
            val manager = context.getSystemService(NotificationManager::class.java) ?: return
            val activeChannel = NotificationChannel(
                CHANNEL_ACTIVE_RUN,
                "Active Praxis runs",
                NotificationManager.IMPORTANCE_LOW,
            ).apply {
                description = "Shows when a Praxis agent run is active in the background."
                setShowBadge(false)
            }
            val alertChannel = NotificationChannel(
                CHANNEL_RUN_ALERTS,
                "Praxis run alerts",
                NotificationManager.IMPORTANCE_DEFAULT,
            ).apply {
                description = "Alerts when Praxis needs approval, an answer, or finishes a run."
            }
            manager.createNotificationChannel(activeChannel)
            manager.createNotificationChannel(alertChannel)
        }

        fun startOrUpdate(context: Context, state: RunNotificationState) {
            val appContext = context.applicationContext
            latestState = state
            ensureChannels(appContext)
            val manager = appContext.getSystemService(NotificationManager::class.java)

            if (state.kind.isOngoing) {
                manager?.cancel(NOTIFICATION_ID_ALERT)
                val intent = Intent(appContext, ActiveRunService::class.java).setAction(ACTION_UPDATE)
                val started = runCatching {
                    appContext.startForegroundService(intent)
                }.isSuccess
                if (!started && canPostNotifications(appContext)) {
                    runCatching {
                        manager?.notify(
                            NOTIFICATION_ID_ALERT,
                            buildNotification(appContext, state, ongoing = false),
                        )
                    }
                }
            } else {
                stop(appContext, clearState = false)
                if (canPostNotifications(appContext)) {
                    runCatching {
                        manager?.notify(
                            NOTIFICATION_ID_ALERT,
                            buildNotification(appContext, state, ongoing = false),
                        )
                    }
                }
            }
        }

        fun stop(context: Context, clearState: Boolean = true) {
            val appContext = context.applicationContext
            if (clearState) {
                latestState = null
                appContext.getSystemService(NotificationManager::class.java)?.cancel(NOTIFICATION_ID_ALERT)
            }
            runCatching {
                appContext.stopService(Intent(appContext, ActiveRunService::class.java))
            }
        }

        private fun buildNotification(
            context: Context,
            state: RunNotificationState,
            ongoing: Boolean,
        ): Notification {
            val channelId = if (state.kind == RunAlertKind.Running) {
                CHANNEL_ACTIVE_RUN
            } else {
                CHANNEL_RUN_ALERTS
            }
            val contentTitle = state.title.ifBlank { state.deviceName }
            val contentText = buildString {
                if (state.deviceName.isNotBlank() && state.deviceName != contentTitle) {
                    append(state.deviceName)
                    if (state.detail.isNotBlank()) append(" · ")
                }
                append(state.detail)
            }.ifBlank { state.kind.defaultLabel() }

            val builder = NotificationCompat.Builder(context, channelId)
                .setSmallIcon(android.R.drawable.stat_notify_sync)
                .setContentTitle(contentTitle)
                .setContentText(contentText)
                .setStyle(NotificationCompat.BigTextStyle().bigText(contentText))
                .setContentIntent(openPendingIntent(context, state))
                .setOnlyAlertOnce(state.kind == RunAlertKind.Running)
                .setOngoing(ongoing)
                .setAutoCancel(!ongoing)
                .setCategory(
                    if (ongoing) NotificationCompat.CATEGORY_PROGRESS else NotificationCompat.CATEGORY_STATUS,
                )

            when (state.kind) {
                RunAlertKind.Running -> {
                    builder.setProgress(0, 0, true)
                    builder.addAction(
                        0,
                        "Stop",
                        serviceActionPendingIntent(context, ACTION_STOP_RUN, REQUEST_STOP_RUN, state),
                    )
                }

                RunAlertKind.NeedsPermission -> {
                    if (!state.permissionToolCallId.isNullOrBlank() && !state.approveOptionId.isNullOrBlank()) {
                        builder.addAction(
                            0,
                            "Approve",
                            serviceActionPendingIntent(context, ACTION_APPROVE, REQUEST_APPROVE, state),
                        )
                    }
                    builder.addAction(
                        0,
                        "Stop",
                        serviceActionPendingIntent(context, ACTION_STOP_RUN, REQUEST_STOP_RUN, state),
                    )
                }

                RunAlertKind.NeedsQuestion -> Unit

                RunAlertKind.Completed -> Unit

                RunAlertKind.Failed -> {
                    builder.addAction(
                        0,
                        "Retry",
                        serviceActionPendingIntent(context, ACTION_RETRY, REQUEST_RETRY, state),
                    )
                }
            }

            return builder.build()
        }

        private fun openPendingIntent(context: Context, state: RunNotificationState): PendingIntent {
            val intent = Intent(context, MainActivity::class.java)
                .setAction(ACTION_OPEN)
                .addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP)
                .putStateExtras(state)
            return PendingIntent.getActivity(
                context,
                REQUEST_OPEN,
                intent,
                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
            )
        }

        private fun serviceActionPendingIntent(
            context: Context,
            action: String,
            requestCode: Int,
            state: RunNotificationState,
        ): PendingIntent {
            val intent = Intent(context, ActiveRunService::class.java)
                .setAction(action)
                .putStateExtras(state)
            return PendingIntent.getService(
                context,
                requestCode,
                intent,
                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
            )
        }

        private fun Intent.putStateExtras(state: RunNotificationState): Intent = apply {
            putExtra(EXTRA_CHANNEL, state.channel)
            state.windowId?.let { putExtra(EXTRA_WINDOW_ID, it) }
            state.sessionId?.let { putExtra(EXTRA_SESSION_ID, it) }
            state.permissionToolCallId?.let { putExtra(EXTRA_TOOL_CALL_ID, it) }
            state.approveOptionId?.let { putExtra(EXTRA_OPTION_ID, it) }
            state.questionId?.let { putExtra(EXTRA_QUESTION_ID, it) }
        }

        private fun RunAlertKind.defaultLabel(): String = when (this) {
            RunAlertKind.Running -> "Agent run in progress"
            RunAlertKind.NeedsPermission -> "Permission required"
            RunAlertKind.NeedsQuestion -> "Question waiting for reply"
            RunAlertKind.Completed -> "Run completed"
            RunAlertKind.Failed -> "Run failed"
        }
    }
}
