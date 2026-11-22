package com.vibecast.receiver

import android.content.Context
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.util.Log
import androidx.annotation.OptIn
import androidx.core.content.edit
import androidx.media3.common.MediaItem
import androidx.media3.common.PlaybackException
import androidx.media3.common.Player
import androidx.media3.common.util.UnstableApi
import androidx.media3.datasource.DefaultHttpDataSource
import androidx.media3.exoplayer.ExoPlayer
import androidx.media3.exoplayer.source.DefaultMediaSourceFactory
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import org.json.JSONArray
import org.json.JSONObject
import java.util.UUID
import java.util.concurrent.TimeUnit

/**
 * This device's own player, speaking the bridge's player protocol.
 *
 * It takes the place of the browser page the bridge used to serve: the same
 * WebSocket frames, but decoded by [ExoPlayer] instead of by a WebView. That
 * substitution is the point — a native player fetches media itself, so a CDN
 * that answers without CORS headers (YouTube's does not) can no longer have the
 * request blocked before it is even sent.
 *
 * [ExoPlayer] must be touched only from the thread that built it, so every
 * WebSocket callback — which arrives on OkHttp's reader thread — is posted to
 * the main looper first.
 */
@OptIn(markerClass = [UnstableApi::class])
class BridgePlayer(
    context: Context,
    private val port: Int,
    /** Invoked on the main thread when a session hands over something to play. */
    private val onLoadRequested: () -> Unit = {},
) {
    /**
     * The HTTP source every request goes through, manifest and segment alike.
     *
     * The receiver resolves media as a particular client, and the CDN answers
     * 403 when the segments are then fetched as somebody else — so each load
     * installs that session's headers here before playback begins.
     */
    private val httpFactory =
        DefaultHttpDataSource
            .Factory()
            .setAllowCrossProtocolRedirects(true)

    /** The player to attach to a `PlayerView`. */
    val player: ExoPlayer =
        ExoPlayer
            .Builder(context)
            .setMediaSourceFactory(DefaultMediaSourceFactory(httpFactory))
            .build()

    private val main = Handler(Looper.getMainLooper())
    private val playerId = stablePlayerId(context)
    private val http =
        OkHttpClient
            .Builder()
            // The bridge is on loopback, but a ping still surfaces a socket the
            // receiver dropped on shutdown instead of waiting for a read.
            .pingInterval(PING_SECONDS, TimeUnit.SECONDS)
            .build()

    private var socket: WebSocket? = null
    private var sessionId: String? = null

    /** Last state put on the log, so a transition is printed once, not every tick. */
    private var lastReportedState: String? = null

    /** Set by [stopPlayback], so the session hears a cancellation, not an end. */
    private var cancelled = false

    /** Set by [skipToNext]: the session advances its queue on a finished video. */
    private var skipped = false

    /** Set by [stop] so a closing socket does not schedule another connect. */
    private var released = false

    private val ticker =
        object : Runnable {
            override fun run() {
                report()
                main.postDelayed(this, PROGRESS_INTERVAL_MS)
            }
        }

    init {
        player.addListener(
            object : Player.Listener {
                override fun onPlaybackStateChanged(playbackState: Int) = report()

                override fun onIsPlayingChanged(isPlaying: Boolean) = report()

                override fun onPlayerError(error: PlaybackException) {
                    Log.e(TAG, "playback failed", error)
                    report(error.errorCodeName, error.message ?: error.errorCodeName)
                }
            },
        )
    }

    /** Connect to the bridge and register as a player. */
    fun start() {
        connect()
        main.postDelayed(ticker, PROGRESS_INTERVAL_MS)
    }

    /**
     * Stop at the viewer's request: the screen goes back to idle and the
     * session is told the video was cancelled rather than finished, so the
     * phone drops its "now playing" instead of queueing the next one.
     */
    fun stopPlayback() {
        cancelled = true
        player.stop()
        player.clearMediaItems()
        report()
    }

    /**
     * Move to the next video in the session's queue.
     *
     * The bridge has no "next" command — the queue lives in the receiver, not
     * here — so the screen reports the video as finished, which is exactly the
     * signal the receiver already advances on.
     */
    fun skipToNext() {
        skipped = true
        player.stop()
        player.clearMediaItems()
        report()
    }

    /** Disconnect and release the player; the instance is not reusable. */
    fun stop() {
        released = true
        main.removeCallbacks(ticker)
        socket?.close(NORMAL_CLOSURE, null)
        socket = null
        player.release()
    }

    // --- bridge socket ----------------------------------------------------

    private fun connect() {
        if (released) return
        val request =
            Request
                .Builder()
                .url("ws://$LOOPBACK_HOST:$port/player")
                .build()
        socket = http.newWebSocket(request, Listener())
    }

    private inner class Listener : WebSocketListener() {
        override fun onOpen(
            webSocket: WebSocket,
            response: Response,
        ) {
            Log.i(TAG, "bridge connected as $playerId")
            webSocket.send(registration().toString())
        }

        override fun onMessage(
            webSocket: WebSocket,
            text: String,
        ) {
            val frame =
                runCatching { JSONObject(text) }.getOrElse { error ->
                    Log.w(TAG, "unparseable bridge frame", error)
                    return
                }
            main.post { dispatch(frame) }
        }

        override fun onFailure(
            webSocket: WebSocket,
            t: Throwable,
            response: Response?,
        ) {
            Log.w(TAG, "bridge socket failed", t)
            scheduleReconnect()
        }

        override fun onClosed(
            webSocket: WebSocket,
            code: Int,
            reason: String,
        ) {
            Log.i(TAG, "bridge socket closed ($code)")
            scheduleReconnect()
        }
    }

    private fun scheduleReconnect() {
        if (released) return
        main.postDelayed({ connect() }, RECONNECT_DELAY_MS)
    }

    private fun send(frame: JSONObject) {
        socket?.send(frame.toString())
    }

    /**
     * What this screen can actually decode.
     *
     * Deliberately narrow: an Echo Show 8 Gen1 has hardware H.264 and AAC and
     * little else, and apps pick renditions from exactly this list — claiming
     * more would hand back a stream the device then fails to play.
     */
    private fun registration(): JSONObject =
        JSONObject()
            .put("type", "register")
            .put(
                "player",
                JSONObject()
                    .put("playerId", playerId)
                    .put("name", Build.MODEL ?: "Echo")
                    .put(
                        "capabilities",
                        JSONObject()
                            .put("platform", "android")
                            .put("videoCodecs", JSONArray(listOf("h264")))
                            .put("audioCodecs", JSONArray(listOf("aac")))
                            .put(
                                "maxResolution",
                                JSONObject().put("width", SCREEN_WIDTH).put("height", SCREEN_HEIGHT),
                            ).put("frameRates", JSONArray(listOf(24, 25, 30, 50, 60))),
                    ),
            )

    // --- commands ---------------------------------------------------------

    private fun dispatch(frame: JSONObject) {
        when (val type = frame.optString("type")) {
            "load" -> load(frame)
            "play" -> player.play()
            "pause" -> player.pause()
            "seek" -> player.seekTo((frame.optDouble("position", 0.0) * MILLIS).toLong())
            "stop" -> {
                player.stop()
                player.clearMediaItems()
                sessionId = null
            }
            "volume" -> {
                val muted = frame.optBoolean("muted", false)
                player.volume = if (muted) 0f else frame.optDouble("level", 1.0).toFloat()
            }
            // Settings frames are for a player with a UI to render; this one has none.
            "settingsSnapshot", "settingsUpdateResult" -> Unit
            else -> Log.w(TAG, "ignoring unknown bridge frame: $type")
        }
    }

    private fun load(frame: JSONObject) {
        val session = frame.optString("sessionId")
        sessionId = session
        val stream = frame.optJSONObject("media")?.optJSONArray("streams")?.optJSONObject(0)
        val media = frame.optJSONObject("media")
        if (stream == null || media == null) {
            Log.e(TAG, "load command carried no stream")
            report("LOAD_FAILED", "no playable stream in load command")
            return
        }

        cancelled = false
        skipped = false
        val contentType = stream.optString("contentType").takeIf { it.isNotEmpty() }
        httpFactory.setDefaultRequestProperties(requestHeaders(stream))
        player.setMediaItem(
            MediaItem
                .Builder()
                .setUri(stream.optString("url"))
                // The manifest kind is known from the receiver, so state it
                // rather than letting ExoPlayer infer it from the URL.
                .setMimeType(contentType)
                .build(),
        )
        val startMs = (media.optDouble("startTime", 0.0) * MILLIS).toLong()
        if (startMs > 0) {
            player.seekTo(startMs)
        }
        player.playWhenReady = media.optBoolean("autoplay", true)
        player.prepare()
        Log.i(
            TAG,
            "loading $contentType for session $session " +
                "startMs=$startMs",
        )
        // There is something to show now, so whoever owns the screen should
        // claim it — a receiver that renders behind the launcher is useless.
        onLoadRequested()
    }

    /**
     * The headers the receiver wants on every upstream request for this stream.
     *
     * Always built, empty included: leaving the previous load's headers in
     * place would send one session's identity with another's URLs.
     */
    private fun requestHeaders(stream: JSONObject): Map<String, String> {
        val headers = stream.optJSONObject("headers") ?: return emptyMap()
        return headers
            .keys()
            .asSequence()
            .associateWith { key -> headers.optString(key) }
    }

    // --- reporting --------------------------------------------------------

    /** Report current state, preceded by an error frame when one is given. */
    private fun report(
        errorCode: String? = null,
        errorMessage: String? = null,
    ) {
        val session = sessionId ?: return
        if (errorCode != null) {
            send(
                JSONObject()
                    .put("type", "error")
                    .put("sessionId", session)
                    .put("code", errorCode)
                    .put("message", errorMessage.orEmpty()),
            )
        }
        val state = playerState()
        if (state != lastReportedState) {
            lastReportedState = state
            Log.i(TAG, "player state=$state position=${player.currentPosition}ms")
        }
        val frame =
            JSONObject()
                .put("type", "state")
                .put("sessionId", session)
                .put("playerState", state)
                .put("currentTime", player.currentPosition / MILLIS)
        duration()?.let { frame.put("duration", it) }
        idleReason(errorCode != null)?.let { frame.put("idleReason", it) }
        send(frame)
    }

    private fun playerState(): String =
        when {
            player.playerError != null -> "IDLE"
            player.playbackState == Player.STATE_BUFFERING -> "BUFFERING"
            player.playbackState == Player.STATE_ENDED -> "IDLE"
            player.playbackState == Player.STATE_IDLE -> "IDLE"
            player.playWhenReady -> "PLAYING"
            else -> "PAUSED"
        }

    private fun idleReason(errored: Boolean): String? =
        when {
            errored || player.playerError != null -> "ERROR"
            skipped -> "FINISHED"
            cancelled -> "CANCELLED"
            player.playbackState == Player.STATE_ENDED -> "FINISHED"
            else -> null
        }

    /** Duration in seconds, or null while it is still unknown. */
    private fun duration(): Double? {
        val millis = player.duration
        return if (millis > 0) millis / MILLIS else null
    }

    private companion object {
        const val TAG = "vibecast"
        const val LOOPBACK_HOST = "127.0.0.1"
        const val MILLIS = 1000.0
        const val PROGRESS_INTERVAL_MS = 1000L
        const val RECONNECT_DELAY_MS = 1500L
        const val PING_SECONDS = 20L
        const val NORMAL_CLOSURE = 1000
        const val SCREEN_WIDTH = 1280
        const val SCREEN_HEIGHT = 800
        const val PREFS = "vibecast.player"
        const val KEY_PLAYER_ID = "player_id"

        /**
         * A player id that survives restarts, so the receiver treats a
         * reconnect as the same screen rather than as a second device.
         */
        fun stablePlayerId(context: Context): String {
            val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            prefs.getString(KEY_PLAYER_ID, null)?.let { return it }
            val id = "echo-${UUID.randomUUID()}"
            prefs.edit { putString(KEY_PLAYER_ID, id) }
            return id
        }
    }
}
