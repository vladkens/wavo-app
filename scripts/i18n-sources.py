#!/usr/bin/env python3
"""The app's sources without their tests, for xgettext: test-only strings stay out of the catalog.

  scripts/i18n-sources.py <src dir> <out dir>

Each `.rs` file under <src dir> is written to <out dir> under the same name, with every item
behind `#[cfg(test)]` (or `#[cfg(all(test, …))]`) blanked out line for line: the line numbers in
the catalog's references stay the real ones. An item ends at the `}` that closes its first `{`,
or at its `;` when it has none (`mod testing;`). Braces in strings and comments are not told
apart: the app's test items have none unbalanced.
"""

import re
import sys
from pathlib import Path

TEST = re.compile(r"^\s*#\[cfg\((all\()?test\b")


def strip(text: str) -> str:
    out, skip, depth, opened = [], False, 0, False
    for line in text.splitlines():
        if not skip and TEST.match(line):
            skip, depth, opened = True, 0, False

        if not skip:
            out.append(line)
            continue

        out.append("")
        code = line.split("//", 1)[0]
        depth += code.count("{") - code.count("}")
        opened = opened or "{" in code
        if (opened and depth <= 0) or (not opened and code.rstrip().endswith(";")):
            skip = False

    return "\n".join(out) + "\n"


def main() -> None:
    src, out = Path(sys.argv[1]), Path(sys.argv[2])
    for path in sorted(src.rglob("*.rs")):
        to = out / path.relative_to(src.parent)
        to.parent.mkdir(parents=True, exist_ok=True)
        to.write_text(strip(path.read_text()))


if __name__ == "__main__":
    main()
