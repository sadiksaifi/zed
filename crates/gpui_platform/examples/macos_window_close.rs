//! Native window-close and accessibility-host acceptance. Uses the real
//! application because the crash needs AppKit's touch bar observation of the
//! key window's responder chain.
//!
//! Run with `cargo run -p gpui_platform --example macos_window_close`.
#[cfg(target_os = "macos")]
fn main() {
    native::main();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("the window-close fixture requires macOS");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
mod native {
    use anyhow::{Context as _, Result, anyhow, ensure};
    use gpui::{
        AnyWindowHandle, App, AsyncApp, Bounds, Context, Window, WindowBounds, WindowOptions, div,
        prelude::*, px, size,
    };
    use std::time::Duration;

    struct Fixture;

    impl Render for Fixture {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full()
        }
    }

    fn open(
        cx: &mut App,
        title: &'static str,
        bounds: Bounds<gpui::Pixels>,
    ) -> Result<AnyWindowHandle> {
        let window = cx.open_window(
            WindowOptions {
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some(title.into()),
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| Fixture),
        )?;
        Ok(window.into())
    }

    /// Returns the classes of the accessibility children AppKit reaches from
    /// the window's content view, where assistive technologies enter the view
    /// hierarchy.
    fn content_view_accessibility_classes(
        window: AnyWindowHandle,
        cx: &mut AsyncApp,
    ) -> Result<Vec<String>> {
        use cocoa::base::id;
        use objc::{msg_send, sel, sel_impl};
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let view = window
            .update(cx, |_, window, _| {
                match HasWindowHandle::window_handle(window).map(|handle| handle.as_raw()) {
                    Ok(RawWindowHandle::AppKit(handle)) => Some(handle.ns_view.as_ptr() as id),
                    _ => None,
                }
            })
            .map_err(|_| anyhow!("window unavailable"))?
            .context("native view unavailable")?;
        unsafe {
            let native_window: id = msg_send![view, window];
            let content_view: id = msg_send![native_window, contentView];
            let children: id = msg_send![content_view, accessibilityChildren];
            let count: usize = if children.is_null() {
                0
            } else {
                msg_send![children, count]
            };
            let mut classes = Vec::with_capacity(count);
            for index in 0..count {
                let child: id = msg_send![children, objectAtIndex: index];
                let class: id = msg_send![child, className];
                let utf8: *const std::ffi::c_char = msg_send![class, UTF8String];
                classes.push(
                    std::ffi::CStr::from_ptr(utf8)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
            Ok(classes)
        }
    }

    async fn run(cx: &mut AsyncApp) -> Result<()> {
        let first = cx.update(|cx| {
            let bounds = Bounds::centered(None, size(px(320.), px(200.)), cx);
            open(cx, "first", bounds)
        })?;
        cx.update(|cx| cx.activate(true));
        cx.background_executor()
            .timer(Duration::from_millis(500))
            .await;

        let classes = content_view_accessibility_classes(first, cx)?;
        ensure!(
            classes == ["AccessKitNode"],
            "content view exposed {classes:?} instead of the accessibility tree root"
        );

        let second = cx.update(|cx| {
            let bounds = Bounds::centered(None, size(px(200.), px(160.)), cx);
            open(cx, "second", bounds)
        })?;
        second
            .update(cx, |_, window, _| window.activate_window())
            .map_err(|_| anyhow!("second window unavailable"))?;
        cx.background_executor()
            .timer(Duration::from_millis(1500))
            .await;

        // Closing the key window used to raise `NSRangeException` from AppKit's
        // touch bar observation during the next display cycle.
        second
            .update(cx, |_, window, _| window.remove_window())
            .map_err(|_| anyhow!("second window unavailable"))?;
        cx.background_executor()
            .timer(Duration::from_millis(1000))
            .await;
        first
            .update(cx, |_, window, _| window.remove_window())
            .map_err(|_| anyhow!("first window unavailable"))?;
        cx.background_executor()
            .timer(Duration::from_millis(1000))
            .await;
        Ok(())
    }

    pub fn main() {
        gpui_platform::application().run(|cx: &mut App| {
            cx.spawn(async move |cx| {
                if let Err(error) = run(cx).await {
                    eprintln!("window_close status=fail reason={error}");
                    // AppKit's terminate: exits with status 0, so fail before quitting.
                    std::process::exit(1);
                }
                println!("window_close accessibility=pass close=pass status=pass");
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
    }
}
