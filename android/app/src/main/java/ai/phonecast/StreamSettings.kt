package ai.phonecast

import android.content.Context
import android.content.Intent

enum class Transport { USB, WIFI }

data class StreamSettings(
    val transport: Transport = Transport.USB,
    /** PC address for Wi-Fi mode. */
    val host: String = "",
    val codec: Int = Protocol.CODEC_H264,
    /** Longest side of the captured video, 0 = native screen resolution. */
    val maxSize: Int = 0,
    val fps: Int = 60,
    /** 0 = pick automatically for the transport. */
    val bitrateBps: Int = 0,
) {
    /** Bitrate actually used: near-lossless over USB, lighter on Wi-Fi. */
    val effectiveBitrate: Int
        get() = when {
            bitrateBps > 0 -> bitrateBps
            // 50 Mbps CBR at native resolution: visually identical to the screen.
            transport == Transport.USB -> 50_000_000
            else -> 20_000_000
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
    }

    fun save(context: Context) {
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit()
            .putString("transport", transport.name)
            .putString("host", host)
            .putInt("codec", codec)
            .putInt("maxSize", maxSize)
            .putInt("fps", fps)
            .putInt("bitrate", bitrateBps)
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
        )

        fun load(context: Context): StreamSettings {
            val p = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            val d = StreamSettings()
            return StreamSettings(
                transport = runCatching { Transport.valueOf(p.getString("transport", null)!!) }
                    .getOrDefault(d.transport),
                host = p.getString("host", d.host) ?: d.host,
                codec = p.getInt("codec", d.codec),
                maxSize = p.getInt("maxSize", d.maxSize),
                fps = p.getInt("fps", d.fps),
                bitrateBps = p.getInt("bitrate", d.bitrateBps),
            )
        }
    }
}
