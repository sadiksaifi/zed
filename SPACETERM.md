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
- `feat(gpui): record window management requests on test windows`
- `feat(gpui): simulate window decorations, controls and button layout in tests`
- `feat(gpui): activate accessibility on test windows`
- `feat(gpui): record traffic light positions on every test platform`
- `fix(gpui): treat GNOME appmenu, icon and spacer as known button layout items`
- `feat(gpui_linux): perform the desktop's titlebar double-click action`
- `fix(gpui_linux): honor non-resizable and non-minimizable windows`
- `fix(gpui_linux): give X11 client-decorated windows a transparent surface`
- `feat(gpui): report which window backgrounds the platform supports`
- `fix(gpui_linux): mark the X11 click that activates a window as first_mouse`
- `fix(gpui_linux): set X11 WM_CLASS from the app ID before mapping`
- `perf(gpui_linux): stop the X11 refresh timer while a window requests no frames`
- `feat(gpui_linux): report touchpad scroll phases on Wayland`
- `feat(gpui): draw a client frame for client-decorated windows in the WGPU renderer`
- `feat(gpui_linux): report the active layout's shift pairs`
- `fix(gpui_linux): report every keymap and layout group change`
- `feat(gpui_linux): report native XKB key facts`
- `fix(gpui_linux): mark X11 key repeats as held`
- `fix(gpui_linux): commit composed text through the input handler`
- `fix(gpui_linux): commit input method text over the pre-edit through the input handler`
- `feat(gpui_linux): read and write clipboard file lists`
- `refactor(gpui_linux): carry each Wayland activation request on its token`
- `feat(gpui_linux): request window attention on Wayland`
- `feat(gpui): withdraw window attention requests`
- `feat(gpui): activate windows with activation tokens from other programs`
- `fix(gpui_linux): report X11 window bounds to AccessKit`
- `feat(gpui_linux): resolve .SystemUIFont through fontconfig`
- `fix(gpui_linux): serve the UTF-8 text targets that the X11 clipboard advertises`
- `fix(gpui_linux): build the titlebar action only with a display backend`
- `fix(gpui_wgpu): retain emoji fonts when loading families`
- `fix(gpui_wgpu): preserve combining mark shaping offsets`
- `test(gpui_wgpu): isolate emoji font regression fixture`

- `fix(gpui_linux): preserve printable digit-row shortcut keys`
- `fix(gpui_linux): keep Latin letters distinct from shortcut punctuation`
- `feat(gpui): retain native display ownership for exported window parents`
- `feat(gpui): offer HTML clipboard alternates on Linux`
- `fix(gpui_web): report unsupported window background effects`
- `fix(gpui_linux): retain held modifiers when the other side releases`
- `fix(gpui_linux): honor display backend feature gates`
- `feat(gpui): expose native interactive resize state`
- `fix(gpui_linux): preserve client frame geometry and input regions`
- `fix(gpui_linux): bound clipboard reads and prefer text alternates`
- `fix(gpui_linux): suppress physical key events consumed by Compose`
- `fix(gpui_linux): suppress releases intercepted by Wayland input methods`
- `fix(gpui_linux): preserve clipboard fixtures across X11 transfers`

- `feat(gpui_linux): follow native titlebar click actions and live KDE window settings`
- `fix(gpui_linux): retain explicitly requested client decorations on Wayland`
- `fix(gpui_linux): keep client-decorated Wayland windows free of server decorations`
- `fix(gpui_linux): paste Wayland text offered only as text/plain`
- `fix(gpui_linux): read KDE window settings only from regular files`

- `fix(gpui_linux): expose bounded exact selection text and typed writes`
- `fix(gpui_linux): bound nonblocking outgoing Wayland clipboard transfers`

- `feat(gpui): move and resize windows with set_bounds`
- `feat(gpui): drag files with their own icons`
- `feat(gpui): drag files as a caller-drawn image`

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

The five `gpui_wgpu` headless renderer tests run on native platforms and serialize their GPU
device use within the test process. Each test creates a device, so concurrent tests can contend
for the adapter's device capacity. Headless contexts enumerate all native backends, including
Metal on macOS. Previously they used the window context's Vulkan/GL-only instance, which
enumerated no macOS adapter; the Linux-only test gate hid that failure on macOS.

Windows cross check:

```sh
rustup target add x86_64-pc-windows-msvc
mise x conda:llvm-tools@21.1.8 -- cargo clippy -p gpui_windows --target x86_64-pc-windows-msvc
```

The `gpui` build script needs `llvm-rc` from LLVM 21 for the Windows check; mise provides it from
conda-forge's `llvm-tools`.

Linux check, with the Wayland and X11 development libraries installed:

```sh
cargo check -p gpui_platform --features wayland,x11,font-kit
cargo test -p gpui --lib --features test-support
cargo test -p gpui_linux --features wayland,x11
cargo clippy -p gpui -p gpui_linux -p gpui_wgpu -p gpui_platform --all-targets --features gpui/test-support,gpui_linux/wayland,gpui_linux/x11 -- -D warnings
cargo clippy -p gpui_linux --all-targets --no-default-features --features wayland -- -D warnings
cargo clippy -p gpui_linux --all-targets --no-default-features --features x11 -- -D warnings
cargo clippy -p gpui_linux --all-targets --no-default-features -- -D warnings
```

The `gpui_wgpu` headless renderer tests need a GPU adapter. Without a GPU, Mesa lavapipe provides a
software Vulkan adapter.

On Linux, `Window::set_bounds` moves and resizes X11 windows on their existing screen, honoring
the scale factor and updating fixed-size hints for non-resizable windows. Moving an existing
X11 window between separate screens is unsupported and logs a warning. Wayland applies only
the size through the existing resize path; the compositor controls placement and output
selection. Headless windows update their stored bounds, and test windows also record requests.

Wayland native file drags transfer the file URI list without an icon surface, including when
the caller requests a drawn image, so the compositor chooses their appearance. X11 native
outgoing file drags remain unsupported. The test platform records the complete drag payload,
including the requested icon.

Wasm check, matching `.github/workflows/run_tests.yml`:

```sh
rustup toolchain install nightly --component rust-src --target wasm32-unknown-unknown
CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS="-C target-feature=+atomics,+bulk-memory,+mutable-globals" RUSTC_BOOTSTRAP=1 cargo -Zbuild-std=std,panic_abort check --target wasm32-unknown-unknown -p gpui_platform -p cloud_api_client
```
