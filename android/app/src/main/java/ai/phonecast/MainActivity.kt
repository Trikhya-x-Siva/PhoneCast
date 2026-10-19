package ai.phonecast

import android.Manifest
import android.app.Activity
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Color
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.media.projection.MediaProjectionManager
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.text.InputType
import android.view.Gravity
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.ArrayAdapter
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.Spinner
import android.widget.Switch
import android.widget.TextView
import android.widget.Toast
import android.app.AlertDialog
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetAddress

/** Settings + start/stop. Plain Android views: no libraries, tiny APK. */
class MainActivity : Activity() {

    private lateinit var transport: Spinner
    private lateinit var host: EditText
    private lateinit var codec: Spinner
    private lateinit var quality: Spinner
    private lateinit var fps: Spinner
    private lateinit var bitrate: Spinner
    private lateinit var heat: Switch
    private lateinit var wifiRow: LinearLayout
    private lateinit var statusView: TextView
    private lateinit var startButton: Button

    private val ui = Handler(Looper.getMainLooper())

    private val qualityOptions = listOf("1920 · stream size (recommended, cooler)" to 1920, "Native (sharpest, hotter)" to 0, "1280 (battery saver)" to 1280)
    private val fpsOptions = listOf(60, 30)
    private val bitrateOptions = listOf(
        "Auto · best (USB 24 / Wi-Fi 16 Mbps)" to 0, "10 Mbps" to 10_000_000, "20 Mbps" to 20_000_000,
        "30 Mbps" to 30_000_000, "40 Mbps" to 40_000_000, "60 Mbps" to 60_000_000, "80 Mbps (max)" to 80_000_000,
    )

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        buildUi()
        load(StreamSettings.load(this))
    }

    override fun onResume() {
        super.onResume()
        ui.post(refresh)
    }

    override fun onPause() {
        super.onPause()
        ui.removeCallbacks(refresh)
    }

    private val refresh = object : Runnable {
        override fun run() {
            statusView.text = CaptureService.status
            startButton.text = if (CaptureService.running) "Stop" else "Start streaming"
            ui.postDelayed(this, 500)
        }
    }

    private fun onStartClicked() {
        if (CaptureService.running) {
            CaptureService.stop(this)
            CaptureService.running = false
            return
        }
        val s = readSettings()
        if (s.transport == Transport.WIFI && s.host.isBlank()) {
            host.error = "Enter the PC address shown in the PhoneCast PC app"
            return
        }
        s.save(this)
        val missing = listOf(Manifest.permission.RECORD_AUDIO, Manifest.permission.POST_NOTIFICATIONS)
            .filter { checkSelfPermission(it) != PackageManager.PERMISSION_GRANTED }
        if (missing.isNotEmpty()) {
            requestPermissions(missing.toTypedArray(), REQ_PERMS)
            return
        }
        askScreenCapture()
    }

    override fun onRequestPermissionsResult(code: Int, perms: Array<out String>, results: IntArray) {
        super.onRequestPermissionsResult(code, perms, results)
        if (code == REQ_PERMS) askScreenCapture()
    }

    @Suppress("DEPRECATION")
    private fun askScreenCapture() {
        val mpm = getSystemService(MediaProjectionManager::class.java)
        // Android 14+ offers "Entire screen" or "A single app" (pick the game:
        // notifications and other apps then stay off the stream). Both work:
        // the capture follows the shared app's size. The earlier 0 fps was a
        // frame-pacer bug, not the single-app choice.
        startActivityForResult(mpm.createScreenCaptureIntent(), REQ_CAPTURE)
    }

    @Deprecated("Deprecated in Java")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != REQ_CAPTURE) return
        if (resultCode != RESULT_OK || data == null) {
            CaptureService.status = "Screen capture was not allowed"
            return
        }
        CaptureService.start(this, resultCode, data, readSettings())
        // Get out of the way so the user can open the game.
        moveTaskToBack(true)
    }

    private fun readSettings() = StreamSettings(
        transport = if (transport.selectedItemPosition == 1) Transport.WIFI else Transport.USB,
        host = host.text.toString().trim(),
        codec = if (codec.selectedItemPosition == 1) Protocol.CODEC_H265 else Protocol.CODEC_H264,
        maxSize = qualityOptions[quality.selectedItemPosition].second,
        fps = fpsOptions[fps.selectedItemPosition],
        bitrateBps = bitrateOptions[bitrate.selectedItemPosition].second,
        heatProtect = heat.isChecked,
    )

    private fun load(s: StreamSettings) {
        transport.setSelection(if (s.transport == Transport.WIFI) 1 else 0)
        host.setText(s.host)
        codec.setSelection(if (s.codec == Protocol.CODEC_H265) 1 else 0)
        quality.setSelection(qualityOptions.indexOfFirst { it.second == s.maxSize }.coerceAtLeast(0))
        fps.setSelection(fpsOptions.indexOf(s.fps).coerceAtLeast(0))
        bitrate.setSelection(bitrateOptions.indexOfFirst { it.second == s.bitrateBps }.coerceAtLeast(0))
        heat.isChecked = s.heatProtect
        wifiRow.visibility = if (s.transport == Transport.WIFI) View.VISIBLE else View.GONE
    }

    /**
     * Finds PhoneCast PCs on this Wi-Fi: broadcasts PCST-DISCOVER on UDP 27185
     * and lists the PCs that answer (see PROTOCOL.md).
     */
    private fun findPc() {
        CaptureService.status = "Looking for PhoneCast PCs on this Wi-Fi…"
        Thread {
            val found = linkedMapOf<String, String>() // ip -> name
            try {
                DatagramSocket().use { sock ->
                    sock.broadcast = true
                    sock.soTimeout = 400
                    val msg = "PCST-DISCOVER".toByteArray()
                    sock.send(DatagramPacket(msg, msg.size, InetAddress.getByName("255.255.255.255"), 27185))
                    val deadline = System.currentTimeMillis() + 2000
                    val buf = ByteArray(256)
                    while (System.currentTimeMillis() < deadline) {
                        val p = DatagramPacket(buf, buf.size)
                        try {
                            sock.receive(p)
                        } catch (_: java.net.SocketTimeoutException) {
                            continue
                        }
                        val text = String(buf, 0, p.length)
                        if (text.startsWith("PCST-HERE")) {
                            found[p.address.hostAddress ?: continue] = text.removePrefix("PCST-HERE").trim()
                        }
                    }
                }
            } catch (e: Exception) {
                runOnUiThread { Toast.makeText(this, "Search failed: ${e.message}", Toast.LENGTH_LONG).show() }
                return@Thread
            }
            runOnUiThread {
                when (found.size) {
                    0 -> {
                        CaptureService.status = "No PC found"
                        Toast.makeText(
                            this,
                            "No PhoneCast PC found. Is the PC app open and on the same Wi-Fi?",
                            Toast.LENGTH_LONG
                        ).show()
                    }
                    1 -> {
                        host.setText(found.keys.first())
                        CaptureService.status = "Found ${found.values.first()}"
                    }
                    else -> {
                        val ips = found.keys.toList()
                        AlertDialog.Builder(this)
                            .setTitle("Which PC?")
                            .setItems(ips.map { "${found[it]}  ($it)" }.toTypedArray()) { _, i -> host.setText(ips[i]) }
                            .show()
                    }
                }
            }
        }.start()
    }

    // --- UI ---

    private fun buildUi() {
        val dp = resources.displayMetrics.density
        fun px(v: Int) = (v * dp).toInt()

        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(px(20), px(32), px(20), px(24))
            setBackgroundColor(BG)
        }

        // Rounded card surface with a hairline border (PhantomBlitz palette).
        fun card(radius: Int) = GradientDrawable().apply {
            cornerRadius = px(radius).toFloat(); setColor(CARD); setStroke(px(1), LINE)
        }

        root.addView(TextView(this).apply {
            text = "OSSG"
            textSize = 30f
            letterSpacing = 0.04f
            setTypeface(Typeface.create("sans-serif-medium", Typeface.BOLD))
            setTextColor(TEXT)
            paint.shader = android.graphics.LinearGradient(0f, 0f, px(90).toFloat(), 0f,
                Color.parseColor("#C4B5FD"), Color.parseColor("#67E8F9"), android.graphics.Shader.TileMode.CLAMP)
        })
        root.addView(TextView(this).apply {
            text = "Open Source Streaming for Gaming"
            setTextColor(MUTED)
            textSize = 13f
            letterSpacing = 0.02f
            setPadding(0, px(2), 0, px(20))
        })

        statusView = TextView(this).apply {
            setTextColor(TEXT)
            textSize = 14f
            setLineSpacing(0f, 1.2f)
            setPadding(px(16), px(14), px(16), px(14))
            background = card(16)
        }
        root.addView(statusView, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))

        fun label(t: String) = root.addView(TextView(this).apply {
            text = t
            setTextColor(MUTED)
            textSize = 12f
            letterSpacing = 0.06f
            isAllCaps = true
            setTypeface(typeface, Typeface.BOLD)
            setPadding(px(2), px(18), 0, px(6))
        })

        fun spinner(items: List<String>) = Spinner(this).apply {
            adapter = ArrayAdapter(this@MainActivity, android.R.layout.simple_spinner_dropdown_item, items)
            background = card(12)
            setPadding(px(10), px(6), px(10), px(6))
            setPopupBackgroundDrawable(GradientDrawable().apply { cornerRadius = px(12).toFloat(); setColor(CARD) })
            root.addView(this, LinearLayout.LayoutParams(MATCH_PARENT, px(48)))
        }

        label("Connection")
        transport = spinner(listOf("USB cable (most stable)", "Wi-Fi (wireless)"))
        host = EditText(this).apply {
            hint = "PC address, e.g. 192.168.1.20"
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_URI
            setTextColor(TEXT)
            setHintTextColor(MUTED)
            background = card(12)
            setPadding(px(14), px(10), px(14), px(10))
        }
        wifiRow = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            addView(host, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f))
            addView(Button(this@MainActivity).apply {
                text = "Find PC"
                isAllCaps = false
                setTextColor(CYAN)
                setTypeface(typeface, Typeface.BOLD)
                background = card(12)
                setOnClickListener { findPc() }
            }, LinearLayout.LayoutParams(WRAP_CONTENT, px(48)).apply { marginStart = px(8) })
            root.addView(this, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT).apply { topMargin = px(8) })
        }
        transport.onItemSelectedListener = SimpleSelect {
            wifiRow.visibility = if (it == 1) View.VISIBLE else View.GONE
        }

        label("Video codec")
        codec = spinner(listOf("H.264 (most compatible)", "H.265 (better for Wi-Fi)"))
        label("Resolution")
        quality = spinner(qualityOptions.map { it.first })
        label("Frame rate")
        fps = spinner(fpsOptions.map { "$it fps" })
        label("Bitrate (phone → PC)")
        bitrate = spinner(bitrateOptions.map { it.first })

        heat = Switch(this).apply {
            text = "Protect phone from heat (lower quality while hot)"
            setTextColor(TEXT)
            isChecked = true
            setPadding(px(4), px(16), px(4), 0)
            root.addView(this, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
        }

        startButton = Button(this).apply {
            text = "Start streaming"
            setTextColor(Color.WHITE)
            textSize = 17f
            isAllCaps = false
            setTypeface(typeface, Typeface.BOLD)
            // Violet -> cyan primary action, with a ripple for press feedback.
            background = android.graphics.drawable.RippleDrawable(
                android.content.res.ColorStateList.valueOf(Color.parseColor("#40FFFFFF")),
                GradientDrawable(GradientDrawable.Orientation.TL_BR, intArrayOf(ACCENT, ACCENT_2)).apply {
                    cornerRadius = px(16).toFloat()
                },
                null,
            )
            setOnClickListener { onStartClicked() }
        }
        root.addView(startButton, LinearLayout.LayoutParams(MATCH_PARENT, px(60)).apply { topMargin = px(28) })

        root.addView(TextView(this).apply {
            text = "After you tap Start, Android asks to share your screen: choose " +
                "\"Entire screen\". Then open your game."
            setTextColor(MUTED)
            textSize = 12f
            gravity = Gravity.CENTER
            setLineSpacing(0f, 1.25f)
            setPadding(px(8), px(16), px(8), 0)
        })

        setContentView(ScrollView(this).apply {
            setBackgroundColor(BG)
            addView(root)
        })
    }

    private class SimpleSelect(val onSelect: (Int) -> Unit) : android.widget.AdapterView.OnItemSelectedListener {
        override fun onItemSelected(p: android.widget.AdapterView<*>?, v: View?, pos: Int, id: Long) = onSelect(pos)
        override fun onNothingSelected(p: android.widget.AdapterView<*>?) {}
    }

    companion object {
        private const val REQ_PERMS = 1
        private const val REQ_CAPTURE = 2
        // OSSG / PhantomBlitz palette (same tokens as desktop/ui/style.css)
        private val BG = Color.parseColor("#070A14")
        private val CARD = Color.parseColor("#11172E")
        private val LINE = Color.parseColor("#222B4D")
        private val TEXT = Color.parseColor("#E8ECF8")
        private val MUTED = Color.parseColor("#8E97B5")
        private val CYAN = Color.parseColor("#67E8F9")
        private val ACCENT = Color.parseColor("#7C3AED")
        private val ACCENT_2 = Color.parseColor("#0E7FA8")
    }
}
