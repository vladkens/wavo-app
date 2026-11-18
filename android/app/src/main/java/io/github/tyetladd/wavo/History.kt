// Copyright (c) tyetladd | MIT License | https://github.com/tyetladd/wavo-app

package io.github.tyetladd.wavo

import android.content.ContentValues
import android.content.Context
import android.database.sqlite.SQLiteDatabase
import android.database.sqlite.SQLiteOpenHelper
import kotlinx.coroutines.flow.MutableStateFlow
import java.io.File

/** A dictation kept. */
data class Entry(
    val id: Long,
    /** When, in milliseconds since 1970. */
    val at: Long,
    val text: String,
    val model: String,
    val audioMs: Long,
    val tookMs: Long,
    /** The recording's name in the recordings folder, or `null` when none was kept. */
    val audio: String? = null,
    /** What the person named this dictation, or `null` while it has no name. */
    val label: String? = null,
)

/**
 * A dictation as shared text: its name when it has one, when it was said, and what was said. No
 * model, no timings, no audio — enough to read in a note, and nothing to clutter it.
 */
fun export(entry: Entry): String {
    val when_ = java.text.SimpleDateFormat("d MMM yyyy, HH:mm", java.util.Locale.getDefault())
        .format(java.util.Date(entry.at))
    return listOfNotNull(entry.label?.trim()?.takeIf { it.isNotEmpty() }, when_, entry.text)
        .joinToString("\n\n")
}

/**
 * Every dictation with text, the newest first, and the recording each points at. The newest 500
 * are shown; nothing is pruned, so a row and its recording stay until the person deletes them.
 */
class History(context: Context) : SQLiteOpenHelper(context, "history.db", null, 3) {
    val entries = MutableStateFlow<List<Entry>>(emptyList())

    /** The recordings the rows point at: History deletes them with their rows. */
    val recordings = Recordings(File(context.filesDir, "recordings"))

    init {
        reload()
    }

    override fun onCreate(db: SQLiteDatabase) {
        db.execSQL(
            "CREATE TABLE history (id INTEGER PRIMARY KEY AUTOINCREMENT, at INTEGER NOT NULL, " +
                "text TEXT NOT NULL, model TEXT NOT NULL, audio_ms INTEGER NOT NULL, took_ms INTEGER NOT NULL, " +
                "audio TEXT, label TEXT)",
        )
    }

    override fun onUpgrade(db: SQLiteDatabase, from: Int, to: Int) {
        // Version 1 kept the text only; version 2 added the recording. Its rows stay as they are.
        if (from < 2) db.execSQL("ALTER TABLE history ADD COLUMN audio TEXT")
        if (from < 3) db.execSQL("ALTER TABLE history ADD COLUMN label TEXT")
    }

    /** Names a dictation, or clears its name with `null` or a blank. */
    fun label(id: Long, label: String?) {
        val value = label?.trim()?.takeIf { it.isNotEmpty() }
        val row = ContentValues().apply { put("label", value) }
        writableDatabase.update("history", row, "id = ?", arrayOf(id.toString()))
        reload()
    }

    fun add(heard: Heard, audio: String? = null) {
        val row = ContentValues().apply {
            put("at", System.currentTimeMillis())
            put("text", heard.text)
            put("model", heard.model)
            put("audio_ms", heard.audioMs)
            put("took_ms", heard.tookMs)
            put("audio", audio)
        }
        writableDatabase.insert("history", null, row)
        reload()
    }

    fun delete(id: Long) {
        val audio = audioOf(id)
        writableDatabase.delete("history", "id = ?", arrayOf(id.toString()))
        // Several versions of one recording share its file: it goes only when the last one does.
        if (audio != null && !stillReferenced(audio)) recordings.delete(audio)
        reload()
    }

    /** Whether another row still points at `audio`. */
    private fun stillReferenced(audio: String): Boolean =
        readableDatabase.query("history", arrayOf("id"), "audio = ?", arrayOf(audio), null, null, null, "1").use { c ->
            c.count > 0
        }

    fun clear() {
        val kept = ArrayList<String>()
        readableDatabase.query("history", arrayOf("audio"), null, null, null, null, null).use { c ->
            while (c.moveToNext()) c.getString(0)?.let { kept += it }
        }
        writableDatabase.delete("history", null, null)
        kept.forEach { recordings.delete(it) }
        reload()
    }

    /** The recording a row points at, read before the row goes. */
    private fun audioOf(id: Long): String? =
        readableDatabase.query("history", arrayOf("audio"), "id = ?", arrayOf(id.toString()), null, null, null).use { c ->
            if (c.moveToFirst()) c.getString(0) else null
        }

    /**
     * What a start reconciles: a recording no row points at goes — a crash between the file and the
     * row left it, and the dictation it came from produced no text to link it to — and a row whose
     * recording is gone keeps its text and loses the reference.
     */
    fun reconcile(since: Long = Long.MAX_VALUE) {
        val rows = all()
        recordings.orphans(rows.mapNotNull { it.audio }.toSet(), since).forEach { recordings.delete(it) }
        val gone = recordings.dangling(rows)
        if (gone.isNotEmpty()) {
            gone.forEach { writableDatabase.execSQL("UPDATE history SET audio = NULL WHERE id = ?", arrayOf(it)) }
            reload()
        }
    }

    private fun reload() {
        entries.value = read("500")
    }

    /** Every row, not only the 500 shown: nothing is pruned, so reconciliation sees them all. */
    private fun all(): List<Entry> = read(null)

    private fun read(limit: String?): List<Entry> {
        val out = ArrayList<Entry>()
        val columns = arrayOf("id", "at", "text", "model", "audio_ms", "took_ms", "audio", "label")
        readableDatabase.query("history", columns, null, null, null, null, "id DESC", limit).use { c ->
            while (c.moveToNext()) {
                out += Entry(
                    c.getLong(0), c.getLong(1), c.getString(2), c.getString(3),
                    c.getLong(4), c.getLong(5), c.getString(6), c.getString(7),
                )
            }
        }
        return out
    }
}
