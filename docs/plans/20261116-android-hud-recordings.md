# Android: the macOS recording pill, and recordings kept with their text

## Intent

The Android app shows almost nothing while it records: the microphone button pulses with the
loudness (`MainActivity.kt:276`, `WavoIme.kt:255`). The desktop shows a gradient pill — a red dot,
sixteen bars of recent loudness, the clock, a language chip — and keeps each dictation's recording
beside its text. This brings both to Android, so the phone looks and behaves like the Mac.

The person asked for "the same visual as macOS". That is a level meter, **not a spectrogram**: no
FFT is needed, because `Recorder` already produces the loudness the bars are drawn from.

## Goals

1. The macOS pill during recording, in the Dictate screen and in the voice keyboard, including the
   transcribing wave that replaces the bars after the stop.
2. The gradient pixel-identical to macOS, from one shared implementation.
3. A "Keep recordings" setting, **on by default**.
4. Each kept dictation's recording saved as a WAV and linked to its history text: play and share
   from History, and delete/clear removing the files with the rows.
5. A switch in Settings that turns the recording animation off, for CPU and battery.

## Non-goals

- A floating pill over other apps. That needs "Display over other apps"; the keyboard covers the
  moment the macOS HUD covers, without a new permission.
- A spectrogram, or any frequency-domain display.
- Transcribing audio files (still under the readme's "Not yet").
- The desktop's full `Retention` enum (count, 3 days, 2 weeks, 3 months, until quit). Android gets
  one switch.
- Any change to how the desktop HUD looks.

## Anti-goals

- Jank, or slower transcription. The engine already runs on four NEON threads while the person
  speaks; the visual must not compete with it.
- A second copy of the colour maths that drifts from macOS.
- Orphaned recordings left behind after a delete, a clear, or a crash. Keeping every recording the
  person made is intended; keeping files no row points to is not.
- Behaviour the person cannot see and change in Settings.

## Constraints

- **The keyboard is not Compose.** `WavoIme.onCreateInputView()` returns a `View` built from
  `ImageButton` and `LinearLayout`. The pill therefore needs two draw paths — a Compose `Canvas` in
  `DictateScreen`, a custom `View` with `onDraw(android.graphics.Canvas)` in the IME — over one
  shared source of pixels and geometry.
- **The recorder's cadence is load-bearing.** `Recorder` reads 100 ms chunks and `Dictation.heard`
  feeds `Pieces` once per chunk. Shrinking the chunks to get 30 Hz bars would change pause
  detection. Derive three peaks per chunk instead.
- **`history.db` is version 1 with a no-op `onUpgrade`.** Adding a column needs a real migration.
- **History shows the newest 500 rows but never deletes any.** `reload()` only limits its query;
  there is no pruning today. **No bound is added**: every recording is kept, by the person's
  choice, accepting that a long dictation is about 19 MB and storage grows without limit.
- **The core builds without nimbo and cpal**, in its own workspace, and already shares desktop
  source through `#[path = "../../../src/cleanup.rs"]`. `field()`'s only nimbo dependency is `Size`
  (a width and a height); the extraction must not drag nimbo into the core.
- The release build is R8-shrunk and not debuggable: a new JNI entry point needs a keep rule in
  `app/proguard-rules.pro`.
- minSdk 29, targetSdk 36, arm64-v8a, 16 KB page alignment already set in `core/.cargo/config.toml`.
- No generated or binary files in git; every `.rs` starts with the copyright header; `make check`
  and `make test` pass.

## Approaches

**A — share the macOS generator (chosen).** Extract the gradient out of `src/hud.rs` into
`src/gradient.rs`; the HUD uses it, and the Android core includes it with `#[path]` as it already
does for `cleanup.rs` and `numbers.rs`. One JNI call returns RGBA bytes for a size and a time.
Identical by construction, one source of truth, and the project's established pattern. Because
`field` is only about six thousand pixels, it can run at the desktop's full 33 ms cadence in native
code without troubling the transcription threads — no reduced-resolution compromise is needed.
Costs: a mechanical extraction touching the desktop HUD (covered by its nimbo-harness tests), one
JNI entry point, and the desktop checks must still pass.

**B — port the generator to Kotlin.** Roughly a hundred lines of OKLCH to sRGB with gamut mapping
by bisection, a Catmull-Rom hue spline, three Gaussian clouds and dither. No desktop change and no
JNI, but a second implementation of subtle colour maths that will drift from macOS — the gamut
bisection and the short-way hue interpolation are easy to get quietly wrong. Rejected: drift is
exactly what "the same visual" rules out.

**C — approximate with Compose `Brush`.** Animated radial gradients in the brand colours. Cheapest
and GPU-composited, but visibly different: no dither, no cloud wander, different colour mixing.
Rejected: it does not meet the request.

## Decided by the person

- The same visual as macOS.
- The pill in both the Dictate screen and the voice keyboard; no floating overlay.
- Approach A.
- Recordings kept **on by default**, as on the desktop.
- The animation is **optional**, with a switch in Settings, to save CPU load.
- **No storage bound**: every recording is kept, with no pruning and no size cap.

## The design

### The shared gradient

`src/gradient.rs` takes `field`, `edge`, `pixels`, `brand`, `srgb`, the `SHADES` table and their
constants (`BRAND`, `HUES_PERIOD`, `HUES_APART`, `CLOUD`, `SPREAD`, `WANDER`, `CLOUDS`, `DEEPEN`,
`RIM`, `GLOW`). nimbo's `Size` becomes a plain width/height struct local to the module. `hud.rs`
calls it exactly as before; its appearance does not change.

The core adds `#[path = "../../../src/gradient.rs"] mod gradient;` and one JNI entry point returning
RGBA bytes for a width, height, scale and time, with a flag selecting the **field or the edge**. The
pill's white rim and top glow are `edge()`, a second RGBA layer with its own alpha — the field's
alpha is always 255 — so both layers come from native code and neither is re-drawn in Kotlin. No new
ProGuard rule is needed: `proguard-rules.pro` already keeps all of `Native`.

The field is about six thousand pixels at scale 1, times the square of the display scale — roughly
46,000 on a 2.625-density phone. Still small beside transcription, but the cost is measured, not
assumed.

### The bars

`Recorder` splits each 100 ms chunk into thirds and reports the **RMS** of each third, keeping the
chunk itself and its cadence intact. RMS, not peak: the desktop's `bar_height` is calibrated for a
linear RMS (`src/audio.rs` `LEVEL_WINDOW = SAMPLE_RATE / 30`, about 33 ms — exactly a third of the
Android chunk), and peaks sit several dB above RMS, so feeding peaks would pin the bars visibly
higher than macOS for the same voice. `Dictation.Ui` carries a sixteen-entry ring, oldest first. A
bar's height is the desktop's: `20·log10(max(level, 1e-6))`, mapped from −60…−12 dB onto 3…20 px.

### The pill

One set of geometry, shared by both draw paths: 176×36, padding 14, dot 10 with a 1.2 white ring,
sixteen bars 3 wide on a 5 step, the clock in mono, the language chip 24×17 in white with violet
text. The gradient bitmap comes from JNI every 33 ms while recording; the dot, bars, clock and chip
are drawn over it.

After the stop the bars settle to 3 px over 180 ms and become a wave (half period 600 ms, stagger
75 ms, between 3 and 9 px) at 60% opacity; the dot fades, the clock dims to 76%, and a dictation of
more than one piece shows `n/total` instead of the clock.

### The recordings

`filesDir/recordings/YYYY-MM-DD/<unix ms>.wav`, 16 kHz mono 16-bit, the whole recording, named for
its start as on the desktop. The file is written before the history row, so a crash in between
leaves a file to reconcile rather than a row pointing at nothing. A path built from a stored name
cannot point outside the recordings folder.

`Settings.keepRecordings`, default true, with a switch in Settings. Turning it off stops new
recordings being kept; what is already kept stays until deleted — nothing is removed implicitly.

### The animation switch

`Settings.animate`, default true, with a switch in Settings beside the recording one. Off means the
pill is not drawn at all and neither surface asks the core for a gradient: the microphone button's
existing pulse is the whole visual, as today. That removes the per-frame JNI call, the bitmap upload
and the 30 fps redraw, which is where any cost would be.

The cost is to be **measured, not assumed**: `field` is about six thousand pixels of native maths per
frame, so it may be negligible beside four NEON threads of transcription. The switch is worth having
either way on a phone that is already warm, but the measurement decides what the setting's help text
claims.

### The history

`history.db` migrates to version 2, adding `audio TEXT`. `Entry` gains the field, and `History.add`
takes the recording's name beside the `Heard`. Delete and clear remove the files with the rows, and
pruning past 500 entries removes the files of the rows it drops. A row with a recording gains play
and share in History; a row without one looks as it does now.

On start, recordings with no entry are deleted, and entries whose file is gone keep their text and
lose their audio reference. This is deliberately simpler than the desktop's `recover()`, which
adopts an orphan recording as a textless entry when `earshot`'s VAD finds speech in it: the Android
core has no VAD, adding one is adjacent to the "transcribing audio files" non-goal, and Android's
History has no textless row. The cost of the simpler rule is that a recording left by a crash
between the write and the insert is discarded — but that dictation produced no text either, so
there is nothing to link it to.

A row's `at` keeps its present meaning, the moment the dictation completed, while the file is named
for the recording's start. The two differ by the recording's length; the link is the `audio` column,
never a computed time, so nothing has to reconcile them. Existing rows are untouched.

A recording's name carries the desktop's `-<n>` suffix when two start in the same millisecond, so
one never overwrites another.

## Left to the builder

- How the pill sits in each screen: above the microphone button in Dictate, and in the IME's view
  hierarchy, with the existing pulse either kept or replaced.
- Whether the language chip shows, and from what — the desktop shows it when the language follows
  the keyboard layout; Android has a language setting and a model-detected language.
- `FileProvider` setup for sharing, and `MediaPlayer` lifecycle for play.
- The thread and handler arrangement for the 33 ms tick in each surface.
- Where the tests go: the shared gradient's pixel maths, the bar mapping, the migration, and the
  file-with-row lifecycle.
