#!/usr/bin/env python3
"""Speech fixtures for the tests: 16 kHz mono 16-bit WAV plus the reference transcript (.txt).

  scripts/fixtures.py [out_dir]    # default: target/fixtures
  scripts/fixtures.py --lecture    # target/fixtures/files/lecture.m4a, 4.5 h, for make test-lecture

Texts live in scripts/fixtures/*.txt; blank lines between paragraphs become 2.5 s pauses. Lengths
follow real dictations (median ~4 s, p99 ~27 s): nothing is longer than ~30 s. Needs the macOS
`say` voices Milena (ru), Samantha and Daniel (en), and ffmpeg.
"""

from __future__ import annotations

import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

SRC = Path(__file__).parent / "fixtures"
VOICES = {"ru-short": "Milena", "ru-typical": "Milena", "ru-long": "Milena",
          "en-short": "Samantha", "en-paragraph": "Daniel"}


# Mono in both channels as it is: ffmpeg's own upmix takes 3 dB off each.
STEREO = ["-af", "pan=stereo|c0=c0|c1=c0"]
# Extension (with a name part when one container has several codecs) → ffmpeg's codec arguments.
FORMATS = {
    "mp3": ["-c:a", "libmp3lame", "-b:a", "64k"],
    "m4a": ["-ar", "44100", *STEREO, "-c:a", "aac", "-b:a", "96k"],
    "alac.m4a": ["-c:a", "alac"],
    "flac": ["-c:a", "flac"],
    # ffmpeg's own Vorbis encoder: experimental, stereo only.
    "ogg": [*STEREO, "-c:a", "vorbis", "-strict", "-2"],
    "mkv": ["-c:a", "aac"],
    "aiff": ["-c:a", "pcm_s16be"],
    "caf": ["-c:a", "pcm_s16le"],
    "opus": ["-c:a", "libopus"],
}


def ff(*args: str | Path) -> None:
    cmd = ["ffmpeg", "-nostdin", "-loglevel", "error", "-y", *map(str, args)]
    subprocess.run(cmd, check=True)


def lecture(out: Path) -> None:
    """4.5 h of the fixtures' speech with pauses, as a phone records a lecture: 64 kbps AAC."""
    files = out / "files"
    files.mkdir(parents=True, exist_ok=True)
    # Absolute: ffmpeg takes a list's paths from the list's folder.
    talk = [(out / f"{name}.wav").resolve() for name in ("ru-long", "ru-typical", "ru-short")]
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        ff("-f", "lavfi", "-i", "anullsrc=r=16000:cl=mono", "-t", "3", "-c:a", "pcm_s16le",
           tmp / "pause.wav")
        once = tmp / "once.wav"
        parts = [p for wav in talk for p in (wav, tmp / "pause.wav")]
        (tmp / "once.txt").write_text("".join(f"file '{p}'\n" for p in parts))
        ff("-f", "concat", "-safe", "0", "-i", tmp / "once.txt", "-c", "copy", once)
        ff("-stream_loop", "-1", "-i", once, "-t", str(4.5 * 3600), "-c:a", "aac", "-b:a", "64k",
           files / "lecture.m4a")


def main() -> int:
    if sys.argv[1:] == ["--lecture"]:
        lecture(Path("target/fixtures"))
        return 0

    out = Path(sys.argv[1] if len(sys.argv) > 1 else "target/fixtures")
    out.mkdir(parents=True, exist_ok=True)
    for tool in ("say", "ffmpeg", "ffprobe"):
        if not shutil.which(tool):
            sys.exit(f"fixtures: {tool} not found")

    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        for name, voice in VOICES.items():
            text = (SRC / f"{name}.txt").read_text()
            paragraphs = [p.strip() for p in text.split("\n\n") if p.strip()]
            (tmp / f"{name}.say").write_text(" [[slnc 2500]] ".join(paragraphs))
            aiff = tmp / f"{name}.aiff"
            subprocess.run(["say", "-v", voice, "-o", aiff, "-f", tmp / f"{name}.say"], check=True)
            ff("-i", aiff, "-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le", out / f"{name}.wav")
            shutil.copy(SRC / f"{name}.txt", out / f"{name}.txt")

        # The en phrase again as 44.1 kHz stereo float: WAV decoding, downmix and resampling.
        ff("-i", tmp / "en-short.aiff", "-ar", "44100", "-ac", "2", "-c:a", "pcm_f32le",
           out / "en-short-44k.wav")
        shutil.copy(SRC / "en-short.txt", out / "en-short-44k.txt")

        # The ru paragraph in each format a file to transcribe may come in (decode.rs), and in
        # Opus, which is refused.
        files = out / "files"
        files.mkdir(exist_ok=True)
        aiff = tmp / "ru-long.aiff"
        for name, codec in FORMATS.items():
            ff("-i", aiff, *codec, files / f"ru-long.{name}")

        # A video: a black picture over the sound.
        ff("-f", "lavfi", "-i", "color=c=black:s=320x240:r=5", "-i", aiff, "-shortest",
           "-c:v", "libx264", "-c:a", "aac", "-b:a", "96k", files / "ru-long-video.mp4")
        # Two Ogg streams one after the other, as a recorder that went on does: each encode has
        # a serial of its own.
        ff("-i", aiff, *FORMATS["ogg"], tmp / "again.ogg")
        chained = (files / "ru-long.ogg").read_bytes() + (tmp / "again.ogg").read_bytes()
        (files / "ru-long-chained.ogg").write_bytes(chained)

    # No speech: digital silence and quiet pink noise, 5 s each.
    ff("-f", "lavfi", "-i", "anullsrc=r=16000:cl=mono", "-t", "5", "-c:a", "pcm_s16le",
       out / "silence.wav")
    ff("-f", "lavfi", "-i", "anoisesrc=d=5:c=pink:r=16000:a=0.05", "-ac", "1", "-c:a",
       "pcm_s16le", out / "noise.wav")

    for wav in sorted([*out.glob("*.wav"), *(out / "files").glob("*.*")]):
        secs = subprocess.run(
            ["ffprobe", "-v", "error", "-show_entries", "format=duration", "-of",
             "default=nw=1:nk=1", wav], capture_output=True, text=True, check=True,
        ).stdout.strip()
        # None for the chained Ogg.
        secs = float(secs) if secs != "N/A" else float("nan")
        print(f"{wav.name:<28} {secs:6.1f} s")

    return 0


if __name__ == "__main__":
    sys.exit(main())
