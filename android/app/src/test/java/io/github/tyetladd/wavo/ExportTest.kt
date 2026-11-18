// Copyright (c) tyetladd | MIT License | https://github.com/tyetladd/wavo-app

package io.github.tyetladd.wavo

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** What a dictation shares as text: its name, when, and what was said — nothing else. */
class ExportTest {
    private val at = 1_794_771_673_000L

    private fun entry(label: String? = null, text: String = "Hello there.") =
        Entry(1, at, text, "gigaam-v3", 4200, 900, "2026-11-16/$at.wav", label)

    @Test
    fun aNamedDictationLeadsWithItsName() {
        val out = export(entry(label = "Shopping list"))
        assertTrue(out, out.startsWith("Shopping list\n"))
        assertTrue(out, out.endsWith("Hello there."))
    }

    @Test
    fun anUnnamedOneHasNoEmptyNameLine() {
        val out = export(entry())
        assertFalse(out, out.startsWith("\n"))
        assertTrue(out, out.endsWith("Hello there."))
    }

    @Test
    fun theDateAndTimeAreThereOnce() {
        val out = export(entry(label = "Note"))
        // Three parts: the name, the moment, the text.
        assertEquals(3, out.split("\n\n").size)
        assertTrue(out, out.contains("2026"))
    }

    @Test
    fun noModelOrTimingsOrAudioLeakIntoIt() {
        val out = export(entry(label = "Note"))
        for (noise in listOf("gigaam", "real time", ".wav", "of speech")) {
            assertFalse("shared text must not carry $noise: $out", out.contains(noise))
        }
    }

    @Test
    fun aConversationKeepsItsLines() {
        val talk = "0:00 Speaker 1: Hi.\n0:02 Speaker 2: Hello."
        assertTrue(export(entry(text = talk)).endsWith(talk))
    }
}
