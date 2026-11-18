// Copyright (c) tyetladd | MIT License | https://github.com/tyetladd/wavo-app

package io.github.tyetladd.wavo

import android.annotation.SuppressLint
import android.media.AudioFormat
import android.media.AudioRecord
import android.media.MediaRecorder
import android.os.Handler
import android.os.Looper
import kotlin.concurrent.thread
import kotlin.math.sqrt

/**
 * The microphone as the engine takes it: 16 kHz mono floats in [-1, 1]. A recording ends by
 * itself at [MAX_SECONDS].
 */
class Recorder {
    private val main = Handler(Looper.getMainLooper())
    private var record: AudioRecord? = null
    private var reader: Thread? = null

    @Volatile private var running = false
    private val chunks = ArrayList<FloatArray>()
    private var samples = 0
    private var held = false

    val recording: Boolean get() = running
    /** Suspended by [pause], with what is recorded kept for [resume]. */
    val paused: Boolean get() = held

    /**
     * Starts recording; `false` when the microphone can't be opened. `level` gets the RMS of each
     * thirtieth of a second — three to every tenth, the desktop meter's own window — and `full`
     * runs when the recording reaches its limit, both on the main thread. The caller has checked
     * the permission.
     */
    @SuppressLint("MissingPermission")
    fun start(level: (Float) -> Unit, full: () -> Unit): Boolean {
        synchronized(chunks) {
            chunks.clear()
            samples = 0
        }
        held = false
        return open(level, full)
    }

    /**
     * Suspends the microphone and keeps what is recorded: a pause is not the end of the dictation,
     * and [resume] goes on into the same recording. Nothing is captured while paused, so the gap is
     * not in the audio.
     */
    fun pause() {
        if (!running) return
        running = false
        record?.let { runCatching { it.stop() } }
        reader?.join(1000)
        record?.release()
        record = null
        reader = null
        held = true
    }

    /** Goes on after [pause], appending to the same recording. */
    fun resume(level: (Float) -> Unit, full: () -> Unit): Boolean = open(level, full)

    @SuppressLint("MissingPermission")
    private fun open(level: (Float) -> Unit, full: () -> Unit): Boolean {
        if (running) return true

        val smallest = AudioRecord.getMinBufferSize(RATE, CHANNEL, FORMAT)
        if (smallest <= 0) return false
        // VOICE_RECOGNITION: the raw microphone, without the processing tuned for calls.
        val opened = runCatching {
            AudioRecord(MediaRecorder.AudioSource.VOICE_RECOGNITION, RATE, CHANNEL, FORMAT, maxOf(smallest, RATE * 4))
        }.getOrNull()
        if (opened == null || opened.state != AudioRecord.STATE_INITIALIZED) {
            opened?.release()
            return false
        }

        opened.startRecording()
        if (opened.recordingState != AudioRecord.RECORDSTATE_RECORDING) {
            opened.release()
            return false
        }

        record = opened
        running = true
        held = false
        reader = thread(name = "wavo-mic") {
            val buffer = FloatArray(RATE / 10)
            while (running) {
                val n = opened.read(buffer, 0, buffer.size, AudioRecord.READ_BLOCKING)
                if (n <= 0) break

                val chunk = buffer.copyOf(n)
                val total = synchronized(chunks) {
                    chunks.add(chunk)
                    samples += n
                    samples
                }
                // Three levels to a chunk: the pill's meter ticks at the desktop's ~33 ms, a third
                // of this read. The chunk itself stays whole, so Pieces keeps its own cadence.
                val third = n / 3
                for (part in 0..2) {
                    val from = part * third
                    val value = rms(chunk, from, if (part == 2) n else from + third)
                    main.post { if (running) level(value) }
                }
                if (total >= RATE * MAX_SECONDS) {
                    main.post { if (running) full() }
                    break
                }
            }
        }
        return true
    }

    /** A copy of what is recorded so far past the first `from` samples. */
    fun since(from: Int): FloatArray = synchronized(chunks) {
        val out = FloatArray(maxOf(samples - from, 0))
        var at = 0
        for (chunk in chunks) {
            val end = at + chunk.size
            if (end > from) {
                val skip = maxOf(from - at, 0)
                chunk.copyInto(out, at + skip - from, skip, chunk.size)
            }
            at = end
        }
        out
    }

    /** Ends the recording and returns it. */
    fun stop(): FloatArray {
        running = false
        held = false
        record?.let { runCatching { it.stop() } }
        reader?.join(1000)
        record?.release()
        record = null
        reader = null

        synchronized(chunks) {
            val out = FloatArray(samples)
            var at = 0
            for (chunk in chunks) {
                chunk.copyInto(out, at)
                at += chunk.size
            }
            chunks.clear()
            samples = 0
            return out
        }
    }

    companion object {
        const val RATE = 16_000
        const val MAX_SECONDS = 600
        private const val CHANNEL = AudioFormat.CHANNEL_IN_MONO
        private const val FORMAT = AudioFormat.ENCODING_PCM_FLOAT
    }
}

/**
 * The root mean square of `pcm` from `from` to `until`, the level the desktop's meter takes: a
 * peak sits several dB above it, and the pill's bars are calibrated for this. Nothing is 0.
 */
internal fun rms(pcm: FloatArray, from: Int, until: Int): Float {
    val n = until - from
    if (n <= 0) return 0f
    var sum = 0.0
    for (i in from until until) {
        val s = pcm[i].toDouble()
        sum += s * s
    }
    return sqrt(sum / n).toFloat()
}
