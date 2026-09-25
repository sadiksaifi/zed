//! Exercises the display-link registry against real CoreVideo links and
//! dispatch sources on the macOS main thread.
//!
//! Run with `cargo run -p gpui_macos --example display_link_lifecycle --features native-test-support`.

#[cfg(target_os = "macos")]
#[allow(dead_code)]
#[path = "../src/display_link.rs"]
mod display_link;

#[cfg(target_os = "macos")]
#[allow(dead_code)]
#[path = "../src/frame_test_support.rs"]
mod frame_test_support;

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    native::run()
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("the display-link lifecycle fixture requires macOS");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
mod native {
    use super::display_link::WindowFrameSource;
    use super::frame_test_support::FrameTestSnapshot;
    use anyhow::{Result, ensure};
    use core_foundation::runloop::{CFRunLoop, kCFRunLoopDefaultMode};
    use core_graphics::display::CGDisplay;
    use std::{
        cell::Cell,
        ffi::c_void,
        time::{Duration, Instant},
    };

    #[derive(Default)]
    struct Probe {
        ticks: Cell<usize>,
        closed: Cell<bool>,
        late_ticks: Cell<usize>,
    }

    extern "C" fn tick(context: *mut c_void) {
        // Every probe outlives the final main-queue drain.
        let probe = unsafe { &*context.cast::<Probe>() };
        probe.ticks.set(probe.ticks.get() + 1);
        if probe.closed.get() {
            probe.late_ticks.set(probe.late_ticks.get() + 1);
        }
    }

    fn source(probe: &Probe) -> WindowFrameSource {
        WindowFrameSource::new(std::ptr::from_ref(probe).cast_mut().cast(), tick)
    }

    fn pump_for(duration: Duration) {
        let until = Instant::now() + duration;
        while let Some(remaining) = until.checked_duration_since(Instant::now()) {
            CFRunLoop::run_in_mode(
                unsafe { kCFRunLoopDefaultMode },
                remaining.min(Duration::from_millis(20)),
                false,
            );
        }
    }

    fn await_ticks(probe: &Probe, previous: usize) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(2);
        while probe.ticks.get() == previous && Instant::now() < deadline {
            pump_for(Duration::from_millis(20));
        }
        ensure!(
            probe.ticks.get() > previous,
            "frame callback deadline exceeded"
        );
        Ok(())
    }

    pub(super) fn run() -> Result<()> {
        const CYCLES: usize = 32;
        let display_id = CGDisplay::main().id;
        let first = Box::<Probe>::default();
        let second = Box::<Probe>::default();
        let mut first_source = source(&first);
        let mut second_source = source(&second);
        first_source.start(display_id)?;
        first_source.start(display_id)?;
        ensure!(
            first.ticks.get() == 0,
            "starting a frame source invoked its callback synchronously"
        );
        second_source.start(display_id)?;
        await_ticks(&first, 0)?;
        await_ticks(&second, 0)?;

        first_source.stop();
        first_source.stop();
        // A queued dispatch event may remain after unsubscribing. Drain it
        // before checking that the stopped source receives no further ticks.
        pump_for(Duration::from_millis(100));
        let stopped_ticks = first.ticks.get();
        let continuing_ticks = second.ticks.get();
        pump_for(Duration::from_millis(100));
        ensure!(
            first.ticks.get() == stopped_ticks,
            "stopped source kept ticking"
        );
        ensure!(
            second.ticks.get() > continuing_ticks,
            "shared display stopped early"
        );

        first_source.start(display_id)?;
        await_ticks(&first, stopped_ticks)?;
        drop(first_source);
        first.closed.set(true);
        pump_for(Duration::from_millis(100));
        let closed_ticks = first.ticks.get();
        let continuing_ticks = second.ticks.get();
        pump_for(Duration::from_millis(100));
        ensure!(
            first.ticks.get() == closed_ticks,
            "cancelled source kept ticking"
        );
        ensure!(
            second.ticks.get() > continuing_ticks,
            "closing one source stopped another"
        );

        drop(second_source);
        second.closed.set(true);
        // Keep every callback context alive, so unexpected late delivery fails
        // an assertion instead of reading freed memory.
        let mut probes = Vec::with_capacity(CYCLES * 2);
        for cycle in 0..CYCLES {
            let probe = Box::<Probe>::default();
            let mut current = source(&probe);
            current.start(display_id)?;
            await_ticks(&probe, 0)?;
            current.stop();
            let previous = probe.ticks.get();
            current.start(display_id)?;
            await_ticks(&probe, previous)?;
            drop(current);
            probe.closed.set(true);
            probes.push(probe);

            let undelivered = Box::<Probe>::default();
            let mut pending = source(&undelivered);
            if cycle % 2 == 0 {
                // Cancel the initial asynchronous frame request before the
                // main queue runs it. Other cycles cover never-started sources.
                pending.start(display_id)?;
            }
            drop(pending);
            undelivered.closed.set(true);
            probes.push(undelivered);
        }
        pump_for(Duration::from_millis(100));
        let settled_ticks = probes.iter().map(|probe| probe.ticks.get()).sum::<usize>();
        let settled_first = first.ticks.get();
        let settled_second = second.ticks.get();
        pump_for(Duration::from_millis(100));
        ensure!(
            probes.iter().map(|probe| probe.ticks.get()).sum::<usize>() == settled_ticks
                && first.ticks.get() == settled_first
                && second.ticks.get() == settled_second,
            "closed frame sources did not settle"
        );
        ensure!(
            probes
                .iter()
                .skip(1)
                .step_by(2)
                .all(|probe| probe.ticks.get() == 0),
            "cancelled undelivered sources received callbacks"
        );
        let late_ticks = probes
            .iter()
            .map(|probe| probe.late_ticks.get())
            .sum::<usize>()
            + first.late_ticks.get()
            + second.late_ticks.get();
        ensure!(late_ticks == 0, "cancelled source invoked a closed context");

        let snapshot = FrameTestSnapshot::capture();
        ensure!(
            snapshot.native_links_created == 1,
            "native links accumulated across source lifetimes"
        );
        ensure!(
            snapshot.window_sources_created == (CYCLES * 2 + 2) as u64
                && snapshot.window_sources_created == snapshot.window_sources_released,
            "window frame sources were not released"
        );
        ensure!(
            snapshot.window_sources_subscribed == snapshot.window_sources_unsubscribed
                && snapshot.native_links_started == snapshot.native_links_stopped,
            "frame subscriptions did not balance"
        );
        println!(
            "display_link_lifecycle cycles={CYCLES} callback_contexts={} settled_cycle_ticks={settled_ticks} late_ticks={late_ticks} links_created={} sources_created={} sources_released={} status=pass",
            probes.len() + 2,
            snapshot.native_links_created,
            snapshot.window_sources_created,
            snapshot.window_sources_released,
        );
        Ok(())
    }
}
