# SpaceTerm fork

SpaceTerm uses this Zed fork as a Cargo git dependency pinned to a `spaceterm-YYYY-MM-DD` tag.
Published tags are immutable. Another release on the same date adds `.1`, `.2`, and so on.

## Patches

- `fix(gpui_apple): use source-over destination alpha when compositing`
- `perf(gpui_apple): allocate path targets when a scene first draws a path`
- `fix(gpui_macos): host accessibility on GPUIView without class swizzling`
- `feat(gpui_macos): run the display link only while frames are requested`
- `feat(gpui): clip drop shadows out of an element with shadow_outside_only`
- `feat(gpui): add backdrop filters`
- `feat(gpui): paint glyphs and quads around a region`
- `fix(gpui): keep empty content masks empty when snapping`
- `fix(gpui_macos): report glyph offsets in GPUI coordinates`
- `test(gpui): record traffic light positions and export the macOS text system with test-support`

## Monthly rebase

1. Fetch upstream main with `git fetch upstream main` and rebase the `spaceterm` branch with `git rebase upstream/main`.
2. Run the validation commands below.
3. Create the tag with `git tag -a <tag>`. Use `spaceterm-YYYY-MM-DD` for the first release of the
   day and the next numeric suffix for another release that day. Never move or delete a published tag.
4. Push the rebased branch with `git push --force-with-lease origin spaceterm`, then push the tag with
   `git push origin <tag>`.
5. In SpaceTerm, run `mise run gpui:bump <tag>`, then `mise run validate` and `mise run test:macos`.
   The task updates every fork tag, the fork crates in `Cargo.lock`, and the Rust toolchain channel
   to match this fork. SpaceTerm builds the same dependencies as Zed, so the same toolchain keeps
   upstream compiler fixes, such as the future-incompatibility warning in `block` 0.1.6 pulled in by
   `cocoa`, owned by Zed.

## Validation

```sh
cargo test -p gpui --lib
cargo test -p gpui_apple
cargo test -p gpui_macos --features font-kit,test-support
cargo test -p gpui_wgpu
```

Windows cross check:

```sh
rustup target add x86_64-pc-windows-msvc
PATH="/opt/homebrew/opt/llvm@21/bin:$PATH" cargo clippy -p gpui_windows --target x86_64-pc-windows-msvc
```

The `gpui` build script needs `llvm-rc` from LLVM 21 for the Windows check.

Wasm check, matching `.github/workflows/run_tests.yml`:

```sh
rustup toolchain install nightly --component rust-src --target wasm32-unknown-unknown
CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS="-C target-feature=+atomics,+bulk-memory,+mutable-globals" RUSTC_BOOTSTRAP=1 cargo -Zbuild-std=std,panic_abort check --target wasm32-unknown-unknown -p gpui_platform -p cloud_api_client
```
