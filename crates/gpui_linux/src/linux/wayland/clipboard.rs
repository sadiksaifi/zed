use std::{
    cell::Cell,
    fs::File,
    io::{ErrorKind, Write},
    os::fd::{AsRawFd, BorrowedFd, OwnedFd},
    rc::Rc,
    time::{Duration, Instant},
};

use calloop::{
    LoopHandle, PostAction,
    timer::{TimeoutAction, Timer},
};
use filedescriptor::Pipe;
use strum::IntoEnumIterator;
use wayland_client::{Connection, protocol::wl_data_offer::WlDataOffer};
use wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1;

use crate::linux::{
    WaylandClientStatePtr,
    clipboard_formats::{
        ClipboardOffer, GNOME_COPIED_FILES_MIME_TYPE, HTML_MIME_TYPE, URI_LIST_MIME_TYPE,
        file_list_item, parse_gnome_copied_files, parse_uri_list,
    },
    clipboard_transfer::{CLIPBOARD_READ_TIMEOUT, ClipboardTransfer},
    platform::read_fd_with_budget,
};
use gpui::{ClipboardEntry, ClipboardItem, ExternalPaths, Image, ImageFormat, hash};

/// Text mime types offered to and accepted from other programs, in preference order.
///
/// `text/plain` names no charset, so it is the last choice. GPUI writes it as UTF-8 and accepts it
/// only when its contents are valid UTF-8.
pub(crate) const TEXT_MIME_TYPES: [&str; 3] =
    ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"];
pub(crate) const FILE_LIST_MIME_TYPE: &str = URI_LIST_MIME_TYPE;
const OUTGOING_TRANSFER_TIMEOUT: Duration = Duration::from_secs(4);

pub(crate) struct Clipboard {
    connection: Connection,
    loop_handle: LoopHandle<'static, WaylandClientStatePtr>,
    self_mime: String,

    // Internal clipboard
    contents: Option<OwnedSelection>,
    primary_contents: Option<OwnedSelection>,

    // External clipboard
    cached_read: Option<ClipboardItem>,
    current_offer: Option<DataOffer<WlDataOffer>>,
    cached_primary_read: Option<ClipboardItem>,
    current_primary_offer: Option<DataOffer<ZwpPrimarySelectionOfferV1>>,
}

/// A selection that GPUI owns, with the representations it offers to other programs.
struct OwnedSelection {
    item: ClipboardItem,
    offer: ClipboardOffer,
}

impl OwnedSelection {
    fn new(item: ClipboardItem) -> Self {
        let offer = ClipboardOffer::new(&item);
        Self { item, offer }
    }

    fn bytes_for(&self, mime_type: &str) -> Option<&[u8]> {
        let representation = match mime_type {
            HTML_MIME_TYPE => self.offer.html(),
            URI_LIST_MIME_TYPE => self.offer.uri_list(),
            GNOME_COPIED_FILES_MIME_TYPE => self.offer.gnome_copied_files(),
            _ if TEXT_MIME_TYPES.contains(&mime_type) => self.offer.text(),
            _ => None,
        };
        representation.map(str::as_bytes)
    }

    /// The mime types offered to other programs, in preference order.
    fn mime_types(&self) -> Vec<&'static str> {
        let mut mime_types = Vec::new();
        if self.offer.uri_list().is_some() {
            mime_types.push(URI_LIST_MIME_TYPE);
        }
        if self.offer.gnome_copied_files().is_some() {
            mime_types.push(GNOME_COPIED_FILES_MIME_TYPE);
        }
        if self.offer.html().is_some() {
            mime_types.push(HTML_MIME_TYPE);
        }
        if self.offer.text().is_some() {
            mime_types.extend(TEXT_MIME_TYPES);
        }
        mime_types
    }
}

pub(crate) trait ReceiveData {
    fn receive_data(&self, mime_type: String, fd: BorrowedFd<'_>);
}

impl ReceiveData for WlDataOffer {
    fn receive_data(&self, mime_type: String, fd: BorrowedFd<'_>) {
        self.receive(mime_type, fd);
    }
}

impl ReceiveData for ZwpPrimarySelectionOfferV1 {
    fn receive_data(&self, mime_type: String, fd: BorrowedFd<'_>) {
        self.receive(mime_type, fd);
    }
}

#[derive(Clone, Debug)]
/// Wrapper for `WlDataOffer` and `ZwpPrimarySelectionOfferV1`, used to help track mime types.
pub(crate) struct DataOffer<T: ReceiveData> {
    pub inner: T,
    mime_types: Vec<String>,
}

impl<T: ReceiveData> DataOffer<T> {
    pub fn new(offer: T) -> Self {
        Self {
            inner: offer,
            mime_types: Vec::new(),
        }
    }

    pub fn add_mime_type(&mut self, mime_type: String) {
        self.mime_types.push(mime_type)
    }

    fn has_mime_type(&self, mime_type: &str) -> bool {
        self.mime_types.iter().any(|t| t == mime_type)
    }

    fn read_bytes(
        &self,
        connection: &Connection,
        mime_type: &str,
        transfer: &mut ClipboardTransfer,
    ) -> Option<Vec<u8>> {
        transfer.remaining_time().ok()?;
        let pipe = Pipe::new().ok()?;
        self.inner.receive_data(mime_type.to_string(), unsafe {
            BorrowedFd::borrow_raw(pipe.write.as_raw_fd())
        });
        let fd = pipe.read;
        drop(pipe.write);

        connection.flush().ok()?;

        match read_fd_with_budget(fd, transfer) {
            Ok(bytes) => Some(bytes),
            Err(_) => {
                log::error!("clipboard transfer failed");
                None
            }
        }
    }

    /// The most preferred text mime type this offer contains.
    fn text_mime_type(&self) -> Option<&'static str> {
        TEXT_MIME_TYPES
            .into_iter()
            .find(|mime_type| self.has_mime_type(mime_type))
    }

    fn read_string(
        &self,
        connection: &Connection,
        transfer: &mut ClipboardTransfer,
    ) -> Option<String> {
        // Ordinary Paste retains its existing line-ending normalization.
        self.read_string_exact(connection, transfer)
            .map(|text| text.replace("\r\n", "\n"))
    }

    fn read_string_exact(
        &self,
        connection: &Connection,
        transfer: &mut ClipboardTransfer,
    ) -> Option<String> {
        let mime_type = self.text_mime_type()?;
        let bytes = self.read_bytes(connection, mime_type, transfer)?;
        String::from_utf8(bytes).ok()
    }

    fn read_text(&self, connection: &Connection, max_bytes: usize) -> Option<String> {
        let mut transfer = ClipboardTransfer::new(CLIPBOARD_READ_TIMEOUT);
        transfer.limit_bytes(max_bytes);
        self.read_string_exact(connection, &mut transfer)
    }

    fn read_file_paths(
        &self,
        connection: &Connection,
        transfer: &mut ClipboardTransfer,
    ) -> Option<ExternalPaths> {
        if self.has_mime_type(URI_LIST_MIME_TYPE) {
            let bytes = self.read_bytes(connection, URI_LIST_MIME_TYPE, transfer)?;
            parse_uri_list(&bytes)
        } else if self.has_mime_type(GNOME_COPIED_FILES_MIME_TYPE) {
            let bytes = self.read_bytes(connection, GNOME_COPIED_FILES_MIME_TYPE, transfer)?;
            parse_gnome_copied_files(&bytes)
        } else {
            None
        }
    }

    /// Reads the offer as a file list, then text, then an image.
    fn read_item(&self, connection: &Connection) -> Option<ClipboardItem> {
        let transfer = &mut ClipboardTransfer::new(CLIPBOARD_READ_TIMEOUT);
        if let Some(paths) = self.read_file_paths(connection, transfer) {
            return Some(file_list_item(
                paths,
                self.read_string(connection, transfer),
            ));
        }
        self.read_string(connection, transfer)
            .map(ClipboardItem::new_string)
            .or_else(|| self.read_image(connection, transfer))
    }

    fn read_image(
        &self,
        connection: &Connection,
        transfer: &mut ClipboardTransfer,
    ) -> Option<ClipboardItem> {
        for format in ImageFormat::iter() {
            let mime_type = format.mime_type();
            if !self.has_mime_type(mime_type) {
                continue;
            }

            if let Some(bytes) = self.read_bytes(connection, mime_type, transfer) {
                let id = hash(&bytes);
                return Some(ClipboardItem {
                    entries: vec![ClipboardEntry::Image(Image { format, bytes, id })],
                });
            }
        }
        None
    }
}

impl Clipboard {
    pub fn new(
        connection: Connection,
        loop_handle: LoopHandle<'static, WaylandClientStatePtr>,
    ) -> Self {
        Self {
            connection,
            loop_handle,
            self_mime: format!("pid/{}", std::process::id()),

            contents: None,
            primary_contents: None,

            cached_read: None,
            current_offer: None,
            cached_primary_read: None,
            current_primary_offer: None,
        }
    }

    /// Owns the clipboard contents and returns the mime types to offer for them.
    pub fn set(&mut self, item: ClipboardItem) -> Vec<&'static str> {
        let contents = OwnedSelection::new(item);
        let mime_types = contents.mime_types();
        self.contents = Some(contents);
        mime_types
    }

    /// Owns the primary selection contents and returns the mime types to offer for them.
    pub fn set_primary(&mut self, item: ClipboardItem) -> Vec<&'static str> {
        let contents = OwnedSelection::new(item);
        let mime_types = contents.mime_types();
        self.primary_contents = Some(contents);
        mime_types
    }

    pub fn set_offer(&mut self, data_offer: Option<DataOffer<WlDataOffer>>) {
        self.cached_read = None;
        self.current_offer = data_offer;
    }

    pub fn set_primary_offer(&mut self, data_offer: Option<DataOffer<ZwpPrimarySelectionOfferV1>>) {
        self.cached_primary_read = None;
        self.current_primary_offer = data_offer;
    }

    pub fn self_mime(&self) -> String {
        self.self_mime.clone()
    }

    pub fn send(&self, mime_type: String, fd: OwnedFd) {
        if let Some(bytes) = self
            .contents
            .as_ref()
            .and_then(|contents| contents.bytes_for(&mime_type))
        {
            self.send_bytes(fd, bytes.to_owned());
        }
    }

    pub fn send_primary(&self, mime_type: String, fd: OwnedFd) {
        if let Some(bytes) = self
            .primary_contents
            .as_ref()
            .and_then(|contents| contents.bytes_for(&mime_type))
        {
            self.send_bytes(fd, bytes.to_owned());
        }
    }

    pub fn read(&mut self) -> Option<ClipboardItem> {
        let offer = self.current_offer.as_ref()?;
        if let Some(cached) = self.cached_read.clone() {
            return Some(cached);
        }

        if offer.has_mime_type(&self.self_mime) {
            return self.contents.as_ref().map(|contents| contents.item.clone());
        }

        let item = offer.read_item(&self.connection)?;

        self.cached_read = Some(item.clone());
        Some(item)
    }

    pub fn read_primary(&mut self) -> Option<ClipboardItem> {
        let offer = self.current_primary_offer.as_ref()?;
        if let Some(cached) = self.cached_primary_read.clone() {
            return Some(cached);
        }

        if offer.has_mime_type(&self.self_mime) {
            return self
                .primary_contents
                .as_ref()
                .map(|contents| contents.item.clone());
        }

        let item = offer.read_item(&self.connection)?;

        self.cached_primary_read = Some(item.clone());
        Some(item)
    }

    pub fn read_text(
        &self,
        selection: gpui::ClipboardSelection,
        max_bytes: usize,
    ) -> Option<String> {
        let max_bytes = max_bytes.min(crate::linux::clipboard_transfer::MAX_CLIPBOARD_BYTES);
        match selection {
            gpui::ClipboardSelection::Clipboard => {
                let offer = self.current_offer.as_ref()?;
                if offer.has_mime_type(&self.self_mime) {
                    return self.contents.as_ref()?.item.bounded_text(max_bytes);
                }
                offer.read_text(&self.connection, max_bytes)
            }
            gpui::ClipboardSelection::Primary => {
                let offer = self.current_primary_offer.as_ref()?;
                if offer.has_mime_type(&self.self_mime) {
                    return self.primary_contents.as_ref()?.item.bounded_text(max_bytes);
                }
                offer.read_text(&self.connection, max_bytes)
            }
        }
    }

    pub fn claim(
        &mut self,
        selection: gpui::ClipboardSelection,
        item: ClipboardItem,
        serial: Option<super::serial::SelectionSerial>,
    ) -> Result<(Vec<&'static str>, super::serial::SelectionSerial), gpui::ClipboardWriteError>
    {
        let serial = serial.ok_or(gpui::ClipboardWriteError::Unavailable)?;
        let mime_types = match selection {
            gpui::ClipboardSelection::Clipboard => self.set(item),
            gpui::ClipboardSelection::Primary => self.set_primary(item),
        };
        Ok((mime_types, serial))
    }

    pub fn send_bytes(&self, fd: OwnedFd, bytes: Vec<u8>) {
        if send_bytes(&self.loop_handle, fd, bytes, OUTGOING_TRANSFER_TIMEOUT).is_err() {
            log::error!("outgoing clipboard transfer failed");
        }
    }
}

fn send_bytes<Data: 'static>(
    loop_handle: &LoopHandle<'static, Data>,
    fd: OwnedFd,
    bytes: Vec<u8>,
    timeout: Duration,
) -> calloop::Result<()> {
    // Writable readiness cannot make a peer-provided blocking pipe safe to write.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags == -1
        || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
    {
        return Err(std::io::Error::last_os_error().into());
    }

    let deadline = Instant::now() + timeout;
    let timer_token = Rc::new(Cell::new(None));
    let mut written = 0;
    let writer_token = loop_handle
        .insert_source(
            calloop::generic::Generic::new(
                File::from(fd),
                calloop::Interest::WRITE,
                calloop::Mode::Level,
            ),
            {
                let loop_handle = loop_handle.downgrade();
                let timer_token = timer_token.clone();
                move |_, file, _| {
                    let action = if Instant::now() >= deadline {
                        log::warn!("outgoing clipboard transfer timed out");
                        PostAction::Remove
                    } else {
                        let file = unsafe { file.get_mut() };
                        let end = written + (bytes.len() - written).min(64 * 1024);
                        match file.write(&bytes[written..end]) {
                            Ok(0) => PostAction::Remove,
                            Ok(length) => {
                                written += length;
                                if written == bytes.len() {
                                    PostAction::Remove
                                } else {
                                    PostAction::Continue
                                }
                            }
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    ErrorKind::WouldBlock | ErrorKind::Interrupted
                                ) =>
                            {
                                PostAction::Continue
                            }
                            Err(_) => {
                                log::error!("outgoing clipboard transfer failed");
                                PostAction::Remove
                            }
                        }
                    };
                    if action == PostAction::Remove
                        && let Some(timer_token) = timer_token.take()
                        && let Some(loop_handle) = loop_handle.upgrade()
                    {
                        loop_handle.remove(timer_token);
                    }
                    Ok(action)
                }
            },
        )
        .map_err(|error| error.error)?;

    match loop_handle.insert_source(Timer::from_deadline(deadline), {
        let loop_handle = loop_handle.downgrade();
        move |_, _, _| {
            if let Some(loop_handle) = loop_handle.upgrade() {
                loop_handle.remove(writer_token);
            }
            log::warn!("outgoing clipboard transfer timed out");
            TimeoutAction::Drop
        }
    }) {
        Ok(token) => timer_token.set(Some(token)),
        Err(error) => {
            loop_handle.remove(writer_token);
            return Err(error.error);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Read, sync::mpsc};

    fn blocking_pipe() -> anyhow::Result<(File, OwnedFd)> {
        let pipe = Pipe::new()?;
        Ok((pipe.read.as_file()?, pipe.write.as_file()?.into()))
    }

    #[test]
    fn outgoing_blocking_pipe_stays_responsive_and_expires() -> anyhow::Result<()> {
        let (mut reader, writer) = blocking_pipe()?;
        let mut event_loop = calloop::EventLoop::<bool>::try_new()?;
        let timeout = Duration::from_millis(100);
        send_bytes(
            &event_loop.handle(),
            writer,
            vec![b'x'; 1024 * 1024],
            timeout,
        )?;
        event_loop
            .handle()
            .insert_source(
                calloop::timer::Timer::from_duration(Duration::from_millis(10)),
                |_, _, responsive| {
                    *responsive = true;
                    calloop::timer::TimeoutAction::Drop
                },
            )
            .map_err(|error| error.error)?;

        // Rescue a regressed blocking callback so this test fails instead of hanging the suite.
        let (watchdog_sender, watchdog_receiver) = mpsc::channel::<()>();
        let mut watchdog_reader = reader.try_clone()?;
        let watchdog = std::thread::spawn(move || -> std::io::Result<bool> {
            if watchdog_receiver.recv_timeout(Duration::from_secs(1))
                == Err(mpsc::RecvTimeoutError::Timeout)
            {
                watchdog_reader.read_to_end(&mut Vec::new())?;
                return Ok(true);
            }
            Ok(false)
        });
        let mut responsive = false;
        event_loop.dispatch(Some(Duration::from_millis(20)), &mut responsive)?;
        event_loop.dispatch(Some(Duration::from_millis(20)), &mut responsive)?;
        drop(watchdog_sender);
        assert!(!watchdog.join().expect("watchdog panicked")?);
        assert!(
            responsive,
            "another event source must run while the pipe is full"
        );

        event_loop.dispatch(Some(timeout), &mut responsive)?;
        filedescriptor::FileDescriptor::dup(&reader)?.set_non_blocking(true)?;
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        assert!(!bytes.is_empty());
        assert!(
            bytes.len() < 1024 * 1024,
            "the stalled transfer must be cancelled"
        );
        Ok(())
    }

    #[test]
    fn outgoing_partial_writes_complete_with_a_slow_reader() -> anyhow::Result<()> {
        let (mut reader, writer) = blocking_pipe()?;
        let expected: Vec<u8> = (0..128 * 1024 + 123)
            .map(|index| (index % 251) as u8)
            .collect();
        let mut event_loop = calloop::EventLoop::<()>::try_new()?;
        send_bytes(
            &event_loop.handle(),
            writer,
            expected.clone(),
            Duration::from_secs(2),
        )?;
        let reader = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
            let mut bytes = Vec::new();
            let mut buffer = [0; 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => return Ok(bytes),
                    Ok(length) => bytes.extend_from_slice(&buffer[..length]),
                    Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });

        let deadline = Instant::now() + Duration::from_secs(3);
        while !reader.is_finished() && Instant::now() < deadline {
            event_loop.dispatch(Some(Duration::from_millis(20)), &mut ())?;
        }
        let finished = reader.is_finished();
        drop(event_loop);
        let received = reader.join().expect("reader panicked")?;
        assert!(finished, "the completed transfer must close its descriptor");
        assert_eq!(received, expected);
        Ok(())
    }

    #[test]
    fn clipboard_html_offer_serves_exact_alternate_and_plain_fallback() {
        let selection = OwnedSelection::new(
            ClipboardEntry::String(
                gpui::ClipboardString::new("plain <text>".into())
                    .with_html("<pre>plain &lt;text&gt;</pre>".into()),
            )
            .into(),
        );
        assert!(selection.mime_types().contains(&HTML_MIME_TYPE));
        assert_eq!(
            selection.bytes_for(HTML_MIME_TYPE),
            Some(b"<pre>plain &lt;text&gt;</pre>".as_slice())
        );
        for text_type in TEXT_MIME_TYPES {
            assert_eq!(
                selection.bytes_for(text_type),
                Some(b"plain <text>".as_slice())
            );
        }
        let plain = OwnedSelection::new(ClipboardItem::new_string("plain".into()));
        assert!(!plain.mime_types().contains(&HTML_MIME_TYPE));
        assert_eq!(plain.bytes_for(HTML_MIME_TYPE), None);
    }

    struct FakeOffer;

    impl ReceiveData for FakeOffer {
        fn receive_data(&self, _mime_type: String, _fd: BorrowedFd<'_>) {}
    }

    fn offer(mime_types: &[&str]) -> DataOffer<FakeOffer> {
        let mut offer = DataOffer::new(FakeOffer);
        for mime_type in mime_types {
            offer.add_mime_type((*mime_type).to_string());
        }
        offer
    }

    #[test]
    fn text_offers_prefer_explicit_utf8_types_over_plain_text() {
        assert_eq!(
            offer(&["text/plain", "UTF8_STRING", "text/plain;charset=utf-8"]).text_mime_type(),
            Some("text/plain;charset=utf-8")
        );
        assert_eq!(
            offer(&["text/plain", "UTF8_STRING"]).text_mime_type(),
            Some("UTF8_STRING")
        );
        assert_eq!(
            offer(&["image/png", "text/plain"]).text_mime_type(),
            Some("text/plain")
        );
        assert_eq!(offer(&["image/png", "text/html"]).text_mime_type(), None);
    }
}

#[cfg(test)]
#[path = "clipboard_tests.rs"]
mod native_tests;
