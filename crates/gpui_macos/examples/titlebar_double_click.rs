//! Native regression fixture for the macOS Fill title bar action.
//! Set "Double-click a window's title bar" to Fill, then run with
//! `cargo run -p gpui_macos --example titlebar_double_click`.
//! Run once with tiling margins enabled and once with them disabled.

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use anyhow::{Result, ensure};
    use cocoa::{
        base::{id, nil},
        foundation::{NSAutoreleasePool, NSUserDefaults},
    };
    use gpui::{
        Application, Bounds, Context, Render, Window, WindowBounds, WindowOptions, div, prelude::*,
        px, size,
    };
    use objc::{
        msg_send,
        runtime::{BOOL, YES},
        sel, sel_impl,
    };
    use objc2::rc::Retained;
    use objc2_foundation::NSString;
    use std::{rc::Rc, time::Duration};

    // SAFETY: The defaults and strings are live Objective-C objects on the main thread.
    let fill_selected = unsafe {
        let pool = NSAutoreleasePool::new(nil);
        let defaults: id = NSUserDefaults::standardUserDefaults();
        let domain = NSString::from_str("NSGlobalDomain");
        let key = NSString::from_str("AppleActionOnDoubleClick");
        let fill = NSString::from_str("Fill");
        let domain: id =
            msg_send![defaults, persistentDomainForName: Retained::as_ptr(&domain) as id];
        let action: id = msg_send![domain, objectForKey: Retained::as_ptr(&key) as id];
        let selected: BOOL = msg_send![action, isEqualToString: Retained::as_ptr(&fill) as id];
        pool.drain();
        selected == YES
    };
    ensure!(
        fill_selected,
        "set the macOS title bar double-click action to Fill before running this fixture"
    );

    struct Fixture;

    impl Render for Fixture {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full()
        }
    }

    Application::with_platform(Rc::new(gpui_macos::MacPlatform::new(false))).run(move |cx| {
        let handle = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                        None,
                        size(px(700.), px(450.)),
                        cx,
                    ))),
                    ..Default::default()
                },
                |_, cx| cx.new(|_| Fixture),
            )
            .expect("native fixture window");
        cx.activate(true);
        cx.spawn(async move |cx| {
            let check: Result<()> = async {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                for cycle in 0..2 {
                    let original = handle.update(cx, |_, window, _| window.bounds())?;
                    handle.update(cx, |_, window, _| window.titlebar_double_click())?;
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    let filled = handle.update(cx, |_, window, _| window.bounds())?;
                    ensure!(
                        filled.size.width > original.size.width
                            && filled.size.height > original.size.height,
                        "cycle {cycle}: first double-click did not fill: {original:?} -> {filled:?}"
                    );
                    handle.update(cx, |_, window, _| window.titlebar_double_click())?;
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    let restored = handle.update(cx, |_, window, _| window.bounds())?;
                    // AppKit's native untile action can round an edge by one point.
                    ensure!(
                        (restored.origin.x - original.origin.x).abs() <= px(1.)
                            && (restored.origin.y - original.origin.y).abs() <= px(1.)
                            && (restored.size.width - original.size.width).abs() <= px(1.)
                            && (restored.size.height - original.size.height).abs() <= px(1.),
                        "cycle {cycle}: second double-click did not restore: {original:?} -> {restored:?}"
                    );
                    println!("cycle {cycle}: {original:?} -> {filled:?} -> {restored:?}");
                }
                Ok(())
            }
            .await;
            if let Err(error) = check {
                eprintln!("{error:#}");
                std::process::exit(1);
            }
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
    anyhow::bail!("native fixture exited before completing")
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("the title bar fixture requires macOS");
    std::process::exit(1);
}
