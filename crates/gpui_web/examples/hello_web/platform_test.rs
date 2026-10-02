use gpui::WindowBackgroundSupport;
use wasm_bindgen::prelude::*;

fn main() {}

/// Checks the actual browser platform without requiring a GPU adapter or an open window.
#[wasm_bindgen]
pub fn verify_window_background_capabilities() {
    let platform = gpui_platform::current_platform(false);
    assert_eq!(
        platform.window_background_support(),
        WindowBackgroundSupport::NONE,
        "the opaque browser canvas cannot provide transparent or blurred window backgrounds"
    );
}
