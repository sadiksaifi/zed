# /// script
# requires-python = ">=3.13"
# dependencies = ["fonttools==4.66.0"]
# ///
"""Reproduce the real Noto Color Emoji test subset from its pinned upstream font."""

import argparse
import hashlib
from pathlib import Path

from fontTools import subset
from fontTools.ttLib import TTFont

SOURCE_SHA256 = "72a635cb3d2f3524c51620cdde406b217204e8a6a06c6a096ff8ed4b5fd6e27b"
DESTINATION = Path(__file__).resolve().parent / "NotoColorEmoji-Subset.ttf"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    args = parser.parse_args()
    if hashlib.sha256(args.source.read_bytes()).hexdigest() != SOURCE_SHA256:
        parser.error("expected the unmodified NotoColorEmoji.ttf from Noto Emoji v2.051")
    options = subset.Options()
    options.name_IDs = ["*"]
    options.name_legacy = True
    options.name_languages = ["*"]
    options.recalc_timestamp = False
    font = TTFont(args.source, recalcTimestamp=False)
    subsetter = subset.Subsetter(options=options)
    subsetter.populate(unicodes=[0x20, 0x1F600])
    subsetter.subset(font)
    font.save(DESTINATION, reorderTables=False)
    print(f"SHA-256: {hashlib.sha256(DESTINATION.read_bytes()).hexdigest()}")


if __name__ == "__main__":
    main()
