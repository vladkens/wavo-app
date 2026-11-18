// Copyright (c) tyetladd | MIT License | https://github.com/tyetladd/wavo-app

package io.github.tyetladd.wavo

import org.junit.Assert.assertEquals
import org.junit.Test

class SpeakersTest {
    private fun words(vararg said: Pair<Int, String>) = said.map { (ms, text) -> Word(ms.toLong(), text) }

    @Test
    fun wordsGoToTheSpeakerTheyStartIn() {
        // The engine's speaker numbers are whatever its clustering gave: 7 speaks first here.
        val segments = listOf(Segment(0.2f, 2.0f, 7), Segment(2.4f, 4.0f, 3), Segment(4.5f, 6.0f, 7))
        val said = words(300 to "Hi", 800 to "there.", 2500 to "Hello.", 3000 to "How", 3400 to "are", 3700 to "you?", 4600 to "Fine.")

        val turns = turns(said, segments)
        assertEquals(listOf(Turn(1, "Hi there.", 300), Turn(2, "Hello. How are you?", 2500), Turn(1, "Fine.", 4600)), turns)
        assertEquals(
            "0:00 Speaker 1: Hi there.\n0:02 Speaker 2: Hello. How are you?\n0:04 Speaker 1: Fine.",
            script(turns),
        )
    }

    @Test
    fun aTurnCarriesWhenItStarted() {
        val segments = listOf(Segment(0.2f, 2.0f, 7), Segment(2.4f, 4.0f, 3))
        val said = words(300 to "Hi", 800 to "there.", 2500 to "Hello.")
        assertEquals(listOf(300L, 2500L), turns(said, segments).map { it.ms })
    }

    @Test
    fun eachTurnIsItsOwnLineWithItsTime() {
        val said = listOf(Turn(1, "Hi there.", 300), Turn(2, "Hello.", 2500), Turn(1, "Fine.", 61_000))
        assertEquals(
            "0:00 Speaker 1: Hi there.\n0:02 Speaker 2: Hello.\n1:01 Speaker 1: Fine.",
            script(said),
        )
    }

    @Test
    fun oneSpeakerIsStillOneLinePerTurnWithTimes() {
        val said = listOf(Turn(1, "Just me.", 1500), Turn(1, "And more.", 75_400))
        assertEquals("0:01 Just me.\n1:15 And more.", script(said))
    }

    @Test
    fun aWordBetweenSegmentsGoesToTheNearest() {
        val segments = listOf(Segment(0f, 1.0f, 0), Segment(3.0f, 5.0f, 1))
        // 1.2 s is nearer the first speaker's end, 2.9 s the second's start; 9 s is after everyone.
        val turns = turns(words(100 to "a", 1200 to "b", 2900 to "c", 3500 to "d", 9000 to "e"), segments)
        assertEquals(listOf(Turn(1, "a b", 100), Turn(2, "c d e", 2900)), turns)
    }

    @Test
    fun aLoneWordInsideATurnStaysInIt() {
        val segments = listOf(Segment(0f, 2.0f, 0), Segment(2.0f, 2.2f, 1), Segment(2.2f, 5.0f, 0), Segment(5.0f, 8.0f, 1))
        val turns = turns(words(100 to "one", 1000 to "two", 2100 to "three", 3000 to "four", 5500 to "five", 6000 to "six"), segments)
        assertEquals(listOf(Turn(1, "one two three four", 100), Turn(2, "five six", 5500)), turns)
    }

    @Test
    fun overlappingSegmentsGiveTheWordToTheLaterOne() {
        val segments = listOf(Segment(0f, 3.0f, 0), Segment(2.0f, 4.0f, 1))
        assertEquals(
            listOf(Turn(1, "a b", 500), Turn(2, "c d", 2500)),
            turns(words(500 to "a", 1500 to "b", 2500 to "c", 3500 to "d"), segments),
        )
    }

    @Test
    fun oneSpeakerOrNoneHasNoLabels() {
        val said = words(0 to "Just", 400 to "me.")
        assertEquals("0:00 Just me.", script(turns(said, listOf(Segment(0f, 1f, 4)))))
        assertEquals("0:00 Just me.", script(turns(said, emptyList())))
        assertEquals("", script(turns(emptyList(), listOf(Segment(0f, 1f, 0)))))
    }
}
