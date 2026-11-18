# Wavo for Android

Local dictation on a phone: a voice keyboard that types what you say into any app, and a screen to dictate, copy and share. Speech is recognized on the phone by [wavo](https://github.com/vladkens/wavo), the engine of the desktop app, with the same models; only a model's download uses the network.

It runs on a phone. The baseline is a Pixel 7 Pro; it is also used daily on a Galaxy S20 Ultra (Android 13), where the figures below were measured.

## What is here

- `core/`: a Rust library, the engine behind JNI. It wraps wavo (the CPU backend with NEON, or Vulkan), downloads models into the app's folder (its own downloader: wavo's takes file locks, which Rust lacks on Android), and reuses the desktop app's `src/cleanup.rs`, `src/numbers.rs` and `src/gradient.rs` as they are. It keeps the engine's threads off the phone's slowest cores.
- `app/`: the Kotlin app. `Speakers` tells a conversation's speakers apart. `WavoIme` is the keyboard, `MainActivity` the screens (Dictate, History, Settings — Models is under Settings), `Engine` the one thread that owns the model, `Recordings` the WAVs, `RecordService` the recording while the app is left.

## The recording pill

While a dictation runs, a pill shows above the microphone button: the same one the desktop HUD draws. Its colours come from `src/gradient.rs` through JNI, so the phone and the Mac show one picture rather than two that drift; the bars are the loudness of the last sixteen thirtieths of a second, on the desktop's dB scale. After the stop the bars settle into a wave while the text is made.

Settings › Recording › **Recording animation** turns it off: the microphone button's pulse is then the whole visual, and the engine goes about a sixth faster — 6× against 5× real time with GigaAM v3 on the S20 Ultra.

## Pause, and recording while the app is left

The button beside stop pauses: the microphone is released, the parts complete so far are transcribed, and resuming goes on into the **same** recording and the same history entry. The paused gap is in neither the audio nor the clock.

Settings › Recording › **Record in the background**, off by default, keeps a dictation going when the app is left. A microphone foreground service is what makes that possible — without one Android hands a background app silence — and its notification carries the time and pause and stop. The keyboard is not affected: its text goes into a field, and leaving means a different field.

## History

Each dictation keeps its text, and with Settings › Recording › **Keep recordings** (on by default) its audio too, as `recordings/YYYY-MM-DD/<unix ms>.wav`, 16 kHz mono 16-bit. The file is written before its row, so a crash in between leaves a file to reconcile rather than a row pointing at nothing; a start tidies those away. Nothing is pruned: a recording stays until it is deleted, and deleting the last version of one deletes its file.

A card carries:

- an editable **name**, which leads the card and the shared text;
- the text, the model, how long it took, and the date and time;
- **Share text** — the name, the moment and the text, and nothing else, for a note app;
- **Play** and **Audio** (the WAV, shared as a file);
- **Transcribe with…** — the same recording through another model, added as a new version pointing at the same file, so two transcriptions can be compared. It honours the Conversation switch, and the picker shows each model's languages: GigaAM v3 is Russian-only, so English wants Parakeet Ultra.

## Build

Needs the Android SDK with NDK 26.1.10909125, Rust with the `aarch64-linux-android` target, [cargo-ndk](https://github.com/bbqsrc/cargo-ndk), and Gradle 9.9 (no Gradle wrapper is kept in git). AGP 9.5.1 and Kotlin 2.4.21 are pinned to match the sshmanager project, so one copy of the toolchain serves both.

```sh
rustup target add aarch64-linux-android
cargo install --locked cargo-ndk
sdkmanager --install "ndk;26.1.10909125"   # the build says so if it is missing
cd android
gradle :app:assembleDebug        # builds core/ first; the APK is in app/build/outputs/apk/debug/
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

`gradle :app:assembleRelease` builds the release: shrunk by R8 and not debuggable, in `app/build/outputs/apk/release/`. It is signed with the key named by the Gradle properties `wavo.keystore`, `wavo.keystore.password`, `wavo.key.alias` and `wavo.key.password`, and without them with the machine's debug key — installable, not for a store, and a build from another machine won't install over it.

`-Pwavo.abis=arm64-v8a,x86_64` adds the emulator's ABI (`rustup target add x86_64-linux-android` first).

## Test

```sh
cd android/core && cargo test --release       # the wrapper, the gradient and the desktop cleanup
cd android && gradle :app:testDebugUnitTest   # the Kotlin logic
```

With a model and a recording, the engine's test transcribes on the CPU as the phone does:

```sh
WAVO_TEST_MODEL=parakeet-ultra-Q8_0.gguf WAVO_TEST_WAV=jfk.wav WAVO_TEST_TEXT=country \
  cargo test --release transcribes -- --nocapture
```

The screens, the database migrations and the service are verified on a phone: they need Android behind them, and the unit tests have none.

## Use

1. Open Wavo, download a model in Settings › Models (Parakeet Ultra for 25 European languages, GigaAM v3 for Russian, Moonshine for the smallest) and allow the microphone at the first dictation.
2. Settings › Keyboard › Turn on, and enable "Wavo voice keyboard".
3. In any app, switch to the Wavo keyboard, then tap the microphone and tap again to stop, or hold it and speak. ⌨ goes back to the previous keyboard.

Settings › Text › **Pause that ends a part** sets how much quiet sends a part to the engine, half a second by default. Shorter shows the text sooner; longer keeps a breath or a search for a word from ending a sentence, which puts full stops inside one.

## Conversations

The switch Settings › Conversation makes the Dictate screen label who said what, a turn a line with the time it started: `0:02 Speaker 2: …`. The wavo engine has no diarization, so this part is [sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx) with pyannote's segmentation model and a 3D-Speaker embedding model (34 MB, downloaded once from the switch). The text shows while you speak, transcribed at your pauses as in a dictation; after the stop sherpa-onnx tells the speakers apart on the CPU, over the whole recording, and each word goes to the speaker it starts with. The keyboard has no such mode.

The sherpa-onnx AAR isn't in git: the `sherpaAar` Gradle task downloads it from GitHub's releases into `app/libs/` and checks its SHA-256.

## Not yet

- The desktop's live preview, where words show before a pause. Here a dictation is cut at its pauses and each piece is transcribed while you speak on.
- A system speech recognizer service, for other keyboards' microphone buttons.
- A download that goes on with the app closed: it stops with the app and resumes at the next tap on Download.
- A logo and a look of its own for the Dictate screen, which has no title for now.
- The GPU (Vulkan) is the default, measured on a Pixel 7 Pro (Mali-G710): 7× real time with GigaAM v3 against 4× on the CPU. It does **not** work on the S20 Ultra's Adreno 650, where the engine falls back to the CPU as it is meant to; the CPU there runs 4–8× depending on the model and the load.
