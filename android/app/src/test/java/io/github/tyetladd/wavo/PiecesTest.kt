// Copyright (c) tyetladd | MIT License | https://github.com/tyetladd/wavo-app

package io.github.tyetladd.wavo

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import kotlin.math.PI
import kotlin.math.sin

class PiecesTest {
    private val rate = Recorder.RATE

    /** A tone of `seconds` at `level`; level 0.001 is a quiet room. */
    private fun tone(seconds: Double, level: Float): FloatArray =
        FloatArray((seconds * rate).toInt()) { i -> level * sin(2 * PI * 220 * i / rate).toFloat() }

    /** Feeds the audio a tenth of a second at a time, as the recorder does, and collects the pieces. */
    private fun run(audio: FloatArray, pauseFrames: Int = Pieces.PAUSE): Pair<List<FloatArray>, FloatArray> {
        val pieces = Pieces(pauseFrames)
        val out = ArrayList<FloatArray>()
        for (start in audio.indices step rate / 10) {
            pieces.feed(audio.copyOfRange(start, minOf(start + rate / 10, audio.size)))
            while (true) {
                val piece = pieces.next() ?: break
                // A piece says where in the recording it starts: its samples are the recording's there.
                assertEquals(audio.copyOfRange(pieces.start, pieces.start + piece.size).toList(), piece.toList())
                out += piece
            }
        }
        val rest = pieces.rest()
        assertEquals(audio.size - rest.size, pieces.start)
        return out to rest
    }

    @Test
    fun the_pause_setting_is_milliseconds_as_frames() {
        // A frame is 20 ms.
        assertEquals(50, Pieces.frames(1000))
        assertEquals(25, Pieces.frames(500))
        assertEquals(15, Pieces.frames(300))
        // Never zero: a pause of nothing would cut at every quiet frame.
        assertEquals(1, Pieces.frames(0))
    }

    @Test
    fun a_shorter_pause_shows_the_text_sooner() {
        // Speech, then a quiet 0.6 s, then speech again: too short for the default second.
        val audio = tone(3.0, 0.3f) + tone(0.6, 0.001f) + tone(2.05, 0.3f)
        val (slow, _) = run(audio, Pieces.frames(1000))
        val (quick, _) = run(audio, Pieces.frames(300))

        assertEquals("a 0.6 s gap is not a pause of a second", 0, slow.size)
        assertEquals("but it is a pause of 300 ms", 1, quick.size)
        assertTrue("the quicker piece ends sooner", quick[0].size < (3.6 * rate).toInt())
    }

    @Test
    fun cutsInThePauseAndLosesNothing() {
        val audio = tone(3.0, 0.3f) + tone(1.4, 0.001f) + tone(2.05, 0.3f)
        val (pieces, rest) = run(audio)

        assertEquals(1, pieces.size)
        // In the pause, from 3 s to 4.4 s.
        assertTrue(pieces[0].size in (3.1 * rate).toInt()..(4.3 * rate).toInt())
        assertEquals(audio.size, pieces[0].size + rest.size)
        assertEquals(audio.toList(), (pieces[0] + rest).toList())
    }

    @Test
    fun aShortPhraseWaitsForTheStop() {
        val (pieces, rest) = run(tone(1.0, 0.3f) + tone(0.4, 0.001f) + tone(1.0, 0.3f))
        assertTrue(pieces.isEmpty())
        assertEquals((2.4 * rate).toInt(), rest.size)

        // A breath of 0.7 s in a sentence is no pause: the sentence stays whole.
        val (none, whole) = run(tone(3.0, 0.3f) + tone(0.7, 0.001f) + tone(3.0, 0.3f))
        assertTrue(none.isEmpty())
        assertEquals((6.7 * rate).toInt(), whole.size)
    }

    @Test
    fun silenceIsDroppedButForItsLastSeconds() {
        val (pieces, rest) = run(tone(6.0, 0.001f))
        assertTrue(pieces.isEmpty())
        assertTrue(rest.size <= (2.1 * rate).toInt())

        // The word after a long silence is whole, with at most the 2 s of silence not dropped yet.
        val (none, word) = run(tone(5.0, 0.001f) + tone(1.0, 0.3f))
        assertTrue(none.isEmpty())
        assertTrue(word.size >= (1.2 * rate).toInt() && word.size <= (3.1 * rate).toInt())
    }

    @Test
    fun speechWithoutAPauseIsCutAtItsQuietest() {
        // Twenty seconds, softer for 0.3 s at 15 s, but never quiet.
        val audio = tone(15.0, 0.3f) + tone(0.3, 0.1f) + tone(4.7, 0.3f)
        val (pieces, rest) = run(audio)

        assertEquals(1, pieces.size)
        assertTrue(pieces[0].size in 15 * rate..(15.3 * rate).toInt())
        assertEquals(audio.size, pieces[0].size + rest.size)
    }

    @Test
    fun everyPauseCuts() {
        val phrase = tone(2.5, 0.2f) + tone(1.3, 0.002f)
        val (pieces, rest) = run(phrase + phrase + phrase + tone(1.0, 0.2f))
        assertEquals(3, pieces.size)
        assertTrue(rest.size > rate && rest.size < 2 * rate)
        assertNull(Pieces().next())
    }
}
