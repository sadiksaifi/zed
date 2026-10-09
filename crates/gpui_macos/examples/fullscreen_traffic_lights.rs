//! Native regression fixture for custom traffic light positions around native fullscreen.
//! The window opens without a custom position and receives one while fullscreen.
//! Run with `cargo run -p gpui_macos --example fullscreen_traffic_lights`.

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use anyhow::{Result, ensure};
    use cocoa::{
        base::{NO, id, nil},
        foundation::NSRect,
    };
    use gpui::{
        Application, Bounds, Context, Point, Render, TitlebarOptions, Window, WindowBounds,
        WindowOptions, div, point, prelude::*, px, size,
    };
    use objc::{msg_send, sel, sel_impl};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::{
        rc::Rc,
        time::{Duration, Instant},
    };

    const POSITION: Point<gpui::Pixels> = point(px(15.5), px(14.0));

    fn native_window(window: &Window) -> Result<id> {
        let handle = HasWindowHandle::window_handle(window)
            .map_err(|error| anyhow::anyhow!("native window handle: {error:?}"))?;
        let RawWindowHandle::AppKit(native) = handle.as_raw() else {
            anyhow::bail!("expected an AppKit window");
        };
        // SAFETY: The view and its window remain live on the foreground thread.
        Ok(unsafe { msg_send![native.ns_view.as_ptr() as id, window] })
    }

    /// The autoresizing masks of the close, minimize, and zoom buttons.
    fn button_masks(window: &Window) -> Result<Vec<u64>> {
        let native_window = native_window(window)?;
        // SAFETY: The window and its buttons remain live on the foreground thread.
        unsafe {
            (0u64..3)
                .map(|kind| {
                    let button: id = msg_send![native_window, standardWindowButton: kind];
                    ensure!(button != nil, "missing standard window button {kind}");
                    Ok(msg_send![button, autoresizingMask])
                })
                .collect()
        }
    }

    /// The close button's origin, measured from the window's top-left corner.
    fn close_button_origin(window: &Window) -> Result<Option<(f64, f64)>> {
        let native_window = native_window(window)?;
        // SAFETY: The window and its buttons remain live on the foreground thread.
        unsafe {
            let close: id = msg_send![native_window, standardWindowButton: 0u64];
            if close == nil {
                return Ok(None);
            }
            let hidden: cocoa::base::BOOL = msg_send![close, isHiddenOrHasHiddenAncestor];
            if hidden != NO {
                return Ok(None);
            }
            let bounds: NSRect = msg_send![close, bounds];
            let in_window: NSRect = msg_send![close, convertRect: bounds toView: nil];
            let window_frame: NSRect = msg_send![native_window, frame];
            Ok(Some((
                in_window.origin.x,
                window_frame.size.height - in_window.origin.y - in_window.size.height,
            )))
        }
    }

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
                    titlebar: Some(TitlebarOptions {
                        title: None,
                        appears_transparent: true,
                        traffic_light_position: None,
                    }),
                    app_owns_titlebar_drag: true,
                    ..Default::default()
                },
                |_, cx| cx.new(|_| Fixture),
            )
            .expect("native fixture window");
        cx.activate(true);
        cx.spawn(async move |cx| {
            let check: Result<()> = async {
                let expected = (POSITION.x.to_f64(), POSITION.y.to_f64());
                let at_rest = |origin: (f64, f64)| {
                    (origin.0 - expected.0).abs() < 0.01 && (origin.1 - expected.1).abs() < 0.01
                };
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let native_masks = handle.update(cx, |_, window, _| button_masks(window))??;
                for cycle in 0..3 {
                    if cycle > 0 {
                        let windowed =
                            handle.update(cx, |_, window, _| close_button_origin(window))??;
                        ensure!(
                            windowed.is_some_and(at_rest),
                            "cycle {cycle}: windowed traffic lights at {windowed:?}, expected {expected:?}"
                        );
                    }
                    handle.update(cx, |_, window, _| window.toggle_fullscreen())?;
                    cx.background_executor().timer(Duration::from_secs(2)).await;
                    ensure!(
                        handle.update(cx, |_, window, _| window.is_fullscreen())?,
                        "cycle {cycle}: window did not enter fullscreen"
                    );
                    // Fullscreen entry hands the buttons back to AppKit with its own masks.
                    let masks = handle.update(cx, |_, window, _| button_masks(window))??;
                    ensure!(
                        masks == native_masks,
                        "cycle {cycle}: fullscreen button masks {masks:?}, expected AppKit's {native_masks:?}"
                    );
                    if cycle == 0 {
                        handle.update(cx, |_, window, _| {
                            window.set_traffic_light_position(POSITION)
                        })?;
                    }
                    handle.update(cx, |_, window, _| window.toggle_fullscreen())?;
                    // Sample every visible button position until the transition settles.
                    let started = Instant::now();
                    let mut windowed_samples = 0;
                    let mut misplaced = Vec::new();
                    while started.elapsed() < Duration::from_secs(2) {
                        let (fullscreen, origin) = handle.update(cx, |_, window, _| {
                            close_button_origin(window).map(|o| (window.is_fullscreen(), o))
                        })??;
                        if !fullscreen && let Some(origin) = origin {
                            windowed_samples += 1;
                            if !at_rest(origin) {
                                misplaced.push((started.elapsed(), origin));
                            }
                        }
                        cx.background_executor().timer(Duration::from_millis(8)).await;
                    }
                    ensure!(
                        !handle.update(cx, |_, window, _| window.is_fullscreen())?
                            && windowed_samples > 0,
                        "cycle {cycle}: window did not exit fullscreen"
                    );
                    ensure!(
                        misplaced.is_empty(),
                        "cycle {cycle}: windowed traffic lights left {expected:?} after exiting fullscreen: {misplaced:?}"
                    );
                    println!("cycle {cycle}: traffic lights stayed at {expected:?}");
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
    eprintln!("the fullscreen traffic light fixture requires macOS");
    std::process::exit(1);
}
