# Noto Color Emoji test subset

Source: [Noto Emoji v2.051](https://github.com/googlefonts/noto-emoji/releases/tag/v2.051),
[NotoColorEmoji.ttf](https://raw.githubusercontent.com/googlefonts/noto-emoji/v2.051/fonts/NotoColorEmoji.ttf).
Source SHA-256: `72a635cb3d2f3524c51620cdde406b217204e8a6a06c6a096ff8ed4b5fd6e27b`.

Copyright 2022 Google Inc. Licensed under the SIL Open Font License 1.1 in
`OFL.txt`, copied from the upstream `fonts/LICENSE`.

This modified test fixture contains only U+0020 and U+1F600. It retains the
upstream family `Noto Color Emoji`, PostScript name `NotoColorEmoji`, metadata,
and actual color bitmap glyph. It intentionally has no Latin `m` glyph, as in
the original font, so loading it exercises the emoji font validation path.
Normal builds do not embed this test fixture.

Reproduce with `uv run crates/gpui_wgpu/test_data/noto-color-emoji/prepare.py <NotoColorEmoji.ttf>`.
The preparation script pins fontTools 4.66.0 and the source SHA-256.
`SHA256SUMS` records the fixture and upstream license hashes.
