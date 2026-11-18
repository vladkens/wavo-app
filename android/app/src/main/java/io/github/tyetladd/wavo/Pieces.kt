// Copyright (c) tyetladd | MIT License | https://github.com/tyetladd/wavo-app

package io.github.tyetladd.wavo

import kotlin.math.sqrt

/**
 * A recording cut at its pauses while it is made, so the engine transcribes each piece as the
 * person speaks on and only the last one is left at the stop. A piece ends in the middle of a
 * pause of [PAUSE] frames, once it has speech and is [MIN] long; speech without a pause is cut at
 * its quietest moment past [MAX], inside the models' windows. Silence with no speech in it is
 * dropped, not sent.
 *
 * Quiet is a frame whose level is under a tenth or so of the loudest frame so far, and never
 * under [FLOOR]: a quiet room on a phone's microphone is about 0.001–0.005, speech 0.03 and up.
 */
class Pieces(private val pauseFrames: Int = PAUSE) {
    private var pcm = FloatArray(Recorder.RATE * 30)
    private var size = 0

    /** The level (RMS) of each whole frame fed. */
    private val levels = ArrayList<Float>()
    private var loudest = 0f

    /** Where the audio not yet handed out starts: a sample at a frame's start. */
    private var from = 0

    /** The sample of the recording the last piece handed out starts at. */
    var start = 0
        private set

    fun feed(chunk: FloatArray) {
        if (size + chunk.size > pcm.size) pcm = pcm.copyOf(maxOf(pcm.size * 2, size + chunk.size))
        chunk.copyInto(pcm, size)
        size += chunk.size

        while ((levels.size + 1) * FRAME <= size) {
            val start = levels.size * FRAME
            var sum = 0.0
            for (i in start until start + FRAME) sum += pcm[i] * pcm[i]
            val level = sqrt(sum / FRAME).toFloat()
            levels += level
            loudest = maxOf(loudest, level)
        }
    }

    private fun quiet(frame: Int): Boolean = levels[frame] < maxOf(FLOOR, loudest * 0.12f)

    /** The next piece that is complete, if there is one. Call it until it returns `null`. */
    fun next(): FloatArray? {
        val first = from / FRAME
        val frames = levels.size
        var speech = 0
        var run = 0
        for (i in first until frames) {
            if (quiet(i)) run++ else {
                run = 0
                speech++
            }
            if (run >= pauseFrames && speech >= SPEECH && (i + 1 - first) * FRAME >= MIN) {
                return cut((i - run + 1 + run / 2) * FRAME)
            }
        }

        // Silence so far: keep its end, where the first word may start.
        if (speech < SPEECH && frames - first > SKIP) {
            from = (frames - KEEP) * FRAME
            return null
        }

        // Speech without a pause: cut where it is quietest, in the last third.
        if ((frames - first) * FRAME > MAX) {
            var best = frames - SEARCH
            var least = Float.MAX_VALUE
            for (i in frames - SEARCH until frames - DIP) {
                var sum = 0f
                for (j in i until i + DIP) sum += levels[j]
                if (sum < least) {
                    least = sum
                    best = i
                }
            }
            return cut((best + DIP / 2) * FRAME)
        }

        return null
    }

    private fun cut(at: Int): FloatArray {
        start = from
        val piece = pcm.copyOfRange(from, at)
        from = at
        return piece
    }

    /** What is left at the stop, the part of a frame at the end with it. */
    fun rest(): FloatArray = cut(size)

    companion object {
        /** 20 ms. */
        const val FRAME = Recorder.RATE / 50
        const val FLOOR = 0.006f

        /**
         * Frames of quiet that make a pause when none is given: a second. A piece ends a sentence
         * for the model, so a shorter pause, a breath or a search for a word, put full stops in the
         * middle of one. The app passes the setting instead, which defaults to half a second: the
         * text shows sooner, at the cost of some of those full stops.
         */
        const val PAUSE = 50

        /** The pause a person chooses, in milliseconds, as frames of [FRAME]. */
        fun frames(ms: Int): Int = maxOf(1, ms / (FRAME * 1000 / Recorder.RATE))

        /** Frames of speech a piece has at least: 0.2 s. */
        const val SPEECH = 10

        /** Samples of a piece at least, and at most without a pause: 2 s and 18 s. */
        const val MIN = Recorder.RATE * 2
        const val MAX = Recorder.RATE * 18

        /** Frames of silence dropped at once (2 s), but for the last ones (0.3 s). */
        const val SKIP = 100
        const val KEEP = 15

        /** Frames from the end a forced cut is looked for in (6 s), and those of the dip it looks for (0.2 s). */
        const val SEARCH = 300
        const val DIP = 10
    }
}
