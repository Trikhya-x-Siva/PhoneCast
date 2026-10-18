package ai.phonecast

import android.annotation.SuppressLint
import android.media.AudioFormat
import android.media.AudioRecord
import android.media.AudioTimestamp
import android.media.MediaRecorder
import android.os.Process
import android.util.Log

/**
 * The phone's own microphone, for streamers who talk into the phone (or a
 * headset plugged into it) instead of a PC mic. Only runs while the PC has
 * "Phone mic" chosen.
 *
 * Uses the voice-call input, so the phone's echo cancelling and noise
 * suppression keep the game sound from the speaker out of the voice.
 * Sent as 10 ms chunks of 48 kHz stereo s16 (mono copied to both sides),
 * stamped on the same clock as the video and game audio.
 */
class MicCapture(private val onPcm: (Packet) -> Unit) {
    private val record: AudioRecord
    @Volatile private var running = true
    private val thread: Thread

    init {
        record = build()
        record.startRecording()
        thread = Thread(::loop, "mic-capture").apply { start() }
        Log.i("PhoneCast", "phone mic on")
    }

    @SuppressLint("MissingPermission") // RECORD_AUDIO is granted before streaming starts
    private fun build(): AudioRecord {
        val minBuf = AudioRecord.getMinBufferSize(
            SAMPLE_RATE, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT
        )
        return AudioRecord(
            MediaRecorder.AudioSource.VOICE_COMMUNICATION,
            SAMPLE_RATE, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT,
            maxOf(minBuf, CHUNK_FRAMES * 2 * 8)
        )
    }

    private fun loop() {
        Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_AUDIO)
        val ts = AudioTimestamp()
        val mono = ShortArray(CHUNK_FRAMES)
        var framesRead = 0L
        try {
            while (running) {
                var off = 0
                while (off < mono.size && running) {
                    val n = record.read(mono, off, mono.size - off)
                    if (n < 0) throw IllegalStateException("mic read error $n")
                    off += n
                }
                if (!running) break
                val startFrame = framesRead
                framesRead += off
                val pts = if (record.getTimestamp(ts, AudioTimestamp.TIMEBASE_MONOTONIC) ==
                    AudioRecord.SUCCESS
                ) {
                    ts.nanoTime / 1000 + (startFrame - ts.framePosition) * 1_000_000L / SAMPLE_RATE
                } else {
                    System.nanoTime() / 1000 - off * 1_000_000L / SAMPLE_RATE
                }
                val out = ByteArray(off * 4)
                for (i in 0 until off) {
                    val v = mono[i].toInt()
                    val lo = (v and 0xff).toByte()
                    val hi = (v shr 8 and 0xff).toByte()
                    out[i * 4] = lo; out[i * 4 + 1] = hi
                    out[i * 4 + 2] = lo; out[i * 4 + 3] = hi
                }
                onPcm(Packet(Protocol.MIC_PCM, 0, pts, out))
            }
        } catch (e: Exception) {
            if (running) Log.e("PhoneCast", "phone mic failed", e)
        }
    }

    fun stop() {
        running = false
        runCatching { record.stop() }
        thread.join(500)
        runCatching { record.release() }
        Log.i("PhoneCast", "phone mic off")
    }

    companion object {
        private const val SAMPLE_RATE = 48_000
        private const val CHUNK_FRAMES = SAMPLE_RATE / 100
    }
}
