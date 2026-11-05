package io.github.rosinality.artty

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.IBinder

/**
 * Keeps the app running while it's connected — in the background too, so the connection
 * outlives switching apps — with an ongoing notification saying to what, and a way to
 * disconnect. The connection itself is [Session]'s.
 */
class ConnectionService : Service() {
    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == DISCONNECT) {
            Session.disconnect()
            return START_NOT_STICKY
        }
        val manager = getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(CHANNEL, "Connection", NotificationManager.IMPORTANCE_LOW).apply {
                description = "While connected to a server"
                setShowBadge(false)
            },
        )
        val open = PendingIntent.getActivity(
            this, 0,
            Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val disconnect = PendingIntent.getService(
            this, 1,
            Intent(this, ConnectionService::class.java).setAction(DISCONNECT),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val notification = Notification.Builder(this, CHANNEL)
            .setSmallIcon(R.drawable.ic_connection)
            .setContentTitle("artty")
            .setContentText(intent?.getStringExtra(TEXT) ?: "Connected")
            .setContentIntent(open)
            .setOngoing(true)
            .addAction(Notification.Action.Builder(null, "Disconnect", disconnect).build())
            .build()
        startForeground(ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
        return START_NOT_STICKY
    }

    companion object {
        const val TEXT = "text"
        private const val DISCONNECT = "io.github.rosinality.artty.DISCONNECT"
        private const val CHANNEL = "connection"
        private const val ID = 1
    }
}
