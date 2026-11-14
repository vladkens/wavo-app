# Live preview

Done: 2026-11-14

Goal: the HUD shows the words while the person speaks, with the models Wavo has (none of them streams), so a wrong word or a wrong language is seen before the stop. The text pasted stays the whole recording's, transcribed after the stop as today.

Design: the mockup at https://claude.ai/code/artifact/6b1662e7-4f92-4adf-bc03-4a64ab99cf75 (version 2: one pill, the row still, the text growing away from the screen's edge), after the overlays of Handy 0.9 ("Live"), VoiceInk, Voz and FluidVoice.

## Decisions

- A spike on an M5 with the fixtures: GigaAM v3 on the Neural Engine decodes up to 15 s in 39 ms (median, 59 ms at most), Parakeet v3 on the GPU in 60 ms, Whisper large v3 turbo in ~0.5 s (1.6 s at most). Re-decoding the recording every half second is cheap with the first two, slower but usable with Whisper.
- Every 500 ms ([live::STEP]) the engine transcribes the recording since the last fixed piece: one job at a time, so a slower model updates less often. At a pause (300 ms below the VAD's pause score, at least 2 s in, speech after it) the piece before it is transcribed once more and fixed; past 10 s without one it is cut in the longest gap, so the audio decoded again stays short and the words shown stay put.
- A word shown stays once two jobs in a row agree on it (LocalAgreement-2, as whisper_streaming), but for the last one, which may be half said or carry the model's period; it keeps the case and punctuation it showed with. The words that stay are found again in each job's text by the fewest edits, as a job may join, split, drop or add a word. At first the whole audio since the last fixed piece was the tail: in the person's real dictations (continuous speech, no pause to fix a piece for 10 s) that was 12 dim words on average, punctuation and words far back rewritten at every step, which read as flicker. With the agreement: 1.6 dim words with GigaAM, 2 with Parakeet, and the words that stay never change.
- The engine's queue: dictations first, previews next, background jobs (Transcribe Again, untried entries) last; a newer preview replaces a queued one. A preview changes no progress or phase.
- The cleanup (`clean_text`) applies to the preview as to the text.
- The setting "Show the text while speaking" in Advanced → HUD, off by default (`live_preview`).
- HUD: one shape. The row (dot, level, time, language) stays where it is; with the first words the pill opens to 400 points and grows a line (18 points) at a time, up from the bottom of the screen, down from the top, up to three lines, then the text scrolls and the line leaving fades. Corners from round ends to 15 points. Moves are a critically damped spring's (360 ms; a scroll 300 ms). New words fade in rising 3 points, one after another (70 ms apart, a step's all within 420 ms), so a step's words read as speech; a rewritten word is swapped in place at once (a fade at every step read as blinking); the tail the next step may change is dimmer (65 %, still 3:1 on the gradient), and a word leaving it brightens over 240 ms. After the stop the pill stays open while transcribing and fades out as it is, never squeezing the text.
- The overlay window has room for the open pill only with the setting on, and never resizes while it shows (dynamic resizing gave Handy's #864 artifacts); the setting applies at the next dictation, as the position does.
- Not in this work: a separate dictaphone screen, file transcription (next), the wavo update with the tracks model.
- No commits: the person reviews and commits. Work stays uncommitted in this worktree on `feat/live-preview`.

## Tasks

### 1. Engine and recorder

Files: `src/engine.rs`, `src/audio.rs`.

- [x] `Engine::preview`: jobs between dictations and the background, the newest replacing a queued one, cancelled.
- [x] The recording so far in whole VAD frames (`Meter::since`), `Clip::piece_end` and `Clip::split_at`.
- [x] Tests: the queue's order and the replaced preview; where a piece ends.

### 2. Preview and session

Files: new `src/live.rs`, `src/session.rs`, `src/settings.rs`.

- [x] `Preview`: the fixed text and the tail, the next job's audio, a fixed piece keeping the tail's words after it, silence fixed without a job.
- [x] The session asks every step while recording, cancels at the stop, starts over at a switch of layout; `Status::preview`.
- [x] The words two jobs agree on stay (`aligned`, `key`).
- [x] Tests: the preview's steps; agreement and alignment; on GigaAM with `ru-long` (in `make test-models`) the words that stay never change and ≤ 2.5 change on average; a session with the setting shows the engine's text while recording and pastes the whole recording's, without it none.

### 3. HUD and settings

Files: `src/hud.rs`, `src/app.rs`, `src/window.rs`.

- [x] `Show::Recording`'s text; the open pill in both styles and positions, the words' layout and animations; the window's room with the setting.
- [x] The setting's toggle with its ⓘ.
- [x] Harness tests: opening away from the edge and the row in place, three lines then the scroll, fading out open; words one by one, a rewrite in place, a fixed piece; the shots `hud-live-*`; the tail's contrast.

## Checks

Each task: `make check` and `make test`. Never open the app or a window, never record from the microphone, a temp `WAVO_HOME` in every run.

Follow-up for the person, not a task: `make -C /Users/user/Code/wavo-app/.worktrees/live-preview run`, turn on Advanced → HUD → "Show the text while speaking", and dictate a long paragraph with GigaAM and with Whisper, the HUD at the bottom and at the top.

## Result

`make check` and `make test` pass (219 tests); `make test-models`' live test: 1.5 changing words on average on `ru-long`, the engine busy 4.4 % of the recording (GigaAM, ~20 ms a step); in the person's run macmon showed 0.2–0.25 W on the Neural Engine, so the step stays at 500 ms. The HUD's `hud-live-*` shots in `target/shots/`.
