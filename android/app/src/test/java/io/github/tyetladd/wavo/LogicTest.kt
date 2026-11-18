// Copyright (c) tyetladd | MIT License | https://github.com/tyetladd/wavo-app

package io.github.tyetladd.wavo

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class LogicTest {
    // As the engine's `catalog` writes it.
    private val json = """[
        {"name":"whisper-turbo","family":"Whisper","maker":"OpenAI","languages":"100 languages","langs":["en","ru"],"size":886381760,"punctuated":true,"speed":1.0,"path":null},
        {"name":"gigaam-v3-ctc","family":"GigaAM","maker":"Sber","languages":"Russian","langs":["ru"],"size":271803328,"punctuated":false,"speed":null,"path":null},
        {"name":"gigaam-v3","family":"GigaAM","maker":"Sber","languages":"Russian","langs":["ru"],"size":273724832,"punctuated":true,"speed":13.1,"path":"/data/hub/gigaam.gguf"},
        {"name":"parakeet-ultra","family":"Parakeet","maker":"Moondream","languages":"25 European languages","langs":["en","de"],"size":740363168,"punctuated":true,"speed":7.0,"path":null}
    ]"""

    @Test
    fun modelsParseWithTheRecommendedFirst() {
        val models = ModelInfo.parse(json)
        assertEquals(listOf("parakeet-ultra", "gigaam-v3", "whisper-turbo", "gigaam-v3-ctc"), models.map { it.name })

        val gigaam = models[1]
        assertTrue(gigaam.downloaded && gigaam.recommended && !gigaam.slow)
        assertEquals("/data/hub/gigaam.gguf", gigaam.path)
        assertEquals(listOf("ru"), gigaam.langs)

        val turbo = models[2]
        assertNull(turbo.path)
        assertTrue(turbo.slow && !turbo.recommended)
        assertFalse(models[3].punctuated)
    }

    @Test
    fun heardParses() {
        val heard = Heard.parse("""{"text":"Привет.","language":"ru","took_ms":310,"audio_ms":4200}""", "gigaam-v3", "CPU")
        assertEquals(Heard("Привет.", "ru", 310, 4200, "gigaam-v3", "CPU"), heard)
        assertNull(Heard.parse("""{"text":"","language":null,"took_ms":0,"audio_ms":90}""", "m", "d").language)
    }

    @Test
    fun aSpaceLeadsTheTextAfterAWord() {
        assertEquals("", lead(null))
        assertEquals("", lead(""))
        assertEquals(" ", lead("d"))
        assertEquals(" ", lead("."))
        assertEquals("", lead(" "))
        assertEquals("", lead("\n"))
        assertEquals("", lead("("))
        assertEquals("", lead("«"))
    }

    @Test
    fun sizesAndClocks() {
        assertEquals("0.74 GB", size(740_363_168))
        assertEquals("77 MB", size(77_476_480))
        assertEquals("0:07", clock(7_300))
        assertEquals("2:05", clock(125_000))
    }
}
