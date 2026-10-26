#!/usr/bin/env python3
"""Speech fixtures for the tests: 16 kHz mono 16-bit WAV plus the reference transcript (.txt).

  scripts/fixtures.py [out_dir]    # default: target/fixtures

Texts live in scripts/fixtures/*.txt; blank lines between paragraphs become 2.5 s pauses. Lengths
follow real dictations (median ~4 s, p99 ~27 s): nothing is longer than ~30 s. Needs the macOS
`say` voices Milena (ru), Samantha and Daniel (en), and ffmpeg.
"""

import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

SRC = Path(__file__).parent / "fixtures"
VOICES = {"ru-short": "Milena", "ru-typical": "Milena", "ru-long": "Milena",
          "en-short": "Samantha", "en-paragraph": "Daniel"}


def ff(*args: str | Path) -> None:
    cmd = ["ffmpeg", "-nostdin", "-loglevel", "error", "-y", *map(str, args)]
    subprocess.run(cmd, check=True)


def main() -> int:
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

    # No speech: digital silence and quiet pink noise, 5 s each.
    ff("-f", "lavfi", "-i", "anullsrc=r=16000:cl=mono", "-t", "5", "-c:a", "pcm_s16le",
       out / "silence.wav")
    ff("-f", "lavfi", "-i", "anoisesrc=d=5:c=pink:r=16000:a=0.05", "-ac", "1", "-c:a",
       "pcm_s16le", out / "noise.wav")

    for wav in sorted(out.glob("*.wav")):
        secs = subprocess.run(
            ["ffprobe", "-v", "error", "-show_entries", "format=duration", "-of",
             "default=nw=1:nk=1", wav], capture_output=True, text=True, check=True,
        ).stdout.strip()
        print(f"{wav.name:<28} {float(secs):6.1f} s")

    return 0


if __name__ == "__main__":
    sys.exit(main())
