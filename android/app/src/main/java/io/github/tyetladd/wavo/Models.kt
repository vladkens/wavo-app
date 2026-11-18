// Copyright (c) tyetladd | MIT License | https://github.com/tyetladd/wavo-app

package io.github.tyetladd.wavo

import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.update
import org.json.JSONArray
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.concurrent.thread

/** A model of the engine's list. */
data class ModelInfo(
    val name: String,
    val family: String,
    val maker: String,
    /** For people: `25 European languages`. */
    val languages: String,
    /** For programs: `[ru]`. */
    val langs: List<String>,
    val size: Long,
    val punctuated: Boolean,
    /** The file, once downloaded. */
    val path: String?,
) {
    val downloaded: Boolean get() = path != null

    /** Small and quick enough for a phone's CPU, one for each need. */
    val recommended: Boolean get() = name in RECOMMENDED

    /** Many seconds of work for a short phrase on a phone's CPU. */
    val slow: Boolean get() = SLOW.any { name.startsWith(it) }

    companion object {
        val RECOMMENDED = listOf("parakeet-ultra", "gigaam-v3", "parakeet-v2", "moonshine-base", "whisper-small")
        val SLOW = listOf("whisper-turbo", "whisper-medium", "whisper-large", "breeze-asr")

        /** The engine's list from its JSON, the recommended models first, in [RECOMMENDED]'s order. */
        fun parse(json: String): List<ModelInfo> {
            val array = JSONArray(json)
            val all = (0 until array.length()).map { i ->
                val m = array.getJSONObject(i)
                val langs = m.getJSONArray("langs")
                ModelInfo(
                    name = m.getString("name"),
                    family = m.getString("family"),
                    maker = m.getString("maker"),
                    languages = m.getString("languages"),
                    langs = (0 until langs.length()).map(langs::getString),
                    size = m.getLong("size"),
                    punctuated = m.getBoolean("punctuated"),
                    path = if (m.isNull("path")) null else m.getString("path"),
                )
            }
            val rank = { m: ModelInfo -> RECOMMENDED.indexOf(m.name).let { if (it < 0) Int.MAX_VALUE else it } }
            return all.sortedBy(rank)
        }
    }
}

/** A download as the models screen shows it. */
data class Download(val done: Long = 0, val total: Long = 0, val error: String? = null)

/** The models in the app's folder, and their downloads. */
object Models {
    val list = MutableStateFlow<List<ModelInfo>>(emptyList())
    val downloads = MutableStateFlow<Map<String, Download>>(emptyMap())

    private val cancelled = ConcurrentHashMap<String, AtomicBoolean>()
    private val root: String get() = App.instance.models.path

    /** Reads the folder again. Milliseconds: it only looks for the files. */
    fun refresh(): List<ModelInfo> {
        val all = ModelInfo.parse(Native.models(root))
        list.value = all
        return all
    }

    /** The file of a downloaded model. */
    fun path(name: String): String? = refresh().firstOrNull { it.name == name }?.path

    /**
     * Downloads a model on a thread of its own; one cut short resumes at the next call. The first
     * model downloaded becomes the one that transcribes.
     */
    fun download(name: String) {
        if (cancelled.putIfAbsent(name, AtomicBoolean(false)) != null) return
        downloads.update { it + (name to Download()) }

        thread(name = "wavo-pull-$name") {
            val stop = cancelled.getValue(name)
            val pulled = runCatching {
                Native.pull(root, name) { done, total ->
                    downloads.update { it + (name to Download(done, total)) }
                    !stop.get()
                }
            }
            cancelled.remove(name)

            val error = pulled.exceptionOrNull()?.message
            if (error == null || stop.get()) {
                downloads.update { it - name }
            } else {
                downloads.update { it + (name to (it[name] ?: Download()).copy(error = error)) }
            }

            val settings = App.instance.settings
            if (pulled.isSuccess && settings.model == null) settings.model = name
            refresh()
        }
    }

    /** Stops a download; what it got stays for the next one to resume. */
    fun cancel(name: String) {
        cancelled[name]?.set(true)
        downloads.update { it - name }
    }

    /** Deletes a model; the one that transcribes leaves the memory first. */
    fun delete(name: String) {
        val settings = App.instance.settings
        if (settings.model == name) {
            settings.model = null
            Engine.unload()
        }

        thread(name = "wavo-remove-$name") {
            runCatching { Native.remove(root, name) }
            refresh()
        }
    }
}

/** `0.74 GB`, `80 MB`. */
fun size(bytes: Long): String = when {
    bytes >= 200_000_000 -> "%.2f GB".format(java.util.Locale.US, bytes / 1e9)
    else -> "%d MB".format(java.util.Locale.US, bytes / 1_000_000)
}
