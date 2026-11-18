// Copyright (c) tyetladd | MIT License | https://github.com/tyetladd/wavo-app

package io.github.tyetladd.wavo

import android.Manifest
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.os.Bundle
import android.graphics.Bitmap
import android.media.MediaPlayer
import android.net.Uri
import android.os.SystemClock
import android.provider.Settings as SystemSettings
import android.text.format.DateUtils
import android.view.inputmethod.InputMethodManager
import androidx.activity.ComponentActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.clickable
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.Delete
import androidx.compose.material.icons.filled.Menu
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.LargeFloatingActionButton
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Scaffold
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.dynamicDarkColorScheme
import androidx.compose.material3.dynamicLightColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.graphics.drawscope.drawIntoCanvas
import androidx.compose.ui.graphics.nativeCanvas
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.platform.LocalDensity
import androidx.activity.compose.BackHandler
import androidx.compose.ui.unit.dp
import androidx.core.content.FileProvider
import androidx.core.content.pm.PackageInfoCompat
import kotlinx.coroutines.delay
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

class MainActivity : ComponentActivity() {
    // Owned by the app, not by this Activity: with the background setting on the recording goes on
    // while the screen is left, and the service's notification drives the same instance.
    private val dictation get() = App.instance.dictation

    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        Models.refresh()
        setContent { Theme { Wavo(dictation) } }
    }

    override fun onStart() {
        super.onStart()
        Engine.prewarm()
    }

    override fun onStop() {
        // With the background setting on, the service keeps the recording and its notification.
        // Off, leaving the app ends the dictation, as it always did.
        if (!App.instance.settings.background) dictation.cancel()
        super.onStop()
    }
}

@Composable
private fun Theme(content: @Composable () -> Unit) {
    val context = LocalContext.current
    val dark = isSystemInDarkTheme()
    val colors = when {
        android.os.Build.VERSION.SDK_INT >= 31 ->
            if (dark) dynamicDarkColorScheme(context) else dynamicLightColorScheme(context)
        dark -> darkColorScheme()
        else -> lightColorScheme()
    }
    MaterialTheme(colorScheme = colors, content = content)
}

private enum class Tab(val label: String) { Dictate("Dictate"), Models("Models"), History("History"), Settings("Settings") }

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun Wavo(dictation: Dictation) {
    var tab by rememberSaveable { mutableStateOf(Tab.Dictate) }
    val models by Models.list.collectAsState()
    // Models is a screen under Settings, so the system back goes there, not out of the app.
    BackHandler(enabled = tab == Tab.Models) { tab = Tab.Settings }
    // Without a model there is nothing to dictate with: the first screen is where to get one.
    LaunchedEffect(Unit) { if (models.none { it.downloaded }) tab = Tab.Models }

    Scaffold(
        topBar = {
            TopAppBar(
                // Nothing over the Dictate screen: a logo and its look are still to come.
                title = { if (tab != Tab.Dictate) Text(tab.label) },
                navigationIcon = {
                    // Models lives under Settings now, so it is a screen you come back from.
                    if (tab == Tab.Models) {
                        IconButton(onClick = { tab = Tab.Settings }) {
                            Icon(Icons.AutoMirrored.Filled.ArrowBack, "Back to Settings")
                        }
                    }
                },
            )
        },
        bottomBar = {
            NavigationBar {
                Tab.entries.filter { it != Tab.Models }.forEach { t ->
                    NavigationBarItem(
                        // Settings stays lit while Models is open: that is where it came from.
                        selected = tab == t || (t == Tab.Settings && tab == Tab.Models),
                        onClick = { tab = t },
                        label = { Text(t.label) },
                        icon = {
                            when (t) {
                                Tab.Dictate -> Icon(painterResource(R.drawable.ic_mic), null)
                                Tab.History -> Icon(Icons.Filled.Menu, null)
                                else -> Icon(Icons.Filled.Settings, null)
                            }
                        },
                    )
                }
            }
        },
    ) { padding ->
        Box(Modifier.padding(padding).fillMaxSize()) {
            when (tab) {
                Tab.Dictate -> DictateScreen(dictation, toModels = { tab = Tab.Models })
                Tab.Models -> ModelsScreen()
                Tab.History -> HistoryScreen()
                Tab.Settings -> SettingsScreen(onModels = { tab = Tab.Models })
            }
        }
    }
}

private fun openUrl(context: Context, url: String) {
    runCatching { context.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(url))) }
}

private fun copy(context: Context, text: String) {
    val clipboard = context.getSystemService(ClipboardManager::class.java)
    clipboard.setPrimaryClip(ClipData.newPlainText("Wavo", text))
}

private fun share(context: Context, text: String) {
    val send = Intent(Intent.ACTION_SEND).setType("text/plain").putExtra(Intent.EXTRA_TEXT, text)
    context.startActivity(Intent.createChooser(send, null))
}

/** `11.0 s of speech in 0.94 s (12× real time)`. */
private fun timing(audioMs: Long, tookMs: Long): String {
    val times = if (tookMs > 0) " (%.0f× real time)".format(audioMs.toDouble() / tookMs) else ""
    return "%.1f s of speech in %.2f s%s".format(audioMs / 1000.0, tookMs / 1000.0, times)
}

// MARK: Dictate

@Composable
private fun DictateScreen(dictation: Dictation, toModels: () -> Unit) {
    val context = LocalContext.current
    val ui by dictation.ui.collectAsState()
    val engine by Engine.state.collectAsState()
    var allowed by remember { mutableStateOf(dictation.allowed) }
    val ask = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        allowed = granted
        if (granted) dictation.start()
    }
    val chosen = App.instance.settings.model

    // The clock of a recording.
    var now by remember { mutableLongStateOf(SystemClock.elapsedRealtime()) }
    val recording = ui.phase == Dictation.Phase.Recording
    val paused = ui.phase == Dictation.Phase.Paused
    LaunchedEffect(recording) {
        while (recording) {
            now = SystemClock.elapsedRealtime()
            delay(200)
        }
    }
    // With the background setting on, the service takes over the moment a dictation starts, so
    // leaving the app keeps the microphone live under its notification.
    LaunchedEffect(ui.phase) {
        if (ui.phase == Dictation.Phase.Recording && App.instance.settings.background) {
            RecordService.start(context)
        }
    }

    // The screen stays on while the person speaks.
    val view = LocalView.current
    DisposableEffect(recording) {
        view.keepScreenOn = recording
        onDispose { view.keepScreenOn = false }
    }

    Column(
        Modifier.fillMaxSize().padding(horizontal = 20.dp, vertical = 12.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        val scroll = rememberScrollState()
        // The words keep coming while the person speaks: follow them to the end, so the newest are
        // on screen rather than the first lines of a long dictation. Only while recording — a
        // finished text is left where the person scrolled it.
        LaunchedEffect(ui.partial, ui.phase) {
            if (ui.phase == Dictation.Phase.Recording) scroll.animateScrollTo(scroll.maxValue)
        }
        Column(Modifier.weight(1f).fillMaxWidth().verticalScroll(scroll)) {
            val last = ui.last
            if (ui.phase != Dictation.Phase.Idle) {
                // A new dictation clears the field at once, rather than leaving the last text on
                // screen until the first words of this one arrive.
                if (ui.partial.isNotEmpty()) {
                    Text(ui.partial, style = MaterialTheme.typography.headlineSmall)
                }
            } else if (last == null) {
                Text(
                    "Tap the microphone and speak. The text shows here; nothing leaves the phone.",
                    style = MaterialTheme.typography.bodyLarge,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            } else {
                SelectionContainer { Text(last.text, style = MaterialTheme.typography.headlineSmall) }
                Spacer(Modifier.height(12.dp))
                Text(
                    "${timing(last.audioMs, last.tookMs)}\n" +
                        "Ready %.1f s after the stop\n".format(last.waitMs / 1000.0) +
                        "${last.model} on ${last.device}",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Spacer(Modifier.height(8.dp))
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    FilledTonalButton(onClick = { copy(context, last.text) }) { Text("Copy") }
                    OutlinedButton(onClick = { share(context, last.text) }) { Text("Share") }
                }
            }
        }

        val status = when {
            recording -> "Listening  ${clock(ui.elapsed(now))}"
            paused -> "Paused — the same dictation goes on when you resume"
            ui.phase == Dictation.Phase.Transcribing ->
                (engine as? Engine.State.Loading)?.let { "Loading ${it.model}…" }
                    ?: if (ui.conversation) "Telling the speakers apart…" else "Transcribing…"
            chosen == null -> "No model yet"
            ui.error != null -> ui.error!!
            engine is Engine.State.Loading -> "Loading ${(engine as Engine.State.Loading).model}…"
            engine is Engine.State.Failed -> (engine as Engine.State.Failed).message
            engine is Engine.State.Ready -> "Ready"
            else -> ""
        }
        if (chosen != null && (ui.conversation || (ui.phase == Dictation.Phase.Idle && App.instance.settings.conversation))) {
            Text(
                "Conversation: speakers are labelled after the stop",
                style = MaterialTheme.typography.labelMedium,
                color = MaterialTheme.colorScheme.primary,
            )
        }
        Text(status, textAlign = TextAlign.Center, color = MaterialTheme.colorScheme.onSurfaceVariant)
        Spacer(Modifier.height(16.dp))

        // The pill as on the Mac, only with the animation setting on: the core's two layers,
        // refreshed at the desktop's own thirty-third of a second.
        val animating = Pill.animates(ui.phase, App.instance.settings.animate)
        val density = LocalDensity.current.density
        val language = App.instance.settings.language ?: ui.last?.language
        val pillWidth = Pill.width(language)
        val fieldLayer = remember { Pill.Layer() }
        val rimLayer = remember { Pill.Layer() }
        var field by remember { mutableStateOf<Bitmap?>(null) }
        var rim by remember { mutableStateOf<Bitmap?>(null) }
        var phaseStart by remember { mutableLongStateOf(SystemClock.elapsedRealtime()) }
        var recorded by remember { mutableLongStateOf(0L) }
        var tick by remember { mutableLongStateOf(SystemClock.elapsedRealtime()) }

        LaunchedEffect(ui.phase) {
            val now = SystemClock.elapsedRealtime()
            if (ui.phase == Dictation.Phase.Transcribing) recorded = ui.elapsed(now)
            phaseStart = now
        }
        LaunchedEffect(animating, pillWidth) {
            if (!animating) {
                // Paused keeps the last frame on screen; only going idle clears it.
                if (ui.phase == Dictation.Phase.Idle) {
                    field = null
                    rim = null
                }
                return@LaunchedEffect
            }
            // The rim does not move: it is made once, and only the colours are made again.
            rim = rimLayer.update(
                Native.gradient(pillWidth, Pill.HEIGHT, density, 0L, true),
                pillWidth, Pill.HEIGHT, density,
            )
            while (true) {
                tick = SystemClock.elapsedRealtime()
                // The flow follows the recording's own clock, so it freezes with a pause and
                // carries on across the stop, as the desktop's does.
                field = fieldLayer.update(
                    Native.gradient(pillWidth, Pill.HEIGHT, density, ui.elapsed(tick), false),
                    pillWidth, Pill.HEIGHT, density,
                )
                delay(33)
            }
        }
        if (App.instance.settings.animate && ui.phase != Dictation.Phase.Idle) {
            val elapsed = tick - phaseStart
            val transcribing = ui.phase == Dictation.Phase.Transcribing
            val settle =
                if (transcribing) Pill.ease((elapsed.toFloat() / Pill.SETTLE_MS).coerceIn(0f, 1f)) else 0f
            val label = when {
                transcribing && ui.pieces > 1 -> "${ui.piecesDone.coerceAtMost(ui.pieces)}/${ui.pieces}"
                transcribing -> clock(recorded)
                else -> clock(ui.elapsed(tick))
            }
            Canvas(Modifier.width(pillWidth.dp).height(Pill.HEIGHT.dp)) {
                drawIntoCanvas {
                    it.save()
                    it.scale(density, density)
                    Pill.draw(
                        it.nativeCanvas, ui.levels, ui.phase, settle,
                        (elapsed - Pill.SETTLE_MS).coerceAtLeast(0L), label, language, field, rim,
                    )
                    it.restore()
                }
            }
            Spacer(Modifier.height(12.dp))
        }

        if (chosen == null) {
            Button(onClick = toModels) { Text("Download a model") }
        } else {
            val scale = if (recording) 1f + minOf(ui.level * 2f, 1f) * 0.15f else 1f
            val ending = recording || paused
            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(16.dp),
            ) {
                LargeFloatingActionButton(
                    onClick = {
                        when {
                            ui.phase != Dictation.Phase.Idle -> dictation.toggle()
                            dictation.allowed -> dictation.start()
                            else -> ask.launch(Manifest.permission.RECORD_AUDIO)
                        }
                    },
                    containerColor = when {
                        recording -> MaterialTheme.colorScheme.error
                        paused || ui.phase == Dictation.Phase.Transcribing -> MaterialTheme.colorScheme.surfaceVariant
                        else -> MaterialTheme.colorScheme.primary
                    },
                    contentColor = if (recording) MaterialTheme.colorScheme.onError else MaterialTheme.colorScheme.onPrimary,
                    modifier = Modifier.graphicsLayer(scaleX = scale, scaleY = scale),
                ) {
                    Icon(
                        painterResource(if (ending) R.drawable.ic_stop else R.drawable.ic_mic),
                        contentDescription = if (ending) "Stop" else "Dictate",
                        modifier = Modifier.size(36.dp),
                    )
                }
                // Pause keeps the dictation open: resuming goes on into the same record. Always in
                // place, so the layout does not move when a dictation starts — inactive until there
                // is one to pause, and the same size as the stop beside it.
                LargeFloatingActionButton(
                    onClick = {
                        if (ending) {
                            if (paused) dictation.resume() else dictation.pause()
                        }
                    },
                    containerColor = if (ending) MaterialTheme.colorScheme.secondaryContainer else MaterialTheme.colorScheme.surfaceVariant,
                    contentColor = if (ending) MaterialTheme.colorScheme.onSecondaryContainer else MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.alpha(if (ending) 1f else 0.38f),
                ) {
                    Icon(
                        painterResource(if (paused) R.drawable.ic_play else R.drawable.ic_pause),
                        contentDescription = if (paused) "Resume" else "Pause",
                        modifier = Modifier.size(36.dp),
                    )
                }
            }
        }
        Spacer(Modifier.height(12.dp))
        if (!allowed && chosen != null) {
            Text("Wavo asks for the microphone at the first tap.", style = MaterialTheme.typography.bodySmall)
        }
    }
}

/** The switch that makes the app's recordings conversations, and the speaker models' download under it. */
@Composable
private fun ConversationRow() {
    val settings = App.instance.settings
    var on by remember { mutableStateOf(settings.conversation) }
    val ready by Speakers.ready.collectAsState()
    val download by Speakers.download.collectAsState()

    Column(Modifier.fillMaxWidth().padding(bottom = 8.dp)) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f)) {
                Text("Conversation")
                Text(
                    "The Dictate screen labels who said what: the text shows as you speak, the speakers " +
                        "after the stop. Up to 10 minutes. The keyboard isn't affected.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            Switch(checked = on, onCheckedChange = {
                on = it
                settings.conversation = it
            })
        }

        val pull = download
        if (on && !ready) {
            when {
                pull != null && pull.error == null -> {
                    LinearProgressIndicator(
                        progress = { pull.done.toFloat() / pull.total },
                        modifier = Modifier.fillMaxWidth().padding(top = 6.dp),
                    )
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Text("${size(pull.done)} of ${size(pull.total)}", Modifier.weight(1f), style = MaterialTheme.typography.bodySmall)
                        TextButton(onClick = { Speakers.cancel() }) { Text("Cancel") }
                    }
                }
                else -> {
                    pull?.error?.let {
                        Text(it, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall)
                    }
                    FilledTonalButton(onClick = { Speakers.download() }) {
                        Text(if (pull?.error != null) "Retry" else "Download the speaker models (${size(Speakers.size)})")
                    }
                }
            }
        }
    }
}

// MARK: Models

@Composable
private fun ModelsScreen() {
    val models by Models.list.collectAsState()
    val downloads by Models.downloads.collectAsState()
    var chosen by remember { mutableStateOf(App.instance.settings.model) }
    // The first download chooses itself.
    LaunchedEffect(models) { chosen = App.instance.settings.model }

    // A download goes on only while the app does: the screen stays on for it.
    val view = LocalView.current
    val downloading = downloads.values.any { it.error == null }
    DisposableEffect(downloading) {
        view.keepScreenOn = downloading
        onDispose { view.keepScreenOn = false }
    }

    LazyColumn(contentPadding = PaddingValues(16.dp), verticalArrangement = Arrangement.spacedBy(10.dp)) {
        item {
            Text(
                "A model is downloaded once and then works offline. Pick one for your language: " +
                    "Parakeet Ultra for 25 European languages, GigaAM v3 for Russian, Moonshine for the smallest.",
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        items(models, key = { it.name }) { model ->
            ModelCard(
                model = model,
                download = downloads[model.name],
                chosen = chosen == model.name,
                choose = {
                    App.instance.settings.model = model.name
                    chosen = model.name
                    Engine.unload()
                    Engine.prewarm()
                },
            )
        }
    }
}

@Composable
private fun ModelCard(model: ModelInfo, download: Download?, chosen: Boolean, choose: () -> Unit) {
    var confirm by remember { mutableStateOf(false) }

    Card(Modifier.fillMaxWidth()) {
        Column(Modifier.padding(14.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                if (model.downloaded) RadioButton(selected = chosen, onClick = choose)
                Column(Modifier.weight(1f)) {
                    Text(model.name, fontWeight = FontWeight.SemiBold)
                    Text(
                        "${model.languages} · ${size(model.size)} · ${model.maker}",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    val notes = listOfNotNull(
                        "Recommended".takeIf { model.recommended },
                        "Slow on a phone".takeIf { model.slow },
                        "Lowercase, no punctuation".takeIf { !model.punctuated },
                    )
                    if (notes.isNotEmpty()) {
                        Text(
                            notes.joinToString(" · "),
                            style = MaterialTheme.typography.labelMedium,
                            color = if (model.slow) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.primary,
                        )
                    }
                }
                when {
                    download != null && download.error == null ->
                        TextButton(onClick = { Models.cancel(model.name) }) { Text("Cancel") }
                    model.downloaded ->
                        IconButton(onClick = { confirm = true }) { Icon(Icons.Filled.Delete, "Delete ${model.name}") }
                    else ->
                        FilledTonalButton(onClick = { Models.download(model.name) }) {
                            Text(if (download?.error != null) "Retry" else "Download")
                        }
                }
            }

            if (download != null) {
                Spacer(Modifier.height(8.dp))
                if (download.error != null) {
                    Text(download.error, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall)
                } else if (download.total > 0) {
                    LinearProgressIndicator(
                        progress = { download.done.toFloat() / download.total },
                        modifier = Modifier.fillMaxWidth(),
                    )
                    Text(
                        "${size(download.done)} of ${size(download.total)}",
                        style = MaterialTheme.typography.bodySmall,
                    )
                } else {
                    LinearProgressIndicator(Modifier.fillMaxWidth())
                }
            }
        }
    }

    if (confirm) {
        AlertDialog(
            onDismissRequest = { confirm = false },
            title = { Text("Delete ${model.name}?") },
            text = { Text("It frees ${size(model.size)}. The model can be downloaded again.") },
            confirmButton = {
                TextButton(onClick = {
                    confirm = false
                    Models.delete(model.name)
                }) { Text("Delete") }
            },
            dismissButton = { TextButton(onClick = { confirm = false }) { Text("Cancel") } },
        )
    }
}

// MARK: History

@Composable
private fun HistoryScreen() {
    val context = LocalContext.current
    val history = App.instance.history
    val entries by history.entries.collectAsState()
    var confirm by remember { mutableStateOf(false) }
    var player by remember { mutableStateOf<MediaPlayer?>(null) }
    DisposableEffect(Unit) { onDispose { player?.release(); player = null } }
    val models by Models.list.collectAsState()
    var pickFor by remember { mutableStateOf<Entry?>(null) }
    var naming by remember { mutableStateOf<Entry?>(null) }
    var name by remember { mutableStateOf("") }
    var busy by remember { mutableStateOf<Long?>(null) }
    var failed by remember { mutableStateOf<String?>(null) }

    /**
     * Another version of the same recording, by another model: a new row pointing at the same file,
     * so the two texts can be compared and either kept. The engine runs it on its own thread, after
     * any dictation under way.
     */
    fun retranscribe(entry: Entry, model: String) {
        val audio = entry.audio ?: return
        val pcm = history.recordings.read(audio)
        if (pcm == null) {
            failed = "The recording is no longer there."
            return
        }
        failed = null
        busy = entry.id
        val settings = App.instance.settings
        // Conversation tells the speakers apart over the whole recording, so its cleanup waits for
        // them, exactly as a live dictation's does. Without the speaker models it is a plain text.
        val conversation = settings.conversation && Speakers.ready.value
        Engine.transcribeWith(model, pcm, clean = if (conversation) false else null) { result ->
            result.onFailure {
                busy = null
                failed = it.message ?: "The engine failed"
            }
            result.onSuccess { heard ->
                if (!conversation) {
                    busy = null
                    if (heard.text.isEmpty()) failed = "$model heard no speech in it."
                    else history.add(heard, audio)
                    return@onSuccess
                }
                kotlin.concurrent.thread(name = "wavo-speakers") {
                    val split = runCatching { Speakers.diarize(pcm) }
                    android.os.Handler(android.os.Looper.getMainLooper()).post {
                        busy = null
                        val turns = turns(heard.words, split.getOrDefault(emptyList())).map {
                            if (settings.clean) {
                                it.copy(text = Native.clean(it.text, heard.language ?: settings.language))
                            } else {
                                it
                            }
                        }
                        val text = script(turns.filter { it.text.isNotEmpty() })
                        if (text.isEmpty()) failed = "$model heard no speech in it."
                        else history.add(heard.copy(text = text), audio)
                    }
                }
            }
        }
    }

    /** Plays a dictation's recording; the one before stops, and it is released when it ends. */
    fun play(entry: Entry) {
        val file = entry.audio?.let { history.recordings.file(it) } ?: return
        player?.release()
        player = runCatching {
            MediaPlayer().apply {
                setDataSource(file.absolutePath)
                setOnCompletionListener { mp -> mp.release(); player = null }
                prepare()
                start()
            }
        }.getOrNull()
    }

    /** The dictation as text, for a note app: its name, when, and what was said. */
    fun shareText(entry: Entry) {
        context.startActivity(
            Intent.createChooser(
                Intent(Intent.ACTION_SEND).apply {
                    type = "text/plain"
                    putExtra(Intent.EXTRA_TEXT, export(entry))
                    entry.label?.let { putExtra(Intent.EXTRA_SUBJECT, it) }
                },
                "Share the text",
            ),
        )
    }

    fun shareAudio(entry: Entry) {
        val file = entry.audio?.let { history.recordings.file(it) } ?: return
        val uri = FileProvider.getUriForFile(context, "${context.packageName}.files", file)
        context.startActivity(
            Intent.createChooser(
                Intent(Intent.ACTION_SEND).apply {
                    type = "audio/wav"
                    putExtra(Intent.EXTRA_STREAM, uri)
                    addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                },
                "Share the recording",
            ),
        )
    }

    if (entries.isEmpty()) {
        Box(Modifier.fillMaxSize().padding(24.dp), contentAlignment = Alignment.Center) {
            Text(
                "Your dictations show here, from the app and from the keyboard, with their recordings " +
                    "when Keep recordings is on.",
                textAlign = TextAlign.Center,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        return
    }

    LazyColumn(contentPadding = PaddingValues(16.dp), verticalArrangement = Arrangement.spacedBy(10.dp)) {
        item {
            Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.End) {
                TextButton(onClick = { confirm = true }) { Text("Clear all") }
            }
        }
        items(entries, key = { it.id }) { entry ->
            Card(Modifier.fillMaxWidth().clickable { copy(context, entry.text) }) {
                Row(Modifier.padding(start = 14.dp, top = 10.dp, bottom = 10.dp), verticalAlignment = Alignment.Top) {
                    Column(Modifier.weight(1f)) {
                        // The name, editable: tap it to name this dictation, or to clear the name.
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            Text(
                                entry.label ?: "Name this…",
                                style = MaterialTheme.typography.labelLarge,
                                color = if (entry.label != null) MaterialTheme.colorScheme.primary else MaterialTheme.colorScheme.onSurfaceVariant,
                                modifier = Modifier.clickable {
                                    name = entry.label ?: ""
                                    naming = entry
                                },
                            )
                        }
                        Spacer(Modifier.height(4.dp))
                        Text(entry.text)
                        Spacer(Modifier.height(4.dp))
                        val at = DateUtils.getRelativeTimeSpanString(entry.at, System.currentTimeMillis(), DateUtils.MINUTE_IN_MILLIS)
                        Text(
                            "$at · ${entry.model} · ${timing(entry.audioMs, entry.tookMs)}",
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                        // The moment itself, at the end of the diagnostics: relative time above
                        // tells how long ago, this tells when.
                        Text(
                            SimpleDateFormat("d MMM yyyy, HH:mm:ss", Locale.getDefault()).format(Date(entry.at)),
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                        if (busy == entry.id) {
                            Text(
                                "Transcribing…",
                                style = MaterialTheme.typography.bodySmall,
                                color = MaterialTheme.colorScheme.primary,
                            )
                        } else {
                            // One line of commands. Sharing the text needs no recording; the rest do.
                            // The padding is tight so all four fit beside the delete button.
                            val tight = PaddingValues(horizontal = 6.dp)
                            Row(horizontalArrangement = Arrangement.spacedBy(2.dp)) {
                                TextButton(onClick = { shareText(entry) }, contentPadding = tight) { Text("Share text") }
                                if (entry.audio != null) {
                                    TextButton(onClick = { play(entry) }, contentPadding = tight) { Text("Play") }
                                    TextButton(onClick = { shareAudio(entry) }, contentPadding = tight) { Text("Audio") }
                                    TextButton(onClick = { pickFor = entry }, contentPadding = tight) { Text("Transcribe…") }
                                }
                            }
                        }
                    }
                    IconButton(onClick = { history.delete(entry.id) }) { Icon(Icons.Filled.Delete, "Delete") }
                }
            }
        }
        item { Text("Tap a dictation to copy it.", style = MaterialTheme.typography.bodySmall) }
    }

    val named = naming
    if (named != null) {
        AlertDialog(
            onDismissRequest = { naming = null },
            title = { Text("Name this dictation") },
            text = {
                Column {
                    OutlinedTextField(
                        value = name,
                        onValueChange = { name = it },
                        singleLine = true,
                        label = { Text("Name") },
                        modifier = Modifier.fillMaxWidth(),
                    )
                    Spacer(Modifier.height(8.dp))
                    Text(
                        "The name leads the card and the shared text. Clear it to leave the dictation unnamed.",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            },
            confirmButton = {
                TextButton(onClick = {
                    history.label(named.id, name)
                    naming = null
                }) { Text("Save") }
            },
            dismissButton = { TextButton(onClick = { naming = null }) { Text("Cancel") } },
        )
    }

    val pick = pickFor
    if (pick != null) {
        val options = models.filter { it.downloaded }
        AlertDialog(
            onDismissRequest = { pickFor = null },
            title = { Text("Transcribe with") },
            text = {
                Column {
                    Text(
                        "The same recording, another model. Its text is added as a new version, " +
                            "and both point at the one audio file.",
                        style = MaterialTheme.typography.bodySmall,
                    )
                    Spacer(Modifier.height(8.dp))
                    if (options.isEmpty()) Text("No model is downloaded yet.")
                    options.forEach { m ->
                        Row(
                            Modifier.fillMaxWidth().clickable {
                                pickFor = null
                                retranscribe(pick, m.name)
                            }.padding(vertical = 8.dp),
                        ) {
                            Column(Modifier.weight(1f)) {
                                Text(if (m.name == pick.model) "${m.name} — this one" else m.name)
                                Text(
                                    m.languages,
                                    style = MaterialTheme.typography.bodySmall,
                                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                                )
                            }
                        }
                    }
                }
            },
            confirmButton = {},
            dismissButton = { TextButton(onClick = { pickFor = null }) { Text("Cancel") } },
        )
    }

    val why = failed
    if (why != null) {
        AlertDialog(
            onDismissRequest = { failed = null },
            title = { Text("It didn't transcribe") },
            text = { Text(why) },
            confirmButton = { TextButton(onClick = { failed = null }) { Text("OK") } },
        )
    }

    if (confirm) {
        AlertDialog(
            onDismissRequest = { confirm = false },
            title = { Text("Clear the history?") },
            text = { Text("Every dictation kept here is deleted, with its recording.") },
            confirmButton = {
                TextButton(onClick = {
                    confirm = false
                    history.clear()
                }) { Text("Clear") }
            },
            dismissButton = { TextButton(onClick = { confirm = false }) { Text("Cancel") } },
        )
    }
}

// MARK: Settings

@Composable
private fun SettingsScreen(onModels: () -> Unit) {
    val context = LocalContext.current
    val settings = App.instance.settings
    val engine by Engine.state.collectAsState()
    val models by Models.list.collectAsState()

    var backend by remember { mutableStateOf(settings.backend) }
    var language by remember { mutableStateOf(settings.language) }
    var clean by remember { mutableStateOf(settings.clean) }
    var crashed by remember { mutableStateOf(settings.gpuCrashed) }
    var live by remember { mutableStateOf(settings.live) }
    var pauseMs by remember { mutableIntStateOf(settings.pauseMs) }
    var keep by remember { mutableStateOf(settings.keepRecordings) }
    var animate by remember { mutableStateOf(settings.animate) }
    var background by remember { mutableStateOf(settings.background) }
    val askNotifications = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { }
    var unloadAfter by remember { mutableIntStateOf(settings.unloadAfterMinutes) }

    Column(
        Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Section("About")
        val info = remember {
            runCatching { context.packageManager.getPackageInfo(context.packageName, 0) }.getOrNull()
        }
        Text("Wavo for Android", style = MaterialTheme.typography.titleMedium)
        Text(
            "Version ${info?.versionName ?: "?"} · build " +
                (info?.let { PackageInfoCompat.getLongVersionCode(it) }?.toString() ?: "?"),
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Text(
            "Local dictation: speech is recognized on this phone, and nothing leaves it. " +
                "Only a model's download uses the network.",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            TextButton(onClick = { openUrl(context, "https://github.com/vladkens/wavo-app") }) { Text("Source") }
            TextButton(onClick = { openUrl(context, "https://github.com/vladkens/wavo-app/blob/main/LICENSE") }) {
                Text("MIT licence")
            }
        }

        Section("Keyboard")
        Text(
            "Turn the Wavo voice keyboard on once, then switch to it in any app to dictate into a field.",
            style = MaterialTheme.typography.bodyMedium,
        )
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Button(onClick = { context.startActivity(Intent(SystemSettings.ACTION_INPUT_METHOD_SETTINGS)) }) {
                Text("Turn on")
            }
            OutlinedButton(onClick = { context.getSystemService(InputMethodManager::class.java).showInputMethodPicker() }) {
                Text("Switch keyboard")
            }
        }

        Section("Conversation")
        ConversationRow()

        Section("Engine")
        // The engine may have gone back to the CPU by itself: the stored value shows.
        LaunchedEffect(engine) {
            backend = settings.backend
            crashed = settings.gpuCrashed
        }
        Choice("GPU", "Vulkan. The faster one where it works; if it fails, the engine goes back to the CPU.", backend == "gpu") {
            backend = "gpu"
            settings.backend = "gpu"
            settings.gpuCrashed = false
            crashed = false
            Engine.unload()
            Engine.prewarm()
        }
        Choice("CPU", "The phone's fast cores. Works on every phone.", backend == "cpu") {
            backend = "cpu"
            settings.backend = "cpu"
            Engine.unload()
            Engine.prewarm()
        }
        if (crashed) {
            Text(
                "The GPU failed or crashed the app last time, so the engine went back to the CPU.",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.error,
            )
        }
        Text(
            when (val e = engine) {
                is Engine.State.Ready -> "${e.model} runs on ${e.device}"
                is Engine.State.Loading -> "Loading ${e.model}…"
                is Engine.State.Failed -> "Failed: ${e.message}"
                Engine.State.Idle -> "No model in memory"
            },
            style = MaterialTheme.typography.bodySmall,
            color = if (engine is Engine.State.Failed) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.onSurfaceVariant,
        )

        Row(verticalAlignment = Alignment.CenterVertically) {
            Text("Free the model's memory after", Modifier.weight(1f))
            Picker(
                shown = "$unloadAfter min",
                options = listOf(1, 5, 10, 30, 120).map { it to "$it min" },
            ) {
                unloadAfter = it
                settings.unloadAfterMinutes = it
            }
        }

        // Models is a screen of its own, reached from here rather than from the bar.
        Row(
            Modifier.fillMaxWidth().clickable(onClick = onModels).padding(vertical = 4.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Column(Modifier.weight(1f)) {
                Text("Models")
                Text(
                    "Download, choose and delete the models that transcribe.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            OutlinedButton(onClick = onModels) { Text("Open") }
        }

        val speakers by Speakers.ready.collectAsState()
        if (speakers) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text("Speaker models, ${size(Speakers.size)}", Modifier.weight(1f))
                OutlinedButton(onClick = { Speakers.delete() }) { Text("Delete") }
            }
        }

        Section("Text")
        Row(verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f)) {
                Text("Transcribe while speaking")
                Text(
                    "Each part is transcribed at a pause of a second, so the text is ready sooner after the stop, " +
                        "and the keyboard types it as you go. A pause may end a sentence.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            Switch(checked = live, onCheckedChange = {
                live = it
                settings.live = it
            })
        }
        Row(verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f)) {
                Text("Pause that ends a part")
                Text(
                    "How much quiet sends a part to the engine. Shorter shows the text sooner; " +
                        "longer keeps a breath from ending a sentence.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            Picker(
                shown = if (pauseMs < 1000) "$pauseMs ms" else "%.1f s".format(pauseMs / 1000.0),
                options = listOf(200, 300, 500, 700, 1000, 1500, 2000).map {
                    it to if (it < 1000) "$it ms" else "%.1f s".format(it / 1000.0)
                },
            ) {
                pauseMs = it
                settings.pauseMs = it
            }
        }
        val langs = models.firstOrNull { it.name == settings.model }?.langs.orEmpty().sorted()
        Row(verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f)) {
                Text("Language of the speech")
                Text(
                    "Whisper transcribes in it instead of guessing; the cleanup follows it.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            Picker(
                shown = language ?: "Auto",
                options = listOf<Pair<String?, String>>(null to "Auto") + langs.map { it to it },
            ) {
                language = it
                settings.language = it
            }
        }
        Row(verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f)) {
                Text("Clean up the text")
                Text(
                    "Takes out “um” and “uh”, and writes Russian numbers in digits.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            Switch(checked = clean, onCheckedChange = {
                clean = it
                settings.clean = it
            })
        }

        Section("Recording")
        Row(verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f)) {
                Text("Record in the background")
                Text(
                    "Leaving the app keeps the dictation going, under a notification with the time " +
                        "and pause and stop. Off, leaving ends it. The keyboard is not affected.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            Switch(checked = background, onCheckedChange = {
                background = it
                settings.background = it
                if (it) askNotifications.launch(Manifest.permission.POST_NOTIFICATIONS)
            })
        }
        Row(verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f)) {
                Text("Keep recordings")
                Text(
                    "Each dictation's audio is kept beside its text in History, to play or share. Nothing " +
                        "leaves the phone, and deleting a dictation deletes its recording with it.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            Switch(checked = keep, onCheckedChange = {
                keep = it
                settings.keepRecordings = it
            })
        }
        Row(verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f)) {
                Text("Recording animation")
                Text(
                    "The pill of colour and bars while you speak, as on the Mac. Off, the microphone " +
                        "button's pulse is all and the engine goes about a sixth faster — 6× against " +
                        "5× real time with gigaam-v3 on a Galaxy S20 Ultra.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            Switch(checked = animate, onCheckedChange = {
                animate = it
                settings.animate = it
            })
        }

        Spacer(Modifier.height(16.dp))
        Text(
            "Wavo for Android runs the wavo engine by vladkens on this phone. Only a model's download uses the network.",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

@Composable
private fun Section(title: String) {
    Spacer(Modifier.height(8.dp))
    Text(title, style = MaterialTheme.typography.titleMedium, color = MaterialTheme.colorScheme.primary)
    HorizontalDivider()
}

@Composable
private fun Choice(title: String, about: String, selected: Boolean, choose: () -> Unit) {
    Row(Modifier.fillMaxWidth().clickable(onClick = choose), verticalAlignment = Alignment.CenterVertically) {
        RadioButton(selected = selected, onClick = choose)
        Column {
            Text(title)
            Text(about, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
    }
}

@Composable
private fun <T> Picker(shown: String, options: List<Pair<T, String>>, choose: (T) -> Unit) {
    var open by remember { mutableStateOf(false) }
    Box {
        OutlinedButton(onClick = { open = true }) { Text(shown) }
        DropdownMenu(expanded = open, onDismissRequest = { open = false }) {
            options.forEach { (value, label) ->
                DropdownMenuItem(text = { Text(label) }, onClick = {
                    open = false
                    choose(value)
                })
            }
        }
    }
}
