// Copyright (c) tyetladd | MIT License | https://github.com/tyetladd/wavo-app

package io.github.tyetladd.wavo

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.IBinder
import android.os.SystemClock
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch

/**
 * The recording while the app is left. A microphone foreground service is what keeps the
 * microphone live in the background — without one Android hands a background app silence — and its
 * notification carries the time and the pause and stop.
 *
 * It does not own the dictation: [App.dictation] does, so the screen and the notification drive one
 * instance, and a pause from either is the same pause.
 */
class RecordService : Service() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main)
    private var watching = false

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val dictation = App.instance.dictation
        when (intent?.action) {
            PAUSE -> dictation.pause()
            RESUME -> dictation.resume()
            STOP -> dictation.stop()
        }

        // Always go foreground first: a service started with startForegroundService must, even if
        // the action it carried has just ended the dictation.
        val ui = dictation.ui.value
        startForeground(ID, notification(ui), ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE)
        if (!watching) {
            watching = true
            scope.launch {
                dictation.ui.collect { now ->
                    if (now.phase == Dictation.Phase.Idle) {
                        stopForeground(STOP_FOREGROUND_REMOVE)
                        stopSelf()
                    } else {
                        manager().notify(ID, notification(now))
                    }
                }
            }
        }
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        scope.cancel()
        super.onDestroy()
    }

    private fun manager() = getSystemService(NotificationManager::class.java)

    private fun notification(ui: Dictation.Ui): Notification {
        val paused = ui.phase == Dictation.Phase.Paused
        val recording = ui.phase == Dictation.Phase.Recording
        val builder = Notification.Builder(this, CHANNEL)
            .setSmallIcon(R.drawable.ic_mic)
            .setContentTitle(
                when {
                    paused -> "Wavo — paused"
                    recording -> "Wavo — recording"
                    else -> "Wavo — transcribing"
                },
            )
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setContentIntent(open())
            .addAction(
                if (paused) R.drawable.ic_play else R.drawable.ic_pause,
                if (paused) "Resume" else "Pause",
                command(if (paused) RESUME else PAUSE),
            )
            .addAction(R.drawable.ic_stop, "Stop", command(STOP))

        if (recording) {
            // The chronometer counts from a wall-clock base shifted by the time spent paused, so it
            // shows the recording's own clock and freezes with a pause.
            builder.setWhen(System.currentTimeMillis() - ui.elapsed(SystemClock.elapsedRealtime()))
            builder.setUsesChronometer(true)
            builder.setShowWhen(true)
        } else {
            builder.setUsesChronometer(false)
            builder.setShowWhen(false)
            builder.setContentText(clock(ui.elapsed(SystemClock.elapsedRealtime())))
        }
        return builder.build()
    }

    private fun command(action: String): PendingIntent =
        PendingIntent.getService(
            this,
            action.hashCode(),
            Intent(this, RecordService::class.java).setAction(action),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )

    private fun open(): PendingIntent =
        PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )

    companion object {
        private const val ID = 1
        private const val CHANNEL = "recording"
        private const val PAUSE = "io.github.tyetladd.wavo.PAUSE"
        private const val RESUME = "io.github.tyetladd.wavo.RESUME"
        private const val STOP = "io.github.tyetladd.wavo.STOP"

        /** The channel, made once at the app's start: low importance, so it never makes a sound. */
        fun channel(context: Context) {
            context.getSystemService(NotificationManager::class.java).createNotificationChannel(
                NotificationChannel(CHANNEL, "Recording", NotificationManager.IMPORTANCE_LOW),
            )
        }

        fun start(context: Context) {
            context.startForegroundService(Intent(context, RecordService::class.java))
        }
    }
}
