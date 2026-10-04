use gpui::{Bounds, Pixels, WindowDecorations, px};

/// Converts visible content bounds to surface bounds only when the native window uses a
/// transparent client frame. Minimum sizes stay in visible content coordinates.
pub(crate) fn client_frame_bounds(
    bounds: Bounds<Pixels>,
    decorations: WindowDecorations,
    requested_inset: Pixels,
    transparent_frame_supported: bool,
) -> (Bounds<Pixels>, Pixels) {
    let inset = if decorations == WindowDecorations::Client && transparent_frame_supported {
        requested_inset.max(px(0.0))
    } else {
        px(0.0)
    };
    (bounds.dilate(inset), inset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, size};

    #[test]
    fn client_frame_preserves_initial_visible_content_and_center() {
        let content = Bounds::new(point(px(100.), px(200.)), size(px(900.), px(580.)));
        let (surface, inset) =
            client_frame_bounds(content, WindowDecorations::Client, px(24.), true);
        assert_eq!(surface.origin, point(px(76.), px(176.)));
        assert_eq!(surface.size, size(px(948.), px(628.)));
        assert_eq!(surface.inset(inset), content);
    }

    #[test]
    fn opaque_client_and_server_frames_reserve_no_shadow_gutter() {
        let content = Bounds::new(point(px(100.), px(200.)), size(px(900.), px(580.)));
        for (decorations, supported) in [
            (WindowDecorations::Client, false),
            (WindowDecorations::Server, true),
        ] {
            let (surface, inset) = client_frame_bounds(content, decorations, px(24.), supported);
            assert_eq!(surface, content);
            assert_eq!(inset, px(0.));
        }
    }
}
