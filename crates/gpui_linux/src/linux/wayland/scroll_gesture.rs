use gpui::TouchPhase;
use wayland_client::protocol::wl_pointer::AxisSource;

/// Tracks a touchpad scroll gesture across `wl_pointer` frames to report its phases.
///
/// Compositors send `axis_stop` when the fingers lift from a touchpad, which ends the gesture.
/// Other sources, such as wheels, send no stop and report every event as moved.
#[derive(Debug, Default)]
pub(crate) struct ScrollGesture {
    active: bool,
}

impl ScrollGesture {
    /// Returns the phase of a frame that scrolled with `source`.
    pub(crate) fn scrolled(&mut self, source: AxisSource) -> TouchPhase {
        if source != AxisSource::Finger {
            return TouchPhase::Moved;
        }
        if std::mem::replace(&mut self.active, true) {
            TouchPhase::Moved
        } else {
            TouchPhase::Started
        }
    }

    /// Ends the gesture, returning whether one was in progress.
    pub(crate) fn end(&mut self) -> bool {
        std::mem::replace(&mut self.active, false)
    }
}

#[cfg(test)]
mod tests {
    use super::ScrollGesture;
    use gpui::TouchPhase;
    use wayland_client::protocol::wl_pointer::AxisSource;

    #[test]
    fn finger_scroll_starts_moves_and_ends() {
        let mut gesture = ScrollGesture::default();
        assert_eq!(gesture.scrolled(AxisSource::Finger), TouchPhase::Started);
        assert_eq!(gesture.scrolled(AxisSource::Finger), TouchPhase::Moved);
        assert!(gesture.end());
        assert!(!gesture.end());
        assert_eq!(gesture.scrolled(AxisSource::Finger), TouchPhase::Started);
    }

    #[test]
    fn other_sources_only_move() {
        let mut gesture = ScrollGesture::default();
        for source in [
            AxisSource::Wheel,
            AxisSource::Continuous,
            AxisSource::WheelTilt,
        ] {
            assert_eq!(gesture.scrolled(source), TouchPhase::Moved);
        }
        assert!(!gesture.end());
    }
}
