use std::{
    fs::File,
    io::{ErrorKind, Write},
    os::fd::{AsRawFd, BorrowedFd, OwnedFd},
};

use calloop::{LoopHandle, PostAction};
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
        let mime_type = self.text_mime_type()?;
        let bytes = self.read_bytes(connection, mime_type, transfer)?;
        let text_content = match String::from_utf8(bytes) {
            Ok(content) => content,
            Err(_) => {
                log::error!("clipboard text conversion failed");
                return None;
            }
        };

        // Normalize the text to unix line endings, otherwise
        // copying from eg: firefox inserts a lot of blank
        // lines, and that is super annoying.
        Some(text_content.replace("\r\n", "\n"))
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

    pub fn send_bytes(&self, fd: OwnedFd, bytes: Vec<u8>) {
        let mut written = 0;
        self.loop_handle
            .insert_source(
                calloop::generic::Generic::new(
                    File::from(fd),
                    calloop::Interest::WRITE,
                    calloop::Mode::Level,
                ),
                move |_, file, _| {
                    let file = unsafe { file.get_mut() };
                    loop {
                        match file.write(&bytes[written..]) {
                            Ok(n) if written + n == bytes.len() => {
                                written += n;
                                break Ok(PostAction::Remove);
                            }
                            Ok(n) => written += n,
                            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                                break Ok(PostAction::Continue);
                            }
                            Err(_) => break Ok(PostAction::Remove),
                        }
                    }
                },
            )
            .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
