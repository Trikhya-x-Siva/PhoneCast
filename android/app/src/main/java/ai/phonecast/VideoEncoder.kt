package ai.phonecast

import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.os.Bundle
import android.util.Log
import android.view.Surface
import java.nio.ByteBuffer
import java.util.concurrent.atomic.AtomicLong

/**
 * Hardware screen encoder. The VirtualDisplay renders straight into the
 * encoder's input surface, so frames never touch the CPU on the phone.
 */
class VideoEncoder(
    val codec: Int,
    val width: Int,
    val height: Int,
    private val fps: Int,
    bitrate: Int,
    /** True when [FramePacer] feeds a steady frame rate (no limiter needed). */
    private val paced: Boolean,
    private val onConfig: (Packet) -> Unit,
    private val onFrame: (Packet) -> Unit,
) {
    private val mime = if (codec == Protocol.CODEC_H265)
        MediaFormat.MIMETYPE_VIDEO_HEVC else MediaFormat.MIMETYPE_VIDEO_AVC

    private val mediaCodec: MediaCodec = MediaCodec.createEncoderByType(mime)
    val inputSurface: Surface
    @Volatile private var running = true
    private val thread: Thread

    val frames = AtomicLong()
    val bytes = AtomicLong()
    @Volatile var bitrate = bitrate
        private set
    // Declared before init: it is set while configuring.
    @Volatile var bitrateModeName = "?"
        private set

    init {
        configure(withProfile = true)
        inputSurface = mediaCodec.createInputSurface()
        mediaCodec.start()
        thread = Thread(::drain, "video-encoder").apply {
            priority = Thread.MAX_PRIORITY
            start()
        }
        Log.i(TAG, "encoder ${mediaCodec.name} ${width}x$height@$fps ${bitrate / 1000} kbps")
    }

    private fun configure(withProfile: Boolean) {
        val f = MediaFormat.createVideoFormat(mime, width, height).apply {
            setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
            setInteger(MediaFormat.KEY_BIT_RATE, bitrate)
            // CBR: the Qualcomm encoder overshoots badly in VBR (measured 97 Mbps
            // for a 40 Mbps target), which overloads the PC decoder.
            setInteger(MediaFormat.KEY_BITRATE_MODE, bitrateMode())
            setInteger(MediaFormat.KEY_FRAME_RATE, fps)
            if (!paced) {
                // Fallback path only: the encoder's limiter drops real frames
                // unevenly, which is why FramePacer exists.
                setFloat(MediaFormat.KEY_MAX_FPS_TO_ENCODER, fps.toFloat())
                // Keep sending frames on a static screen so the PC never stalls.
                setLong(MediaFormat.KEY_REPEAT_PREVIOUS_FRAME_AFTER, 100_000L)
            }
            // Keyframes are also sent on demand (new viewer, packet loss).
            setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 5)
            // 0 = realtime priority.
            setInteger(MediaFormat.KEY_PRIORITY, 0)
            setInteger(MediaFormat.KEY_LATENCY, 1)
            // No B-frames: they make the decoder hold frames back (delay + judder).
            setInteger(MediaFormat.KEY_MAX_B_FRAMES, 0)
            // Standard HD colour, tagged in the stream so the PC, players and
            // YouTube/Twitch show the exact colours (not washed out or crushed).
            setInteger(MediaFormat.KEY_COLOR_STANDARD, MediaFormat.COLOR_STANDARD_BT709)
            setInteger(MediaFormat.KEY_COLOR_RANGE, MediaFormat.COLOR_RANGE_LIMITED)
            setInteger(MediaFormat.KEY_COLOR_TRANSFER, MediaFormat.COLOR_TRANSFER_SDR_VIDEO)
            if (withProfile) {
                if (codec == Protocol.CODEC_H265) {
                    setInteger(MediaFormat.KEY_PROFILE, MediaCodecInfo.CodecProfileLevel.HEVCProfileMain)
                } else {
                    setInteger(MediaFormat.KEY_PROFILE, MediaCodecInfo.CodecProfileLevel.AVCProfileHigh)
                }
            }
        }
        try {
            mediaCodec.configure(f, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
        } catch (e: Exception) {
            if (!withProfile) throw e
            Log.w(TAG, "configure with profile failed, retrying default", e)
            mediaCodec.reset()
            configure(withProfile = false)
        }
    }

    val encoderName: String get() = mediaCodec.name

    private fun bitrateMode(): Int {
        val caps = runCatching {
            mediaCodec.codecInfo.getCapabilitiesForType(mime).encoderCapabilities
        }.getOrNull()
        val cbr = MediaCodecInfo.EncoderCapabilities.BITRATE_MODE_CBR
        return if (caps?.isBitrateModeSupported(cbr) == true) {
            bitrateModeName = "CBR"
            cbr
        } else {
            bitrateModeName = "VBR"
            MediaCodecInfo.EncoderCapabilities.BITRATE_MODE_VBR
        }
    }

    fun setBitrate(bps: Int) {
        bitrate = bps
        runCatching {
            mediaCodec.setParameters(Bundle().apply {
                putInt(MediaCodec.PARAMETER_KEY_VIDEO_BITRATE, bps)
            })
        }
    }

    private var lastKeyframeRequestMs = 0L

    /**
     * At most one forced keyframe per 2 s (unless [force]): each costs 5–10×
     * a normal frame, and at a fixed bitrate a burst of them starves the
     * frames around them, which showed as constant pixelation on stream.
     */
    fun requestKeyframe(force: Boolean = false) {
        val now = android.os.SystemClock.uptimeMillis()
        if (!force && now - lastKeyframeRequestMs < 2000) return
        lastKeyframeRequestMs = now
        runCatching {
            mediaCodec.setParameters(Bundle().apply {
                putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0)
            })
        }
    }

    fun stop() {
        running = false
        runCatching { mediaCodec.signalEndOfInputStream() }
        thread.join(1000)
        runCatching { mediaCodec.stop() }
        runCatching { mediaCodec.release() }
        runCatching { inputSurface.release() }
    }

    private fun configPacket(csd: ByteArray) = Packet(
        Protocol.VIDEO_CONFIG, 0, 0,
        ByteBuffer.allocate(5 + csd.size)
            .put(codec.toByte())
            .putShort(width.toShort())
            .putShort(height.toShort())
            .put(csd)
            .array()
    )

    private fun drain() {
        val info = MediaCodec.BufferInfo()
        try {
            while (running) {
                val index = mediaCodec.dequeueOutputBuffer(info, 100_000)
                if (index == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED) {
                    val fmt = mediaCodec.outputFormat
                    val csd = listOfNotNull(
                        fmt.getByteBuffer("csd-0"), fmt.getByteBuffer("csd-1"), fmt.getByteBuffer("csd-2")
                    )
                    if (csd.isNotEmpty()) {
                        val all = ByteBuffer.allocate(csd.sumOf { it.remaining() })
                        csd.forEach { all.put(it.duplicate()) }
                        onConfig(configPacket(all.array()))
                    }
                    continue
                }
                if (index < 0) continue

                val buf = mediaCodec.getOutputBuffer(index)
                if (buf != null && info.size > 0) {
                    buf.position(info.offset)
                    buf.limit(info.offset + info.size)
                    val data = ByteArray(info.size)
                    buf.get(data)
                    if (info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0) {
                        onConfig(configPacket(data))
                    } else {
                        val key = info.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME != 0
                        frames.incrementAndGet()
                        bytes.addAndGet(data.size.toLong())
                        onFrame(
                            Packet(
                                Protocol.VIDEO_FRAME,
                                if (key) Protocol.FLAG_KEYFRAME else 0,
                                info.presentationTimeUs,
                                data,
                            )
                        )
                    }
                }
                mediaCodec.releaseOutputBuffer(index, false)
                if (info.flags and MediaCodec.BUFFER_FLAG_END_OF_STREAM != 0) break
            }
        } catch (e: Exception) {
            if (running) Log.e(TAG, "encoder drain failed", e)
        }
    }

    companion object {
        private const val TAG = "PhoneCast"
    }
}
