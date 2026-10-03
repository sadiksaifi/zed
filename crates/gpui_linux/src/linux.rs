#[cfg(any(feature = "wayland", feature = "x11"))]
mod clipboard_formats;
#[cfg(any(feature = "wayland", feature = "x11"))]
mod clipboard_transfer;
#[cfg(any(feature = "wayland", feature = "x11"))]
mod compose;
#[cfg(any(feature = "wayland", feature = "x11"))]
mod desktop_window_settings;
mod dispatcher;
mod headless;
mod keyboard;
mod platform;
mod system_notifications;
#[cfg(any(feature = "wayland", feature = "x11"))]
mod text_system;
#[cfg(any(feature = "wayland", feature = "x11"))]
mod titlebar_action;
#[cfg(feature = "wayland")]
mod wayland;
#[cfg(any(feature = "wayland", feature = "x11"))]
mod window_frame;
#[cfg(feature = "x11")]
mod x11;

#[cfg(any(feature = "wayland", feature = "x11"))]
mod xdg_desktop_portal;
#[cfg(any(feature = "wayland", feature = "x11"))]
mod xkb_facts;

pub use dispatcher::*;
pub(crate) use headless::*;
pub(crate) use keyboard::*;
pub(crate) use platform::*;
#[cfg(any(feature = "wayland", feature = "x11"))]
pub(crate) use text_system::*;
#[cfg(any(feature = "wayland", feature = "x11"))]
pub(crate) use titlebar_action::*;
#[cfg(feature = "wayland")]
pub(crate) use wayland::*;
#[cfg(feature = "x11")]
pub(crate) use x11::*;

use std::rc::Rc;

/// Returns the default platform implementation for the current OS.
pub fn current_platform(headless: bool) -> Rc<dyn gpui::Platform> {
    #[cfg(feature = "x11")]
    use anyhow::Context as _;

    if headless {
        return Rc::new(LinuxPlatform {
            inner: HeadlessClient::new(),
        });
    }

    match gpui::guess_compositor() {
        #[cfg(feature = "wayland")]
        "Wayland" => Rc::new(LinuxPlatform {
            inner: WaylandClient::new(),
        }),

        #[cfg(feature = "x11")]
        "X11" => Rc::new(LinuxPlatform {
            inner: X11Client::new()
                .context("Failed to initialize X11 client.")
                .unwrap(),
        }),

        "Headless" => Rc::new(LinuxPlatform {
            inner: HeadlessClient::new(),
        }),
        _ => unreachable!(
            r#"At least one of the "wayland" or "x11" features must be enabled on gpui_linux or gpui_platform."#
        ),
    }
}

/// The desktop fallback is available even when the settings portal has no provider.
pub(crate) fn desktop_button_layout() -> gpui::WindowButtonLayout {
    let desktops = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    if desktops
        .split(':')
        .any(|desktop| desktop.eq_ignore_ascii_case("GNOME"))
    {
        gpui::WindowButtonLayout {
            left: [None; 3],
            right: [Some(gpui::WindowButton::Close), None, None],
        }
    } else {
        gpui::WindowButtonLayout::linux_default()
    }
}
