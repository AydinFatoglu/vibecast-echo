package com.vibecast.receiver

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.View
import android.widget.Button
import android.widget.ImageButton
import android.widget.ImageView
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AlertDialog
import androidx.appcompat.app.AppCompatActivity
import androidx.core.content.ContextCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import androidx.media3.common.Player
import androidx.media3.ui.PlayerView

/**
 * The whole screen, and nothing on it until it is touched.
 *
 * At rest it shows either the video or the idle image; a touch brings up the
 * centered playback icons while playing, and **Kapat** while idle, which puts
 * the app in the background *without* tearing it down. Closing for real would take the
 * player with it, and the player is what makes this device a cast target.
 *
 * The [PlayerView] is this device's *own* player: [BridgePlayer] registers over
 * the bridge exactly as an external player would, and renders here. It decodes
 * natively, so media is fetched by the app rather than by a browser page — the
 * reason a CDN without CORS headers is no longer an obstacle.
 */
class MainActivity : AppCompatActivity() {
    private lateinit var idleView: ImageView
    private lateinit var playerView: PlayerView
    private lateinit var controls: View
    private lateinit var playPauseButton: ImageButton
    private lateinit var closeButton: Button
    private lateinit var privacyButton: Button

    private val notificationPermission =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) {
            CastReceiverService.start(this)
        }

    private val localNetworkPermission =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
            if (granted) {
                startReceiverWithPermissions()
            } else {
                AlertDialog
                    .Builder(this)
                    .setTitle(R.string.local_network_permission_title)
                    .setMessage(R.string.local_network_permission_message)
                    .setPositiveButton(R.string.retry_button) { _, _ -> startReceiverWithPermissions() }
                    .setNegativeButton(android.R.string.cancel, null)
                    .show()
            }
        }

    // Created when the bridge is first reported listening, so its socket never
    // opens against a port nothing is serving yet.
    private var bridgePlayer: BridgePlayer? = null

    private val main = Handler(Looper.getMainLooper())
    private val hideButtons = Runnable { showButtons(false) }

    /** Keeps the idle image and playback icon in step with the player. */
    private val playbackWatcher =
        object : Player.Listener {
            override fun onIsPlayingChanged(isPlaying: Boolean) = updateIdleImage()

            override fun onPlaybackStateChanged(playbackState: Int) = updateIdleImage()

            override fun onPlayWhenReadyChanged(
                playWhenReady: Boolean,
                reason: Int,
            ) = updatePlayPauseButton()
        }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)
        hideSystemBars()
        idleView = findViewById(R.id.idle)
        playerView = findViewById(R.id.player)
        controls = findViewById(R.id.controls)
        playPauseButton = findViewById(R.id.play_pause_button)
        closeButton = findViewById(R.id.close_button)
        privacyButton = findViewById(R.id.privacy_button)
        privacyButton.setOnClickListener {
            AlertDialog
                .Builder(this)
                .setTitle(R.string.privacy_button)
                .setMessage(R.string.privacy_policy_text)
                .setPositiveButton(android.R.string.ok, null)
                .show()
        }

        playPauseButton.setOnClickListener {
            val player = bridgePlayer?.player ?: return@setOnClickListener
            if (player.playWhenReady) player.pause() else player.play()
            showButtons(true)
        }
        findViewById<ImageButton>(R.id.next_button).setOnClickListener {
            bridgePlayer?.skipToNext()
            showButtons(false)
        }
        findViewById<ImageButton>(R.id.stop_button).setOnClickListener {
            bridgePlayer?.stopPlayback()
            showButtons(false)
            updateIdleImage()
        }
        // Not finish(): the task stays alive in the background, so the player
        // keeps its place on the bridge and the next stream can bring the
        // screen straight back (see BridgePlayer's load callback).
        closeButton.setOnClickListener { moveTaskToBack(true) }

        findViewById<View>(R.id.root).setOnClickListener { showButtons(true) }
        startReceiverWithPermissions()
    }

    override fun dispatchTouchEvent(event: MotionEvent): Boolean {
        val handled = super.dispatchTouchEvent(event)
        if (event.actionMasked == MotionEvent.ACTION_DOWN) showButtons(true)
        return handled
    }

    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        val selecting = event.keyCode == KeyEvent.KEYCODE_DPAD_CENTER || event.keyCode == KeyEvent.KEYCODE_ENTER
        val controlsHidden =
            controls.visibility != View.VISIBLE && closeButton.visibility != View.VISIBLE
        if (event.action == KeyEvent.ACTION_DOWN && selecting && controlsHidden) {
            showButtons(true)
            if (hasPicture()) playPauseButton.requestFocus() else closeButton.requestFocus()
            return true
        }
        return super.dispatchKeyEvent(event)
    }

    private fun startReceiverWithPermissions() {
        when {
            !CastReceiverService.hasLocalNetworkPermission(this) ->
                localNetworkPermission.launch(Manifest.permission.ACCESS_LOCAL_NETWORK)
            Build.VERSION.SDK_INT >= 33 &&
                ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS) !=
                PackageManager.PERMISSION_GRANTED ->
                notificationPermission.launch(Manifest.permission.POST_NOTIFICATIONS)
            else -> CastReceiverService.start(this)
        }
    }

    override fun onStart() {
        super.onStart()
        ReceiverState.setListener { render() }
        render()
    }

    override fun onStop() {
        ReceiverState.setListener(null)
        super.onStop()
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        // A system dialog or the volume panel restores the bars; take them back.
        if (hasFocus) hideSystemBars()
    }

    override fun onDestroy() {
        main.removeCallbacks(hideButtons)
        releasePlayer()
        super.onDestroy()
    }

    /** Immersive: no status or navigation bar, restored only on a swipe. */
    private fun hideSystemBars() {
        WindowCompat.setDecorFitsSystemWindows(window, false)
        WindowCompat.getInsetsController(window, window.decorView).apply {
            systemBarsBehavior = WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
            hide(WindowInsetsCompat.Type.systemBars())
        }
    }

    /** Attach this device's player to the bridge, once. */
    private fun attachPlayer() {
        if (bridgePlayer != null) return
        val player = BridgePlayer(this, Settings(this).playerPort.toInt()) { bringToFront() }
        player.player.addListener(playbackWatcher)
        playerView.player = player.player
        player.start()
        bridgePlayer = player
        updateIdleImage()
    }

    private fun releasePlayer() {
        bridgePlayer?.player?.removeListener(playbackWatcher)
        playerView.player = null
        bridgePlayer?.stop()
        bridgePlayer = null
        updateIdleImage()
    }

    /**
     * Claim the screen when playback starts.
     *
     * The same intent the launcher icon sends, because `REORDER_TO_FRONT`
     * quietly does nothing here when another app's task is on top — the
     * command is accepted and the screen never changes. The retry covers a
     * second app claiming the screen a moment later, as this device's own
     * video apps do.
     */
    private fun bringToFront() {
        claimScreen()
        main.postDelayed(::claimScreen, RECLAIM_DELAY_MS)
    }

    private fun claimScreen() {
        startActivity(
            Intent(this, MainActivity::class.java).apply {
                action = Intent.ACTION_MAIN
                addCategory(Intent.CATEGORY_LAUNCHER)
                addFlags(
                    Intent.FLAG_ACTIVITY_NEW_TASK or
                        Intent.FLAG_ACTIVITY_SINGLE_TOP or
                        Intent.FLAG_ACTIVITY_CLEAR_TOP,
                )
            },
        )
    }

    /** Show whichever controls fit the current state, then hide them again. */
    private fun showButtons(visible: Boolean) {
        main.removeCallbacks(hideButtons)
        val showing = hasPicture()
        controls.visibility = if (visible && showing) View.VISIBLE else View.GONE
        closeButton.visibility = if (visible && !showing) View.VISIBLE else View.GONE
        privacyButton.visibility = if (visible && !showing) View.VISIBLE else View.GONE
        updatePlayPauseButton()
        if (visible) {
            main.postDelayed(hideButtons, BUTTON_TIMEOUT_MS)
        }
    }

    private fun updatePlayPauseButton() {
        val playing = bridgePlayer?.player?.playWhenReady == true
        playPauseButton.setImageResource(if (playing) R.drawable.ic_pause else R.drawable.ic_play)
        playPauseButton.contentDescription =
            getString(if (playing) R.string.pause_button else R.string.play_button)
    }

    /** The idle image covers the player whenever there is no video loaded. */
    private fun updateIdleImage() {
        idleView.visibility = if (hasPicture()) View.GONE else View.VISIBLE
    }

    /**
     * Whether a video is on screen — paused counts, since the frame stays up
     * and covering it with the idle image would look like a failure.
     */
    private fun hasPicture(): Boolean {
        val player = bridgePlayer?.player ?: return false
        return player.mediaItemCount > 0 &&
            player.playbackState != Player.STATE_IDLE &&
            player.playbackState != Player.STATE_ENDED
    }

    private fun render() {
        if (ReceiverState.status == ReceiverState.RUNNING) {
            attachPlayer()
        } else {
            releasePlayer()
        }
    }

    private companion object {
        const val BUTTON_TIMEOUT_MS = 4000L
        const val RECLAIM_DELAY_MS = 2000L
    }
}
