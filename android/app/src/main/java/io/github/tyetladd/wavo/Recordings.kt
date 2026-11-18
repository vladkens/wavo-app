// Copyright (c) tyetladd | MIT License | https://github.com/tyetladd/wavo-app

package io.github.tyetladd.wavo

import java.io.BufferedOutputStream
import java.io.File
import java.io.FileOutputStream
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

/**
 * A dictation's recording: `recordings/YYYY-MM-DD/<unix ms>.wav`, 16 kHz mono 16-bit, the whole
 * recording, named for its start as the desktop's are. The file is written before its history row,
 * so a crash in between leaves a file to reconcile rather than a row pointing at nothing.
 *
 * Nothing is pruned and no cap is applied: a recording stays until the person deletes it.
 */
class Recordings(private val root: File) {
    /** A name for a recording starting at `atMs`, never one already taken. */
    fun name(atMs: Long): String {
        val day = SimpleDateFormat("yyyy-MM-dd", Locale.US).format(Date(atMs))
        var name = "$day/$atMs.wav"
        var n = 1
        while (File(root, name).exists()) name = "$day/$atMs-${n++}.wav"
        return name
    }

    /** The file `name` points at, or `null` when it could point outside the recordings folder. */
    fun file(name: String): File? {
        if (name.isEmpty()) return null
        // A name is a relative path inside the folder and nothing else: no absolute path, no "..".
        // File(root, "/x") would quietly resolve inside root, so the shape is checked before the
        // canonical one, and the canonical check stays as a second wall.
        if (File(name).isAbsolute || name.startsWith("/") || name.startsWith("\\")) return null
        if (name.split('/', '\\').any { it == ".." }) return null
        val base = runCatching { root.canonicalFile }.getOrNull() ?: return null
        val file = runCatching { File(root, name).canonicalFile }.getOrNull() ?: return null
        return file.takeIf { it.path.startsWith(base.path + File.separator) }
    }

    /** Writes `pcm` as a 16 kHz mono 16-bit WAV, making the day's folder. */
    fun write(name: String, pcm: FloatArray) {
        val file = file(name) ?: throw IllegalArgumentException("a recording name outside the folder: $name")
        file.parentFile?.mkdirs()
        BufferedOutputStream(FileOutputStream(file)).use { out ->
            out.write(header(pcm.size * 2))
            val buf = ByteArray(8192)
            var at = 0
            while (at < pcm.size) {
                val n = minOf(buf.size / 2, pcm.size - at)
                for (i in 0 until n) {
                    val v = (pcm[at + i].coerceIn(-1f, 1f) * 32768f).toInt().coerceIn(-32768, 32767)
                    buf[i * 2] = (v and 0xFF).toByte()
                    buf[i * 2 + 1] = ((v shr 8) and 0xFF).toByte()
                }
                out.write(buf, 0, n * 2)
                at += n
            }
        }
    }

    /** The recording's samples back as floats, or `null` when it is gone or not a WAV we wrote. */
    fun read(name: String): FloatArray? {
        val file = file(name)?.takeIf { it.isFile } ?: return null
        val bytes = runCatching { file.readBytes() }.getOrNull() ?: return null
        if (bytes.size < 44 || String(bytes, 0, 4) != "RIFF" || String(bytes, 8, 4) != "WAVE") return null
        val data = le32(bytes, 40)
        if (data < 0 || 44 + data > bytes.size) return null
        return FloatArray(data / 2) { i ->
            val at = 44 + i * 2
            val v = (bytes[at].toInt() and 0xFF) or (bytes[at + 1].toInt() shl 8)
            v.toShort() / 32768f
        }
    }

    private fun le32(b: ByteArray, at: Int): Int =
        (b[at].toInt() and 0xFF) or ((b[at + 1].toInt() and 0xFF) shl 8) or
            ((b[at + 2].toInt() and 0xFF) shl 16) or ((b[at + 3].toInt() and 0xFF) shl 24)

    /** Removes a recording; a missing one, or a name outside the folder, is nothing to do. */
    fun delete(name: String) {
        val file = file(name) ?: return
        if (file.delete()) file.parentFile?.takeIf { it.list()?.isEmpty() == true }?.delete()
    }

    /**
     * The recordings no history row points at: what a crash between the file and the row left.
     * `olderThan` spares a file written since this process started — a dictation completing now
     * has its file before its row, and is not an orphan.
     */
    fun orphans(known: Set<String>, olderThan: Long = Long.MAX_VALUE): List<String> =
        root.walkTopDown()
            .filter { it.isFile && it.extension == "wav" && it.lastModified() < olderThan }
            .map { it.relativeTo(root).path }
            .filter { it !in known }
            .sorted()
            .toList()

    /** The rows whose recording is gone: their text stays, their audio reference goes. */
    fun dangling(rows: List<Entry>): List<Long> =
        rows.filter { it.audio != null && file(it.audio)?.exists() != true }.map { it.id }

    /** The canonical 44-byte WAV header for `data` bytes of 16 kHz mono 16-bit samples. */
    private fun header(data: Int): ByteArray {
        val head = ByteArray(44)
        fun text(at: Int, s: String) = s.forEachIndexed { i, c -> head[at + i] = c.code.toByte() }
        fun le16(at: Int, v: Int) {
            head[at] = (v and 0xFF).toByte()
            head[at + 1] = ((v shr 8) and 0xFF).toByte()
        }
        fun le32(at: Int, v: Int) {
            le16(at, v and 0xFFFF)
            le16(at + 2, (v ushr 16) and 0xFFFF)
        }

        text(0, "RIFF")
        le32(4, 36 + data)
        text(8, "WAVE")
        text(12, "fmt ")
        le32(16, 16)
        le16(20, 1)
        le16(22, 1)
        le32(24, Recorder.RATE)
        le32(28, Recorder.RATE * 2)
        le16(32, 2)
        le16(34, 16)
        text(36, "data")
        le32(40, data)
        return head
    }
}
