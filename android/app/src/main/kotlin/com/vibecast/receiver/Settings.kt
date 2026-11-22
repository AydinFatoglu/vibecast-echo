package com.vibecast.receiver

import android.content.Context

/**
 * User settings backed by [android.content.SharedPreferences].
 *
 * Per-player Cast identities and their CastV2/eureka ports are now assigned
 * dynamically as players register, so only the device model and the shared
 * player-bridge port are configured here.
 */
class Settings(
    context: Context,
) {
    private val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    val model: String
        get() = prefs.getString(KEY_MODEL, DEFAULT_MODEL) ?: DEFAULT_MODEL

    val playerPort: UShort
        get() = prefs.getInt(KEY_PLAYER_PORT, DEFAULT_PLAYER_PORT).toUShort()

    private companion object {
        const val PREFS = "vibecast"
        const val KEY_MODEL = "model"
        const val KEY_PLAYER_PORT = "player_port"

        // A TV-style screen, not a Cast dongle: YouTube's phone app treats a
        // screen that calls itself a Chromecast as one backed by a Cast
        // session, and drops the session when it finds none.
        const val DEFAULT_MODEL = "Echo Show 8"
        const val DEFAULT_PLAYER_PORT = 8010
    }
}
