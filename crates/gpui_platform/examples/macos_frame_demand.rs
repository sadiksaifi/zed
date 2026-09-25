//! Verifies demand-driven frame scheduling on macOS with the real application,
//! AppKit run loop, display link, and Metal renderer. The test platform draws
//! automatically and cannot observe idle frame sources.
//!
//! Run with `cargo run -p gpui_platform --example macos_frame_demand --features native-test-support`.

#[cfg(target_os = "macos")]
fn main() {
    native::main();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("the frame-demand fixture requires macOS");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
mod native {
    use anyhow::{Result, anyhow, ensure};
    use gpui::{
        App, AsyncApp, Bounds, Context, Window, WindowBounds, WindowHandle, WindowOptions, div,
        prelude::*, px, rgb, size,
    };
    use gpui_platform::FrameTestSnapshot;
    use std::{cell::Cell, rc::Rc, time::Duration};

    #[derive(Default)]
    struct Observations {
        renders: Cell<u32>,
        rendered_revision: Cell<u32>,
        animation_frames: Cell<u32>,
        callbacks: Cell<u32>,
    }

    struct Fixture {
        revision: u32,
        animation_remaining: u32,
        observed: Rc<Observations>,
    }

    impl Render for Fixture {
        fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            self.observed.renders.set(self.observed.renders.get() + 1);
            self.observed.rendered_revision.set(self.revision);
            if self.animation_remaining > 0 {
                self.animation_remaining -= 1;
                self.observed
                    .animation_frames
                    .set(self.observed.animation_frames.get() + 1);
                window.request_animation_frame();
            }
            div().size_full().bg(rgb(0x202020 + self.revision))
        }
    }

    fn inject_key(window: WindowHandle<Fixture>, cx: &mut AsyncApp) -> Result<()> {
        use cocoa::{
            appkit::{NSEvent, NSEventModifierFlags, NSEventType},
            base::{id, nil},
            foundation::{NSInteger, NSPoint},
        };
        use objc::{class, msg_send, sel, sel_impl};
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let view = window
            .update(cx, |_, window, _| {
                match HasWindowHandle::window_handle(window).map(|handle| handle.as_raw()) {
                    Ok(RawWindowHandle::AppKit(handle)) => Some(handle.ns_view.as_ptr() as id),
                    _ => None,
                }
            })
            .map_err(|_| anyhow!("input window unavailable"))?
            .ok_or_else(|| anyhow!("native input view unavailable"))?;
        // Deliver only to this fixture's view, after releasing the App borrow.
        // Posting to the system event queue could reach an unrelated app.
        unsafe {
            let native_window: id = msg_send![view, window];
            let number: NSInteger = msg_send![native_window, windowNumber];
            let characters: id = msg_send![class!(NSString), stringWithUTF8String: c"a".as_ptr()];
            let event = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode_(
                nil, NSEventType::NSKeyDown, NSPoint::new(0., 0.),
                NSEventModifierFlags::empty(), 0., number, nil, characters, characters, false, 0,
            );
            let _: () = msg_send![view, keyDown: event];
        }
        Ok(())
    }

    async fn wait(cx: &AsyncApp, milliseconds: u64) {
        cx.background_executor()
            .timer(Duration::from_millis(milliseconds))
            .await;
    }

    async fn settle(cx: &AsyncApp) {
        wait(cx, 1500).await;
    }

    async fn assert_idle(stage: &str, cx: &AsyncApp) -> Result<()> {
        let before = FrameTestSnapshot::capture();
        wait(cx, 250).await;
        let after = FrameTestSnapshot::capture();
        if after.logical_frames != before.logical_frames
            || after.native_vsync_callbacks != before.native_vsync_callbacks
            || after.scene_presents != before.scene_presents
        {
            eprintln!("frame_demand_idle stage={stage} before={before:?} after={after:?}");
        }
        ensure!(
            after.logical_frames == before.logical_frames,
            "{stage}: idle window kept requesting frames"
        );
        ensure!(
            after.native_vsync_callbacks == before.native_vsync_callbacks,
            "{stage}: idle display link kept ticking"
        );
        ensure!(
            after.scene_presents == before.scene_presents,
            "{stage}: idle window kept presenting"
        );
        Ok(())
    }

    fn unavailable(what: &'static str) -> impl FnOnce(anyhow::Error) -> anyhow::Error {
        move |_| anyhow!("{what} unavailable")
    }

    async fn run(
        window: WindowHandle<Fixture>,
        observed: Rc<Observations>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        settle(cx).await;
        assert_idle("launch", cx).await?;

        window
            .update(cx, |view, _, cx| {
                view.revision = 1;
                cx.notify();
            })
            .map_err(unavailable("notification window"))?;
        wait(cx, 250).await;
        ensure!(
            observed.rendered_revision.get() == 1,
            "entity notification did not wake rendering"
        );
        assert_idle("notification", cx).await?;

        let callback_observed = observed.clone();
        window
            .update(cx, |_, window, _| {
                window.on_next_frame(move |window, _| {
                    callback_observed
                        .callbacks
                        .set(callback_observed.callbacks.get() + 1);
                    window.on_next_frame(move |_, _| {
                        callback_observed
                            .callbacks
                            .set(callback_observed.callbacks.get() + 1);
                    });
                });
            })
            .map_err(unavailable("callback window"))?;
        wait(cx, 250).await;
        ensure!(
            observed.callbacks.get() == 2,
            "standalone or chained frame callback was stranded"
        );
        assert_idle("callbacks", cx).await?;

        window
            .update(cx, |view, _, cx| {
                view.animation_remaining = 3;
                cx.notify();
            })
            .map_err(unavailable("animation window"))?;
        wait(cx, 250).await;
        ensure!(
            observed.animation_frames.get() == 3,
            "animation did not complete"
        );
        assert_idle("animation", cx).await?;

        let before = FrameTestSnapshot::capture();
        gpui::AnyWindowHandle::from(window)
            .update(cx, |_, window, cx| window.draw(cx).clear(cx))
            .map_err(unavailable("direct draw window"))?;
        wait(cx, 250).await;
        ensure!(
            FrameTestSnapshot::capture().scene_presents > before.scene_presents,
            "direct draw was not presented"
        );
        assert_idle("direct_draw", cx).await?;

        let before = FrameTestSnapshot::capture();
        inject_key(window, cx)?;
        wait(cx, 250).await;
        ensure!(
            FrameTestSnapshot::capture().logical_frames > before.logical_frames,
            "input did not reach the frame loop"
        );
        settle(cx).await;
        assert_idle("input", cx).await?;

        cx.update(|cx| cx.hide());
        wait(cx, 250).await;
        let renders_before_hidden_work = observed.renders.get();
        let callback_observed = observed.clone();
        window
            .update(cx, |view, window, cx| {
                view.revision = 2;
                cx.notify();
                view.revision = 3;
                cx.notify();
                window.on_next_frame(move |_, _| {
                    callback_observed
                        .callbacks
                        .set(callback_observed.callbacks.get() + 1)
                });
            })
            .map_err(unavailable("hidden window"))?;
        wait(cx, 250).await;
        ensure!(
            observed.callbacks.get() == 2,
            "hidden frame callback ran before restoration"
        );
        ensure!(
            observed.renders.get() == renders_before_hidden_work,
            "hidden notification rendered before restoration"
        );
        cx.update(|cx| cx.activate(true));
        window
            .update(cx, |_, window, _| window.activate_window())
            .map_err(unavailable("restored window"))?;
        wait(cx, 500).await;
        ensure!(
            observed.rendered_revision.get() == 3 && observed.callbacks.get() == 3,
            "restoration lost scene or callback demand"
        );
        settle(cx).await;
        assert_idle("restoration", cx).await?;

        let other_bounds = window
            .update(cx, |_, window, _| {
                let bounds = window.bounds();
                Bounds::new(
                    gpui::point(
                        bounds.origin.x + bounds.size.width + px(20.),
                        bounds.origin.y,
                    ),
                    size(px(160.), px(160.)),
                )
            })
            .map_err(unavailable("window bounds"))?;
        let other = cx
            .update(|cx| {
                cx.open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(other_bounds)),
                        ..Default::default()
                    },
                    |_, cx| {
                        cx.new(|_| Fixture {
                            revision: 0,
                            animation_remaining: 0,
                            observed: Rc::new(Observations::default()),
                        })
                    },
                )
            })
            .map_err(|_| anyhow!("second window creation failed"))?;
        other
            .update(cx, |_, window, _| window.activate_window())
            .map_err(unavailable("second window"))?;
        settle(cx).await;
        ensure!(
            !window
                .update(cx, |_, window, _| window.is_window_active())
                .map_err(unavailable("first window"))?,
            "first window did not become inactive"
        );
        assert_idle("two_windows", cx).await?;
        window
            .update(cx, |view, _, cx| {
                view.revision = 4;
                cx.notify();
            })
            .map_err(unavailable("inactive window"))?;
        wait(cx, 250).await;
        ensure!(
            observed.rendered_revision.get() == 4,
            "inactive visible notification was stranded"
        );
        assert_idle("inactive_notification", cx).await?;
        other
            .update(cx, |_, window, _| window.remove_window())
            .map_err(unavailable("second window"))?;
        settle(cx).await;
        assert_idle("second_window_closed", cx).await?;

        // Close the window during a real frame callback. The frame driver must
        // not restore the callback or use the destroyed renderer afterwards.
        window
            .update(cx, |_, window, _| {
                window.on_next_frame(|window, _| window.remove_window())
            })
            .map_err(unavailable("closing window"))?;
        wait(cx, 250).await;
        ensure!(
            window.update(cx, |_, _, _| ()).is_err(),
            "frame callback failed to close its window"
        );
        let closed = FrameTestSnapshot::capture();
        ensure!(
            closed.window_sources_created == closed.window_sources_released,
            "closed windows retained frame sources"
        );
        assert_idle("all_windows_closed", cx).await?;
        println!("frame_demand status=pass");
        Ok(())
    }

    pub(super) fn main() {
        let success = Rc::new(Cell::new(false));
        let result = success.clone();
        gpui_platform::application().run(move |cx: &mut App| {
            let observed = Rc::new(Observations::default());
            let bounds = Bounds::centered(None, size(px(480.), px(320.)), cx);
            let window = cx
                .open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        ..Default::default()
                    },
                    |_, cx| {
                        cx.new(|_| Fixture {
                            revision: 0,
                            animation_remaining: 0,
                            observed: observed.clone(),
                        })
                    },
                )
                .expect("fixture window creation failed");
            cx.activate(true);
            cx.spawn(async move |cx| {
                match run(window, observed, cx).await {
                    Ok(()) => result.set(true),
                    Err(error) => {
                        eprintln!("frame_demand status=fail reason={error}");
                        // AppKit's terminate: exits with status 0 instead of
                        // returning from `run`, so fail before quitting.
                        std::process::exit(1);
                    }
                }
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
        if !success.get() {
            std::process::exit(1);
        }
    }
}
