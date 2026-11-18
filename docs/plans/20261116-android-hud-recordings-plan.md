# Android HUD Pill and Recordings Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bring the macOS recording pill to the Android app's Dictate screen and voice keyboard, and keep each dictation's recording linked to its history text.

**Architecture:** The desktop's gradient generator moves into a shared `src/gradient.rs` that both `src/hud.rs` and the Android Rust core include, so the pixels are identical by construction. One JNI call returns RGBA bytes; two draw paths (Compose `Canvas`, and a custom `View` for the IME) render the same geometry over them. Recordings are WAV files written before their history row, linked by a new `audio` column.

**Tech Stack:** Rust (no nimbo in the core), JNI 0.21, Kotlin, Jetpack Compose (Dictate) and classic Views (IME), SQLite, `MediaPlayer`, `FileProvider`.

**Spec:** `docs/plans/20261116-android-hud-recordings.md`

## Global Constraints

- Every `.rs` file starts with `// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app`.
- No generated or binary files in git. Recordings and WAV fixtures are made into `target/` or the app's own folder, never committed.
- `make check` (nightly fmt, clippy `-D warnings`) and `make test` pass; the desktop HUD's appearance must not change.
- Android builds with `--release --locked`; Gradle 9.9.0, AGP 9.5.1, Kotlin 2.4.21, compileSdk 37, targetSdk 36, minSdk 29, NDK 26.1.10909125, arm64-v8a.
- The core is a separate workspace and must not gain a nimbo or cpal dependency.
- No implicit settings: every behaviour follows a setting the person sees, and the UI shows the stored value.
- A commit is one short lowercase line, no body.
- Tests never touch the person's data; on-device runs use the connected phone, and the desktop's mic rules are unchanged.

## Review Focus

- A stored recording name containing `../` or an absolute path: reading, sharing or deleting it must stay inside the recordings folder.
- A device upgrading with a full v1 history: every existing row survives the migration with no audio, and the app opens.
- Silence or a zero level: `log10(0)` must not produce NaN or a crash; bars sit at their minimum.
- Animation switched off: no gradient JNI call happens at all, and dictation still works.
- A crash between the WAV write and the history insert: the orphan file is reconciled at the next start, not left forever.
- A 600-second recording (the recorder's limit): writing ~19 MB of WAV must not block the main thread or exhaust memory.

---

### Task 1: The shared gradient module

**Files:**
- Create: `src/gradient.rs`
- Modify: `src/hud.rs` (remove the moved items, import from `gradient`)
- Test: `src/gradient.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Produces: `pub struct Size { pub w: f32, pub h: f32 }`; `pub fn field(size: Size, t: Duration, scale: f32) -> (usize, usize, Vec<u8>)`; `pub fn edge(size: Size, scale: f32) -> (usize, usize, Vec<u8>)`; `pub const WIDTH: f32 = 176.0`; `pub const HEIGHT: f32 = 36.0`.

- [ ] **Step 1: Write the failing tests**

In `src/gradient.rs` tests: `field(Size{w:176.0,h:36.0}, Duration::ZERO, 1.0)` returns `(176, 36, bytes)` with `bytes.len() == 176*36*4`; the same inputs give byte-identical output twice; `Duration::from_secs(1)` gives different bytes; every alpha inside the pill is 255. `srgb` of each `BRAND` stop stays within 0..255.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --release gradient`
Expected: FAIL — the module does not exist yet.

- [ ] **Step 3: Move the code**

Move from `src/hud.rs` into `src/gradient.rs`: `field`, `edge`, `pixels`, `inside`, `corner`, `lerp`, `brand`, `srgb`, the `SHADES` static, and the constants they close over (`WIDTH`, `HEIGHT`, `BRAND`, `HUES_PERIOD`, `HUES_APART`, `CLOUD`, `SPREAD`, `WANDER`, `CLOUDS`, `DEEPEN`, `RIM`, `GLOW`, `OPEN_RADIUS`, `LINE`, `TEXT_EDGE`, and `use std::f32::consts::TAU`). Replace nimbo's `Size` with the local struct; `hud.rs` converts at its call sites. Add `mod gradient;` to `src/main.rs` and make the items `pub` as the interface above requires. Behaviour is unchanged.

- [ ] **Step 4: Run the tests and the desktop checks**

Run: `cargo test --release gradient` then `make check` and `make test`
Expected: PASS, including the existing HUD harness tests.

- [ ] **Step 5: Commit**

```bash
git add src/gradient.rs src/hud.rs src/main.rs
git commit -m "hud: the gradient in a module of its own"
```

---

### Task 2: The gradient behind JNI

**Files:**
- Modify: `android/core/src/lib.rs`, `android/core/src/jni.rs`
- Modify: `android/app/src/main/java/io/github/tyetladd/wavo/Native.kt`
- Test: `android/core/src/jni.rs` or a new `android/core/src/gradient_test.rs`

**Interfaces:**
- Consumes: Task 1's `gradient::field` / `gradient::edge` / `gradient::Size`.
- Produces: `Native.gradient(w: Float, h: Float, scale: Float, ms: Long, edge: Boolean): ByteArray` — RGBA, `w*scale` by `h*scale` pixels, `ms` the flow time in milliseconds.

- [ ] **Step 1: Write the failing test**

A core test calling the shared `gradient::field` at 176×36, scale 1, `Duration::ZERO`: assert the byte length and that two calls agree. (The JNI wrapper itself is exercised on device in Task 5.)

- [ ] **Step 2: Run to verify failure**

Run: `cd android/core && cargo test --release gradient`
Expected: FAIL — the core has no `gradient` module.

- [ ] **Step 3: Implement**

In `lib.rs`, beside the existing `cleanup`/`numbers` includes: `#[allow(dead_code)] #[path = "../../../src/gradient.rs"] mod gradient;`. In `jni.rs` add `Java_io_github_tyetladd_wavo_Native_gradient` following the existing functions' error handling, returning a `jbyteArray` from `field` or `edge`. In `Native.kt` add the `@JvmStatic external fun` with the signature above. **No new ProGuard rule is needed** — `proguard-rules.pro` already keeps all of `Native`.

- [ ] **Step 4: Run the tests and build**

Run: `cd android/core && cargo test --release` then the Gradle build with the cached 9.9.0 distribution and `JAVA_HOME` set to JBR 17.
Expected: PASS; `:app:assembleDebug` succeeds.

- [ ] **Step 5: Commit**

```bash
git add android/core/src/lib.rs android/core/src/jni.rs android/app/src/main/java/io/github/tyetladd/wavo/Native.kt
git commit -m "android: the hud gradient behind jni"
```

---

### Task 3: Thirty levels a second, and the bar height

**Files:**
- Modify: `android/app/src/main/java/io/github/tyetladd/wavo/Recorder.kt:61-80`, `Dictation.kt` (`Ui`, `heard`)
- Create: `android/app/src/main/java/io/github/tyetladd/wavo/Pill.kt` (geometry and the bar mapping only, for now)
- Test: `android/app/src/test/java/io/github/tyetladd/wavo/PillTest.kt`

**Interfaces:**
- Produces: `Dictation.Ui.levels: List<Float>` — sixteen entries, oldest first, newest last. `Pill.barHeight(level: Float): Float`. `Pill` constants: `WIDTH=176f, HEIGHT=36f, PAD=14f, DOT=10f, RING=1.2f, BARS=16, BAR_WIDTH=3f, BAR_STEP=5f, BAR_MIN=3f, BAR_MAX=20f, WAVE_LOW=3f, WAVE_HIGH=9f, CHIP_W=24f, CHIP_H=17f`.

- [ ] **Step 1: Write the failing tests**

`PillTest`: `barHeight(0f) == 3f` and is finite (the silence guard); `barHeight(1f) == 20f`; `barHeight(0.001f)` ≈ 3f (−60 dB); a level of 0.25 (−12 dB) gives 20f; the mapping is monotonic across 0.0001…1.0. A ring test: feeding seventeen levels keeps sixteen, oldest dropped.

- [ ] **Step 2: Run to verify failure**

Run: `gradle :app:testDebugUnitTest --tests '*PillTest*'`
Expected: FAIL — `Pill` does not exist.

- [ ] **Step 3: Implement**

`Recorder`: keep the 100 ms chunk and its `Pieces` cadence, but split each chunk into thirds and call `level` once per third with that third's **RMS** (`sqrt(sum(s*s)/n)`), not its peak. The desktop calibrates `bar_height` for a linear RMS over `SAMPLE_RATE/30` ≈ 33 ms, which is exactly a third of this chunk; peaks would pin the bars several dB too high. `Dictation.heard`: push each level into a sixteen-entry ring in `Ui`. `Pill.barHeight`: `20*log10(max(level, 1e-6))`, then `lerp(BAR_MIN, BAR_MAX, ((db + 60) / 48).clamp(0, 1))`.

- [ ] **Step 4: Run the tests**

Run: `gradle :app:testDebugUnitTest`
Expected: PASS, including the existing `PiecesTest` (the cadence must not have changed).

- [ ] **Step 5: Commit**

```bash
git add android/app/src/main/java/io/github/tyetladd/wavo/Recorder.kt android/app/src/main/java/io/github/tyetladd/wavo/Dictation.kt android/app/src/main/java/io/github/tyetladd/wavo/Pill.kt android/app/src/test/java/io/github/tyetladd/wavo/PillTest.kt
git commit -m "android: the hud's levels and bar height"
```

---

### Task 4: The two settings

**Files:**
- Modify: `android/app/src/main/java/io/github/tyetladd/wavo/App.kt` (`Settings`), `MainActivity.kt` (`SettingsScreen`)
- Test: `android/app/src/test/java/io/github/tyetladd/wavo/LogicTest.kt`

**Interfaces:**
- Produces: `Settings.keepRecordings: Boolean` (default `true`), `Settings.animate: Boolean` (default `true`).

- [ ] **Step 1: Write the failing test**

Assert both defaults are `true` on a fresh `Settings`, and that each round-trips a written `false`.

- [ ] **Step 2: Run to verify failure**

Run: `gradle :app:testDebugUnitTest --tests '*LogicTest*'`
Expected: FAIL — the properties do not exist.

- [ ] **Step 3: Implement**

Add both properties to `Settings` in the existing style, with a doc comment each. Add two switch rows to `SettingsScreen` following the existing `live`/`clean` rows: "Keep recordings" and "Recording animation". The animation row's help text states what it costs — fill in the measured figure from Task 9.

- [ ] **Step 4: Run the tests**

Run: `gradle :app:testDebugUnitTest`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add android/app/src/main/java/io/github/tyetladd/wavo/App.kt android/app/src/main/java/io/github/tyetladd/wavo/MainActivity.kt android/app/src/test/java/io/github/tyetladd/wavo/LogicTest.kt
git commit -m "android: keep recordings and recording animation settings"
```

---

### Task 5: The pill in the Dictate screen

**Files:**
- Modify: `android/app/src/main/java/io/github/tyetladd/wavo/Pill.kt` (the shared draw), `MainActivity.kt` (`DictateScreen`)
- Test: `android/app/src/test/java/io/github/tyetladd/wavo/PillTest.kt`

**Interfaces:**
- Consumes: `Native.gradient`, `Pill.barHeight`, `Dictation.Ui.levels`, `Settings.animate`.
- Produces: `Pill.draw(canvas: android.graphics.Canvas, levels: List<Float>, elapsedMs: Long, phase: Dictation.Phase, language: String?, gradient: Bitmap?, edge: Bitmap?, settle: Float)` — the one draw both surfaces use, so the IME task does not re-derive the geometry. `Pill.animates(phase: Dictation.Phase, animate: Boolean): Boolean` — true only while recording or transcribing **and** the setting is on; both surfaces gate their tick and their `Native.gradient` call on it.

- [ ] **Step 1: Write the failing test**

Test the wave the transcribing phase draws: at `settle = 1f` every bar is `WAVE_LOW`; the wave stays within `WAVE_LOW..WAVE_HIGH`; the stagger means bar 0 and bar 1 differ at a time where one is at a peak. Test that the chip is omitted when `language` is null.

- [ ] **Step 2: Run to verify failure**

Run: `gradle :app:testDebugUnitTest --tests '*PillTest*'`
Expected: FAIL — `Pill.draw` and the wave do not exist.

- [ ] **Step 3: Implement**

`Pill.draw` paints the gradient bitmap, the rim bitmap, the red dot with its white ring, the sixteen bars, the mono clock and the language chip, at the geometry constants. Recording: bars from `barHeight`. Transcribing: settle to `WAVE_LOW` over 180 ms, then the wave (half period 600 ms, stagger 75 ms) at 0.6 alpha, dot faded, clock at 0.76 alpha, `n/total` when there is more than one piece. In `DictateScreen`, a Compose `Canvas` calls `Pill.draw` via `drawIntoCanvas`; a `LaunchedEffect` ticks at 33 ms while recording **only when `Settings.animate` is true**, and only then calls `Native.gradient`. With the setting off, no pill is drawn and the existing button pulse remains.

- [ ] **Step 4: Run the tests and build**

Run: `gradle :app:testDebugUnitTest :app:assembleDebug`
Expected: PASS.

- [ ] **Step 5: Verify on the phone, and measure**

Install on `R3CN309GX0F` and record: the pill matches the macOS HUD, the bars move with speech, the wave plays after the stop. Toggle the animation off and confirm the pill is gone and dictation still works. Measure the cost of the 33 ms gradient (CPU time of the render thread with the setting on versus off) and record the figure for Task 4's help text and Task 9.

- [ ] **Step 6: Commit**

```bash
git add android/app/src/main/java/io/github/tyetladd/wavo/Pill.kt android/app/src/main/java/io/github/tyetladd/wavo/MainActivity.kt android/app/src/test/java/io/github/tyetladd/wavo/PillTest.kt
git commit -m "android: the macos recording pill in dictate"
```

---

### Task 6: The pill in the voice keyboard

**Files:**
- Create: `android/app/src/main/java/io/github/tyetladd/wavo/PillView.kt`
- Modify: `android/app/src/main/java/io/github/tyetladd/wavo/WavoIme.kt`
- Test: `android/app/src/test/java/io/github/tyetladd/wavo/PillTest.kt`

**Interfaces:**
- Consumes: `Pill.draw` from Task 5 — the IME must not re-implement the geometry.
- Produces: `PillView(context: Context) : View`, with `fun render(ui: Dictation.Ui, language: String?)`.

- [ ] **Step 1: Write the failing test**

The project's unit tests are plain JUnit with no Robolectric, so a `View` cannot be measured on the JVM. Test the gate instead: `Pill.animates(Recording, false)` is false, `Pill.animates(Idle, true)` is false, `Pill.animates(Recording, true)` and `Pill.animates(Transcribing, true)` are true. `PillView` itself is verified on the phone in Step 5.

- [ ] **Step 2: Run to verify failure**

Run: `gradle :app:testDebugUnitTest --tests '*PillTest*'`
Expected: FAIL — `PillView` does not exist.

- [ ] **Step 3: Implement**

`PillView` is a plain `View` whose `onDraw` calls `Pill.draw` on the `android.graphics.Canvas`. It owns the 33 ms tick and the `Native.gradient` call, both gated on `Pill.animates`, and invalidates only while that is true. `WavoIme.onCreateInputView` adds it above the microphone button; `render(ui)` forwards to it. The existing pulse stays as the fallback when the setting is off.

- [ ] **Step 4: Run the tests and build**

Run: `gradle :app:testDebugUnitTest :app:assembleDebug`
Expected: PASS.

- [ ] **Step 5: Verify on the phone**

Enable the Wavo keyboard, dictate into another app: the pill shows in the keyboard, moves with speech, and disappears with the animation setting off.

- [ ] **Step 6: Commit**

```bash
git add android/app/src/main/java/io/github/tyetladd/wavo/PillView.kt android/app/src/main/java/io/github/tyetladd/wavo/WavoIme.kt android/app/src/test/java/io/github/tyetladd/wavo/PillTest.kt
git commit -m "android: the recording pill in the voice keyboard"
```

---

### Task 7: The recordings on disk

**Files:**
- Create: `android/app/src/main/java/io/github/tyetladd/wavo/Recordings.kt`
- Test: `android/app/src/test/java/io/github/tyetladd/wavo/RecordingsTest.kt`

**Interfaces:**
- Produces: `Recordings(root: File)`; `fun name(atMs: Long): String` → `YYYY-MM-DD/<unix ms>.wav` in local time, with the desktop's `-<n>` suffix when that name is taken, so two dictations in the same millisecond never overwrite one another; `fun file(name: String): File?` → null when the name could point outside `root`; `fun write(name: String, pcm: FloatArray)` → 16 kHz mono 16-bit WAV; `fun delete(name: String)`; `fun orphans(known: Set<String>): List<String>`.

- [ ] **Step 1: Write the failing tests**

`name` matches `\d{4}-\d{2}-\d{2}/\d+\.wav`. `file("../../etc/passwd")`, `file("/data/x")` and `file("2026-11-16/../../x.wav")` all return null; a valid name returns a file under `root`. `write` then read back: the RIFF header says 16000 Hz, 1 channel, 16 bits, and the samples round-trip within one quantisation step. `write` of a full 600-second array completes off the main thread. `orphans` returns a file with no entry and not one with an entry.

- [ ] **Step 2: Run to verify failure**

Run: `gradle :app:testDebugUnitTest --tests '*RecordingsTest*'`
Expected: FAIL — `Recordings` does not exist.

- [ ] **Step 3: Implement**

Write the WAV by hand (a 44-byte canonical header then little-endian 16-bit samples) so no new dependency is needed; clamp floats to [-1, 1]. `file` resolves the canonical path and requires it to stay under `root`. This task is `Recordings` alone — the `Dictation` wiring lands in Task 8, which is where `History.add` gains its second parameter.

- [ ] **Step 4: Run the tests**

Run: `gradle :app:testDebugUnitTest`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add android/app/src/main/java/io/github/tyetladd/wavo/Recordings.kt android/app/src/test/java/io/github/tyetladd/wavo/RecordingsTest.kt
git commit -m "android: a dictation's recording kept as a wav"
```

---

### Task 8: The history links the audio

**Files:**
- Modify: `android/app/src/main/java/io/github/tyetladd/wavo/History.kt`, `Dictation.kt` (`stop`/`finish`), `MainActivity.kt` (`HistoryScreen`), `AndroidManifest.xml` (FileProvider)
- Create: `android/app/src/main/res/xml/file_paths.xml`
- Test: `android/app/src/test/java/io/github/tyetladd/wavo/HistoryTest.kt`

**Interfaces:**
- Consumes: `Recordings` from Task 7.
- Produces: `Entry.audio: String?`; `History.add(heard: Heard, audio: String? = null)`; database version 2.

- [ ] **Step 1: Write the failing tests**

A v1 database with rows migrates to v2 with every row intact and `audio` null. `add` with a name stores it and `Entry.audio` reads it back. `delete(id)` removes the row **and** its file. `clear()` removes every file. A row whose file is missing still reads, with the audio treated as gone. **Nothing is pruned and no size cap is applied** — the 500 in `reload()` stays a query limit, not a deletion.

- [ ] **Step 2: Run to verify failure**

Run: `gradle :app:testDebugUnitTest --tests '*HistoryTest*'`
Expected: FAIL — the column and the file handling do not exist.

- [ ] **Step 3: Implement**

`onUpgrade` from 1 to 2 runs `ALTER TABLE history ADD COLUMN audio TEXT`. `reload` reads it, keeping its `LIMIT 500` as a display window only. `delete` and `clear` delete the files through `Recordings.delete`, which ignores a missing file; no pruning is added. In `Dictation`, when `Settings.keepRecordings` is true, write the recording **before** `history.add`, on a worker thread, and pass its name to the new second parameter; a failed write leaves the text kept with no audio. In `HistoryScreen`, a row with audio gains Play (`MediaPlayer`, released on completion and on dispose) and Share (a `FileProvider` URI, `audio/wav`); a row without one is unchanged. Register the provider in the manifest with `file_paths.xml` scoped to the recordings folder only.

- [ ] **Step 4: Run the tests and build**

Run: `gradle :app:testDebugUnitTest :app:assembleDebug`
Expected: PASS.

- [ ] **Step 5: Verify on the phone**

Dictate, then in History play the recording and share it; delete the row and confirm the file is gone; clear all and confirm the folder is empty.

- [ ] **Step 6: Commit**

```bash
git add android/app/src/main/java/io/github/tyetladd/wavo/History.kt android/app/src/main/java/io/github/tyetladd/wavo/Dictation.kt android/app/src/main/java/io/github/tyetladd/wavo/MainActivity.kt android/app/src/main/AndroidManifest.xml android/app/src/main/res/xml/file_paths.xml android/app/src/test/java/io/github/tyetladd/wavo/HistoryTest.kt
git commit -m "android: history plays and shares a dictation's recording"
```

---

### Task 9: Reconciliation, and the measured cost

**Files:**
- Modify: `android/app/src/main/java/io/github/tyetladd/wavo/App.kt` (`onCreate`), `MainActivity.kt` (the animation row's help text)
- Test: `android/app/src/test/java/io/github/tyetladd/wavo/RecordingsTest.kt`

**Interfaces:**
- Consumes: `Recordings.orphans`, `History.entries`.

- [ ] **Step 1: Write the failing test**

A recordings folder holding a file with no history row is emptied by reconciliation; a file with a row survives; a row whose file is gone keeps its text and loses its audio reference. No VAD and no textless rows: an orphan is deleted, not adopted — the Android core has no `earshot`, and the dictation that crashed produced no text to link it to.

- [ ] **Step 2: Run to verify failure**

Run: `gradle :app:testDebugUnitTest --tests '*RecordingsTest*'`
Expected: FAIL — reconciliation does not exist.

- [ ] **Step 3: Implement**

In `App.onCreate`, after `history` is built, reconcile: delete orphan files, and clear the audio reference of rows whose file is gone. Run it off the main thread. Put the figure measured in Task 5 into the animation setting's help text — claim only what was measured.

- [ ] **Step 4: Run the tests and the full checks**

Run: `gradle :app:testDebugUnitTest :app:assembleDebug`, `cd android/core && cargo test --release`, and `make check` and `make test` for the desktop.
Expected: all PASS.

- [ ] **Step 5: Commit**

```bash
git add android/app/src/main/java/io/github/tyetladd/wavo/App.kt android/app/src/main/java/io/github/tyetladd/wavo/MainActivity.kt android/app/src/test/java/io/github/tyetladd/wavo/RecordingsTest.kt
git commit -m "android: recordings reconciled at start, the animation's cost measured"
```
