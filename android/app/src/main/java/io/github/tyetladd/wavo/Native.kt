// Copyright (c) tyetladd | MIT License | https://github.com/tyetladd/wavo-app

package io.github.tyetladd.wavo

/**
 * The engine, `core/` in Rust: see its `jni.rs` for what each function does. Every function throws
 * a [RuntimeException] with the engine's message when it fails. [load], [transcribe] and [free] are
 * called on one thread, [Engine]'s.
 */
object Native {
    init {
        System.loadLibrary("wavo_android")
    }

    fun interface Progress {
        /** Bytes done and in all; `false` stops the download. */
        fun onProgress(done: Long, total: Long): Boolean
    }

    @JvmStatic external fun models(root: String): String

    @JvmStatic external fun pull(root: String, name: String, progress: Progress): String

    @JvmStatic external fun fetch(root: String, url: String, name: String, progress: Progress): String

    @JvmStatic external fun clean(text: String, language: String?): String

    @JvmStatic external fun remove(root: String, name: String): Long

    @JvmStatic external fun load(path: String, backend: String): Long

    @JvmStatic external fun device(handle: Long): String

    @JvmStatic external fun languages(handle: Long): String

    @JvmStatic external fun transcribe(handle: Long, pcm: FloatArray, language: String?, clean: Boolean): String

    @JvmStatic external fun free(handle: Long)

    /**
     * The HUD pill's pixels: straight RGBA rows, `w * scale` by `h * scale`, the colours at `ms`
     * into their flow, or the rim over them with [edge]. The desktop's own gradient, so the phone's
     * pill is the same picture. Called on the UI thread while a dictation shows; it is pure maths.
     */
    @JvmStatic external fun gradient(w: Float, h: Float, scale: Float, ms: Long, edge: Boolean): ByteArray
}
