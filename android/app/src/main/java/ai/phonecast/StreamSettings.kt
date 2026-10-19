package ai.phonecast

import android.content.Context
import android.content.Intent

enum class Transport { USB, WIFI }

data class StreamSettings(
    val transport: Transport = Transport.USB,
    /** PC address for Wi-Fi mode. */
    val host: String = "",
    val codec: Int = Protocol.CODEC_H264,
    /**
     * Longest side of the captured video, 0 = native screen resolution.
     * Default 1920: the stream shows the game 1920 px wide, so capturing the
     * full 2376 px only made the phone hotter (~35% more pixels to capture
     * and encode) with no visible gain on stream.
     */
    val maxSize: Int = 1920,
    val fps: Int = 60,
    /** 0 = pick automatically for the transport. */
    val bitrateBps: Int = 0,
    /** Lower quality automatically while the phone is hot. */
    val heatProtect: Boolean = true,
) {
    /** Bitrate actually used: near-lossless over USB, lighter on Wi-Fi. */
    val effectiveBitrate: Int
        get() = when {
            bitrateBps > 0 -> bitrateBps
            // 24 Mbps over USB: the picture is compressed twice (phone, then
            // the stream), so the phone's copy must be crisp; USB has room and
            // the bitrate barely changes the heat (the encoder's speed does).
            transport == Transport.USB -> 24_000_000
            else -> 16_000_000
        }

    val connectHost: String
        get() = if (transport == Transport.USB) "127.0.0.1" else host.trim()

    fun writeTo(intent: Intent) {
        intent.putExtra("transport", transport.name)
        intent.putExtra("host", host)
        intent.putExtra("codec", codec)
        intent.putExtra("maxSize", maxSize)
        intent.putExtra("fps", fps)
        intent.putExtra("bitrate", bitrateBps)
        intent.putExtra("heatProtect", heatProtect)
    }

    fun save(context: Context) {
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit()
            .putString("transport", transport.name)
            .putString("host", host)
            .putInt("codec", codec)
            .putInt("maxSize2", maxSize)
            .putInt("fps", fps)
            .putInt("bitrate", bitrateBps)
            .putBoolean("heatProtect", heatProtect)
            .apply()
    }

    companion object {
        private const val PREFS = "settings"

        fun fromIntent(intent: Intent) = StreamSettings(
            transport = Transport.valueOf(intent.getStringExtra("transport") ?: Transport.USB.name),
            host = intent.getStringExtra("host") ?: "",
            codec = intent.getIntExtra("codec", Protocol.CODEC_H264),
            maxSize = intent.getIntExtra("maxSize", 0),
            fps = intent.getIntExtra("fps", 60),
            bitrateBps = intent.getIntExtra("bitrate", 0),
            heatProtect = intent.getBooleanExtra("heatProtect", true),
        )

        fun load(context: Context): StreamSettings {
            val p = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            val d = StreamSettings()
            return StreamSettings(
                transport = runCatching { Transport.valueOf(p.getString("transport", null)!!) }
                    .getOrDefault(d.transport),
                host = p.getString("host", d.host) ?: d.host,
                codec = p.getInt("codec", d.codec),
                maxSize = p.getInt("maxSize2", d.maxSize),
                fps = p.getInt("fps", d.fps),
                bitrateBps = p.getInt("bitrate", d.bitrateBps),
                heatProtect = p.getBoolean("heatProtect", d.heatProtect),
            )
        }
    }
}
