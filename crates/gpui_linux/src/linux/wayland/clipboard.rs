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
use filedescriptor::{FileDescriptor, Pipe};
use strum::IntoEnumIterator;
use wayland_client::{
    Connection, Proxy,
    backend::{InvalidId, ObjectId, WaylandError},
    protocol::wl_data_offer::{self, WlDataOffer},
};
use wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_offer_v1::{
    self, ZwpPrimarySelectionOfferV1,
};

use crate::linux::{
    WaylandClientStatePtr,
    clipboard_formats::{
        ClipboardOffer, GNOME_COPIED_FILES_MIME_TYPE, HTML_MIME_TYPE, URI_LIST_MIME_TYPE,
        file_list_item, parse_gnome_copied_files, parse_uri_list,
    },
    clipboard_transfer::{CLIPBOARD_READ_TIMEOUT, ClipboardTransfer, PreparedRead, TransferError},
    platform::read_fd_with_budget,
};
use gpui::{
    ClipboardEntry, ClipboardItem, ClipboardReadError, ClipboardSelection, ExternalPaths, Image,
    ImageFormat, hash,
};

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
    /// Asks the offer's source to write `mime_type` to `fd`, failing once the offer is destroyed.
    fn receive_data(&self, mime_type: String, fd: BorrowedFd<'_>) -> Result<(), InvalidId>;
}

// The generated `receive` methods discard the failure for a destroyed offer, which would leave the
// pipe to end without contents.
impl ReceiveData for WlDataOffer {
    fn receive_data(&self, mime_type: String, fd: BorrowedFd<'_>) -> Result<(), InvalidId> {
        self.send_request(wl_data_offer::Request::Receive { mime_type, fd })
    }
}

impl ReceiveData for ZwpPrimarySelectionOfferV1 {
    fn receive_data(&self, mime_type: String, fd: BorrowedFd<'_>) -> Result<(), InvalidId> {
        self.send_request(zwp_primary_selection_offer_v1::Request::Receive { mime_type, fd })
    }
}

#[derive(Clone, Debug)]
/// Wrapper for `WlDataOffer` and `ZwpPrimarySelectionOfferV1`, used to help track mime types.
pub(crate) struct DataOffer<T: ReceiveData> {
    pub inner: T,
    mime_types: Vec<String>,
}

/// A representation requested from an offer, with the pipe its source writes to.
struct Requested {
    mime_type: &'static str,
    pipe: FileDescriptor,
}

/// One read's transfers from an offer, within a single budget.
struct OfferTransfer {
    connection: Connection,
    budget: ClipboardTransfer,
    /// Requested while the read was prepared, when the offer was still the selection.
    requested: Option<Requested>,
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

    /// Requests a representation before the offer can be destroyed, so a transfer that runs
    /// later on another thread still receives it.
    fn transfer(
        &self,
        connection: &Connection,
        mime_type: &'static str,
        budget: ClipboardTransfer,
    ) -> Result<OfferTransfer, ClipboardReadError> {
        Ok(OfferTransfer {
            requested: Some(self.request(connection, mime_type)?),
            connection: connection.clone(),
            budget,
        })
    }

    fn request(
        &self,
        connection: &Connection,
        mime_type: &'static str,
    ) -> Result<Requested, ClipboardReadError> {
        let pipe = Pipe::new().map_err(|_| ClipboardReadError::Unavailable)?;
        self.inner
            .receive_data(mime_type.to_owned(), unsafe {
                BorrowedFd::borrow_raw(pipe.write.as_raw_fd())
            })
            .map_err(|InvalidId| {
                log::debug!("clipboard offer was destroyed before its transfer");
                ClipboardReadError::Unavailable
            })?;
        drop(pipe.write);
        // The event loop flushes a request that does not fit in the socket now.
        if let Err(error) = connection.flush()
            && !matches!(&error, WaylandError::Io(error) if error.kind() == ErrorKind::WouldBlock)
        {
            return Err(ClipboardReadError::Unavailable);
        }
        Ok(Requested {
            mime_type,
            pipe: pipe.read,
        })
    }

    fn read_bytes(
        &self,
        transfer: &mut OfferTransfer,
        mime_type: &'static str,
    ) -> Result<Vec<u8>, ClipboardReadError> {
        transfer.budget.remaining_time()?;
        let requested = match transfer.requested.take() {
            Some(requested) if requested.mime_type == mime_type => requested,
            _ => self.request(&transfer.connection, mime_type)?,
        };
        read_fd_with_budget(requested.pipe, &mut transfer.budget).map_err(|error| {
            log::error!("clipboard transfer failed");
            match error.downcast::<TransferError>() {
                Ok(error) => error.into(),
                Err(_) => ClipboardReadError::Unavailable,
            }
        })
    }

    /// The most preferred text mime type this offer contains.
    fn text_mime_type(&self) -> Option<&'static str> {
        TEXT_MIME_TYPES
            .into_iter()
            .find(|mime_type| self.has_mime_type(mime_type))
    }

    fn file_list_mime_type(&self) -> Option<&'static str> {
        [URI_LIST_MIME_TYPE, GNOME_COPIED_FILES_MIME_TYPE]
            .into_iter()
            .find(|mime_type| self.has_mime_type(mime_type))
    }

    fn image_format(&self) -> Option<ImageFormat> {
        ImageFormat::iter().find(|format| self.has_mime_type(format.mime_type()))
    }

    /// The representation an item read requests first: a file list, then text, then an image.
    fn item_mime_type(&self) -> Option<&'static str> {
        self.file_list_mime_type()
            .or_else(|| self.text_mime_type())
            .or_else(|| self.image_format().map(ImageFormat::mime_type))
    }

    fn read_string(
        &self,
        transfer: &mut OfferTransfer,
    ) -> Result<Option<String>, ClipboardReadError> {
        // Ordinary Paste retains its existing line-ending normalization.
        self.read_string_exact(transfer)
            .map(|text| text.map(|text| text.replace("\r\n", "\n")))
    }

    fn read_string_exact(
        &self,
        transfer: &mut OfferTransfer,
    ) -> Result<Option<String>, ClipboardReadError> {
        let Some(mime_type) = self.text_mime_type() else {
            return Ok(None);
        };
        let bytes = self.read_bytes(transfer, mime_type)?;
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| ClipboardReadError::UnsupportedContent)
    }

    fn read_file_paths(
        &self,
        transfer: &mut OfferTransfer,
    ) -> Result<Option<ExternalPaths>, ClipboardReadError> {
        let Some(mime_type) = self.file_list_mime_type() else {
            return Ok(None);
        };
        let bytes = self.read_bytes(transfer, mime_type)?;
        Ok(if mime_type == URI_LIST_MIME_TYPE {
            parse_uri_list(&bytes)
        } else {
            parse_gnome_copied_files(&bytes)
        })
    }

    /// Reads the offer as a file list, then text, then an image, within one transfer budget.
    fn read_item(
        &self,
        transfer: &mut OfferTransfer,
    ) -> Result<Option<ClipboardItem>, ClipboardReadError> {
        if let Some(paths) = self.read_file_paths(transfer)? {
            // The file list stands without its text alternate.
            let text = self.read_string(transfer).ok().flatten();
            return Ok(Some(file_list_item(paths, text)));
        }
        match self.read_string(transfer) {
            Ok(Some(text)) => Ok(Some(ClipboardItem::new_string(text))),
            Ok(None) | Err(ClipboardReadError::UnsupportedContent) => self.read_image(transfer),
            Err(error) => Err(error),
        }
    }

    fn read_image(
        &self,
        transfer: &mut OfferTransfer,
    ) -> Result<Option<ClipboardItem>, ClipboardReadError> {
        let Some(format) = self.image_format() else {
            return Ok(None);
        };
        let bytes = self.read_bytes(transfer, format.mime_type())?;
        let id = hash(&bytes);
        Ok(Some(ClipboardItem {
            entries: vec![ClipboardEntry::Image(Image { format, bytes, id })],
        }))
    }
}

enum SelectionReadRef<'a> {
    Clipboard(SelectionRead<'a, WlDataOffer>),
    Primary(SelectionRead<'a, ZwpPrimarySelectionOfferV1>),
}

/// The selection a read targets, with the offer and contents it is answered from.
struct SelectionRead<'a, T: ReceiveData> {
    offer: Option<&'a DataOffer<T>>,
    owned: Option<&'a OwnedSelection>,
    cached: Option<&'a ClipboardItem>,
}

impl<T: ReceiveData + Proxy + Send + 'static> SelectionRead<'_, T> {
    fn prepare_item(
        self,
        self_mime: &str,
        connection: &Connection,
        transfer: ClipboardTransfer,
    ) -> PreparedRead<Result<Option<ClipboardItem>, ClipboardReadError>> {
        let Some(offer) = self.offer else {
            return PreparedRead::Ready(Ok(None));
        };
        if let Some(cached) = self.cached {
            return PreparedRead::Ready(Ok(Some(cached.clone())));
        }
        if offer.has_mime_type(self_mime) {
            return PreparedRead::Ready(Ok(self.owned.map(|owned| owned.item.clone())));
        }
        let Some(mime_type) = offer.item_mime_type() else {
            return PreparedRead::Ready(Ok(None));
        };
        let mut transfer = match offer.transfer(connection, mime_type, transfer) {
            Ok(transfer) => transfer,
            Err(error) => return PreparedRead::Ready(Err(error)),
        };
        let offer = offer.clone();
        PreparedRead::transfer(move || offer.read_item(&mut transfer))
    }

    fn prepare_text(
        self,
        self_mime: &str,
        connection: &Connection,
        max_bytes: usize,
        mut transfer: ClipboardTransfer,
    ) -> PreparedRead<Result<Option<String>, ClipboardReadError>> {
        let Some(offer) = self.offer else {
            return PreparedRead::Ready(Ok(None));
        };
        if offer.has_mime_type(self_mime) {
            return PreparedRead::Ready(
                self.owned
                    .map_or(Ok(None), |owned| owned.item.bounded_text(max_bytes)),
            );
        }
        let Some(mime_type) = offer.text_mime_type() else {
            return PreparedRead::Ready(Ok(None));
        };
        transfer.limit_bytes(max_bytes);
        let mut transfer = match offer.transfer(connection, mime_type, transfer) {
            Ok(transfer) => transfer,
            Err(error) => return PreparedRead::Ready(Err(error)),
        };
        let offer = offer.clone();
        PreparedRead::transfer(move || offer.read_string_exact(&mut transfer))
    }

    fn offer_id(&self) -> Option<ObjectId> {
        self.offer.map(|offer| offer.inner.id())
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

    fn selection<'a>(&'a self, selection: ClipboardSelection) -> SelectionReadRef<'a> {
        match selection {
            ClipboardSelection::Clipboard => SelectionReadRef::Clipboard(SelectionRead {
                offer: self.current_offer.as_ref(),
                owned: self.contents.as_ref(),
                cached: self.cached_read.as_ref(),
            }),
            ClipboardSelection::Primary => SelectionReadRef::Primary(SelectionRead {
                offer: self.current_primary_offer.as_ref(),
                owned: self.primary_contents.as_ref(),
                cached: self.cached_primary_read.as_ref(),
            }),
        }
    }

    pub fn read(&mut self) -> Option<ClipboardItem> {
        self.read_blocking(ClipboardSelection::Clipboard)
    }

    pub fn read_primary(&mut self) -> Option<ClipboardItem> {
        self.read_blocking(ClipboardSelection::Primary)
    }

    fn read_blocking(&mut self, selection: ClipboardSelection) -> Option<ClipboardItem> {
        let offer = self.offer_id(selection)?;
        match self.prepare_read(selection) {
            PreparedRead::Ready(item) => item.ok().flatten(),
            PreparedRead::Transfer(transfer) => {
                let item = transfer().ok().flatten()?;
                self.retain_read(selection, &offer, item.clone());
                Some(item)
            }
        }
    }

    /// Prepares a read of the selection's item. An external offer is asked for its first
    /// representation now, while it is the selection; the transfer runs on another thread.
    pub fn prepare_read(
        &self,
        selection: ClipboardSelection,
    ) -> PreparedRead<Result<Option<ClipboardItem>, ClipboardReadError>> {
        let transfer = ClipboardTransfer::new(CLIPBOARD_READ_TIMEOUT);
        match self.selection(selection) {
            SelectionReadRef::Clipboard(read) => {
                read.prepare_item(&self.self_mime, &self.connection, transfer)
            }
            SelectionReadRef::Primary(read) => {
                read.prepare_item(&self.self_mime, &self.connection, transfer)
            }
        }
    }

    /// Prepares a bounded exact text read. An external offer is asked for its text now, while it
    /// is the selection; the transfer runs on another thread.
    pub fn prepare_text_read(
        &self,
        selection: ClipboardSelection,
        max_bytes: usize,
    ) -> PreparedRead<Result<Option<String>, ClipboardReadError>> {
        self.prepare_text_read_within(
            selection,
            max_bytes,
            ClipboardTransfer::new(CLIPBOARD_READ_TIMEOUT),
        )
    }

    fn prepare_text_read_within(
        &self,
        selection: ClipboardSelection,
        max_bytes: usize,
        transfer: ClipboardTransfer,
    ) -> PreparedRead<Result<Option<String>, ClipboardReadError>> {
        let max_bytes = max_bytes.min(crate::linux::clipboard_transfer::MAX_CLIPBOARD_BYTES);
        match self.selection(selection) {
            SelectionReadRef::Clipboard(read) => {
                read.prepare_text(&self.self_mime, &self.connection, max_bytes, transfer)
            }
            SelectionReadRef::Primary(read) => {
                read.prepare_text(&self.self_mime, &self.connection, max_bytes, transfer)
            }
        }
    }

    /// Identifies the external offer a prepared read targets.
    pub fn offer_id(&self, selection: ClipboardSelection) -> Option<ObjectId> {
        match self.selection(selection) {
            SelectionReadRef::Clipboard(read) => read.offer_id(),
            SelectionReadRef::Primary(read) => read.offer_id(),
        }
    }

    /// Retains an external item for later reads while its offer is still the selection.
    pub fn retain_read(
        &mut self,
        selection: ClipboardSelection,
        offer: &ObjectId,
        item: ClipboardItem,
    ) {
        if self.offer_id(selection).as_ref() != Some(offer) {
            return;
        }
        match selection {
            ClipboardSelection::Clipboard => self.cached_read = Some(item),
            ClipboardSelection::Primary => self.cached_primary_read = Some(item),
        }
    }

    pub fn claim(
        &mut self,
        selection: ClipboardSelection,
        item: ClipboardItem,
        serial: Option<super::serial::SelectionSerial>,
    ) -> Result<(Vec<&'static str>, super::serial::SelectionSerial), gpui::ClipboardWriteError>
    {
        let serial = serial.ok_or(gpui::ClipboardWriteError::Unavailable)?;
        let mime_types = match selection {
            ClipboardSelection::Clipboard => self.set(item),
            ClipboardSelection::Primary => self.set_primary(item),
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
        fn receive_data(&self, _mime_type: String, _fd: BorrowedFd<'_>) -> Result<(), InvalidId> {
            Ok(())
        }
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
