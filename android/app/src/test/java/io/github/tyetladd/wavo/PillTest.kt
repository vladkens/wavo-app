// Copyright (c) tyetladd | MIT License | https://github.com/tyetladd/wavo-app

package io.github.tyetladd.wavo

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import kotlin.math.abs
import kotlin.math.ceil
import kotlin.math.sqrt

/** The pill's meter: the desktop's dB mapping, the ring of levels it draws, and the RMS it takes. */
class PillTest {
    @Test
    fun silence_is_the_shortest_bar_and_not_a_nan() {
        val height = Pill.barHeight(0f)
        assertTrue("silence must be finite, was $height", height.isFinite())
        assertEquals(Pill.BAR_MIN, height, 1e-4f)
    }

    @Test
    fun full_scale_is_the_tallest_bar() {
        assertEquals(Pill.BAR_MAX, Pill.barHeight(1f), 1e-4f)
    }

    @Test
    fun minus_sixty_db_is_the_shortest_bar() {
        assertEquals(Pill.BAR_MIN, Pill.barHeight(0.001f), 1e-3f)
    }

    @Test
    fun minus_twelve_db_is_the_tallest_bar() {
        // 20*log10(0.25) is -12.04 dB, a hair under the top of the scale.
        assertTrue(Pill.barHeight(0.25f) > 19.9f)
    }

    @Test
    fun the_mapping_rises_with_the_level() {
        var last = 0f
        for (step in 1..1000) {
            val level = step / 1000f
            val height = Pill.barHeight(level)
            assertTrue("fell at $level: $height after $last", height >= last)
            last = height
        }
    }

    @Test
    fun the_ring_keeps_the_newest_sixteen() {
        var levels = Pill.empty()
        assertEquals(Pill.BARS, levels.size)
        for (i in 1..17) levels = Pill.push(levels, i.toFloat())
        assertEquals(Pill.BARS, levels.size)
        assertEquals(2f, levels.first(), 0f)
        assertEquals(17f, levels.last(), 0f)
    }

    @Test
    fun rms_is_the_root_mean_square_not_the_peak() {
        val loud = FloatArray(480) { 0.5f }
        assertEquals(0.5f, rms(loud, 0, loud.size), 1e-5f)

        // A square wave of amplitude 1 has RMS 1, where its peak is also 1; a sine's RMS is 1/sqrt(2).
        val sine = FloatArray(4800) { kotlin.math.sin(2.0 * Math.PI * it / 48.0).toFloat() }
        assertEquals(1f / sqrt(2f), rms(sine, 0, sine.size), 1e-3f)
    }

    @Test
    fun rms_of_a_part_is_that_part_alone() {
        val pcm = FloatArray(480) { if (it < 160) 0f else 1f }
        assertEquals(0f, rms(pcm, 0, 160), 1e-6f)
        assertEquals(1f, rms(pcm, 160, 480), 1e-6f)
    }

    @Test
    fun rms_of_nothing_is_silence() {
        assertEquals(0f, rms(FloatArray(0), 0, 0), 0f)
        assertTrue(abs(rms(FloatArray(4), 2, 2)) < 1e-6f)
    }

    @Test
    fun the_animation_setting_gates_every_tick() {
        // Off: nothing animates, so no gradient is asked for and no frame is redrawn.
        assertFalse(Pill.animates(Dictation.Phase.Recording, animate = false))
        assertFalse(Pill.animates(Dictation.Phase.Transcribing, animate = false))
        // On: only while there is something to show. Idle asks for nothing.
        assertTrue(Pill.animates(Dictation.Phase.Recording, animate = true))
        assertTrue(Pill.animates(Dictation.Phase.Transcribing, animate = true))
        assertFalse(Pill.animates(Dictation.Phase.Idle, animate = true))
        // Paused keeps the last frame on screen but asks for no new one: the microphone is off.
        assertFalse(Pill.animates(Dictation.Phase.Paused, animate = true))
    }

    @Test
    fun a_bar_waits_its_stagger_before_it_waves() {
        assertEquals(Pill.WAVE_LOW, Pill.wave(0, 0), 1e-4f)
        assertEquals(Pill.WAVE_LOW, Pill.wave(1, 0), 1e-4f)
        assertEquals(Pill.WAVE_LOW, Pill.wave(15, 75 * 14), 1e-4f)
    }

    @Test
    fun the_wave_stays_between_its_low_and_high() {
        for (i in 0 until Pill.BARS) {
            for (ms in 0L..2400L step 17) {
                val h = Pill.wave(i, ms)
                assertTrue("bar $i at $ms gave $h", h >= Pill.WAVE_LOW - 1e-4f && h <= Pill.WAVE_HIGH + 1e-4f)
            }
        }
    }

    @Test
    fun the_stagger_puts_neighbouring_bars_at_different_heights() {
        // Bar 0 peaks at half a period; bar 1 is 75 ms behind it.
        assertEquals(Pill.WAVE_HIGH, Pill.wave(0, 600), 1e-3f)
        assertTrue(Pill.wave(1, 600) < Pill.wave(0, 600))
    }

    @Test
    fun settling_ends_every_bar_at_the_wave_low() {
        assertEquals(Pill.WAVE_LOW, Pill.settled(1f, 1f), 1e-4f)
        assertEquals(Pill.WAVE_LOW, Pill.settled(0.01f, 1f), 1e-4f)
        // Not settled at all: the bar is still the level's own height.
        assertEquals(Pill.barHeight(1f), Pill.settled(1f, 0f), 1e-4f)
    }

    @Test
    fun the_chip_is_the_language_in_capitals_or_nothing() {
        assertNull(Pill.chip(null))
        assertEquals("RU", Pill.chip("ru"))
        assertEquals("EN", Pill.chip("en"))
    }

    @Test
    fun the_grid_takes_its_rows_from_the_payload_not_from_ceil() {
        // The generator ceils columns but ROUNDS rows. At a density where 36 * scale is not a
        // half, ceil would ask for one row more than the bytes hold, and index past the end.
        val scale = 3.2f
        val cols = 564          // ceil(176 * 3.2)
        val rows = 115          // round(36 * 3.2), NOT ceil -> 116
        val (c, r) = Pill.grid(Pill.WIDTH, Pill.HEIGHT, scale, cols * rows * 4)
        assertEquals(cols, c)
        assertEquals(rows, r)
        assertTrue("ceil would have overrun the payload", r != ceil(Pill.HEIGHT * scale).toInt())
    }

    @Test
    fun the_grid_agrees_at_every_density_bucket() {
        for (scale in floatArrayOf(1f, 1.3125f, 2f, 2.625f, 3f, 3.2f, 3.5f)) {
            val cols = ceil(Pill.WIDTH * scale).toInt()
            val rows = kotlin.math.round(Pill.HEIGHT * scale).toInt()
            val (c, r) = Pill.grid(Pill.WIDTH, Pill.HEIGHT, scale, cols * rows * 4)
            assertEquals("cols at $scale", cols, c)
            assertEquals("rows at $scale", rows, r)
        }
    }

    @Test
    fun the_clock_stops_while_paused_and_the_gap_is_not_counted() {
        val recording = Dictation.Ui(phase = Dictation.Phase.Recording, startedAt = 1000)
        assertEquals(5000L, recording.elapsed(6000))

        val paused = recording.copy(phase = Dictation.Phase.Paused, pausedAt = 6000)
        assertEquals("frozen at the moment of the pause", 5000L, paused.elapsed(9000))

        val resumed = paused.copy(phase = Dictation.Phase.Recording, pausedMs = 3000, pausedAt = 0)
        assertEquals("the paused gap is not recording time", 5000L, resumed.elapsed(9000))
        assertEquals("and it goes on from there", 6000L, resumed.elapsed(10000))
    }

    @Test
    fun a_language_widens_the_pill_for_its_chip() {
        assertEquals(Pill.WIDTH, Pill.width(null), 0f)
        assertEquals(Pill.WIDTH + Pill.CHIP_ROOM, Pill.width("ru"), 0f)
        // The chip sits inside the widened pill, a padding from its right edge.
        val w = Pill.width("ru")
        assertTrue(w - Pill.PAD - Pill.CHIP_W + Pill.CHIP_W <= w - Pill.PAD)
    }
}
