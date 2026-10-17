package ai.phonecast

import android.annotation.SuppressLint
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioPlaybackCaptureConfiguration
import android.media.AudioRecord
import android.media.AudioTimestamp
import android.media.projection.MediaProjection
import android.os.Process
import android.util.Log
import java.nio.ByteBuffer

/**
 * Captures the game's audio (what the phone is playing) as raw 48 kHz stereo
 * PCM. Sent uncompressed: ~1.5 Mbps is nothing next to the video, and it
 * avoids any quality loss or encode delay on the phone.
 *
 * Each chunk is stamped on the same clock as the video frames, so the PC can
 * keep audio and video in sync without manual offsets.
 */
class AudioCapture(
    projection: MediaProjection,
    private val onConfig: (Packet) -> Unit,
    private val onPcm: (Packet) -> Unit,
) {
    private val record: AudioRecord
    @Volatile private var running = true
    private val thread: Thread

    init {
        val config = AudioPlaybackCaptureConfiguration.Builder(projection)
            .addMatchingUsage(AudioAttributes.USAGE_GAME)
            .addMatchingUsage(AudioAttributes.USAGE_MEDIA)
            .addMatchingUsage(AudioAttributes.USAGE_UNKNOWN)
            .build()
        val format = AudioFormat.Builder()
            .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
            .setSampleRate(SAMPLE_RATE)
            .setChannelMask(AudioFormat.CHANNEL_IN_STEREO)
            .build()
        val minBuf = AudioRecord.getMinBufferSize(
            SAMPLE_RATE, AudioFormat.CHANNEL_IN_STEREO, AudioFormat.ENCODING_PCM_16BIT
        )
        record = buildRecord(config, format, maxOf(minBuf, CHUNK_BYTES * 8))
        record.startRecording()
        onConfig(
            Packet(
                Protocol.AUDIO_CONFIG, 0, 0,
                ByteBuffer.allocate(6).putInt(SAMPLE_RATE).put(CHANNELS.toByte())
                    .put(Protocol.AUDIO_S16LE.toByte()).array()
            )
        )
        thread = Thread(::loop, "audio-capture").apply { start() }
    }

    @SuppressLint("MissingPermission") // checked by MainActivity before starting
    private fun buildRecord(
        config: AudioPlaybackCaptureConfiguration,
        format: AudioFormat,
        bufferBytes: Int,
    ) = AudioRecord.Builder()
        .setAudioPlaybackCaptureConfig(config)
        .setAudioFormat(format)
        .setBufferSizeInBytes(bufferBytes)
        .build()

    private fun loop() {
        Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_AUDIO)
        val ts = AudioTimestamp()
        var framesRead = 0L
        try {
            while (running) {
                val chunk = ByteArray(CHUNK_BYTES)
                var off = 0
                while (off < chunk.size && running) {
                    val n = record.read(chunk, off, chunk.size - off)
                    if (n < 0) throw IllegalStateException("AudioRecord.read error $n")
                    off += n
                }
                if (!running) break
                val chunkFrames = off / BYTES_PER_FRAME
                val startFrame = framesRead
                framesRead += chunkFrames

                // Exact time the first sample of this chunk was captured.
                val pts = if (record.getTimestamp(ts, AudioTimestamp.TIMEBASE_MONOTONIC) ==
                    AudioRecord.SUCCESS
                ) {
                    ts.nanoTime / 1000 + (startFrame - ts.framePosition) * 1_000_000L / SAMPLE_RATE
                } else {
                    System.nanoTime() / 1000 - chunkFrames * 1_000_000L / SAMPLE_RATE
                }
                onPcm(Packet(Protocol.AUDIO_PCM, 0, pts, if (off == chunk.size) chunk else chunk.copyOf(off)))
            }
        } catch (e: Exception) {
            if (running) Log.e("PhoneCast", "audio capture failed", e)
        }
    }

    fun stop() {
        running = false
        runCatching { record.stop() }
        thread.join(500)
        runCatching { record.release() }
    }

    companion object {
        const val SAMPLE_RATE = 48_000
        const val CHANNELS = 2
        private const val BYTES_PER_FRAME = 2 * CHANNELS
        /** 10 ms per packet keeps audio latency low. */
        private const val CHUNK_BYTES = SAMPLE_RATE / 100 * BYTES_PER_FRAME
    }
}
