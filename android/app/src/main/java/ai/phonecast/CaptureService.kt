package ai.phonecast

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.ServiceInfo
import android.graphics.Point
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.net.wifi.WifiManager
import android.os.BatteryManager
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.os.IBinder
import android.os.PowerManager
import android.util.Log
import android.view.Display
import android.view.Surface
import org.json.JSONObject

/**
 * Runs the capture while the game is in front. Everything heavy is done by
 * hardware (display compositor + video encoder); this service only moves
 * buffers, so battery and heat impact stay small.
 */
class CaptureService : Service(), StreamClient.Listener {

    private lateinit var settings: StreamSettings
    private lateinit var worker: Handler
    private lateinit var workerThread: HandlerThread

    private var projection: MediaProjection? = null
    private var virtualDisplay: VirtualDisplay? = null
    @Volatile private var encoder: VideoEncoder? = null
    private var pacer: FramePacer? = null
    private var audio: AudioCapture? = null
    private var client: StreamClient? = null
    private var wifiLock: WifiManager.WifiLock? = null

    // Adaptive bitrate: target is what the user/PC asked for, current is what
    // the link can carry right now.
    @Volatile private var targetBitrate = 0
    @Volatile private var currentBitrate = 0
    @Volatile private var lastCongestionMs = 0L

    private var lastFrames = 0L
    private var lastBytes = 0L

    // Heat protection: when Android reports the phone is getting hot, lower
    // the bitrate (and at "severe", the resolution) until it has cooled down
    // for a minute. Phones throttle the game itself otherwise.
    private var thermalLimited = false
    private var thermalCap = 0
    private var lastHotMs = 0L
    private var sizeCap = 0

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            stopSelf()
            return START_NOT_STICKY
        }
        if (projection != null || intent == null) return START_NOT_STICKY

        settings = StreamSettings.fromIntent(intent)
        // Android 14+: must be in the foreground before using the projection token.
        startForeground(
            NOTIFICATION_ID, buildNotification("Starting…"),
            ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION
        )

        val resultCode = intent.getIntExtra(EXTRA_RESULT_CODE, 0)
        val data: Intent? = if (Build.VERSION.SDK_INT >= 33) {
            intent.getParcelableExtra(EXTRA_RESULT_DATA, Intent::class.java)
        } else {
            @Suppress("DEPRECATION") intent.getParcelableExtra(EXTRA_RESULT_DATA)
        }
        if (data == null) {
            stopSelf()
            return START_NOT_STICKY
        }

        workerThread = HandlerThread("capture").apply { start() }
        worker = Handler(workerThread.looper)
        worker.post { start(resultCode, data) }
        return START_NOT_STICKY
    }

    private fun start(resultCode: Int, data: Intent) {
        try {
            val mpm = getSystemService(MediaProjectionManager::class.java)
            val proj = mpm.getMediaProjection(resultCode, data) ?: error("projection denied")
            projection = proj
            proj.registerCallback(object : MediaProjection.Callback() {
                override fun onStop() {
                    worker.post { stopSelf() }
                }

                // Android 14+: the reliable signal when a game rotates the screen.
                override fun onCapturedContentResize(width: Int, height: Int) {
                    worker.post { resizeTo(width, height, "content resize") }
                }
            }, worker)

            targetBitrate = settings.effectiveBitrate
            currentBitrate = targetBitrate

            if (settings.transport == Transport.WIFI) {
                val wm = applicationContext.getSystemService(WifiManager::class.java)
                @Suppress("DEPRECATION")
                val mode = if (Build.VERSION.SDK_INT >= 29)
                    WifiManager.WIFI_MODE_FULL_LOW_LATENCY else WifiManager.WIFI_MODE_FULL_HIGH_PERF
                wifiLock = wm.createWifiLock(mode, "PhoneCast").apply { acquire() }
            }

            val c = StreamClient(settings.connectHost, Protocol.PORT, this)
            client = c
            c.maxQueuedVideoBytes = queueLimit(targetBitrate)

            val refresh = getSystemService(DisplayManager::class.java)
                .getDisplay(Display.DEFAULT_DISPLAY).refreshRate
            pacer = runCatching { FramePacer(settings.fps, refresh) }
                .onFailure { Log.e(TAG, "frame pacer unavailable, using direct capture", it) }
                .getOrNull()

            val (w, h) = captureSize()
            requestedSize = w to h
            val enc = createEncoder(w, h)
            encoder = enc
            pacer?.setOutput(enc.inputSurface, enc.width, enc.height)
            virtualDisplay = proj.createVirtualDisplay(
                "PhoneCast", enc.width, enc.height, resources.displayMetrics.densityDpi,
                DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,
                pacer?.inputSurface ?: enc.inputSurface, null, worker
            )

            audio = runCatching {
                AudioCapture(proj, onConfig = { c.sendAudioConfig(it) }, onPcm = { c.send(it) })
            }.onFailure { Log.e(TAG, "game audio unavailable", it) }.getOrNull()

            getSystemService(DisplayManager::class.java).registerDisplayListener(displayListener, worker)
            worker.postDelayed(tick, 1000)
            status = "Waiting for PC at ${settings.connectHost}…"
            updateNotification()
        } catch (e: Exception) {
            Log.e(TAG, "start failed", e)
            status = "Error: ${e.message}"
            stopSelf()
        }
    }

    /**
     * Screen size in its current orientation, limited to maxSize, multiple of 8.
     * Orientation comes from the display rotation: on some phones getRealSize()
     * keeps reporting portrait to a background service while a game is landscape.
     */
    private fun captureSize(): Pair<Int, Int> {
        val display = getSystemService(DisplayManager::class.java).getDisplay(Display.DEFAULT_DISPLAY)
        val p = Point()
        @Suppress("DEPRECATION") display.getRealSize(p)
        val short = minOf(p.x, p.y)
        val long = maxOf(p.x, p.y)
        val landscape = display.rotation == Surface.ROTATION_90 || display.rotation == Surface.ROTATION_270
        return if (landscape) fit(long, short, limitSize()) else fit(short, long, limitSize())
    }

    /** The user's size limit, further reduced while heat protection is active. */
    private fun limitSize(): Int = when {
        sizeCap <= 0 -> settings.maxSize
        settings.maxSize <= 0 -> sizeCap
        else -> minOf(settings.maxSize, sizeCap)
    }

    /** Size asked for (before any encoder fallback), to detect real changes. */
    private var requestedSize = 0 to 0

    /** Re-creates the encoder when the captured content changes size (rotation). */
    private fun resizeTo(width: Int, height: Int, reason: String) {
        val old = encoder ?: return
        val (w, h) = fit(width, height, limitSize())
        if (w <= 0 || h <= 0 || (w to h) == requestedSize) return
        Log.i(TAG, "$reason -> ${w}x$h")
        requestedSize = w to h
        val enc = try {
            createEncoder(w, h)
        } catch (e: Exception) {
            Log.e(TAG, "resize failed", e)
            return
        }
        val p = pacer
        if (p != null) {
            p.setOutput(enc.inputSurface, enc.width, enc.height)
            virtualDisplay?.resize(enc.width, enc.height, resources.displayMetrics.densityDpi)
        } else {
            virtualDisplay?.resize(enc.width, enc.height, resources.displayMetrics.densityDpi)
            virtualDisplay?.surface = enc.inputSurface
        }
        encoder = enc
        old.stop()
    }

    private fun fit(w0: Int, h0: Int, maxSize: Int): Pair<Int, Int> {
        var w = w0
        var h = h0
        val longSide = maxOf(w, h)
        if (maxSize in 1 until longSide) {
            w = (w.toLong() * maxSize / longSide).toInt()
            h = (h.toLong() * maxSize / longSide).toInt()
        }
        return (w / 8 * 8) to (h / 8 * 8)
    }

    /** Tries the requested size, then smaller ones if the encoder refuses it. */
    private fun createEncoder(w: Int, h: Int): VideoEncoder {
        val c = client!!
        val candidates = listOf(w to h, fit(w, h, 1920), fit(w, h, 1280)).distinct()
        var last: Exception? = null
        for ((cw, ch) in candidates) {
            try {
                return VideoEncoder(
                    settings.codec, cw, ch, settings.fps, currentBitrate, paced = pacer != null,
                    onConfig = { c.sendVideoConfig(it) },
                    onFrame = { c.sendVideoFrame(it) },
                )
            } catch (e: Exception) {
                Log.w(TAG, "encoder ${cw}x$ch failed", e)
                last = e
            }
        }
        throw last ?: IllegalStateException("no encoder")
    }

    /** Fallback for Android 10–13 (no content-resize callback there). */
    private val displayListener = object : DisplayManager.DisplayListener {
        override fun onDisplayChanged(displayId: Int) {
            if (displayId != Display.DEFAULT_DISPLAY) return
            val (w, h) = captureSize()
            resizeTo(w, h, "rotation")
        }

        override fun onDisplayAdded(displayId: Int) {}
        override fun onDisplayRemoved(displayId: Int) {}
    }

    /** Once per second: adaptive bitrate + phone health report. */
    private val tick = object : Runnable {
        override fun run() {
            val enc = encoder
            val c = client
            if (enc != null && c != null) {
                val now = System.currentTimeMillis()
                val thermal = getSystemService(PowerManager::class.java).currentThermalStatus
                applyThermal(thermal, now)
                val ceiling = if (thermalLimited) minOf(targetBitrate, thermalCap) else targetBitrate
                if (currentBitrate < ceiling && now - lastCongestionMs > 3000) {
                    setCurrentBitrate(minOf(ceiling, (currentBitrate * 1.1).toInt()))
                } else if (currentBitrate > ceiling) {
                    setCurrentBitrate(ceiling)
                }
                val frames = enc.frames.get()
                val bytes = enc.bytes.get()
                val fps = frames - lastFrames
                val kbps = (bytes - lastBytes) * 8 / 1000
                lastFrames = frames
                lastBytes = bytes

                val stats = JSONObject()
                    .put("fps", fps)
                    .put("kbps", kbps)
                    .put("targetKbps", targetBitrate / 1000)
                    .put("currentKbps", currentBitrate / 1000)
                    .put("dropped", c.droppedFrames.get())
                    .put("width", enc.width)
                    .put("height", enc.height)
                    .put("codec", if (enc.codec == Protocol.CODEC_H265) "H.265" else "H.264")
                    .put("encoder", enc.encoderName)
                    .put("bitrateMode", enc.bitrateModeName)
                    .put("paced", pacer != null)
                    .put("repeated", pacer?.repeated ?: 0)
                    .put("pacer", pacer?.summary ?: "off")
                    .put("transport", settings.transport.name)
                    .put("audio", audio != null)
                    .put("thermal", thermal)
                    .put("heatLimited", thermalLimited)
                batteryInfo(stats)
                c.send(Packet(Protocol.STATS, 0, System.nanoTime() / 1000, stats.toString().toByteArray()))

                status = when {
                    !c.connected -> "Waiting for PC at ${settings.connectHost}…"
                    thermalLimited -> "Streaming ${enc.width}x${enc.height} · $fps fps · ${kbps / 1000} Mbps · lowered: phone is hot"
                    else -> "Streaming ${enc.width}x${enc.height} · $fps fps · ${kbps / 1000} Mbps"
                }
                updateNotification()
            }
            worker.postDelayed(this, 1000)
        }
    }

    /** Heat protection: PowerManager thermal status 2 = moderate, 3 = severe. */
    private fun applyThermal(thermal: Int, now: Long) {
        if (!settings.heatProtect) return
        if (thermal >= PowerManager.THERMAL_STATUS_MODERATE) {
            lastHotMs = now
            if (!thermalLimited) {
                thermalLimited = true
                thermalCap = maxOf(MIN_BITRATE, (targetBitrate * 0.6).toInt())
                Log.i(TAG, "phone hot (level $thermal): bitrate capped at ${thermalCap / 1000} kbps")
            }
            if (thermal >= PowerManager.THERMAL_STATUS_SEVERE && sizeCap == 0) {
                sizeCap = 1280
                val (w, h) = captureSize()
                resizeTo(w, h, "heat protection")
            }
        } else if (thermalLimited && now - lastHotMs > 60_000) {
            thermalLimited = false
            if (sizeCap != 0) {
                sizeCap = 0
                val (w, h) = captureSize()
                resizeTo(w, h, "cooled down")
            }
            Log.i(TAG, "phone cooled down: limits removed")
        }
    }

    /** Short vibration so you notice a lost connection mid-game. */
    private fun buzz() {
        runCatching {
            val pattern = longArrayOf(0, 120, 80, 120)
            if (Build.VERSION.SDK_INT >= 31) {
                getSystemService(android.os.VibratorManager::class.java).defaultVibrator
                    .vibrate(android.os.VibrationEffect.createWaveform(pattern, -1))
            } else {
                @Suppress("DEPRECATION")
                (getSystemService(VIBRATOR_SERVICE) as android.os.Vibrator)
                    .vibrate(android.os.VibrationEffect.createWaveform(pattern, -1))
            }
        }
    }

    private fun batteryInfo(json: JSONObject) {
        val i = registerReceiver(null, IntentFilter(Intent.ACTION_BATTERY_CHANGED)) ?: return
        val level = i.getIntExtra(BatteryManager.EXTRA_LEVEL, -1)
        val scale = i.getIntExtra(BatteryManager.EXTRA_SCALE, 100)
        json.put("battery", if (level >= 0) level * 100 / scale else -1)
        json.put("batteryTempC", i.getIntExtra(BatteryManager.EXTRA_TEMPERATURE, 0) / 10.0)
        json.put("charging", i.getIntExtra(BatteryManager.EXTRA_PLUGGED, 0) != 0)
    }

    /**
     * How much video may wait before we call it congestion. Keyframes are
     * 1–2 MB, so a small limit caused false alarms and a drop/keyframe cascade.
     * USB never really congests: allow ~300 ms. Wi-Fi: ~150 ms.
     */
    private fun queueLimit(bitrate: Int): Long {
        val seconds = if (settings.transport == Transport.USB) 0.3 else 0.15
        val floor = if (settings.transport == Transport.USB) 4L shl 20 else 2L shl 20
        return maxOf(floor, (bitrate / 8 * seconds).toLong())
    }

    private fun setCurrentBitrate(bps: Int) {
        currentBitrate = bps
        encoder?.setBitrate(bps)
    }

    // --- StreamClient.Listener (called from network threads) ---

    override fun onConnected() {
        worker.post {
            encoder?.requestKeyframe()
            status = "PC connected"
            updateNotification()
        }
    }

    override fun onDisconnected() {
        worker.post {
            status = "PC disconnected, reconnecting…"
            updateNotification()
            buzz()
        }
    }

    override fun onCongestion() {
        worker.post {
            lastCongestionMs = System.currentTimeMillis()
            setCurrentBitrate(maxOf(MIN_BITRATE, (currentBitrate * 0.75).toInt()))
            encoder?.requestKeyframe()
        }
    }

    override fun onSetBitrate(bps: Int) {
        worker.post {
            targetBitrate = bps.coerceIn(MIN_BITRATE, 150_000_000)
            client?.maxQueuedVideoBytes = queueLimit(targetBitrate)
            setCurrentBitrate(targetBitrate)
        }
    }

    override fun onKeyframeRequest() {
        worker.post { encoder?.requestKeyframe() }
    }

    override fun onStopRequest() {
        worker.post { stopSelf() }
    }

    // --- notification ---

    private fun buildNotification(text: String): Notification {
        val nm = getSystemService(NotificationManager::class.java)
        if (nm.getNotificationChannel(CHANNEL) == null) {
            nm.createNotificationChannel(
                NotificationChannel(CHANNEL, "Streaming", NotificationManager.IMPORTANCE_LOW)
            )
        }
        val stop = PendingIntent.getService(
            this, 0, Intent(this, CaptureService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_IMMUTABLE
        )
        val open = PendingIntent.getActivity(
            this, 0, Intent(this, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE
        )
        return Notification.Builder(this, CHANNEL)
            .setSmallIcon(R.drawable.ic_launcher)
            .setContentTitle("PhoneCast")
            .setContentText(text)
            .setOngoing(true)
            .setContentIntent(open)
            .addAction(Notification.Action.Builder(null, "Stop", stop).build())
            .build()
    }

    private fun updateNotification() {
        getSystemService(NotificationManager::class.java)
            .notify(NOTIFICATION_ID, buildNotification(status))
    }

    override fun onDestroy() {
        running = false
        if (::worker.isInitialized) {
            worker.removeCallbacksAndMessages(null)
            runCatching {
                getSystemService(DisplayManager::class.java).unregisterDisplayListener(displayListener)
            }
        }
        client?.close()
        audio?.stop()
        virtualDisplay?.release()
        pacer?.release()
        encoder?.stop()
        projection?.stop()
        runCatching { wifiLock?.release() }
        if (::workerThread.isInitialized) workerThread.quitSafely()
        status = "Stopped"
        super.onDestroy()
    }

    companion object {
        private const val TAG = "PhoneCast"
        private const val CHANNEL = "stream"
        private const val NOTIFICATION_ID = 1
        private const val MIN_BITRATE = 2_000_000
        const val ACTION_STOP = "ai.phonecast.STOP"
        const val EXTRA_RESULT_CODE = "resultCode"
        const val EXTRA_RESULT_DATA = "resultData"

        /** Read by MainActivity to show live status. */
        @Volatile var status: String = "Idle"
        @Volatile var running: Boolean = false

        fun start(context: Context, resultCode: Int, data: Intent, settings: StreamSettings) {
            val i = Intent(context, CaptureService::class.java)
                .putExtra(EXTRA_RESULT_CODE, resultCode)
                .putExtra(EXTRA_RESULT_DATA, data)
            settings.writeTo(i)
            running = true
            context.startForegroundService(i)
        }

        fun stop(context: Context) {
            context.startService(Intent(context, CaptureService::class.java).setAction(ACTION_STOP))
        }
    }
}
