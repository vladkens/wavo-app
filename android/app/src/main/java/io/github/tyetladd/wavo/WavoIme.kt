// Copyright (c) tyetladd | MIT License | https://github.com/tyetladd/wavo-app

package io.github.tyetladd.wavo

import android.annotation.SuppressLint
import android.content.Intent
import android.content.res.ColorStateList
import android.content.res.Configuration
import android.graphics.Color
import android.graphics.drawable.GradientDrawable
import android.inputmethodservice.InputMethodService
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.TypedValue
import android.view.Gravity
import android.view.HapticFeedbackConstants
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputMethodManager
import android.widget.ImageButton
import android.widget.LinearLayout
import android.widget.TextView
import kotlinx.coroutines.MainScope
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch

/**
 * The voice keyboard: a microphone button in place of the keys. Tap it to start and tap again to
 * stop, or hold it and speak; the text goes into the field at the cursor. Under it: back to the
 * previous keyboard, a space, a backspace and the field's enter.
 */
class WavoIme : InputMethodService() {
    private val scope = MainScope()
    private val main = Handler(Looper.getMainLooper())
    private lateinit var dictation: Dictation

    private var status: TextView? = null
    private var mic: ImageButton? = null
    private var pill: PillView? = null

    // The press on the microphone under way: when it went down, and what it does when it lifts.
    private var downAt = 0L
    private var started = false
    private var stops = false

    private val tick = object : Runnable {
        override fun run() {
            render(dictation.ui.value)
            if (dictation.ui.value.phase == Dictation.Phase.Recording) main.postDelayed(this, 250)
        }
    }

    override fun onCreate() {
        super.onCreate()
        dictation = Dictation(this, onPiece = ::commit)
        scope.launch { dictation.ui.collect(::render) }
        scope.launch { Engine.state.collect { render(dictation.ui.value) } }
    }

    override fun onDestroy() {
        dictation.cancel()
        scope.cancel()
        super.onDestroy()
    }

    override fun onStartInputView(info: EditorInfo?, restarting: Boolean) {
        super.onStartInputView(info, restarting)
        // The model loads while the person gets ready to speak.
        Engine.prewarm()
        render(dictation.ui.value)
    }

    override fun onFinishInputView(finishing: Boolean) {
        // The field is gone: nothing to type the rest into.
        dictation.cancel()
        super.onFinishInputView(finishing)
    }

    // MARK: Text

    /** A piece of the dictation, typed as soon as it is ready; the last one comes after the stop. */
    private fun commit(text: String) {
        val field = currentInputConnection ?: return
        field.commitText(lead(field.getTextBeforeCursor(1, 0)) + text, 1)
    }

    private fun enter() {
        val field = currentInputConnection ?: return
        val options = currentInputEditorInfo?.imeOptions ?: 0
        val action = options and EditorInfo.IME_MASK_ACTION
        val plain = options and EditorInfo.IME_FLAG_NO_ENTER_ACTION != 0 ||
            action == EditorInfo.IME_ACTION_NONE || action == EditorInfo.IME_ACTION_UNSPECIFIED
        if (plain) sendDownUpKeyEvents(KeyEvent.KEYCODE_ENTER) else field.performEditorAction(action)
    }

    // MARK: View

    private val dark: Boolean
        get() = resources.configuration.uiMode and Configuration.UI_MODE_NIGHT_MASK == Configuration.UI_MODE_NIGHT_YES

    private fun dp(value: Int): Int =
        TypedValue.applyDimension(TypedValue.COMPLEX_UNIT_DIP, value.toFloat(), resources.displayMetrics).toInt()

    @SuppressLint("ClickableViewAccessibility")
    override fun onCreateInputView(): View {
        val surface = if (dark) 0xFF1B1B1F.toInt() else 0xFFF1F0F4.toInt()
        val onSurface = if (dark) 0xFFE4E2E6.toInt() else 0xFF1B1B1F.toInt()
        val keyColor = if (dark) 0xFF2F3033.toInt() else 0xFFFFFFFF.toInt()

        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.CENTER_HORIZONTAL
            setBackgroundColor(surface)
            setPadding(dp(8), dp(12), dp(8), dp(8))
        }

        status = TextView(this).apply {
            setTextColor(onSurface)
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
            gravity = Gravity.CENTER
            maxLines = 2
            minHeight = dp(40)
            setOnClickListener { if (!dictation.allowed || App.instance.settings.model == null) openApp() }
        }
        root.addView(status, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))

        // The pill as on the Mac, above the microphone; it shows only while a dictation does.
        pill = PillView(this).apply { visibility = View.GONE }
        root.addView(
            pill,
            LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT).apply { setMargins(0, dp(4), 0, dp(4)) },
        )

        mic = ImageButton(this).apply {
            setImageResource(R.drawable.ic_mic)
            imageTintList = ColorStateList.valueOf(Color.WHITE)
            scaleType = android.widget.ImageView.ScaleType.CENTER_INSIDE
            setPadding(dp(22), dp(22), dp(22), dp(22))
            background = GradientDrawable().apply { shape = GradientDrawable.OVAL }
            contentDescription = "Dictate"
            setOnTouchListener { view, event -> press(view, event) }
        }
        root.addView(mic, LinearLayout.LayoutParams(dp(84), dp(84)).apply { setMargins(0, dp(8), 0, dp(16)) })

        val row = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        fun key(label: String, about: String, weight: Float, tap: () -> Unit): TextView {
            val view = TextView(this).apply {
                text = label
                contentDescription = about
                gravity = Gravity.CENTER
                setTextColor(onSurface)
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 18f)
                background = GradientDrawable().apply {
                    cornerRadius = dp(8).toFloat()
                    setColor(keyColor)
                }
                isHapticFeedbackEnabled = true
                setOnClickListener {
                    it.performHapticFeedback(HapticFeedbackConstants.KEYBOARD_TAP)
                    tap()
                }
            }
            row.addView(view, LinearLayout.LayoutParams(0, dp(48), weight).apply { setMargins(dp(3), 0, dp(3), 0) })
            return view
        }

        key("⌨", "Previous keyboard", 1f) { if (!switchToPreviousInputMethod()) pickKeyboard() }
            .setOnLongClickListener { pickKeyboard(); true }
        key("space", "Space", 3f) { currentInputConnection?.commitText(" ", 1) }
        // A backspace held goes on deleting.
        val back = key("⌫", "Backspace", 1f) { sendDownUpKeyEvents(KeyEvent.KEYCODE_DEL) }
        val repeat = object : Runnable {
            override fun run() {
                sendDownUpKeyEvents(KeyEvent.KEYCODE_DEL)
                main.postDelayed(this, 60)
            }
        }
        back.setOnLongClickListener { main.post(repeat); true }
        back.setOnTouchListener { _, event ->
            if (event.actionMasked == MotionEvent.ACTION_UP || event.actionMasked == MotionEvent.ACTION_CANCEL) {
                main.removeCallbacks(repeat)
            }
            false
        }
        key("⏎", "Enter", 1f, ::enter)
        root.addView(row, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))

        render(dictation.ui.value)
        return root
    }

    /**
     * A press on the microphone. Idle: it starts, and stops at the lift if held (push to talk),
     * else goes on until the next tap. Recording: the lift stops it.
     */
    private fun press(view: View, event: MotionEvent): Boolean {
        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                view.performHapticFeedback(HapticFeedbackConstants.KEYBOARD_TAP)
                downAt = SystemClock.elapsedRealtime()
                when (dictation.ui.value.phase) {
                    Dictation.Phase.Idle -> {
                        started = dictation.start()
                        stops = false
                        if (started) main.post(tick) else if (!dictation.allowed) openApp()
                    }
                    // The keyboard has no pause button of its own: a press ends it either way.
                    Dictation.Phase.Recording, Dictation.Phase.Paused -> {
                        started = false
                        stops = true
                    }
                    Dictation.Phase.Transcribing -> {
                        started = false
                        stops = false
                    }
                }
            }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> {
                val held = SystemClock.elapsedRealtime() - downAt > HOLD_MS
                if (stops || (started && held)) {
                    view.performHapticFeedback(HapticFeedbackConstants.KEYBOARD_TAP)
                    dictation.stop()
                }
                started = false
                stops = false
            }
        }
        return true
    }

    private fun render(ui: Dictation.Ui) {
        val status = status ?: return
        val mic = mic ?: return
        val engine = Engine.state.value

        status.text = when {
            ui.phase == Dictation.Phase.Recording ->
                "Listening  ${clock(SystemClock.elapsedRealtime() - ui.startedAt)}"
            ui.phase == Dictation.Phase.Transcribing ->
                if (engine is Engine.State.Loading) "Loading ${engine.model}…" else "Transcribing…"
            !dictation.allowed -> "Tap here to allow the microphone in Wavo"
            App.instance.settings.model == null -> "Tap here to download a model in Wavo"
            ui.error != null -> ui.error
            engine is Engine.State.Loading -> "Loading ${engine.model}…"
            else -> "Tap to speak, or hold and speak"
        }

        val recording = ui.phase == Dictation.Phase.Recording
        val busy = ui.phase == Dictation.Phase.Transcribing
        mic.setImageResource(if (recording) R.drawable.ic_stop else R.drawable.ic_mic)
        (mic.background as? GradientDrawable)?.setColor(
            when {
                recording -> 0xFFD93025.toInt()
                busy -> 0xFF8A8D93.toInt()
                else -> 0xFF1F6FEB.toInt()
            },
        )
        // The button swells with the voice.
        val scale = if (recording) 1f + minOf(ui.level * 2f, 1f) * 0.18f else 1f
        mic.scaleX = scale
        mic.scaleY = scale
        pill?.render(ui, App.instance.settings.language ?: ui.last?.language)
    }

    private fun openApp() {
        startActivity(Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
    }

    private fun pickKeyboard() {
        getSystemService(InputMethodManager::class.java).showInputMethodPicker()
    }

    private companion object {
        /** A press longer than this is a hold: the lift ends the dictation. */
        const val HOLD_MS = 450L
    }
}
