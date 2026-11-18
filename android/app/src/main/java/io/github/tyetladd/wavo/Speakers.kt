// Copyright (c) tyetladd | MIT License | https://github.com/tyetladd/wavo-app

package io.github.tyetladd.wavo

import com.k2fsa.sherpa.onnx.FastClusteringConfig
import com.k2fsa.sherpa.onnx.OfflineSpeakerDiarization
import com.k2fsa.sherpa.onnx.OfflineSpeakerDiarizationConfig
import com.k2fsa.sherpa.onnx.OfflineSpeakerSegmentationModelConfig
import com.k2fsa.sherpa.onnx.OfflineSpeakerSegmentationPyannoteModelConfig
import com.k2fsa.sherpa.onnx.SpeakerEmbeddingExtractorConfig
import kotlinx.coroutines.flow.MutableStateFlow
import java.io.File
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.concurrent.thread

/** A word and when it starts, in milliseconds from the recording's start. */
data class Word(val ms: Long, val text: String)

/** A stretch of the recording one speaker talks in, in seconds. */
data class Segment(val start: Float, val end: Float, val speaker: Int)

/**
 * What one speaker said before the next one spoke. Speakers count from 1, in the order they first
 * speak; `ms` is when the turn starts, from the recording's start, for its timestamp.
 */
data class Turn(val speaker: Int, val text: String, val ms: Long = 0)

/**
 * Who spoke when, for a conversation: sherpa-onnx with pyannote's segmentation model and a
 * speaker embedding model, on the CPU. The wavo engine has no diarization, so this is a second
 * engine with models of its own, downloaded once (34 MB). It works on any language.
 */
object Speakers {
    private class Model(val name: String, val url: String, val size: Long)

    private val MODELS = listOf(
        Model(
            "pyannote-segmentation-3.0.onnx",
            "https://huggingface.co/csukuangfj/sherpa-onnx-pyannote-segmentation-3-0/resolve/main/model.onnx",
            5_992_913,
        ),
        Model(
            "3dspeaker-campplus-zh-en-16k.onnx",
            "https://huggingface.co/csukuangfj/speaker-embedding-models/resolve/main/" +
                "3dspeaker_speech_campplus_sv_zh_en_16k-common_advanced.onnx",
            28_281_164,
        ),
    )

    val size: Long = MODELS.sumOf { it.size }

    /** The models are in the app's folder. */
    val ready = MutableStateFlow(false)

    /** The download under way, `null` without one. */
    val download = MutableStateFlow<Download?>(null)

    private val stop = AtomicBoolean(false)
    private var engine: OfflineSpeakerDiarization? = null

    private val root: File get() = File(App.instance.filesDir, "speakers")
    private fun file(model: Model) = File(root, model.name)

    fun refresh() {
        ready.value = MODELS.all { file(it).isFile }
    }

    /** Downloads both models on a thread; a download cut short resumes at the next call. */
    fun download() {
        if (download.value?.error == null && download.value != null) return

        stop.set(false)
        download.value = Download(0, size)
        thread(name = "wavo-speakers-pull") {
            var before = 0L
            val pulled = runCatching {
                for (model in MODELS) {
                    Native.fetch(root.path, model.url, model.name) { done, _ ->
                        download.value = Download(before + done, size)
                        !stop.get()
                    }
                    before += model.size
                }
            }
            val error = pulled.exceptionOrNull()?.message
            download.value = if (error == null || stop.get()) null else Download(before, size, error)
            refresh()
        }
    }

    fun cancel() {
        stop.set(true)
        download.value = null
    }

    @Synchronized
    fun delete() {
        engine?.release()
        engine = null
        root.deleteRecursively()
        refresh()
    }

    /**
     * The speakers' segments of 16 kHz mono samples, in the order they start. It blocks for a
     * fifth or so of the recording's length on a phone: call it off the main thread.
     */
    @Synchronized
    fun diarize(pcm: FloatArray): List<Segment> {
        val engine = engine ?: OfflineSpeakerDiarization(
            config = OfflineSpeakerDiarizationConfig(
                segmentation = OfflineSpeakerSegmentationModelConfig(
                    pyannote = OfflineSpeakerSegmentationPyannoteModelConfig(model = file(MODELS[0]).path),
                    numThreads = THREADS,
                ),
                embedding = SpeakerEmbeddingExtractorConfig(model = file(MODELS[1]).path, numThreads = THREADS),
                // How many speakers isn't known: voices closer than the threshold are one.
                clustering = FastClusteringConfig(numClusters = -1, threshold = 0.5f),
                minDurationOn = 0.3f,
                minDurationOff = 0.5f,
            ),
        ).also { this.engine = it }

        return engine.process(pcm).map { Segment(it.start, it.end, it.speaker) }.sortedBy { it.start }
    }

    private const val THREADS = 4
}

/**
 * The words given to the speakers of the segments, as turns. A word is its speaker's whose
 * segment it starts in (the latest one where two overlap), else the nearest segment's. A lone
 * word between two turns of one speaker is theirs too: a word at a segment's edge, not a reply.
 * Without segments everything is one speaker's.
 */
fun turns(words: List<Word>, segments: List<Segment>): List<Turn> {
    if (words.isEmpty()) return emptyList()

    fun speaker(word: Word): Int {
        val at = word.ms / 1000f
        val inside = segments.lastOrNull { at >= it.start && at < it.end }
        val nearest = inside ?: segments.minByOrNull { minOf(kotlin.math.abs(at - it.start), kotlin.math.abs(at - it.end)) }
        return nearest?.speaker ?: 0
    }

    // Runs of words of one speaker, each with when it started.
    val runs = ArrayList<Triple<Int, MutableList<String>, Long>>()
    for (word in words) {
        val who = speaker(word)
        val last = runs.lastOrNull()
        if (last?.first == who) last.second += word.text else runs += Triple(who, mutableListOf(word.text), word.ms)
    }

    val merged = ArrayList<Triple<Int, MutableList<String>, Long>>()
    for ((i, run) in runs.withIndex()) {
        val before = merged.lastOrNull()
        // One word of another speaker inside a turn that goes on after it.
        val lone = run.second.size == 1 && before != null && i < runs.lastIndex && before.first == runs[i + 1].first
        if (before != null && (before.first == run.first || lone)) before.second += run.second
        else merged += Triple(run.first, run.second.toMutableList(), run.third)
    }

    val numbers = HashMap<Int, Int>()
    return merged.map { (who, said, ms) ->
        Turn(numbers.getOrPut(who) { numbers.size + 1 }, said.joinToString(" "), ms)
    }
}

/** The turns as text: `Speaker 1: …`, a turn a paragraph. One speaker's words alone carry no label. */
fun script(turns: List<Turn>): String {
    val many = turns.map { it.speaker }.distinct().size > 1
    // A turn a line, each with the time it started, so a conversation can be followed and referred
    // to. One speaker needs no label, but still a line and a time each.
    return turns.joinToString("\n") {
        val who = if (many) "Speaker ${it.speaker}: " else ""
        "${clock(it.ms)} $who${it.text}"
    }
}
