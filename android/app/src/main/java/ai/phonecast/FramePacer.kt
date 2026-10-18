package ai.phonecast

import android.graphics.SurfaceTexture
import android.opengl.EGL14
import android.opengl.EGLConfig
import android.opengl.EGLContext
import android.opengl.EGLDisplay
import android.opengl.EGLExt
import android.opengl.EGLSurface
import android.opengl.GLES11Ext
import android.opengl.GLES20
import android.os.Handler
import android.os.HandlerThread
import android.os.Process
import android.os.SystemClock
import android.util.Log
import android.view.Surface
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.nio.FloatBuffer
import java.util.concurrent.CountDownLatch

/**
 * Turns the screen's uneven frame delivery into a perfectly steady stream.
 *
 * The phone draws at 120 Hz while games run at 30–120 fps, so captured frames
 * arrive in an irregular rhythm (8 ms, 25 ms, …) and the encoder's own frame
 * limiter throws real frames away. Instead, the virtual display renders into
 * a GPU texture, and on every screen refresh (vsync) that lands on our 60 fps
 * grid we copy the newest frame into the encoder with an exact timestamp.
 * Result: exactly 60 fps, evenly spaced, whatever the game does. The copy is a
 * single textured quad — a negligible GPU cost.
 */
@Suppress("UNUSED_PARAMETER")
class FramePacer(fps: Int, refreshRate: Float) {
    private val thread = HandlerThread("frame-pacer", Process.THREAD_PRIORITY_URGENT_DISPLAY).apply { start() }
    private val handler = Handler(thread.looper)
    private val intervalNs = 1_000_000_000L / fps

    private var display: EGLDisplay = EGL14.EGL_NO_DISPLAY
    private var context: EGLContext = EGL14.EGL_NO_CONTEXT
    private var config: EGLConfig? = null
    private var pbuffer: EGLSurface = EGL14.EGL_NO_SURFACE
    private var output: EGLSurface = EGL14.EGL_NO_SURFACE
    private var outW = 0
    private var outH = 0

    private var texture = 0
    private var program = 0
    private var aPos = 0
    private var aTex = 0
    private var uTexMatrix = 0
    private val texMatrix = FloatArray(16)
    private lateinit var surfaceTexture: SurfaceTexture
    lateinit var inputSurface: Surface
        private set

    @Volatile private var running = true
    private var hasFrame = false
    private var nextNs = 0L

    /** Frames shown that repeated the previous game frame (game slower than 60). */
    @Volatile var repeated = 0L
        private set
    private var freshSinceLast = false

    init {
        runBlocking {
            setupEgl()
            setupGl()
            surfaceTexture = SurfaceTexture(texture)
            surfaceTexture.setOnFrameAvailableListener({ st ->
                // Latch the newest game frame right away; the vsync tick draws it.
                if (!running) return@setOnFrameAvailableListener
                st.updateTexImage()
                hasFrame = true
                freshSinceLast = true
            }, handler)
            inputSurface = Surface(surfaceTexture)
            handler.post(tick)
        }
    }

    /** Points output at a (new) encoder surface. Blocks until switched. */
    fun setOutput(encoderSurface: Surface, width: Int, height: Int) = runBlocking {
        EGL14.eglMakeCurrent(display, pbuffer, pbuffer, context)
        if (output != EGL14.EGL_NO_SURFACE) EGL14.eglDestroySurface(display, output)
        output = EGL14.eglCreateWindowSurface(
            display, config, encoderSurface, intArrayOf(EGL14.EGL_NONE), 0
        )
        outW = width
        outH = height
        surfaceTexture.setDefaultBufferSize(width, height)
        nextNs = 0L
    }

    /**
     * Runs on its own clock, not Choreographer: when the phone is hot or the
     * app is in the background, Android lowers Choreographer's rate for this
     * app (we measured 30 Hz mid-stream), which halved the stream's frame rate.
     * A Handler timer is not throttled, so output stays at exactly `fps`.
     */
    private val tick = object : Runnable {
        override fun run() {
            if (!running) return
            val now = System.nanoTime()
            if (output != EGL14.EGL_NO_SURFACE && hasFrame) {
                if (nextNs == 0L || now - nextNs > intervalNs * 3) nextNs = now // start / resync after a stall
                if (now >= nextNs - 1_500_000L) { // our turn (allow 1.5 ms early wake-up)
                    // Timestamp on the exact grid, so the stream is perfectly even.
                    val pts = nextNs
                    nextNs += intervalNs
                    if (!freshSinceLast) repeated++
                    freshSinceLast = false
                    draw(pts)
                }
            }
            val waitNs = if (nextNs == 0L) intervalNs else nextNs - System.nanoTime()
            val waitMs = ((waitNs + 999_999L) / 1_000_000L).coerceIn(0L, 50L) // round up: never early
            handler.postAtTime(this, SystemClock.uptimeMillis() + waitMs)
        }
    }

    private fun draw(ptsNs: Long) {
        if (!EGL14.eglMakeCurrent(display, output, output, context)) return
        GLES20.glViewport(0, 0, outW, outH)
        surfaceTexture.getTransformMatrix(texMatrix)
        GLES20.glUseProgram(program)
        GLES20.glActiveTexture(GLES20.GL_TEXTURE0)
        GLES20.glBindTexture(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, texture)
        GLES20.glUniformMatrix4fv(uTexMatrix, 1, false, texMatrix, 0)
        GLES20.glEnableVertexAttribArray(aPos)
        GLES20.glVertexAttribPointer(aPos, 2, GLES20.GL_FLOAT, false, 0, QUAD)
        GLES20.glEnableVertexAttribArray(aTex)
        GLES20.glVertexAttribPointer(aTex, 2, GLES20.GL_FLOAT, false, 0, TEX)
        GLES20.glDrawArrays(GLES20.GL_TRIANGLE_STRIP, 0, 4)
        EGLExt.eglPresentationTimeANDROID(display, output, ptsNs)
        EGL14.eglSwapBuffers(display, output)
    }

    fun release() {
        running = false
        runBlocking {
            handler.removeCallbacks(tick)
            runCatching { inputSurface.release() }
            runCatching { surfaceTexture.release() }
            EGL14.eglMakeCurrent(display, EGL14.EGL_NO_SURFACE, EGL14.EGL_NO_SURFACE, EGL14.EGL_NO_CONTEXT)
            if (output != EGL14.EGL_NO_SURFACE) EGL14.eglDestroySurface(display, output)
            if (pbuffer != EGL14.EGL_NO_SURFACE) EGL14.eglDestroySurface(display, pbuffer)
            EGL14.eglDestroyContext(display, context)
            EGL14.eglTerminate(display)
        }
        thread.quitSafely()
    }

    // ---------------- setup ----------------

    private fun setupEgl() {
        display = EGL14.eglGetDisplay(EGL14.EGL_DEFAULT_DISPLAY)
        val version = IntArray(2)
        check(EGL14.eglInitialize(display, version, 0, version, 1)) { "eglInitialize failed" }
        val attribs = intArrayOf(
            EGL14.EGL_RED_SIZE, 8, EGL14.EGL_GREEN_SIZE, 8, EGL14.EGL_BLUE_SIZE, 8,
            EGL14.EGL_RENDERABLE_TYPE, EGL14.EGL_OPENGL_ES2_BIT,
            EGL_RECORDABLE_ANDROID, 1,
            EGL14.EGL_SURFACE_TYPE, EGL14.EGL_WINDOW_BIT or EGL14.EGL_PBUFFER_BIT,
            EGL14.EGL_NONE,
        )
        val configs = arrayOfNulls<EGLConfig>(1)
        val count = IntArray(1)
        check(EGL14.eglChooseConfig(display, attribs, 0, configs, 0, 1, count, 0) && count[0] > 0) {
            "no recordable EGL config"
        }
        config = configs[0]
        context = EGL14.eglCreateContext(
            display, config, EGL14.EGL_NO_CONTEXT,
            intArrayOf(EGL14.EGL_CONTEXT_CLIENT_VERSION, 2, EGL14.EGL_NONE), 0
        )
        check(context != EGL14.EGL_NO_CONTEXT) { "eglCreateContext failed" }
        pbuffer = EGL14.eglCreatePbufferSurface(
            display, config, intArrayOf(EGL14.EGL_WIDTH, 1, EGL14.EGL_HEIGHT, 1, EGL14.EGL_NONE), 0
        )
        check(EGL14.eglMakeCurrent(display, pbuffer, pbuffer, context)) { "eglMakeCurrent failed" }
    }

    private fun setupGl() {
        val tex = IntArray(1)
        GLES20.glGenTextures(1, tex, 0)
        texture = tex[0]
        GLES20.glBindTexture(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, texture)
        GLES20.glTexParameteri(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, GLES20.GL_TEXTURE_MIN_FILTER, GLES20.GL_LINEAR)
        GLES20.glTexParameteri(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, GLES20.GL_TEXTURE_MAG_FILTER, GLES20.GL_LINEAR)
        GLES20.glTexParameteri(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, GLES20.GL_TEXTURE_WRAP_S, GLES20.GL_CLAMP_TO_EDGE)
        GLES20.glTexParameteri(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, GLES20.GL_TEXTURE_WRAP_T, GLES20.GL_CLAMP_TO_EDGE)

        program = GLES20.glCreateProgram()
        GLES20.glAttachShader(program, shader(GLES20.GL_VERTEX_SHADER, VERTEX))
        GLES20.glAttachShader(program, shader(GLES20.GL_FRAGMENT_SHADER, FRAGMENT))
        GLES20.glLinkProgram(program)
        val ok = IntArray(1)
        GLES20.glGetProgramiv(program, GLES20.GL_LINK_STATUS, ok, 0)
        check(ok[0] != 0) { "link failed: ${GLES20.glGetProgramInfoLog(program)}" }
        aPos = GLES20.glGetAttribLocation(program, "aPos")
        aTex = GLES20.glGetAttribLocation(program, "aTex")
        uTexMatrix = GLES20.glGetUniformLocation(program, "uTexMatrix")
    }

    private fun shader(type: Int, src: String): Int {
        val s = GLES20.glCreateShader(type)
        GLES20.glShaderSource(s, src)
        GLES20.glCompileShader(s)
        val ok = IntArray(1)
        GLES20.glGetShaderiv(s, GLES20.GL_COMPILE_STATUS, ok, 0)
        check(ok[0] != 0) { "shader failed: ${GLES20.glGetShaderInfoLog(s)}" }
        return s
    }

    private fun runBlocking(block: () -> Unit) {
        if (Thread.currentThread() == thread) {
            block()
            return
        }
        val done = CountDownLatch(1)
        var error: Throwable? = null
        handler.post {
            try {
                block()
            } catch (t: Throwable) {
                error = t
            } finally {
                done.countDown()
            }
        }
        done.await()
        error?.let {
            Log.e("PhoneCast", "frame pacer", it)
            throw it
        }
    }

    companion object {
        private const val EGL_RECORDABLE_ANDROID = 0x3142

        private const val VERTEX = """
            attribute vec4 aPos;
            attribute vec4 aTex;
            uniform mat4 uTexMatrix;
            varying vec2 vTex;
            void main() {
                gl_Position = aPos;
                vTex = (uTexMatrix * aTex).xy;
            }
        """

        private const val FRAGMENT = """
            #extension GL_OES_EGL_image_external : require
            precision mediump float;
            varying vec2 vTex;
            uniform samplerExternalOES sTex;
            void main() {
                gl_FragColor = texture2D(sTex, vTex);
            }
        """

        private fun floats(vararg v: Float): FloatBuffer =
            ByteBuffer.allocateDirect(v.size * 4).order(ByteOrder.nativeOrder()).asFloatBuffer().apply {
                put(v)
                position(0)
            }

        private val QUAD = floats(-1f, -1f, 1f, -1f, -1f, 1f, 1f, 1f)
        private val TEX = floats(0f, 0f, 1f, 0f, 0f, 1f, 1f, 1f)
    }
}
