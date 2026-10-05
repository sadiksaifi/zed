//! Private protocol owners, with no connection to a desktop compositor.
use super::*;
use std::os::unix::net::UnixStream;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
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

/// When a protocol owner writes its contents after a receive request.
#[derive(Clone, Copy)]
enum Answer {
    Now,
    After(Duration),
    Never,
}

struct OwnerState {
    formats: Vec<String>,
    bytes: Vec<u8>,
    answer: Answer,
    requests: Arc<Mutex<Vec<String>>>,
    /// Kept open without data until the owner stops.
    stalled: Vec<OwnedFd>,
}
impl OwnerState {
    fn send(&mut self, mime: String, fd: OwnedFd) {
        self.requests.lock().unwrap().push(mime);
        let delay = match self.answer {
            Answer::Now => Duration::ZERO,
            Answer::After(delay) => delay,
            Answer::Never => return self.stalled.push(fd),
        };
        let bytes = self.bytes.clone();
        std::thread::spawn(move || {
            std::thread::sleep(delay);
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
        Self::answering(formats, bytes, Answer::Now)
    }

    fn answering(formats: Vec<&str>, bytes: Vec<u8>, answer: Answer) -> Self {
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
                    answer,
                    requests,
                    stalled: Vec::new(),
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
        (vec!["image/png"], b"image".to_vec(), Ok(None)),
        (
            vec![URI_LIST_MIME_TYPE],
            b"file:///tmp/fixture\r\n".to_vec(),
            Ok(None),
        ),
        (
            vec![URI_LIST_MIME_TYPE, "image/png", "text/plain"],
            b"a\r\nb".to_vec(),
            Ok(Some("a\r\nb".to_owned())),
        ),
        (
            vec!["text/plain"],
            vec![0xff],
            Err(ClipboardReadError::UnsupportedContent),
        ),
        (
            vec!["text/plain"],
            vec![b'x'; 1024],
            Ok(Some("x".repeat(1024))),
        ),
        (
            vec!["text/plain"],
            vec![b'x'; 1025],
            Err(ClipboardReadError::TooLarge),
        ),
    ] {
        let owner = Owner::new(formats, bytes);
        let event_loop = calloop::EventLoop::try_new().unwrap();
        let mut clipboard = Clipboard::new(owner.connection.clone(), event_loop.handle());
        clipboard.set_offer(owner.reader.clipboard.clone());
        clipboard.set_primary_offer(owner.reader.primary.clone());
        for selection in [ClipboardSelection::Clipboard, ClipboardSelection::Primary] {
            assert_eq!(
                finish(clipboard.prepare_text_read(selection, 1024)),
                expected
            );
        }
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
                .unwrap()
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
                .unwrap()
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
            .unwrap()
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
            .unwrap()
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
            finish(clipboard.prepare_text_read(selection, 1024)),
            Ok(Some("a\r\nb".to_owned()))
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
                finish(clipboard.prepare_text_read(selection, text.len())),
                Ok(Some(text.to_owned()))
            );
            if !text.is_empty() {
                assert_eq!(
                    finish(clipboard.prepare_text_read(selection, text.len() - 1)),
                    Err(ClipboardReadError::TooLarge)
                );
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
        assert_eq!(
            finish(clipboard.prepare_text_read(selection, 1024)),
            Ok(None)
        );
    }
    assert!(owner.requests.lock().unwrap().is_empty());
}

/// Completes a prepared read on the calling thread.
fn finish<T>(read: PreparedRead<T>) -> T {
    match read {
        PreparedRead::Ready(value) => value,
        PreparedRead::Transfer(transfer) => transfer(),
    }
}

fn external_clipboard(
    owner: &Owner,
) -> (
    Clipboard,
    calloop::EventLoop<'static, WaylandClientStatePtr>,
) {
    let event_loop = calloop::EventLoop::try_new().unwrap();
    let mut clipboard = Clipboard::new(owner.connection.clone(), event_loop.handle());
    clipboard.set_offer(owner.reader.clipboard.clone());
    clipboard.set_primary_offer(owner.reader.primary.clone());
    (clipboard, event_loop)
}

#[test]
fn slow_owner_reads_prepare_at_once_and_complete_on_another_thread() {
    let delay = Duration::from_millis(300);
    let owner = Owner::answering(vec!["text/plain"], b"a\r\nb".to_vec(), Answer::After(delay));
    let (clipboard, _event_loop) = external_clipboard(&owner);
    for selection in [ClipboardSelection::Clipboard, ClipboardSelection::Primary] {
        let started = Instant::now();
        let text = clipboard.prepare_text_read(selection, 1024);
        let item = clipboard.prepare_read(selection);
        assert!(started.elapsed() < delay, "preparing a read must not wait");
        let (PreparedRead::Transfer(text), PreparedRead::Transfer(item)) = (text, item) else {
            panic!("an external owner must answer through a transfer");
        };
        let transfers = std::thread::spawn(move || (text(), item()));
        assert_eq!(
            transfers.join().unwrap(),
            (
                Ok(Some("a\r\nb".to_owned())),
                Ok(Some(ClipboardItem::new_string("a\nb".to_owned())))
            )
        );
        assert!(started.elapsed() >= delay);
    }
}

#[test]
fn stalled_owner_reads_end_at_the_deadline_with_a_typed_failure() {
    let owner = Owner::answering(vec!["text/plain"], b"never".to_vec(), Answer::Never);
    let (clipboard, _event_loop) = external_clipboard(&owner);
    let started = Instant::now();
    let read = clipboard.prepare_text_read_within(
        ClipboardSelection::Clipboard,
        1024,
        ClipboardTransfer::new(Duration::from_millis(100)),
    );
    assert_eq!(finish(read), Err(ClipboardReadError::TimedOut));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn external_items_are_retained_only_while_their_offer_is_the_selection() {
    let owner = Owner::new(vec!["text/plain"], b"external".to_vec());
    let (mut clipboard, _event_loop) = external_clipboard(&owner);
    let selection = ClipboardSelection::Clipboard;
    let offer = clipboard.offer_id(selection).unwrap();
    let item = finish(clipboard.prepare_read(selection)).unwrap().unwrap();
    assert_eq!(item.text().as_deref(), Some("external"));
    clipboard.retain_read(selection, &offer, item.clone());
    assert!(matches!(
        clipboard.prepare_read(selection),
        PreparedRead::Ready(Ok(Some(ref cached))) if *cached == item
    ));
    assert_eq!(owner.requests.lock().unwrap().len(), 1);

    // An item read from one selection's offer cannot answer for the other selection.
    clipboard.retain_read(ClipboardSelection::Primary, &offer, item.clone());
    assert!(matches!(
        clipboard.prepare_read(ClipboardSelection::Primary),
        PreparedRead::Transfer(_)
    ));
    // A read that completes after its offer was replaced cannot answer for the new selection.
    clipboard.set_offer(None);
    clipboard.retain_read(selection, &offer, item);
    assert!(matches!(
        clipboard.prepare_read(selection),
        PreparedRead::Ready(Ok(None))
    ));
}

/// Destroys both selection offers the way the client does when a new offer replaces them.
fn destroy_offers(owner: &Owner, clipboard: &mut Clipboard) {
    owner.reader.clipboard.as_ref().unwrap().inner.destroy();
    owner.reader.primary.as_ref().unwrap().inner.destroy();
    clipboard.set_offer(None);
    clipboard.set_primary_offer(None);
}

#[test]
fn reads_prepared_before_their_offer_is_destroyed_still_complete() {
    let owner = Owner::new(vec!["text/plain"], b"external".to_vec());
    let (mut clipboard, _event_loop) = external_clipboard(&owner);
    let reads = [ClipboardSelection::Clipboard, ClipboardSelection::Primary].map(|selection| {
        (
            clipboard.prepare_text_read(selection, 1024),
            clipboard.prepare_read(selection),
        )
    });
    destroy_offers(&owner, &mut clipboard);
    for (text, item) in reads {
        assert_eq!(finish(text), Ok(Some("external".to_owned())));
        assert_eq!(
            finish(item),
            Ok(Some(ClipboardItem::new_string("external".to_owned())))
        );
    }
}

#[test]
fn reads_of_a_destroyed_offer_fail_instead_of_answering_empty() {
    let owner = Owner::new(vec!["text/plain", "image/png"], vec![0xff]);
    let (mut clipboard, _event_loop) = external_clipboard(&owner);
    // Invalid text sends the item read on to the image, after the offer is destroyed.
    let items = [ClipboardSelection::Clipboard, ClipboardSelection::Primary]
        .map(|selection| clipboard.prepare_read(selection));
    let offers = (owner.reader.clipboard.clone(), owner.reader.primary.clone());
    destroy_offers(&owner, &mut clipboard);
    for item in items {
        assert_eq!(finish(item), Err(ClipboardReadError::Unavailable));
    }

    // An offer destroyed while it is still the selection cannot answer either.
    clipboard.set_offer(offers.0);
    clipboard.set_primary_offer(offers.1);
    for selection in [ClipboardSelection::Clipboard, ClipboardSelection::Primary] {
        assert_eq!(
            finish(clipboard.prepare_text_read(selection, 1024)),
            Err(ClipboardReadError::Unavailable)
        );
        assert_eq!(
            finish(clipboard.prepare_read(selection)),
            Err(ClipboardReadError::Unavailable)
        );
    }
}
