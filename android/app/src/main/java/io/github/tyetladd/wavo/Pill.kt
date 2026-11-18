// Copyright (c) tyetladd | MIT License | https://github.com/tyetladd/wavo-app

package io.github.tyetladd.wavo

import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.RectF
import android.graphics.Typeface
import kotlin.math.ceil
import kotlin.math.log10
import kotlin.math.max
import kotlin.math.roundToInt

/**
 * The recording pill: its geometry, its meter, and the one draw both the Dictate screen and the
 * keyboard use, so the two show one picture. Every value is the desktop HUD's (`src/hud.rs`), in
 * points; the caller scales the canvas to points before drawing.
 */
object Pill {
    const val WIDTH = 176f
    const val HEIGHT = 36f
    const val PAD = 14f
    const val DOT = 10f
    const val RING = 1.2f
    const val BARS = 16
    const val BAR_WIDTH = 3f
    const val BAR_STEP = 5f
    const val BAR_MIN = 3f
    const val BAR_MAX = 20f
    const val WAVE_LOW = 3f
    const val WAVE_HIGH = 9f
    const val CHIP_W = 24f
    const val CHIP_H = 17f
    const val CHIP_GAP = 2f
    /** The room the pill widens by for a language's chip, as the desktop's does. */
    const val CHIP_ROOM = CHIP_W + CHIP_GAP

    /** Where the bars start, and the clock after them. */
    const val BARS_X = PAD + DOT + 10f
    const val TIME_X = BARS_X + BARS * BAR_STEP + 8f

    const val SETTLE_MS = 180L
    const val WAVE_HALF_MS = 600L
    const val WAVE_STAGGER_MS = 75L
    /** The clock while transcribing, and the bars' wave: dimmed, still readable on every colour. */
    const val DIM = 0.76f
    const val WAVE_ALPHA = 0.6f
    /** The pill's colours, as the desktop's: a red dot, white bars, violet on the chip's white. */
    private const val RED = 0xFFFF3B30.toInt()
    private const val CHIP_TEXT = 0xFF3B2A78.toInt()

    /** A level (linear RMS) on a dB scale: −60 dBFS and below the shortest bar, −12 the tallest. */
    fun barHeight(level: Float): Float {
        val db = 20f * log10(max(level, 1e-6f))
        return lerp(BAR_MIN, BAR_MAX, ((db + 60f) / 48f).coerceIn(0f, 1f))
    }

    /** The ring before anything is heard: sixteen silences. */
    fun empty(): List<Float> = List(BARS) { 0f }

    /** One more level, the oldest dropped: the newest last, as the desktop draws it. */
    fun push(levels: List<Float>, level: Float): List<Float> = (levels + level).takeLast(BARS)

    fun lerp(a: Float, b: Float, k: Float): Float = a + (b - a) * k

    fun ease(t: Float): Float = t * t * (3f - 2f * t)

    /**
     * Whether the pill draws and asks the core for its gradient: only while a dictation shows, and
     * only with the setting on. Off, the microphone button's own pulse is the whole visual and no
     * frame is asked for — which is where the saving is.
     */
    fun animates(phase: Dictation.Phase, animate: Boolean): Boolean =
        animate && (phase == Dictation.Phase.Recording || phase == Dictation.Phase.Transcribing)

    /** Bar `i` of the wave: low to high and back, each bar [WAVE_STAGGER_MS] after the one before. */
    fun wave(i: Int, elapsedMs: Long): Float {
        val t = elapsedMs - WAVE_STAGGER_MS * i
        if (t < 0) return WAVE_LOW
        val phase = (t % (2 * WAVE_HALF_MS)).toFloat() / WAVE_HALF_MS
        return lerp(WAVE_LOW, WAVE_HIGH, ease(if (phase <= 1f) phase else 2f - phase))
    }

    /** A bar settling from its own height to the wave's low over the stop's first [SETTLE_MS]. */
    fun settled(level: Float, settle: Float): Float = lerp(barHeight(level), WAVE_LOW, settle)

    /** The language's chip, or nothing when there is no language to show. */
    fun chip(language: String?): String? = language?.take(2)?.takeIf { it.isNotEmpty() }?.uppercase()

    /** The pill's width: wider by the chip's room when a language shows, so the chip is inside it. */
    fun width(language: String?): Float = WIDTH + if (chip(language) != null) CHIP_ROOM else 0f

    /**
     * The grid the core's bytes are: columns ceil, but rows are taken from the payload, because the
     * generator ROUNDS rows where it ceils columns. Deriving them here means the two can never
     * disagree at a density where `h * scale` is not a half — which would index past the bytes.
     */
    fun grid(w: Float, h: Float, scale: Float, bytes: Int): Pair<Int, Int> {
        val cols = ceil(w * scale).toInt()
        return cols to if (cols > 0) bytes / (cols * 4) else 0
    }

    /**
     * The pill in points on `canvas`, which the caller has scaled to points. `gradient` and `edge`
     * are the core's two layers; `settle` is 0 while recording and rises to 1 after the stop, and
     * `waveMs` is the time since the stop less [SETTLE_MS]. `label` is the clock, or `n/total`.
     */
    fun draw(
        canvas: Canvas,
        levels: List<Float>,
        phase: Dictation.Phase,
        settle: Float,
        waveMs: Long,
        label: String,
        language: String?,
        gradient: Bitmap?,
        edge: Bitmap?,
    ) {
        val w = width(language)
        val pill = RectF(0f, 0f, w, HEIGHT)
        gradient?.let { canvas.drawBitmap(it, null, pill, null) }
        edge?.let { canvas.drawBitmap(it, null, pill, null) }

        val mid = HEIGHT / 2f
        val transcribing = phase == Dictation.Phase.Transcribing

        // The red dot, its white ring over the gradient; it fades as the bars settle.
        val dotAlpha = if (transcribing) 1f - settle else 1f
        if (dotAlpha > 0f) {
            val cx = PAD + DOT / 2f
            paint.color = Color.WHITE
            paint.alpha = (255 * dotAlpha).roundToInt()
            canvas.drawCircle(cx, mid, DOT / 2f + RING, paint)
            paint.color = RED
            canvas.drawCircle(cx, mid, DOT / 2f, paint)
        }

        // The bars: the levels while recording, the wave after the stop.
        paint.color = Color.WHITE
        paint.alpha = (255 * if (transcribing) WAVE_ALPHA else 1f).roundToInt()
        for (i in 0 until BARS) {
            val level = levels.getOrElse(i) { 0f }
            val h = when {
                !transcribing -> barHeight(level)
                settle < 1f -> settled(level, settle)
                else -> wave(i, waveMs)
            }
            val x = BARS_X + i * BAR_STEP
            canvas.drawRoundRect(x, mid - h / 2f, x + BAR_WIDTH, mid + h / 2f, BAR_WIDTH / 2f, BAR_WIDTH / 2f, paint)
        }

        // The clock, dimmed while transcribing.
        paint.typeface = Typeface.MONOSPACE
        paint.textSize = 13f
        paint.isAntiAlias = true
        paint.color = Color.WHITE
        paint.alpha = (255 * if (transcribing) DIM else 1f).roundToInt()
        val clockWidth = paint.measureText(label)
        canvas.drawText(label, TIME_X, mid + 4.5f, paint)

        // The language's chip at the far right, inside the widened pill, as the desktop's is.
        val code = chip(language)
        if (code != null) {
            val left = w - PAD - CHIP_W
            paint.alpha = 255
            paint.color = Color.WHITE
            canvas.drawRoundRect(left, mid - CHIP_H / 2f, left + CHIP_W, mid + CHIP_H / 2f, 4f, 4f, paint)
            paint.color = CHIP_TEXT
            paint.textSize = 11f
            paint.typeface = Typeface.create(Typeface.DEFAULT, Typeface.BOLD)
            val codeWidth = paint.measureText(code)
            canvas.drawText(code, left + (CHIP_W - codeWidth) / 2f, mid + 4f, paint)
        }
        paint.typeface = Typeface.DEFAULT
        paint.isAntiAlias = false
    }

    /** The one paint the draw uses; both surfaces draw on the main thread. Lazy so the pure
     * helpers above stay testable on a machine with no Android behind them. */
    private val paint by lazy { Paint() }

    /**
     * One of the pill's two layers, its buffers kept between frames: a recording redraws thirty
     * times a second, and a fresh array and bitmap each time is megabytes of churn on the UI
     * thread. One layer per surface; both draw on the main thread.
     */
    class Layer {
        private var pixels = IntArray(0)
        private var bitmap: Bitmap? = null

        /** The core's RGBA rows as a bitmap, reusing this layer's array and bitmap when they fit. */
        fun update(bytes: ByteArray, w: Float, h: Float, scale: Float): Bitmap {
            val (cols, rows) = grid(w, h, scale, bytes.size)
            if (pixels.size != cols * rows) pixels = IntArray(cols * rows)
            for (i in 0 until cols * rows) {
                val at = i * 4
                pixels[i] = ((bytes[at + 3].toInt() and 0xFF) shl 24) or
                    ((bytes[at].toInt() and 0xFF) shl 16) or
                    ((bytes[at + 1].toInt() and 0xFF) shl 8) or
                    (bytes[at + 2].toInt() and 0xFF)
            }
            val bmp = bitmap?.takeIf { it.width == cols && it.height == rows }
                ?: Bitmap.createBitmap(cols, rows, Bitmap.Config.ARGB_8888).also { bitmap = it }
            bmp.setPixels(pixels, 0, cols, 0, 0, cols, rows)
            return bmp
        }
    }
}
