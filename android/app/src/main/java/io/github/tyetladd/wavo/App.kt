// Copyright (c) tyetladd | MIT License | https://github.com/tyetladd/wavo-app

package io.github.tyetladd.wavo

import android.app.Application
import android.content.Context
import java.io.File

/** What the person set. Every behaviour follows one of these, and the screens show the stored value. */
class Settings(context: Context) {
    private val prefs = context.getSharedPreferences("settings", Context.MODE_PRIVATE)

    /** The model that transcribes, by its short name; `null` until one is downloaded. */
    var model: String?
        get() = prefs.getString("model", null)
        set(value) = prefs.edit().putString("model", value).apply()

    /**
     * `gpu` (Vulkan), the default: about twice the CPU's speed on a Pixel 7 Pro. Or `cpu`, which
     * the engine also goes back to, here in the settings, when the GPU fails or crashes the app.
     */
    var backend: String
        get() = prefs.getString("backend", "gpu") ?: "gpu"
        set(value) = prefs.edit().putString("backend", value).apply()

    /** The language of the speech as a code, `null` to leave it to the model. */
    var language: String?
        get() = prefs.getString("language", null)
        set(value) = prefs.edit().putString("language", value).apply()

    /** Take hesitation sounds out and write Russian numbers in digits. */
    var clean: Boolean
        get() = prefs.getBoolean("clean", true)
        set(value) = prefs.edit().putBoolean("clean", value).apply()

    /**
     * Set while the GPU loads or runs a model, and written at once: a GPU driver that crashes
     * takes the app with it, and the next start finds this still set.
     */
    var gpuBusy: Boolean
        get() = prefs.getBoolean("gpu_busy", false)
        set(value) { prefs.edit().putBoolean("gpu_busy", value).commit() }

    /** The GPU failed or crashed the app, and the engine went back to the CPU: the settings screen says so. */
    var gpuCrashed: Boolean
        get() = prefs.getBoolean("gpu_crashed", false)
        set(value) = prefs.edit().putBoolean("gpu_crashed", value).apply()

    /** Transcribe each piece of a dictation at its pause, while the person speaks on. */
    var live: Boolean
        get() = prefs.getBoolean("live", true)
        set(value) = prefs.edit().putBoolean("live", value).apply()

    /**
     * The quiet that ends a piece, in milliseconds. Shorter shows the text sooner; longer keeps a
     * breath or a search for a word from ending a sentence, which puts full stops inside one.
     */
    var pauseMs: Int
        get() = prefs.getInt("pause_ms", 500)
        set(value) = prefs.edit().putInt("pause_ms", value).apply()

    /** The app's screen records a conversation and labels its speakers. */
    var conversation: Boolean
        get() = prefs.getBoolean("conversation", false)
        set(value) = prefs.edit().putBoolean("conversation", value).apply()

    /** Minutes without a dictation after which the model leaves the memory. */
    var unloadAfterMinutes: Int
        get() = prefs.getInt("unload_after", 10)
        set(value) = prefs.edit().putInt("unload_after", value).apply()

    /**
     * Keep each dictation's recording beside its text in History, as a WAV in the app's folder.
     * On by default, as on the desktop; nothing is pruned, so they stay until deleted.
     */
    var keepRecordings: Boolean
        get() = prefs.getBoolean("keep_recordings", true)
        set(value) = prefs.edit().putBoolean("keep_recordings", value).apply()

    /** The recording pill and its moving gradient; off, the microphone button's pulse is all. */
    var animate: Boolean
        get() = prefs.getBoolean("animate", true)
        set(value) = prefs.edit().putBoolean("animate", value).apply()

    /**
     * Keep recording when the app is left, under a notification with the time and stop/pause.
     * Off, leaving the app ends the dictation, as it always did. The keyboard is not affected:
     * its text goes into a field, and leaving means a different field.
     */
    var background: Boolean
        get() = prefs.getBoolean("background", false)
        set(value) = prefs.edit().putBoolean("background", value).apply()
}

class App : Application() {
    lateinit var settings: Settings
        private set
    lateinit var history: History
        private set

    /**
     * The Dictate screen's dictation, owned here rather than by the Activity: with the background
     * setting on it goes on while the app is left, and the service's notification drives the same
     * instance the screen shows. The keyboard keeps one of its own.
     */
    lateinit var dictation: Dictation
        private set

    /** The models' folder: a file for each model downloaded. */
    val models: File get() = File(filesDir, "models")

    override fun onCreate() {
        super.onCreate()
        instance = this
        settings = Settings(this)
        history = History(this)
        dictation = Dictation(this, conversations = true)
        RecordService.channel(this)
        // The app died with the GPU at work: back to the CPU, or every start would crash again.
        if (settings.gpuBusy) {
            settings.gpuBusy = false
            settings.backend = "cpu"
            settings.gpuCrashed = true
        }
        Speakers.refresh()
        // A crash between a recording and its row: tidied off the main thread, before anything
        // shows. Nothing written since this start is an orphan, and a failure here must not take
        // the process with it.
        val started = System.currentTimeMillis()
        kotlin.concurrent.thread(name = "wavo-reconcile") { runCatching { history.reconcile(started) } }
        // What the first builds' downloader left: nothing reads it now.
        File(filesDir, "hub").deleteRecursively()
    }

    companion object {
        lateinit var instance: App
            private set
    }
}
