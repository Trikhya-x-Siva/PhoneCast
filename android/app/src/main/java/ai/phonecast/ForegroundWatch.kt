package ai.phonecast

import android.app.AppOpsManager
import android.app.usage.UsageEvents
import android.app.usage.UsageStatsManager
import android.content.Context
import android.content.pm.ApplicationInfo
import android.os.Handler
import android.os.Process
import android.util.Log

/**
 * Privacy cover: notices which app is on screen, so the stream can switch to
 * "Be right back" the moment the user leaves the game (for a message, a
 * payment app, anything) and come back when they return.
 *
 * Needs usage access; the PC app grants it over USB debugging
 * (`appops set ai.phonecast GET_USAGE_STATS allow`), or the user can in
 * Settings > Apps > Special app access > Usage access.
 */
class ForegroundWatch(
    private val context: Context,
    private val handler: Handler,
    /** Called on [handler] whenever the app on screen changes. */
    private val onChange: (pkg: String, label: String, game: Boolean) -> Unit,
) {
    private val usage = context.getSystemService(UsageStatsManager::class.java)
    private var current: String? = null
    // First look: the last hour, to find the app on screen right now.
    private var lastQuery = System.currentTimeMillis() - 3_600_000
    private val gameCache = HashMap<String, Boolean>()
    @Volatile private var running = true

    val allowed: Boolean
        get() {
            val ops = context.getSystemService(AppOpsManager::class.java)
            @Suppress("DEPRECATION")
            val mode = ops.checkOpNoThrow(
                AppOpsManager.OPSTR_GET_USAGE_STATS, Process.myUid(), context.packageName
            )
            return mode == AppOpsManager.MODE_ALLOWED
        }

    private val poll = object : Runnable {
        override fun run() {
            if (!running) return
            runCatching { check() }.onFailure { Log.w(TAG, "foreground check failed", it) }
            handler.postDelayed(this, POLL_MS)
        }
    }

    fun start() {
        handler.post(poll)
    }

    fun stop() {
        running = false
        handler.removeCallbacks(poll)
    }

    private fun check() {
        if (usage == null || !allowed) return
        val now = System.currentTimeMillis()
        val events = usage.queryEvents(lastQuery, now)
        lastQuery = now - 50 // a little overlap: events can land late
        val e = UsageEvents.Event()
        var top: String? = null
        while (events.hasNextEvent()) {
            events.getNextEvent(e)
            @Suppress("DEPRECATION")
            if (e.eventType == UsageEvents.Event.ACTIVITY_RESUMED ||
                e.eventType == UsageEvents.Event.MOVE_TO_FOREGROUND
            ) top = e.packageName
        }
        // The notification shade and the "share screen" prompt are not apps
        // the user switched to.
        if (top == null || top == current || top in IGNORED) return
        current = top
        onChange(top, label(top), isGame(top))
    }

    private fun label(pkg: String): String = runCatching {
        val pm = context.packageManager
        pm.getApplicationLabel(pm.getApplicationInfo(pkg, 0)).toString()
    }.getOrDefault(pkg)

    private fun isGame(pkg: String): Boolean = gameCache.getOrPut(pkg) {
        if (pkg in KNOWN_GAMES) return@getOrPut true
        runCatching {
            val info = context.packageManager.getApplicationInfo(pkg, 0)
            @Suppress("DEPRECATION")
            info.category == ApplicationInfo.CATEGORY_GAME ||
                (info.flags and ApplicationInfo.FLAG_IS_GAME) != 0
        }.getOrDefault(false)
    }

    companion object {
        private const val TAG = "PhoneCast"
        private const val POLL_MS = 200L
        private val IGNORED = setOf(
            "com.android.systemui",
            "android",
            "com.google.android.permissioncontroller",
            "com.android.permissioncontroller",
        )
        private val KNOWN_GAMES = setOf(
            "com.pubg.imobile", // BGMI
            "com.tencent.ig", // PUBG Mobile
            "jp.konami.pesam", // eFootball
            "com.activision.callofduty.shooter",
            "com.dts.freefireth",
            "com.dts.freefiremax",
            "com.miHoYo.GenshinImpact",
            "com.supercell.clashroyale",
            "com.supercell.brawlstars",
            "com.mobile.legends",
            "com.riotgames.league.wildrift",
            "com.ea.gp.fifamobile",
        )
    }
}
