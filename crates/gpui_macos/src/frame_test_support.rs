//! Opt-in, content-free counters of native frame activity, for regression
//! fixtures that must observe a real AppKit run loop.

use std::sync::atomic::{AtomicU64, Ordering};

macro_rules! frame_counters {
    ($(#[doc = $doc:literal] $field:ident => $counter:ident),+ $(,)?) => {
        /// Cumulative process-wide frame activity, available with the
        /// `native-test-support` feature.
        ///
        /// Counters never reset. Each value is read atomically, but a snapshot
        /// is not a transaction across the display-link and main threads.
        /// Fixtures compare snapshots taken after bounded run-loop drains.
        #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
        pub struct FrameTestSnapshot {
            $(#[doc = $doc] pub $field: u64),+
        }

        #[derive(Clone, Copy)]
        pub(crate) enum Counter {
            $($counter),+
        }

        static COUNTERS: [AtomicU64; [$(stringify!($counter)),+].len()] =
            [const { AtomicU64::new(0) }; [$(stringify!($counter)),+].len()];

        impl FrameTestSnapshot {
            /// Reads the cumulative counters without resetting them.
            pub fn capture() -> Self {
                Self {
                    $($field: COUNTERS[Counter::$counter as usize].load(Ordering::Relaxed)),+
                }
            }
        }
    };
}

frame_counters! {
    /// CoreVideo output callbacks, before per-window main-queue coalescing.
    native_vsync_callbacks => NativeVsync,
    /// Frame requests delivered to GPUI, whether or not they draw or present.
    logical_frames => LogicalFrame,
    /// Scenes submitted to the Metal renderer.
    scene_presents => ScenePresent,
    /// CVDisplayLinks created and retained by the display registry.
    native_links_created => NativeLinkCreated,
    /// Successful CVDisplayLink start calls.
    native_links_started => NativeLinkStarted,
    /// CVDisplayLink stop calls.
    native_links_stopped => NativeLinkStopped,
    /// Window dispatch sources created and resumed.
    window_sources_created => WindowSourceCreated,
    /// Window dispatch sources cancelled and released.
    window_sources_released => WindowSourceReleased,
    /// Successful window subscriptions to a display's vsync.
    window_sources_subscribed => WindowSourceSubscribed,
    /// Window subscriptions removed from a display's vsync.
    window_sources_unsubscribed => WindowSourceUnsubscribed,
}

pub(crate) fn record(counter: Counter) {
    COUNTERS[counter as usize].fetch_add(1, Ordering::Relaxed);
}
