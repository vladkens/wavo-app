# Clean text

Done: 2026-11-08, PR #61

Goal: a dictation pastes cleaner text: hesitation sounds (`э-э`, `um`) go in Russian and English, every paste ends with one space so parts don't glue, and a part pasted into the same window within 20 s after one that left its sentence open starts in lower case. It is one step between recognition and paste, behind one switch, on by default. No stitching of audio, no numbers, no new model and no LLM.

The person dictates a part, stops, types a term by hand, and goes on. Today each recording is recognized alone, so a part gets a period and the next one a capital. Measured on FLEURS with one mid-sentence cut per clip (`gigaam-v3` ru, `parakeet-ultra` ru, `parakeet-ultra` en): false period in 73 / 83 / 60%, false capital in 90 / 60 / 28%. Dropping the period and lowercasing also kills every real sentence end, so it is rejected. Recognizing the new part together with the audio before it makes the seam equal to one pass over the whole clip in 98 / 95 / 93%, with false periods 8 / 3 / 4%; with 4 s of context in 89 / 91 / 84%. Whisper and Moonshine weren't measured.

## Decisions

- One setting, `clean_text` ("Clean up the text"), on by default: the cleanup is safe and can be turned off. Shown in the settings window, plain words.
- A step that needs a language runs only when the language is known: the model's only language (`ModelInfo::langs` of one entry), else `Settings::engine_language()`: a stale setting doesn't beat a model of one language, which isn't told it. Parakeet v3 is never told the language, so a setting of `en` with German speech loses `er` and `um`; that is accepted: the person chose the language.
- `Last.text` and the history keep the cleaned text with all its marks; copying from the history or the menu gives that. Only the paste differs: `deliver` pastes a seam prefix and the text without its trailing mark. Transcribing an entry again stays as it is.
- The period at a seam is not pasted. `Focused` knows the app and window, not the field: after Enter in a chat, the same target gets a new message, and `. Next` would open it with a stray period. So a seam gives a space, plus a comma or colon when the pass put one there, unless the filler it stood before opened the part and went, and the new part's case as in the pass; a real sentence end between parts has a capital and no period. Reading the text before the caret through Accessibility could bring it back later: that is a nimbo change, out of scope.
- No commits: the person reviews and commits. Work stays uncommitted in this worktree, on `feat/clean-text`.

## Tasks

### 1. Setting and stitching

Files: `src/settings.rs`, `src/window.rs`, `src/session.rs`, `src/engine.rs`, `src/testing.rs`, new `src/stitch.rs`, `Makefile` (`MODEL_TESTS`).

- [x] `Settings::clean_text: bool`, default false, with a switch in the settings window beside the other text switches, and its test in the window's harness.
- [x] `stitch.rs`, pure and unit-tested: finds where the previous part's words end in the text of the pass over context + new audio, comparing words without case and punctuation, and returns the new words, the mark after the previous part's last word and the new first word's case as in the pass. Match: the last `min(3, n)` previous words in a row, ending where the context ends: within the previous word count when the context holds all of it, else within 10% of the pass from the expected share. Anything less is `None`, and so is an empty rest. No timestamps: Moonshine has none and text alignment was more exact than token times. Marks include `。？！؟`.
- [x] The session keeps a chain after a pasted dictation: the previous parts' audio up to `CONTEXT` (10 s), their raw model text, the target, the model and language, and when it was pasted. Its clock is injected for tests. Deleting any entry of its parts ends the chain.
- [x] A new recording continues the chain only when `clean_text` and auto-paste are on, the target is known, the previous dictation was pasted, the target hasn't moved (nimbo's `moved()`, not `==`), the model and language are the same, the model is `punctuated`, the previous text has spaces between words (not zh or ja), and less than `CHAIN_GAP` (60 s) passed since that paste. Anything else starts a new chain. With no speech or a cancel the chain stays.
- [x] Engine requests carry the context length. The Worker, which holds the model, shrinks the context to fit one pass (`max_audio_ms`, 15 s on the Neural Engine, 25 s for Parakeet on the GPU), and `Done` reports the context it used. None left means the new audio alone.
- [x] A part is pasted without its trailing mark (`.`, `?`, `!`, `…` and the marks above) only when a chain is possible: `clean_text`, auto-paste and a known target on, and a model that stitches. Otherwise it is pasted whole. On Wayland, where the target is `None`, the feature does nothing.
- [x] A part that continues a chain, stitched or not, is pasted with a leading separator, so words don't glue. A failed alignment or a failed pass recognizes the new audio alone, on `job.engine` (`next_job` unwraps `self.engine`, which a model change can clear), except on a load error, and pastes it with a space. The dictation never fails because of stitching. `pcm` is kept for that second pass.
- [x] Session tests with a scripted `FakeModels` (a text per call) and a `FakeOutput` that can keep its target: continue, each condition that breaks a chain, seam comma, sentence end, no mark, failed alignment, failed pass, cancel, refused paste, setting off behaves exactly as today.
- [x] A real-model test in `MODEL_TESTS`, through `engine_input` with its silence padding: a fixture cut mid-sentence into two recordings gives no capital and no period at the seam.

### 2. Fillers

Files: new `src/cleanup.rs`, `src/session.rs`.

- [x] A table by known language, one line each, nothing for other languages: `ru`: `ээ`, `э-э`, `э-э-э`, `ммм`, `хм`; `en`: `uh`, `uhh`, `um`, `umm`, `uhm`, `er`, `erm`, `hmm`, `hm`, `mmm`. No list for every language: `um` is a German and Portuguese word, `мм` a unit, and a bare `э` and `эм` letters. `ну`, `вот` and words like them stay. Repeated words stay too in this version: `no no no` is often meant.
- [x] Whole words only, in lower case or with a first capital (`ER` stays), Unicode word boundaries. A removed first word gives its capital to the next word, unless that word has a capital inside it (`iPhone`). Punctuation left behind is tidied: `Э-э, в 1990 году` is `В 1990 году`, no double spaces, no leading comma; a pair of commas or dashes around a filler leaves one, as it may be real. A text that is only fillers stays as it is.
- [x] Runs on the new part only, after stitching, when `clean_text` is on; stitching always compares raw model text. Unit tests for each rule and for languages without a table.

### 3. Numbers

Files: `src/cleanup.rs`, new `src/numbers_ru.rs`, `Cargo.toml` through `cargo add`.

- [x] Compare `text-processing-rs` and `text2num` on about 30 phrases each in English, German, Spanish and French: cardinals, ordinals, years, dates, money, and phrases that must stay (`one of them`, `first of all`). Check build time and binary size. A crate passes with 90% of phrases right and no must-stay phrase changed. Take the better one that passes; if none does, skip it, keep Russian, and report. Write the finding under `## Result`.
- [x] Languages the crate has, when the language is known, run through it on the new part, keeping numbers below 10 as words where it allows that.
- [x] `numbers_ru.rs`: Russian number words of any case to digits. 10 and up only, so `один из`, `первым делом`, `в двух словах` stay. Ordinals keep their ending: `в девяностом` is `в 90-м`, `двадцать пятого` is `25-го`, `девяностые` is `90-е`, `две тысячи двадцать шестой` is `2026-й`. Cardinals: `сорок два` is `42`, `три тысячи двести` is `3200`. A table of the words, no regular expressions per form.
- [x] Other languages stay as the model wrote them.

## Checks

Each task: `make check` and `make test` in this worktree. Tasks 1 and 3 also `make test-models` if its models are in the Hugging Face cache, read only. Never open the app or a window, never record from the microphone, temp `WAVO_HOME` in every run.

Reviewed against the code before work started: the findings are resolved in the decisions and tasks above.

Follow-up for the person, not a task: try it in real dictation with `make run` and the switch on; the numbers above are from read speech.

## Round 2

The person tried it. Seams were missed in 4 of 8 parts (the context was the last 10 s of the whole chain, cut short to fit the Neural Engine's 15 s, and a number written as words in one pass and digits in the other broke the match), sentence ends lost their period, and the leading space doubled the person's own or opened a new chat message with a space. Numbers in other languages aren't wanted. Keep the change small; delete what this round makes unused.

Decisions:

- The app knows what happened since its paste: nimbo reports key presses and mouse clicks seen by its event tap, on macOS. Elsewhere it reports nothing known, and stitching is off there.
- A part is pasted as the model wrote it, with its end mark, followed by one space. No leading space, no withheld period.
- Continuing a chain, the pass gives the text from an anchor: the last words of the previous part that aren't numbers (digits or number words). If nothing happened since the paste, the app deletes its own pasted text after the anchor with Backspace and pastes the pass's text from there: a wrong period goes, a needed one stays, a number split by a pause is written whole. If only printable keys were typed (a term by hand), the new words are pasted after it as they are, with a space first only when the last key wasn't one. Enter, Tab, a click, another window, or anything unknown ends the chain.
- The context is the whole previous part when it fits one pass with the new audio, else the new audio goes alone. `CHAIN_GAP` is 20 s.
- Numbers: Russian only, `numbers_ru.rs`; `text2num` and its four languages go.

### 4. nimbo: input since a mark

Repo: /Users/user/Code/nimbo/.worktrees/input-watch (`feat/input-watch`).

- [x] The macOS event tap counts key downs and mouse downs it sees, ignoring the app's own synthetic events, and keeps the last key's kind: space, return, tab, other printable, other. A small public API: take a mark, then ask what happened since (nothing, only printable keys ending in space or not, or something else), and unknown on platforms without it.
- [x] Backspace n times into the focused app, beside the paste API, with the same focus checks.
- [x] Unit tests for the counting logic; `make check` and `make test`.

### 5. wavo-app: the new seam

- [ ] `.cargo/config.toml` patches nimbo to the local worktree for the build, never committed.
- [ ] The decisions above in `stitch.rs` and `session.rs`; session tests for each case: untouched with a wrong period, untouched with a sentence end, a number across the seam, a term typed with and without a trailing space, Enter, click, unknown platform, 20 s gap.
- [ ] `text2num` removed with `cargo remove`, with its wiring and tests; help text says Russian numbers.
- [ ] `make check`, `make test`, `make test-models`.

## Round 3

Round 2 is dropped: watching keys and clicks is out, and stitching audio is out too. Measured on the person's own history (Nov 2–8, about 4000 dictations, 2378 seams under 20 s, matched to the messages they ended in): joining audio changes words in most seams as noise, reads worse in 24 of 100 seams and better in 16, and 91% of parts before another one already end with `.`, `?` or `!`. The person types a space between parts in 55% of seams; the app pastes none, so parts sometimes glue (`типаФиксировать`). Data and scripts: the session's scratchpad `real-seams/`.

Decisions:

- The feature is the one switch, `clean_text` ("Clean up the text"), default on by the person's decision: the cleanup is safe and can be turned off. With it on, every paste ends with one space, with no separate setting.
- A paste starts with a lowercase letter when the previous paste went to the same target less than 20 s before and its text ended without `.`, `?`, `!` or `…`. Words in Latin script and words with more than one capital keep their case. Measured: applies to 8.7% of seams, right in 74% of the seams the messages can score, against 31% for today's paste.
- Fillers as in Task 2. No numbers in any language: `numbers_ru.rs` goes too.
- Removed: stitching (`stitch.rs`, the chain, the second pass, the engine's context, `window_ms`, the changed `TextOutput`, the related tests and fakes), `text2num` with its wiring. The diff against origin/main keeps only what this round needs.

### 6. Simple cleanup

- [x] Remove what the decisions list, with `cargo remove text2num`; the real-model stitching test and its `MODEL_TESTS` line go too.
- [x] Trailing space and the lowercase rule in `session.rs`, using the target's `moved()` and an injected clock; session tests for each case: after an end mark, after none, other target, over 20 s, Latin word, `iPhone`, setting off unchanged.
- [x] Help text: plain words for what the switch does now.
- [x] `make check`, `make test`, `make test-models` (whisper-turbo isn't cached).

Numbers were removed by the person's decision: about 1% of real dictations had number words of 10 or more, and gigaam writes most numbers as digits itself.

## Result

Delivered: the switch "Clean up the text", on by default: a trailing space on every paste, a lowercase start after a paste into the same window under 20 s before that left its sentence open, and hesitation sounds out in Russian and English. Stitching audio, watching keys and numbers were tried and dropped.

Measured on the person's own history (Nov 2–8, about 4000 dictations, 2378 seams under 20 s): joining audio read worse in 24 of 100 seams and better in 16; 91% of parts before another one already end with `.`, `?` or `!`; the person typed a space between parts in 55% of seams. The lowercase rule applies to 8.7% of seams and is right in 74% of those the messages could score, against 31% for the paste before it. Fillers are in 2.7% of dictations; number words of 10 or more in about 1%, so numbers were cut.
