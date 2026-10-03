use gpui::{Bounds, Pixels};
use x11rb::{
    connection::Connection,
    protocol::{
        shape::{self, ConnectionExt as _},
        xproto,
    },
};

/// Retains the device-pixel shape to avoid sending the same region on every paint.
#[derive(Default)]
pub(super) struct InputRegion {
    rectangles: Option<Vec<(i16, i16, u16, u16)>>,
}

impl InputRegion {
    pub(super) fn set(
        &mut self,
        connection: &impl Connection,
        window: xproto::Window,
        region: Option<&[Bounds<Pixels>]>,
        scale: f32,
    ) -> anyhow::Result<()> {
        let rectangles = region.map(|region| device_rectangles(region, scale));
        let key = rectangles.as_deref().map(rectangle_data);
        if key == self.rectangles {
            return Ok(());
        }
        match &rectangles {
            Some(rectangles) => connection
                .shape_rectangles(
                    shape::SO::SET,
                    shape::SK::INPUT,
                    xproto::ClipOrdering::UNSORTED,
                    window,
                    0,
                    0,
                    rectangles,
                )?
                .check()?,
            // A None pixmap restores the default input shape, including subsequent resizes.
            None => connection
                .shape_mask(shape::SO::SET, shape::SK::INPUT, window, 0, 0, x11rb::NONE)?
                .check()?,
        }
        connection.flush()?;
        self.rectangles = key;
        Ok(())
    }
}

fn rectangle_data(rectangles: &[xproto::Rectangle]) -> Vec<(i16, i16, u16, u16)> {
    rectangles
        .iter()
        .map(|r| (r.x, r.y, r.width, r.height))
        .collect()
}

fn device_rectangles(region: &[Bounds<Pixels>], scale: f32) -> Vec<xproto::Rectangle> {
    region
        .iter()
        .filter_map(|bounds| {
            if bounds.size.width <= gpui::px(0.0) || bounds.size.height <= gpui::px(0.0) {
                return None;
            }
            // Cover partial pixels at both ends, rather than truncating the logical width.
            let x = (f32::from(bounds.left()) * scale)
                .floor()
                .clamp(i16::MIN as f32, i16::MAX as f32);
            let y = (f32::from(bounds.top()) * scale)
                .floor()
                .clamp(i16::MIN as f32, i16::MAX as f32);
            let right = (f32::from(bounds.right()) * scale).ceil();
            let bottom = (f32::from(bounds.bottom()) * scale).ceil();
            let width = (right - x).clamp(0.0, u16::MAX as f32) as u16;
            let height = (bottom - y).clamp(0.0, u16::MAX as f32) as u16;
            (width > 0 && height > 0).then_some(xproto::Rectangle {
                x: x as i16,
                y: y as i16,
                width,
                height,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px, size};
    use x11rb::protocol::xproto::ConnectionExt as _;

    #[test]
    fn input_region_uses_device_pixels_and_covers_fractional_edges() {
        let rectangles = device_rectangles(
            &[
                Bounds::new(point(px(14.25), px(14.5)), size(px(900.5), px(580.25))),
                Bounds::new(point(px(1.), px(1.)), size(px(0.), px(0.))),
            ],
            1.5,
        );
        assert_eq!(rectangle_data(&rectangles), [(21, 21, 1352, 872)]);
    }

    #[test]
    #[ignore = "requires GPUI_TEST_X11_DISPLAY with a private X11 server"]
    fn native_input_region_restricts_and_restores_the_x11_shape() {
        // Only an explicitly supplied private display is permitted for this native check.
        let display = std::env::var("GPUI_TEST_X11_DISPLAY").expect("supply a private X11 display");
        assert_ne!(display, ":0", "the desktop display is not a test display");
        let (connection, screen) =
            x11rb::rust_connection::RustConnection::connect(Some(&display)).unwrap();
        let root = &connection.setup().roots[screen];
        let window = connection.generate_id().unwrap();
        connection
            .create_window(
                root.root_depth,
                window,
                root.root,
                0,
                0,
                948,
                628,
                0,
                xproto::WindowClass::INPUT_OUTPUT,
                root.root_visual,
                &xproto::CreateWindowAux::new(),
            )
            .unwrap()
            .check()
            .unwrap();
        let mut region = InputRegion::default();
        let bounds = Bounds::new(point(px(14.), px(14.)), size(px(920.), px(600.)));
        region
            .set(&connection, window, Some(&[bounds]), 1.0)
            .unwrap();
        let shape = connection
            .shape_get_rectangles(window, shape::SK::INPUT)
            .unwrap()
            .reply()
            .unwrap();
        assert_eq!(rectangle_data(&shape.rectangles), [(14, 14, 920, 600)]);
        region.set(&connection, window, Some(&[]), 1.0).unwrap();
        assert!(
            connection
                .shape_get_rectangles(window, shape::SK::INPUT)
                .unwrap()
                .reply()
                .unwrap()
                .rectangles
                .is_empty()
        );
        region.set(&connection, window, None, 1.0).unwrap();
        let shape = connection
            .shape_get_rectangles(window, shape::SK::INPUT)
            .unwrap()
            .reply()
            .unwrap();
        assert_eq!(rectangle_data(&shape.rectangles), [(0, 0, 948, 628)]);
        connection.destroy_window(window).unwrap().check().unwrap();
    }
}
