// Copyright (c) tyetladd | MIT License | https://github.com/tyetladd/wavo-app

package io.github.tyetladd.wavo

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.os.SystemClock
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.update

/**
 * A dictation from the button to the text: record, transcribe, keep in the history, hand the text
 * over. The app's screen has one and the keyboard has one.
 *
 * With the setting "Transcribe while speaking" the recording is cut at its pauses ([Pieces]) and
 * each piece goes to the engine as the person speaks on, so the stop waits for the last piece
 * only. `onPiece` gets each piece's text as it is ready, in order; `onText` the whole dictation
 * at its end. Without the setting there is one piece, the recording.
 */
class Dictation(
    private val context: Context,
    private val onPiece: (String) -> Unit = {},
    private val onText: (Heard) -> Unit = {},
    /** Follows the setting "Conversation": the app's screen does, the keyboard doesn't. */
    private val conversations: Boolean = false,
) {
    enum class Phase { Idle, Recording, Paused, Transcribing }

    data class Ui(
        val phase: Phase = Phase.Idle,
        /** The RMS of the last thirtieth of a second, 0 to 1. */
        val level: Float = 0f,
        /** The last sixteen levels, oldest first: the pill's bars. */
        val levels: List<Float> = Pill.empty(),
        /** When the recording started, on [SystemClock.elapsedRealtime]. */
        val startedAt: Long = 0,
        /** Why the last dictation gave no text. */
        val error: String? = null,
        val last: Heard? = null,
        /** The text of the pieces ready so far, of the dictation under way. */
        val partial: String = "",
        /** A conversation is recorded, or its speakers are being told apart. */
        val conversation: Boolean = false,
        /** The pieces sent to the engine, and those back: the pill's `n/total` while transcribing. */
        val pieces: Int = 0,
        val piecesDone: Int = 0,
        /** Time spent paused, and when the pause under way began: the clock stops with the mic. */
        val pausedMs: Long = 0,
        val pausedAt: Long = 0,
    ) {
        /** How long the recording has been going, without the time spent paused. */
        fun elapsed(now: Long): Long =
            (if (phase == Phase.Paused) pausedAt else now) - startedAt - pausedMs
    }

    val ui = MutableStateFlow(Ui())

    private val recorder = Recorder()

    // The dictation under way. A result of an earlier one, cancelled, has another number.
    private var session = 0
    private var pieces: Pieces? = null
    private var fed = 0
    private val texts = ArrayList<String>()
    private var pending = 0
    private var stoppedAt = 0L
    private var audioMs = 0L
    private var tookMs = 0L
    private var failure: String? = null
    private var sample: Heard? = null
    private var conversation = false
    private var note: String? = null
    /** The whole recording, kept from the stop until its row is written. */
    private var audio: FloatArray? = null
    /** When the recording started, in wall-clock milliseconds: the recording is named for it. */
    private var startedWall = 0L
    private val words = ArrayList<Word>()
    private var segments: Result<List<Segment>>? = null

    val allowed: Boolean
        get() = context.checkSelfPermission(Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED

    /** Starts recording; `false` with [Ui.error] set when it can't. */
    fun start(): Boolean {
        if (ui.value.phase != Phase.Idle) return false

        val settings = App.instance.settings
        val why = when {
            !allowed -> "Wavo isn't allowed to use the microphone"
            settings.model == null -> "No model yet: download one in Wavo"
            conversations && settings.conversation && !Speakers.ready.value -> "Download the speaker models first"
            !recorder.start(level = ::heard, full = ::stop) -> "The microphone is busy or can't be opened"
            else -> null
        }
        if (why != null) {
            ui.update { it.copy(error = why) }
            return false
        }

        session++
        startedWall = System.currentTimeMillis()
        audio = null
        conversation = conversations && settings.conversation
        pieces = if (settings.live) Pieces(Pieces.frames(settings.pauseMs)) else null
        fed = 0
        texts.clear()
        pending = 0
        stoppedAt = 0
        tookMs = 0
        failure = null
        note = null
        sample = null
        words.clear()
        segments = null
        ui.value = Ui(
            Phase.Recording,
            startedAt = SystemClock.elapsedRealtime(),
            last = ui.value.last,
            conversation = conversation,
        )
        return true
    }

    /** A thirtieth of a second more is recorded: the pieces complete with it go to the engine. */
    private fun heard(level: Float) {
        ui.update { it.copy(level = level, levels = Pill.push(it.levels, level)) }
        val pieces = pieces ?: return
        val more = recorder.since(fed)
        fed += more.size
        pieces.feed(more)
        while (true) {
            val piece = pieces.next() ?: break
            submit(piece, pieces.start)
        }
    }

    /**
     * A piece of the recording, starting at the sample `start`, goes to the engine. Its text shows
     * at once; in a conversation its words are kept with their times from the recording's start,
     * for the speakers found after the stop, and the cleanup waits until then.
     */
    private fun submit(pcm: FloatArray, start: Int) {
        val mine = session
        val startMs = start * 1000L / Recorder.RATE
        pending++
        ui.update { it.copy(pieces = it.pieces + 1) }
        Engine.transcribe(pcm, clean = if (conversation) false else null) { result ->
            if (mine != session) return@transcribe

            pending--
            ui.update { it.copy(piecesDone = it.piecesDone + 1) }
            result.onFailure { failure = it.message ?: "The engine failed" }
            result.onSuccess { heard ->
                sample = sample ?: heard
                tookMs += heard.tookMs
                if (heard.text.isNotEmpty()) {
                    texts += heard.text
                    words += heard.words.map { it.copy(ms = it.ms + startMs) }
                    ui.update { it.copy(partial = texts.joinToString(" ")) }
                    if (!conversation) onPiece(heard.text)
                }
            }
            done()
        }
    }

    /**
     * Ends the recording and transcribes what the engine hasn't had yet. A conversation's
     * speakers are told apart meanwhile, on a thread of their own: that takes the whole recording.
     */
    fun stop() {
        if (ui.value.phase != Phase.Recording && ui.value.phase != Phase.Paused) return

        val all = recorder.stop()
        audio = all
        audioMs = all.size * 1000L / Recorder.RATE
        val pieces = pieces
        val rest = pieces?.let {
            it.feed(all.copyOfRange(minOf(fed, all.size), all.size))
            it.rest()
        } ?: all
        stoppedAt = SystemClock.elapsedRealtime()
        ui.update { it.copy(phase = Phase.Transcribing, level = 0f) }
        submit(rest, pieces?.start ?: 0)

        if (conversation) {
            val mine = session
            val main = android.os.Handler(android.os.Looper.getMainLooper())
            kotlin.concurrent.thread(name = "wavo-speakers") {
                val split = runCatching { Speakers.diarize(all) }
                main.post {
                    if (mine != session) return@post
                    segments = split
                    done()
                }
            }
        }
    }

    /**
     * Suspends the dictation without ending it: the microphone stops, the pieces complete so far go
     * to the engine so their text shows, and [resume] goes on into the same recording and the same
     * history entry. The paused gap is not in the audio.
     */
    fun pause() {
        if (ui.value.phase != Phase.Recording) return
        recorder.pause()
        val pieces = pieces
        if (pieces != null) {
            val more = recorder.since(fed)
            fed += more.size
            pieces.feed(more)
            while (true) {
                val piece = pieces.next() ?: break
                submit(piece, pieces.start)
            }
        }
        ui.update { it.copy(phase = Phase.Paused, level = 0f, pausedAt = SystemClock.elapsedRealtime()) }
    }

    /** Goes on after [pause]; `false` with [Ui.error] set when the microphone can't be reopened. */
    fun resume(): Boolean {
        if (ui.value.phase != Phase.Paused) return false
        if (!recorder.resume(::heard, ::stop)) {
            ui.update { it.copy(error = "The microphone is busy or can't be opened") }
            return false
        }
        ui.update {
            val now = SystemClock.elapsedRealtime()
            it.copy(
                phase = Phase.Recording,
                pausedMs = it.pausedMs + (now - it.pausedAt),
                pausedAt = 0L,
            )
        }
        return true
    }

    /** A result is back: after the stop, the last one ends the dictation. */
    private fun done() {
        if (stoppedAt != 0L && pending == 0 && (!conversation || segments != null)) finish()
    }

    /**
     * A conversation's text: each word goes to the speaker it starts with, and each turn is
     * cleaned as a dictation is. If the speakers couldn't be told apart the text still comes,
     * without them.
     */
    private fun conversed(): String {
        val split = segments ?: return ""
        split.exceptionOrNull()?.let { note = "The speakers couldn't be told apart: ${it.message}" }

        val settings = App.instance.settings
        val language = sample?.language ?: settings.language
        val turns = turns(words, split.getOrDefault(emptyList())).map {
            if (settings.clean) it.copy(text = Native.clean(it.text, language)) else it
        }
        return script(turns.filter { it.text.isNotEmpty() })
    }

    /** Every piece is back: the dictation's text. */
    private fun finish() {
        val text = if (conversation) conversed() else texts.joinToString(" ")
        val heard = sample?.copy(
            text = text,
            tookMs = tookMs,
            audioMs = audioMs,
            waitMs = SystemClock.elapsedRealtime() - stoppedAt,
            words = emptyList(),
        )
        val error = when {
            failure != null -> failure
            text.isEmpty() -> "No speech heard"
            else -> note
        }
        val kept = heard?.takeIf { text.isNotEmpty() }
        ui.value = Ui(error = error, last = kept ?: ui.value.last)
        session++
        if (kept != null) {
            // The text goes at once, so the keyboard types it without waiting for the disk.
            onText(kept)
            keep(kept)
        }
    }

    /**
     * The recording, when the setting keeps it, written before the history row that points at it:
     * a crash in between leaves a file to reconcile, never a row pointing at nothing. A write that
     * fails leaves the text kept with no audio.
     */
    private fun keep(heard: Heard) {
        val pcm = audio
        // The recording is up to 38 MB at the limit: let it go once it is captured, not at the
        // next start, and there are two Dictations — the screen's and the keyboard's.
        audio = null
        val history = App.instance.history
        if (pcm == null || !App.instance.settings.keepRecordings) {
            history.add(heard)
            return
        }

        val name = history.recordings.name(startedWall)
        kotlin.concurrent.thread(name = "wavo-wav") {
            val written = runCatching { history.recordings.write(name, pcm) }.isSuccess
            android.os.Handler(android.os.Looper.getMainLooper()).post {
                history.add(heard, if (written) name else null)
            }
        }
    }

    /** Drops the recording, and the pieces the engine still has. What `onPiece` got stays given. */
    fun cancel() {
        if (ui.value.phase == Phase.Idle) return

        if (ui.value.phase == Phase.Recording) recorder.stop()
        audio = null
        session++
        ui.update { Ui(last = it.last) }
    }

    fun toggle() {
        when (ui.value.phase) {
            Phase.Idle -> start()
            Phase.Recording, Phase.Paused -> stop()
            Phase.Transcribing -> Unit
        }
    }
}

/** `0:07`. */
fun clock(ms: Long): String = "%d:%02d".format(ms / 60_000, ms / 1000 % 60)

/**
 * What goes before a dictated text in a field: a space when the text before the cursor ends in a
 * word or a mark, nothing at the field's start or after whitespace or an opening bracket or quote.
 */
fun lead(before: CharSequence?): String {
    val last = before?.lastOrNull() ?: return ""
    return if (last.isWhitespace() || last in "([{«“\"'") "" else " "
}
