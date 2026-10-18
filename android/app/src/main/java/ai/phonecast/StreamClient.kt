package ai.phonecast

import android.os.Build
import android.util.Log
import java.io.BufferedInputStream
import java.io.BufferedOutputStream
import java.io.DataInputStream
import java.io.DataOutputStream
import java.net.InetSocketAddress
import java.net.Socket
import java.util.concurrent.LinkedBlockingDeque
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicLong

/**
 * Connection to the PC. Keeps reconnecting until [close] is called, and
 * protects latency: if video backs up (slow Wi-Fi), queued frames are dropped
 * and [Listener.onCongestion] lets the encoder lower its bitrate instead of
 * letting delay build up.
 */
class StreamClient(
    private val host: String,
    private val port: Int,
    private val listener: Listener,
) {
    interface Listener {
        fun onConnected()
        fun onDisconnected()
        fun onCongestion()
        fun onSetBitrate(bps: Int)
        fun onKeyframeRequest()
        fun onStopRequest()
        fun onSetMic(on: Boolean)
    }

    @Volatile var connected = false
        private set

    /** "USB", "Wi-Fi" or "Wi-Fi (USB dropped)": how the PC is reached now. */
    @Volatile var via = if (host == "127.0.0.1") "USB" else "Wi-Fi"
        private set

    /** PC found on Wi-Fi after the USB link dropped (reused until it fails). */
    @Volatile private var wifiHost: String? = null
    @Volatile private var running = true
    @Volatile private var socket: Socket? = null

    private val queue = LinkedBlockingDeque<Packet>()
    private val queuedVideoBytes = AtomicLong()
    @Volatile private var waitForKeyframe = false

    /** Max video allowed to wait in the queue, in bytes (≈100 ms at target bitrate). */
    @Volatile var maxQueuedVideoBytes = 512 * 1024L

    // Config is re-sent automatically after every (re)connect.
    @Volatile private var videoConfig: Packet? = null
    @Volatile private var audioConfig: Packet? = null

    val droppedFrames = AtomicLong()
    val sentBytes = AtomicLong()

    private val thread = Thread(::connectLoop, "pc-connection").apply { start() }

    fun sendVideoConfig(p: Packet) {
        videoConfig = p
        if (connected) queue.offer(p)
    }

    fun sendAudioConfig(p: Packet) {
        audioConfig = p
        if (connected) queue.offer(p)
    }

    fun sendVideoFrame(p: Packet) {
        if (!connected) return
        if (waitForKeyframe) {
            if (!p.isKeyframe) {
                droppedFrames.incrementAndGet()
                return
            }
            waitForKeyframe = false
        }
        if (queuedVideoBytes.get() > maxQueuedVideoBytes) {
            dropQueuedVideo()
            if (!p.isKeyframe) {
                droppedFrames.incrementAndGet()
                waitForKeyframe = true
                listener.onCongestion()
                return
            }
        }
        queuedVideoBytes.addAndGet(p.payload.size.toLong())
        queue.offer(p)
    }

    /** Audio and stats are small and never dropped. */
    fun send(p: Packet) {
        if (connected) queue.offer(p)
    }

    fun close() {
        running = false
        runCatching { socket?.close() }
        thread.interrupt()
    }

    private fun dropQueuedVideo() {
        for (p in queue.toTypedArray()) {
            // remove() returns false if the writer thread already took it.
            if (p.isVideoFrame && queue.remove(p)) {
                queuedVideoBytes.addAndGet(-p.payload.size.toLong())
                droppedFrames.incrementAndGet()
            }
        }
    }

    private fun connectLoop() {
        while (running) {
            try {
                val s = open()
                socket = s
                session(s)
            } catch (e: Exception) {
                if (running) Log.d(TAG, "connect/session ended: ${e.message}")
            } finally {
                val wasConnected = connected
                connected = false
                runCatching { socket?.close() }
                socket = null
                queue.clear()
                queuedVideoBytes.set(0)
                if (wasConnected) listener.onDisconnected()
            }
            if (running) {
                try {
                    Thread.sleep(1000)
                } catch (_: InterruptedException) {
                }
            }
        }
    }

    private fun connectTo(h: String): Socket {
        val s = Socket()
        s.tcpNoDelay = true
        // Small kernel buffer: backlog stays in our queue where we can drop it.
        s.sendBufferSize = 256 * 1024
        try {
            s.connect(InetSocketAddress(h, port), 2000)
        } catch (e: Exception) {
            runCatching { s.close() }
            throw e
        }
        return s
    }

    /**
     * USB first. OnePlus/ColorOS phones sometimes switch USB debugging off
     * mid-stream (the cable stays connected but the PC loses the phone), so
     * when USB is unreachable the stream carries on over Wi-Fi: the PC is
     * found with the same broadcast as "Find PC" (UDP 27185).
     */
    private fun open(): Socket {
        if (host != "127.0.0.1") return connectTo(host).also { via = "Wi-Fi" }
        try {
            return connectTo(host).also { via = "USB" }
        } catch (usb: Exception) {
            val wifi = wifiHost ?: findPcOnWifi() ?: throw usb
            return try {
                connectTo(wifi).also {
                    wifiHost = wifi
                    via = "Wi-Fi (USB dropped)"
                    Log.i(TAG, "USB unavailable: streaming over Wi-Fi to $wifi")
                }
            } catch (e: Exception) {
                wifiHost = null
                throw e
            }
        }
    }

    /** First PhoneCast PC that answers on this Wi-Fi, or null. */
    private fun findPcOnWifi(): String? = runCatching {
        java.net.DatagramSocket().use { sock ->
            sock.broadcast = true
            sock.soTimeout = 400
            val msg = "PCST-DISCOVER".toByteArray()
            sock.send(
                java.net.DatagramPacket(msg, msg.size, java.net.InetAddress.getByName("255.255.255.255"), 27185)
            )
            val buf = ByteArray(256)
            val deadline = System.currentTimeMillis() + 1500
            while (System.currentTimeMillis() < deadline) {
                val p = java.net.DatagramPacket(buf, buf.size)
                try {
                    sock.receive(p)
                } catch (_: java.net.SocketTimeoutException) {
                    continue
                }
                if (String(buf, 0, p.length).startsWith("PCST-HERE")) return@runCatching p.address.hostAddress
            }
            null
        }
    }.getOrNull()

    private fun session(s: Socket) {
        val out = DataOutputStream(BufferedOutputStream(s.getOutputStream(), 64 * 1024))
        val name = "${Build.MANUFACTURER} ${Build.MODEL}".toByteArray()
        out.write("PCST".toByteArray())
        out.writeByte(Protocol.VERSION)
        out.writeShort(name.size)
        out.write(name)
        out.flush()

        queue.clear()
        queuedVideoBytes.set(0)
        videoConfig?.let { queue.offer(it) }
        audioConfig?.let { queue.offer(it) }
        waitForKeyframe = true
        connected = true

        val reader = Thread({ readLoop(s) }, "pc-control").apply { start() }
        listener.onConnected()
        try {
            while (running && !s.isClosed) {
                val p = queue.poll(500, TimeUnit.MILLISECONDS) ?: continue
                if (p.isVideoFrame) queuedVideoBytes.addAndGet(-p.payload.size.toLong())
                out.writeByte(p.type)
                out.writeByte(p.flags)
                out.writeLong(p.ptsUs)
                out.writeInt(p.payload.size)
                out.write(p.payload)
                sentBytes.addAndGet(14L + p.payload.size)
                if (queue.isEmpty()) out.flush()
            }
        } finally {
            reader.interrupt()
        }
    }

    private fun readLoop(s: Socket) {
        try {
            val input = DataInputStream(BufferedInputStream(s.getInputStream()))
            while (running) {
                val type = input.readUnsignedByte()
                input.readUnsignedByte() // flags
                input.readLong() // pts
                val len = input.readInt()
                val payload = ByteArray(len)
                input.readFully(payload)
                when (type) {
                    Protocol.SET_BITRATE -> listener.onSetBitrate(
                        java.nio.ByteBuffer.wrap(payload).int
                    )
                    Protocol.REQUEST_KEYFRAME -> listener.onKeyframeRequest()
                    Protocol.PING -> {
                        val pcTime = java.nio.ByteBuffer.wrap(payload).long
                        val reply = java.nio.ByteBuffer.allocate(16)
                            .putLong(pcTime).putLong(System.nanoTime() / 1000).array()
                        queue.offerFirst(Packet(Protocol.PONG, 0, 0, reply))
                    }
                    Protocol.STOP -> listener.onStopRequest()
                    Protocol.SET_MIC -> listener.onSetMic(payload.isNotEmpty() && payload[0].toInt() != 0)
                }
            }
        } catch (_: Exception) {
            runCatching { s.close() }
        }
    }

    companion object {
        private const val TAG = "PhoneCast"
    }
}
