use gpui::WindowDecorations;
use wayland_protocols::xdg::decoration::zv1::client::zxdg_toplevel_decoration_v1::{
    self, ZxdgToplevelDecorationV1,
};

/// The protocol requests a toplevel sends to its optional `zxdg_toplevel_decoration_v1` object.
pub(crate) trait DecorationObject {
    fn set_mode(&self, mode: zxdg_toplevel_decoration_v1::Mode);
    fn destroy(&self);
}

impl DecorationObject for ZxdgToplevelDecorationV1 {
    fn set_mode(&self, mode: zxdg_toplevel_decoration_v1::Mode) {
        ZxdgToplevelDecorationV1::set_mode(self, mode);
    }

    fn destroy(&self) {
        ZxdgToplevelDecorationV1::destroy(self);
    }
}

/// Owns a toplevel's xdg-decoration negotiation.
///
/// Under xdg-decoration-unstable-v1, a toplevel without a decoration object decorates itself,
/// and the compositor may force server-side decorations on any toplevel that has one. A window
/// that requests client decorations therefore never holds the object. Creating the object after
/// the surface has a buffer is a protocol error, so it is only created with the toplevel.
pub(crate) struct ToplevelDecoration<D: DecorationObject> {
    object: Option<D>,
}

impl<D: DecorationObject> ToplevelDecoration<D> {
    /// Creates the decoration object through `create` only when the window requests server
    /// decorations.
    pub(crate) fn new(requested: WindowDecorations, create: impl FnOnce() -> Option<D>) -> Self {
        let object = match requested {
            WindowDecorations::Server => create(),
            WindowDecorations::Client => None,
        };
        Self { object }
    }

    /// Applies a decoration request and returns the decorations the window draws until the
    /// compositor configures a mode.
    ///
    /// A client request destroys the decoration object, which removes any server decoration at
    /// the next commit.
    pub(crate) fn request(&mut self, decorations: WindowDecorations) -> WindowDecorations {
        match decorations {
            WindowDecorations::Client => {
                self.destroy();
                WindowDecorations::Client
            }
            WindowDecorations::Server => match &self.object {
                Some(object) => {
                    object.set_mode(zxdg_toplevel_decoration_v1::Mode::ServerSide);
                    WindowDecorations::Server
                }
                None => {
                    log::info!(
                        "Server-side decorations requested without a decoration object. Falling back to client-side decorations."
                    );
                    WindowDecorations::Client
                }
            },
        }
    }

    /// Destroys the decoration object. It must be destroyed before its toplevel.
    pub(crate) fn destroy(&mut self) {
        if let Some(object) = self.object.take() {
            object.destroy();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use gpui::WindowDecorations;
    use wayland_protocols::xdg::decoration::zv1::client::zxdg_toplevel_decoration_v1::Mode;

    use super::{DecorationObject, ToplevelDecoration};

    #[derive(Debug, PartialEq)]
    enum Request {
        Create,
        SetMode(Mode),
        Destroy,
    }

    #[derive(Clone, Default)]
    struct FakeDecoration {
        requests: Rc<RefCell<Vec<Request>>>,
    }

    impl DecorationObject for FakeDecoration {
        fn set_mode(&self, mode: Mode) {
            self.requests.borrow_mut().push(Request::SetMode(mode));
        }

        fn destroy(&self) {
            self.requests.borrow_mut().push(Request::Destroy);
        }
    }

    fn decoration(
        requested: WindowDecorations,
    ) -> (ToplevelDecoration<FakeDecoration>, FakeDecoration) {
        let fake = FakeDecoration::default();
        let decoration = ToplevelDecoration::new(requested, || {
            fake.requests.borrow_mut().push(Request::Create);
            Some(fake.clone())
        });
        (decoration, fake)
    }

    #[test]
    fn client_window_never_creates_a_decoration_object() {
        let (mut decoration, fake) = decoration(WindowDecorations::Client);
        assert_eq!(
            decoration.request(WindowDecorations::Client),
            WindowDecorations::Client
        );
        decoration.destroy();
        assert_eq!(*fake.requests.borrow(), []);
    }

    #[test]
    fn client_window_cannot_later_acquire_server_decorations() {
        let (mut decoration, fake) = decoration(WindowDecorations::Client);
        assert_eq!(
            decoration.request(WindowDecorations::Server),
            WindowDecorations::Client
        );
        assert_eq!(*fake.requests.borrow(), []);
    }

    #[test]
    fn server_window_requests_server_mode() {
        let (mut decoration, fake) = decoration(WindowDecorations::Server);
        assert_eq!(
            decoration.request(WindowDecorations::Server),
            WindowDecorations::Server
        );
        decoration.destroy();
        assert_eq!(
            *fake.requests.borrow(),
            [
                Request::Create,
                Request::SetMode(Mode::ServerSide),
                Request::Destroy
            ]
        );
    }

    #[test]
    fn client_request_destroys_the_decoration_object_once() {
        let (mut decoration, fake) = decoration(WindowDecorations::Server);
        assert_eq!(
            decoration.request(WindowDecorations::Client),
            WindowDecorations::Client
        );
        assert_eq!(
            decoration.request(WindowDecorations::Server),
            WindowDecorations::Client
        );
        decoration.destroy();
        assert_eq!(*fake.requests.borrow(), [Request::Create, Request::Destroy]);
    }
}
