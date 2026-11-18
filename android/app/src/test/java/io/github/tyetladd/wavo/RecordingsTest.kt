// Copyright (c) tyetladd | MIT License | https://github.com/tyetladd/wavo-app

package io.github.tyetladd.wavo

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File

/** A dictation's recording: its name, the folder it cannot escape, and the WAV it is written as. */
class RecordingsTest {
    @get:Rule
    val tmp = TemporaryFolder()

    private val at = 1_794_771_673_000L

    @Test
    fun name_is_a_day_folder_and_the_start_in_millis() {
        val name = Recordings(tmp.root).name(at)
        assertTrue(name, name.matches(Regex("""\d{4}-\d{2}-\d{2}/\d+\.wav""")))
        assertTrue(name, name.endsWith("$at.wav"))
    }

    @Test
    fun two_in_the_same_millisecond_do_not_overwrite() {
        val r = Recordings(tmp.root)
        val first = r.name(at)
        r.write(first, FloatArray(160))
        val second = r.name(at)
        assertNotEquals(first, second)
        assertTrue(File(tmp.root, second).parentFile == File(tmp.root, first).parentFile)
    }

    @Test
    fun a_name_cannot_point_outside_the_folder() {
        val r = Recordings(tmp.root)
        assertNull(r.file("../../etc/passwd"))
        assertNull(r.file("/data/x"))
        assertNull(r.file("2026-11-16/../../x.wav"))
        assertNull(r.file(""))
        assertNotNull(r.file("2026-11-16/$at.wav"))
    }

    @Test
    fun the_wav_is_sixteen_kilohertz_mono_sixteen_bit() {
        val r = Recordings(tmp.root)
        val name = r.name(at)
        r.write(name, FloatArray(1600))
        val bytes = File(tmp.root, name).readBytes()

        assertEquals("RIFF", String(bytes, 0, 4))
        assertEquals("WAVE", String(bytes, 8, 4))
        assertEquals(16000, le32(bytes, 24))
        assertEquals(1, le16(bytes, 22))
        assertEquals(16, le16(bytes, 34))
        assertEquals(1600 * 2, le32(bytes, 40))
        assertEquals(44 + 1600 * 2, bytes.size)
    }

    @Test
    fun samples_round_trip_within_a_quantisation_step() {
        val r = Recordings(tmp.root)
        val pcm = FloatArray(480) { kotlin.math.sin(it * 0.05).toFloat() * 0.8f }
        val name = r.name(at)
        r.write(name, pcm)
        val bytes = File(tmp.root, name).readBytes()

        for (i in pcm.indices) {
            val sample = le16(bytes, 44 + i * 2).toShort().toInt() / 32768f
            assertTrue("sample $i: $sample vs ${pcm[i]}", kotlin.math.abs(sample - pcm[i]) <= 1f / 32768f)
        }
    }

    @Test
    fun loud_samples_are_clamped_not_wrapped() {
        val r = Recordings(tmp.root)
        val name = r.name(at)
        r.write(name, floatArrayOf(2f, -2f))
        val bytes = File(tmp.root, name).readBytes()
        assertEquals(32767, le16(bytes, 44).toShort().toInt())
        assertEquals(-32768, le16(bytes, 46).toShort().toInt())
    }

    @Test
    fun orphans_are_the_files_no_row_points_to() {
        val r = Recordings(tmp.root)
        val kept = r.name(at)
        val orphan = r.name(at + 1)
        r.write(kept, FloatArray(16))
        r.write(orphan, FloatArray(16))
        assertEquals(listOf(orphan), r.orphans(setOf(kept)))
        assertEquals(emptyList<String>(), r.orphans(setOf(kept, orphan)))
    }

    @Test
    fun delete_removes_the_file_and_ignores_a_missing_one() {
        val r = Recordings(tmp.root)
        val name = r.name(at)
        r.write(name, FloatArray(16))
        assertTrue(File(tmp.root, name).exists())
        r.delete(name)
        assertTrue(!File(tmp.root, name).exists())
        r.delete(name)
        r.delete("../../etc/passwd")
    }

    @Test
    fun a_long_recording_is_written_whole() {
        val r = Recordings(tmp.root)
        val pcm = FloatArray(Recorder.RATE * 600) { 0.1f }
        val name = r.name(at)
        r.write(name, pcm)
        assertEquals(44 + pcm.size * 2, File(tmp.root, name).length().toInt())
    }

    private fun le16(b: ByteArray, at: Int): Int =
        (b[at].toInt() and 0xFF) or ((b[at + 1].toInt() and 0xFF) shl 8)

    private fun le32(b: ByteArray, at: Int): Int =
        le16(b, at) or (le16(b, at + 2) shl 16)

    private fun entry(id: Long, audio: String?) = Entry(id, at, "text", "model", 1000, 100, audio)

    @Test
    fun a_row_whose_recording_is_gone_is_dangling() {
        val r = Recordings(tmp.root)
        val present = r.name(at)
        r.write(present, FloatArray(16))
        val rows = listOf(entry(1, present), entry(2, r.name(at + 1)), entry(3, null))
        // Row 2's file was never written; row 3 keeps no recording at all.
        assertEquals(listOf(2L), r.dangling(rows))
    }

    @Test
    fun a_crash_between_the_file_and_the_row_leaves_an_orphan_to_delete() {
        val r = Recordings(tmp.root)
        val kept = r.name(at)
        val orphan = r.name(at + 1)
        r.write(kept, FloatArray(16))
        r.write(orphan, FloatArray(16))
        val rows = listOf(entry(1, kept))

        assertEquals(listOf(orphan), r.orphans(rows.mapNotNull { it.audio }.toSet()))
        assertEquals(emptyList<Long>(), r.dangling(rows))
    }

    @Test
    fun a_row_pointing_outside_the_folder_is_dangling_not_a_way_out() {
        val r = Recordings(tmp.root)
        assertEquals(listOf(7L), r.dangling(listOf(entry(7, "../../etc/passwd"))))
    }

    @Test
    fun a_recording_reads_back_as_the_samples_it_was_written_from() {
        val r = Recordings(tmp.root)
        val pcm = FloatArray(1600) { kotlin.math.sin(it * 0.07).toFloat() * 0.6f }
        val name = r.name(at)
        r.write(name, pcm)
        val back = r.read(name)
        assertNotNull(back)
        assertEquals(pcm.size, back!!.size)
        for (i in pcm.indices) {
            assertTrue("sample $i", kotlin.math.abs(back[i] - pcm[i]) <= 1f / 32768f)
        }
    }

    @Test
    fun reading_a_missing_or_unsafe_name_gives_nothing() {
        val r = Recordings(tmp.root)
        assertNull(r.read(r.name(at)))
        assertNull(r.read("../../etc/passwd"))
    }
}
