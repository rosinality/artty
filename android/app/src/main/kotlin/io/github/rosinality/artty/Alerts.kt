package io.github.rosinality.artty

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent

/**
 * Notifications of what needs the user — an agent asking, or done — that the server sends
 * this app while its user is here rather than at another app (or at none). Tapping one shows
 * the pane.
 */
object Alerts {
    private const val CHANNEL = "attention"
    const val SHOW = "io.github.rosinality.artty.SHOW"
    const val WINDOW = "window"
    const val PANE = "pane"

    fun post(context: Context, server: String, title: String, body: String, window: ULong?, pane: ULong?) {
        val manager = context.getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(CHANNEL, "Needs you", NotificationManager.IMPORTANCE_HIGH).apply {
                description = "An agent asking, or done, and other programs' notifications"
            },
        )
        val intent = Intent(context, MainActivity::class.java)
            .setAction(SHOW)
            .addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP)
        if (window != null && pane != null) {
            intent.putExtra(WINDOW, window.toLong()).putExtra(PANE, pane.toLong())
        }
        val id = id(pane)
        val open = PendingIntent.getActivity(
            context, id, intent,
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val notification = Notification.Builder(context, CHANNEL)
            .setSmallIcon(R.drawable.ic_connection)
            .setContentTitle("$server · $title")
            .setContentText(body)
            .setStyle(Notification.BigTextStyle().bigText(body))
            .setContentIntent(open)
            .setAutoCancel(true)
            .setCategory(Notification.CATEGORY_MESSAGE)
            .build()
        manager.notify(id, notification)
    }

    /** The pane is shown: what was said about it is seen. */
    fun seen(context: Context, pane: ULong) {
        context.getSystemService(NotificationManager::class.java).cancel(id(pane))
    }

    /** One per pane (a newer one replaces it); the connection's own is 1. */
    private fun id(pane: ULong?) = if (pane == null) 2 else 100 + (pane % 1_000_000uL).toInt()
}
