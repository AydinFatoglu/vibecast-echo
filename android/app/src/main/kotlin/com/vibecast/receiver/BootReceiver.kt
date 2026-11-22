package com.vibecast.receiver

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.os.Build
import android.util.Log

/**
 * Brings the receiver up by itself after a device restart.
 *
 * On older devices it starts [CastReceiverService] and opens [MainActivity], because the
 * rendering surface lives in the activity: a service alone would accept a
 * session with nowhere to draw it. On API 25 there are no background
 * activity-start restrictions, so this works unattended — which is the point,
 * since a device left on a shelf is never going to have its launcher opened.
 */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(
        context: Context,
        intent: Intent,
    ) {
        if (intent.action != Intent.ACTION_BOOT_COMPLETED) return
        if (!CastReceiverService.hasLocalNetworkPermission(context)) return
        Log.i(TAG, "boot completed: starting receiver")
        CastReceiverService.start(context)
        // Android 10+ does not permit opening the player UI from a boot broadcast.
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) {
            context.startActivity(
                Intent(context, MainActivity::class.java)
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            )
        }
    }

    private companion object {
        const val TAG = "vibecast"
    }
}
