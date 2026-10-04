//! Private protocol owners, with no connection to a desktop compositor.
use super::*;
use std::os::unix::net::UnixStream;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use wayland_client::protocol::{
    wl_data_device as client_device, wl_data_device_manager as client_manager, wl_registry,
    wl_seat as client_seat,
};
use wayland_client::{Dispatch as ClientDispatch, QueueHandle};
use wayland_protocols::wp::primary_selection::zv1::client::{
    zwp_primary_selection_device_manager_v1 as client_primary_manager,
    zwp_primary_selection_device_v1 as client_primary_device,
};
use wayland_protocols::wp::primary_selection::zv1::server::{
    zwp_primary_selection_device_manager_v1 as primary_manager,
    zwp_primary_selection_device_v1 as primary_device,
    zwp_primary_selection_offer_v1 as primary_offer,
};
use wayland_server::protocol::{wl_data_device, wl_data_device_manager, wl_data_offer, wl_seat};
use wayland_server::{Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, New};

struct OwnerState {
    formats: Vec<String>,
    bytes: Vec<u8>,
    requests: Arc<Mutex<Vec<String>>>,
}
impl OwnerState {
    fn send(&self, mime: String, fd: OwnedFd) {
        self.requests.lock().unwrap().push(mime);
        let bytes = self.bytes.clone();
        std::thread::spawn(move || {
            let _ = File::from(fd).write_all(&bytes);
        });
    }
}
macro_rules! global {
    ($resource:ty) => {
        impl GlobalDispatch<$resource, ()> for OwnerState {
            fn bind(
                _: &mut Self,
                _: &DisplayHandle,
                _: &Client,
                resource: New<$resource>,
                _: &(),
                init: &mut DataInit<'_, Self>,
            ) {
                init.init(resource, ());
            }
        }
    };
}
global!(wl_seat::WlSeat);
global!(wl_data_device_manager::WlDataDeviceManager);
global!(primary_manager::ZwpPrimarySelectionDeviceManagerV1);
macro_rules! ignore_requests {
    ($resource:ty) => {
        impl Dispatch<$resource, ()> for OwnerState {
            fn request(
                _: &mut Self,
                _: &Client,
                _: &$resource,
                _: <$resource as wayland_server::Resource>::Request,
                _: &(),
                _: &DisplayHandle,
                _: &mut DataInit<'_, Self>,
            ) {
            }
        }
    };
}
ignore_requests!(wl_seat::WlSeat);
ignore_requests!(wl_data_device::WlDataDevice);
ignore_requests!(primary_device::ZwpPrimarySelectionDeviceV1);
impl Dispatch<wl_data_device_manager::WlDataDeviceManager, ()> for OwnerState {
    fn request(
        state: &mut Self,
        client: &Client,
        _: &wl_data_device_manager::WlDataDeviceManager,
        request: wl_data_device_manager::Request,
        _: &(),
        handle: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let wl_data_device_manager::Request::GetDataDevice { id, .. } = request {
            let device = init.init(id, ());
            let offer = client
                .create_resource::<wl_data_offer::WlDataOffer, (), Self>(handle, 3, ())
                .unwrap();
            device.data_offer(&offer);
            for mime in &state.formats {
                offer.offer(mime.clone());
            }
            device.selection(Some(&offer));
        }
    }
}
impl Dispatch<primary_manager::ZwpPrimarySelectionDeviceManagerV1, ()> for OwnerState {
    fn request(
        state: &mut Self,
        client: &Client,
        _: &primary_manager::ZwpPrimarySelectionDeviceManagerV1,
        request: primary_manager::Request,
        _: &(),
        handle: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let primary_manager::Request::GetDevice { id, .. } = request {
            let device = init.init(id, ());
            let offer = client
                .create_resource::<primary_offer::ZwpPrimarySelectionOfferV1, (), Self>(
                    handle,
                    1,
                    (),
                )
                .unwrap();
            device.data_offer(&offer);
            for mime in &state.formats {
                offer.offer(mime.clone());
            }
            device.selection(Some(&offer));
        }
    }
}
impl Dispatch<wl_data_offer::WlDataOffer, ()> for OwnerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &wl_data_offer::WlDataOffer,
        request: wl_data_offer::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let wl_data_offer::Request::Receive { mime_type, fd } = request {
            state.send(mime_type, fd);
        }
    }
}
impl Dispatch<primary_offer::ZwpPrimarySelectionOfferV1, ()> for OwnerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &primary_offer::ZwpPrimarySelectionOfferV1,
        request: primary_offer::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let primary_offer::Request::Receive { mime_type, fd } = request {
            state.send(mime_type, fd);
        }
    }
}

#[derive(Default)]
struct Reader {
    seat: Option<client_seat::WlSeat>,
    manager: Option<client_manager::WlDataDeviceManager>,
    primary_manager: Option<client_primary_manager::ZwpPrimarySelectionDeviceManagerV1>,
    clipboard: Option<DataOffer<WlDataOffer>>,
    primary: Option<DataOffer<ZwpPrimarySelectionOfferV1>>,
}
impl ClientDispatch<wl_registry::WlRegistry, ()> for Reader {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
        {
            match interface.as_str() {
                "wl_seat" => state.seat = Some(registry.bind(name, 1, qh, ())),
                "wl_data_device_manager" => state.manager = Some(registry.bind(name, 3, qh, ())),
                "zwp_primary_selection_device_manager_v1" => {
                    state.primary_manager = Some(registry.bind(name, 1, qh, ()))
                }
                _ => {}
            }
        }
    }
}
wayland_client::delegate_noop!(Reader: ignore client_seat::WlSeat);
wayland_client::delegate_noop!(Reader: ignore client_manager::WlDataDeviceManager);
wayland_client::delegate_noop!(Reader: ignore client_primary_manager::ZwpPrimarySelectionDeviceManagerV1);
impl ClientDispatch<client_device::WlDataDevice, ()> for Reader {
    fn event(
        state: &mut Self,
        _: &client_device::WlDataDevice,
        event: client_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let client_device::Event::DataOffer { id } = event {
            state.clipboard = Some(DataOffer::new(id));
        }
    }
    wayland_client::event_created_child!(Reader, client_device::WlDataDevice, [0 => (WlDataOffer, ())]);
}
impl ClientDispatch<client_primary_device::ZwpPrimarySelectionDeviceV1, ()> for Reader {
    fn event(
        state: &mut Self,
        _: &client_primary_device::ZwpPrimarySelectionDeviceV1,
        event: client_primary_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let client_primary_device::Event::DataOffer { offer } = event {
            state.primary = Some(DataOffer::new(offer));
        }
    }
    wayland_client::event_created_child!(Reader, client_primary_device::ZwpPrimarySelectionDeviceV1, [0 => (ZwpPrimarySelectionOfferV1, ())]);
}
impl ClientDispatch<WlDataOffer, ()> for Reader {
    fn event(
        state: &mut Self,
        _: &WlDataOffer,
        event: wayland_client::protocol::wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wayland_client::protocol::wl_data_offer::Event::Offer { mime_type } = event {
            state.clipboard.as_mut().unwrap().add_mime_type(mime_type);
        }
    }
}
impl ClientDispatch<ZwpPrimarySelectionOfferV1, ()> for Reader {
    fn event(
        state: &mut Self,
        _: &ZwpPrimarySelectionOfferV1,
        event: wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_offer_v1::Event::Offer { mime_type } = event { state.primary.as_mut().unwrap().add_mime_type(mime_type); }
    }
}

struct Owner {
    stopped: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    requests: Arc<Mutex<Vec<String>>>,
    connection: Connection,
    reader: Reader,
}
impl Owner {
    fn new(formats: Vec<&str>, bytes: Vec<u8>) -> Self {
        let (client_socket, server_socket) = UnixStream::pair().unwrap();
        let mut display = Display::<OwnerState>::new().unwrap();
        let mut handle = display.handle();
        handle.insert_client(server_socket, Arc::new(())).unwrap();
        handle.create_global::<OwnerState, wl_seat::WlSeat, _>(1, ());
        handle.create_global::<OwnerState, wl_data_device_manager::WlDataDeviceManager, _>(3, ());
        handle.create_global::<OwnerState, primary_manager::ZwpPrimarySelectionDeviceManagerV1, _>(
            1,
            (),
        );
        let stopped = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let formats = formats.into_iter().map(str::to_owned).collect();
        let thread = std::thread::spawn({
            let stopped = stopped.clone();
            let requests = requests.clone();
            move || {
                let mut state = OwnerState {
                    formats,
                    bytes,
                    requests,
                };
                while !stopped.load(Ordering::Relaxed) {
                    display.dispatch_clients(&mut state).unwrap();
                    display.flush_clients().unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
        });
        let connection = Connection::from_socket(client_socket).unwrap();
        let mut queue = connection.new_event_queue::<Reader>();
        connection.display().get_registry(&queue.handle(), ());
        let mut reader = Reader::default();
        queue.roundtrip(&mut reader).unwrap();
        reader.manager.as_ref().unwrap().get_data_device(
            reader.seat.as_ref().unwrap(),
            &queue.handle(),
            (),
        );
        reader.primary_manager.as_ref().unwrap().get_device(
            reader.seat.as_ref().unwrap(),
            &queue.handle(),
            (),
        );
        queue.roundtrip(&mut reader).unwrap();
        Self {
            stopped,
            thread: Some(thread),
            requests,
            connection,
            reader,
        }
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}

#[test]
fn external_text_only_protocol_owners_preserve_utf8_and_bounds() {
    for (formats, bytes, expected) in [
        (vec!["image/png"], b"image".to_vec(), None),
        (
            vec![URI_LIST_MIME_TYPE],
            b"file:///tmp/fixture\r\n".to_vec(),
            None,
        ),
        (
            vec![URI_LIST_MIME_TYPE, "image/png", "text/plain"],
            b"a\r\nb".to_vec(),
            Some("a\r\nb".to_owned()),
        ),
        (vec!["text/plain"], vec![0xff], None),
        (vec!["text/plain"], vec![b'x'; 1024], Some("x".repeat(1024))),
        (vec!["text/plain"], vec![b'x'; 1025], None),
    ] {
        let owner = Owner::new(formats, bytes);
        let event_loop = calloop::EventLoop::try_new().unwrap();
        let mut clipboard = Clipboard::new(owner.connection.clone(), event_loop.handle());
        clipboard.set_offer(owner.reader.clipboard.clone());
        clipboard.set_primary_offer(owner.reader.primary.clone());
        assert_eq!(
            clipboard.read_text(gpui::ClipboardSelection::Clipboard, 1024),
            expected
        );
        assert_eq!(
            clipboard.read_text(gpui::ClipboardSelection::Primary, 1024),
            expected
        );
        assert!(
            owner
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(|mime| TEXT_MIME_TYPES.contains(&mime.as_str()))
        );
    }
}

#[test]
fn selection_claim_without_press_preserves_retained_contents() {
    use super::super::serial::{SerialKind, SerialTracker};
    use gpui::{ClipboardSelection, ClipboardWriteError};
    let owner = Owner::new(vec!["text/plain"], b"external".to_vec());
    let event_loop = calloop::EventLoop::try_new().unwrap();
    let mut clipboard = Clipboard::new(owner.connection.clone(), event_loop.handle());
    clipboard.set(ClipboardItem::new_string("clipboard".into()));
    clipboard.set_primary(ClipboardItem::new_string("primary".into()));
    let mut serials = SerialTracker::new();
    // Focus and unrelated native serials supply no authority to claim either selection.
    serials.update(SerialKind::MouseEnter, 5);
    serials.update(SerialKind::DataDevice, 6);
    for selection in [ClipboardSelection::Clipboard, ClipboardSelection::Primary] {
        assert_eq!(
            clipboard.claim(
                selection,
                ClipboardItem::new_string("replacement".into()),
                serials.selection_serial()
            ),
            Err(ClipboardWriteError::Unavailable)
        );
        assert_eq!(
            clipboard
                .contents
                .as_ref()
                .unwrap()
                .item
                .bounded_text(1024)
                .as_deref(),
            Some("clipboard")
        );
        assert_eq!(
            clipboard
                .primary_contents
                .as_ref()
                .unwrap()
                .item
                .bounded_text(1024)
                .as_deref(),
            Some("primary")
        );
    }
    serials.update(SerialKind::KeyPress, 7);
    let (_, serial) = clipboard
        .claim(
            ClipboardSelection::Clipboard,
            ClipboardItem::new_string("claimed".into()),
            serials.selection_serial(),
        )
        .unwrap();
    assert_eq!(serial.as_raw(), 7);
    assert_eq!(
        clipboard
            .contents
            .as_ref()
            .unwrap()
            .item
            .bounded_text(1024)
            .as_deref(),
        Some("claimed")
    );
    serials.update(SerialKind::MousePress, 8);
    let (_, serial) = clipboard
        .claim(
            ClipboardSelection::Primary,
            ClipboardItem::new_string("claimed primary".into()),
            serials.selection_serial(),
        )
        .unwrap();
    assert_eq!(serial.as_raw(), 8);
    assert_eq!(
        clipboard
            .primary_contents
            .as_ref()
            .unwrap()
            .item
            .bounded_text(1024)
            .as_deref(),
        Some("claimed primary")
    );
}

#[test]
fn external_text_only_preserves_crlf_after_ordinary_read() {
    let owner = Owner::new(vec!["text/plain"], b"a\r\nb".to_vec());
    let event_loop = calloop::EventLoop::try_new().unwrap();
    let mut clipboard = Clipboard::new(owner.connection.clone(), event_loop.handle());
    clipboard.set_offer(owner.reader.clipboard.clone());
    clipboard.set_primary_offer(owner.reader.primary.clone());
    assert_eq!(clipboard.read().unwrap().text().as_deref(), Some("a\nb"));
    assert_eq!(
        clipboard.read_primary().unwrap().text().as_deref(),
        Some("a\nb")
    );
    for selection in [
        gpui::ClipboardSelection::Clipboard,
        gpui::ClipboardSelection::Primary,
    ] {
        assert_eq!(
            clipboard.read_text(selection, 1024).as_deref(),
            Some("a\r\nb")
        );
    }
}

#[test]
fn owned_text_only_selections_preserve_bytes_without_path_synthesis() {
    let owner = Owner::new(vec!["text/plain"], b"external".to_vec());
    let event_loop = calloop::EventLoop::try_new().unwrap();
    let mut clipboard = Clipboard::new(owner.connection.clone(), event_loop.handle());
    let mut offer = owner.reader.clipboard.clone().unwrap();
    offer.add_mime_type(clipboard.self_mime());
    clipboard.set_offer(Some(offer));
    let mut offer = owner.reader.primary.clone().unwrap();
    offer.add_mime_type(clipboard.self_mime());
    clipboard.set_primary_offer(Some(offer));
    for text in ["", "a\r\nb", "é"] {
        clipboard.set(ClipboardItem::new_string(text.into()));
        clipboard.set_primary(ClipboardItem::new_string(text.into()));
        for selection in [
            gpui::ClipboardSelection::Clipboard,
            gpui::ClipboardSelection::Primary,
        ] {
            assert_eq!(
                clipboard.read_text(selection, text.len()).as_deref(),
                Some(text)
            );
            if !text.is_empty() {
                assert_eq!(clipboard.read_text(selection, text.len() - 1), None);
            }
        }
    }
    let item = ClipboardItem {
        entries: vec![ClipboardEntry::ExternalPaths(ExternalPaths(
            vec!["/tmp/fixture".into()].into(),
        ))],
    };
    clipboard.set(item.clone());
    clipboard.set_primary(item);
    for selection in [
        gpui::ClipboardSelection::Clipboard,
        gpui::ClipboardSelection::Primary,
    ] {
        assert_eq!(clipboard.read_text(selection, 1024), None);
    }
    assert!(owner.requests.lock().unwrap().is_empty());
}
