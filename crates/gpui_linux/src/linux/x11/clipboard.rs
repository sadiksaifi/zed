/*
 * Copyright 2022 - 2025 Zed Industries, Inc.
 * License: Apache-2.0
 * See LICENSE-APACHE for complete license terms
 *
 * Adapted from the x11 submodule of the arboard project https://github.com/1Password/arboard
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 *
 * Copyright 2022 The Arboard contributors
 *
 * The project to which this file belongs is licensed under either of
 * the Apache 2.0 or the MIT license at the licensee's choice. The terms
 * and conditions of the chosen license apply to this file.
*/

// More info about using the clipboard on X11:
// https://tronche.com/gui/x/icccm/sec-2.html#s-2.6
// https://freedesktop.org/wiki/ClipboardManager/

use std::{
    cell::RefCell,
    collections::{HashMap, hash_map::Entry},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    thread_local,
    time::{Duration, Instant},
};

use parking_lot::{Condvar, Mutex, MutexGuard, RwLock};
use x11rb::{
    COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, NONE,
    connection::Connection,
    protocol::{
        Event,
        xproto::{
            Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Property,
            PropertyNotifyEvent, SELECTION_NOTIFY_EVENT, SelectionNotifyEvent,
            SelectionRequestEvent, Time, WindowClass,
        },
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

use gpui::{ClipboardItem, ClipboardReadError, ExternalPaths, Image, ImageFormat, hash};
use strum::IntoEnumIterator;

use crate::linux::clipboard_formats::{
    ClipboardOffer, GNOME_COPIED_FILES_MIME_TYPE, HTML_MIME_TYPE, URI_LIST_MIME_TYPE,
    file_list_item, parse_gnome_copied_files, parse_uri_list,
};

use crate::linux::clipboard_transfer::{
    CLIPBOARD_READ_TIMEOUT, ClipboardTransfer, PreparedRead, TransferError,
};

type Result<T, E = Error> = std::result::Result<T, E>;

static CLIPBOARD: Mutex<Option<GlobalClipboard>> = parking_lot::const_mutex(None);

x11rb::atom_manager! {
    pub Atoms: AtomCookies {
        CLIPBOARD,
        PRIMARY,
        SECONDARY,

        CLIPBOARD_MANAGER,
        SAVE_TARGETS,
        TARGETS,
        ATOM,
        INCR,

        UTF8_STRING,
        UTF8_MIME_0: b"text/plain;charset=utf-8",
        UTF8_MIME_1: b"text/plain;charset=UTF-8",
        // Text in ISO Latin-1 encoding
        // See: https://tronche.com/gui/x/icccm/sec-2.html#s-2.6.2
        STRING,
        // Text in unknown encoding
        // See: https://tronche.com/gui/x/icccm/sec-2.html#s-2.6.2
        TEXT,
        TEXT_MIME_UNKNOWN: b"text/plain",

        HTML: HTML_MIME_TYPE.as_bytes(),
        URI_LIST: URI_LIST_MIME_TYPE.as_bytes(),
        GNOME_COPIED_FILES: GNOME_COPIED_FILES_MIME_TYPE.as_bytes(),

        PNG__MIME: ImageFormat::mime_type(ImageFormat::Png ).as_bytes(),
        JPEG_MIME: ImageFormat::mime_type(ImageFormat::Jpeg).as_bytes(),
        WEBP_MIME: ImageFormat::mime_type(ImageFormat::Webp).as_bytes(),
        GIF__MIME: ImageFormat::mime_type(ImageFormat::Gif ).as_bytes(),
        SVG__MIME: ImageFormat::mime_type(ImageFormat::Svg ).as_bytes(),
        BMP__MIME: ImageFormat::mime_type(ImageFormat::Bmp ).as_bytes(),
        TIFF_MIME: ImageFormat::mime_type(ImageFormat::Tiff).as_bytes(),
        ICO__MIME: ImageFormat::mime_type(ImageFormat::Ico ).as_bytes(),
        PNM__MIME: ImageFormat::mime_type(ImageFormat::Pnm ).as_bytes(),
        // This is just some random name for the property on our window, into which
        // the clipboard owner writes the data we requested.
        ARBOARD_CLIPBOARD,
    }
}

thread_local! {
    static ATOM_NAME_CACHE: RefCell<HashMap<Atom, &'static str>> = Default::default();
}

#[derive(Debug, PartialEq, Eq)]
enum ManagerHandoverState {
    Idle,
    InProgress,
    Finished,
}

struct GlobalClipboard {
    inner: Arc<Inner>,

    /// Join handle to the thread which serves selection requests.
    server_handle: JoinHandle<()>,

    /// Live [`Clipboard`] handles. Background reads also share `inner`, so this count, not the
    /// strong count, decides when the last handle hands the contents to the clipboard manager.
    handles: usize,
}

struct XContext {
    conn: RustConnection,
    win_id: u32,
}

struct Inner {
    /// The context for the thread which serves clipboard read
    /// requests coming to us.
    server: XContext,
    atoms: Atoms,

    clipboard: Selection,
    primary: Selection,
    secondary: Selection,

    handover_state: Mutex<ManagerHandoverState>,
    handover_cv: Condvar,

    serve_stopped: AtomicBool,
}

impl XContext {
    fn new() -> Result<Self> {
        // create a new connection to an X11 server
        let (conn, screen_num): (RustConnection, _) =
            RustConnection::connect(None).map_err(|_| Error::ConnectionFailed)?;
        let screen = conn
            .setup()
            .roots
            .get(screen_num)
            .ok_or(Error::unknown("no screen found"))?;
        let win_id = conn.generate_id().map_err(into_unknown)?;

        let event_mask =
            // Just in case that some program reports SelectionNotify events
            // with XCB_EVENT_MASK_PROPERTY_CHANGE mask.
            EventMask::PROPERTY_CHANGE |
            // To receive DestroyNotify event and stop the message loop.
            EventMask::STRUCTURE_NOTIFY;
        // create the window
        conn.create_window(
            // copy as much as possible from the parent, because no other specific input is needed
            COPY_DEPTH_FROM_PARENT,
            win_id,
            screen.root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::COPY_FROM_PARENT,
            COPY_FROM_PARENT,
            // don't subscribe to any special events because we are requesting everything we need ourselves
            &CreateWindowAux::new().event_mask(event_mask),
        )
        .map_err(into_unknown)?;
        conn.flush().map_err(into_unknown)?;

        Ok(Self { conn, win_id })
    }
}

#[derive(Default)]
struct Selection {
    data: RwLock<Option<Vec<ClipboardData>>>,
    /// Explicit text for exact self-owned reads, independent of ordinary path alternates.
    explicit_text: RwLock<Option<String>>,
    /// Mutex around nothing to use with the below condvar.
    mutex: Mutex<()>,
    /// A condvar that is notified when the contents of this clipboard are changed.
    ///
    /// This is associated with `Self::mutex`.
    data_changed: Condvar,
}

#[derive(Debug, Clone)]
struct ClipboardData {
    bytes: Vec<u8>,

    /// The atom representing the format in which the data is encoded.
    format: Atom,
}

/// Where a read is answered from, decided once when the read starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadSource {
    /// The contents this client retains for a selection it owns.
    Retained,
    /// A transfer from the selection's owner, another client.
    Owner,
}

enum ReadSelNotifyResult {
    GotData(ClipboardData),
    IncrStarted,
    EventNotRecognized,
}

impl Inner {
    fn new() -> Result<Self> {
        let server = XContext::new()?;
        let atoms = Atoms::new(&server.conn)
            .map_err(into_unknown)?
            .reply()
            .map_err(into_unknown)?;

        Ok(Self {
            server,
            atoms,
            clipboard: Selection::default(),
            primary: Selection::default(),
            secondary: Selection::default(),
            handover_state: Mutex::new(ManagerHandoverState::Idle),
            handover_cv: Condvar::new(),
            serve_stopped: AtomicBool::new(false),
        })
    }

    fn write(
        &self,
        data: Vec<ClipboardData>,
        selection: ClipboardKind,
        wait: WaitConfig,
        explicit_text: Option<String>,
    ) -> Result<()> {
        if self.serve_stopped.load(Ordering::Relaxed) {
            return Err(Error::unknown(
                "The clipboard handler thread seems to have stopped. Logging messages may reveal the cause. (See the `log` crate.)",
            ));
        }

        let server_win = self.server.win_id;

        // ICCCM version 2, section 2.6.1.3 states that we should re-assert ownership whenever data
        // changes.
        self.server
            .conn
            .set_selection_owner(server_win, self.atom_of(selection), Time::CURRENT_TIME)
            .map_err(|_| Error::ClipboardOccupied)?;

        self.server.conn.flush().map_err(into_unknown)?;

        // Just setting the data, and the `serve_requests` will take care of the rest.
        let selection = self.selection_of(selection);
        let mut data_guard = selection.data.write();
        *data_guard = Some(data);
        *selection.explicit_text.write() = explicit_text;

        // Lock the mutex to both ensure that no wakers of `data_changed` can wake us between
        // dropping the `data_guard` and calling `wait[_for]` and that we don't we wake other
        // threads in that position.
        let mut guard = selection.mutex.lock();

        // Notify any existing waiting threads that we have changed the data in the selection.
        // It is important that the mutex is locked to prevent this notification getting lost.
        selection.data_changed.notify_all();

        match wait {
            WaitConfig::None => {}
            WaitConfig::Forever => {
                drop(data_guard);
                selection.data_changed.wait(&mut guard);
            }
            WaitConfig::Until(deadline) => {
                drop(data_guard);
                selection.data_changed.wait_until(&mut guard, deadline);
            }
        }

        Ok(())
    }

    /// `formats` must be a slice of atoms, where each atom represents a target format.
    /// The first format from `formats`, which the clipboard owner supports will be the
    /// format of the return value.
    fn read(
        &self,
        formats: &[Atom],
        selection: ClipboardKind,
        source: ReadSource,
        transfer: &mut ClipboardTransfer,
    ) -> Result<ClipboardData> {
        self.read_limited(formats, selection, source, transfer, None)
    }

    fn read_limited(
        &self,
        formats: &[Atom],
        selection: ClipboardKind,
        source: ReadSource,
        transfer: &mut ClipboardTransfer,
        payload_limit: Option<usize>,
    ) -> Result<ClipboardData> {
        match source {
            ReadSource::Retained => self.read_retained(formats, selection, transfer, payload_limit),
            ReadSource::Owner => self.read_from_owner(formats, selection, transfer, payload_limit),
        }
    }

    /// Answers from the contents this client retains, without contacting another client.
    fn read_retained(
        &self,
        formats: &[Atom],
        selection: ClipboardKind,
        transfer: &mut ClipboardTransfer,
        payload_limit: Option<usize>,
    ) -> Result<ClipboardData> {
        if let Some(limit) = payload_limit {
            let selection = self.selection_of(selection);
            let text = selection.explicit_text.read();
            let text = text.as_ref().ok_or(Error::ContentNotAvailable)?;
            transfer.limit_bytes(limit);
            transfer.receive(text.len()).map_err(Error::Transfer)?;
            return Ok(ClipboardData {
                bytes: text.as_bytes().to_vec(),
                format: self.atoms.UTF8_STRING,
            });
        }
        let data = self.selection_of(selection).data.read();
        if let Some(data_list) = &*data {
            for format in formats {
                if let Some(data) = data_list.iter().find(|data| data.format == *format) {
                    return Ok(data.clone());
                }
            }
        }
        Err(Error::ContentNotAvailable)
    }

    /// Transfers the selection from its current owner, which may take until the deadline.
    fn read_from_owner(
        &self,
        formats: &[Atom],
        selection: ClipboardKind,
        transfer: &mut ClipboardTransfer,
        payload_limit: Option<usize>,
    ) -> Result<ClipboardData> {
        let reader = XContext::new()?;

        let highest_precedence_format =
            match self.read_single(&reader, selection, self.atoms.TARGETS, transfer) {
                Err(error @ Error::Transfer(_)) => return Err(error),
                Err(err) => {
                    log::trace!("Clipboard TARGETS query failed with {err:?}");
                    None
                }
                Ok(ClipboardData { bytes, format }) => {
                    if format == self.atoms.ATOM {
                        let available_formats = Self::parse_formats(&bytes);
                        formats
                            .iter()
                            .find(|format| available_formats.contains(format))
                    } else {
                        log::trace!(
                            "Unexpected clipboard TARGETS format {}",
                            self.atom_name(format)
                        );
                        None
                    }
                }
            };

        if let Some(limit) = payload_limit {
            transfer.limit_bytes(limit);
        }
        if let Some(&format) = highest_precedence_format {
            let data = self.read_single(&reader, selection, format, transfer)?;
            if !formats.contains(&data.format) {
                // This shouldn't happen since the format is from the TARGETS list.
                log::trace!(
                    "Conversion to {} responded with {} which is not supported",
                    self.atom_name(format),
                    self.atom_name(data.format),
                );
                return Err(Error::ConversionFailure);
            }
            return Ok(data);
        }

        log::trace!("Falling back on attempting to convert clipboard to each format.");
        for format in formats {
            match self.read_single(&reader, selection, *format, transfer) {
                Ok(data) => {
                    if formats.contains(&data.format) {
                        return Ok(data);
                    } else {
                        log::trace!(
                            "Conversion to {} responded with {} which is not supported",
                            self.atom_name(*format),
                            self.atom_name(data.format),
                        );
                        continue;
                    }
                }
                Err(Error::ContentNotAvailable) => {
                    continue;
                }
                Err(e) => {
                    log::trace!("Conversion to {} failed: {}", self.atom_name(*format), e);
                    return Err(e);
                }
            }
        }
        log::trace!("All conversions to supported formats failed.");
        Err(Error::ContentNotAvailable)
    }

    fn parse_formats(bytes: &[u8]) -> Vec<Atom> {
        bytes
            .chunks_exact(4)
            .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect()
    }

    fn read_single(
        &self,
        reader: &XContext,
        selection: ClipboardKind,
        target_format: Atom,
        transfer: &mut ClipboardTransfer,
    ) -> Result<ClipboardData> {
        // Delete the property so that we can detect (using property notify)
        // when the selection owner receives our request.
        reader
            .conn
            .delete_property(reader.win_id, self.atoms.ARBOARD_CLIPBOARD)
            .map_err(into_unknown)?;

        // request to convert the clipboard selection to our data type(s)
        reader
            .conn
            .convert_selection(
                reader.win_id,
                self.atom_of(selection),
                target_format,
                self.atoms.ARBOARD_CLIPBOARD,
                Time::CURRENT_TIME,
            )
            .map_err(into_unknown)?;
        reader.conn.sync().map_err(into_unknown)?;

        log::trace!("Finished `convert_selection`");

        let mut incr_data: Vec<u8> = Vec::new();
        let mut using_incr = false;

        while transfer.remaining_time().is_ok() {
            let event = reader.conn.poll_for_event().map_err(into_unknown)?;
            let event = match event {
                Some(e) => e,
                None => {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
            };
            match event {
                // The first response after requesting a selection.
                Event::SelectionNotify(event) => {
                    log::trace!("Read SelectionNotify");
                    let result = self.handle_read_selection_notify(
                        reader,
                        target_format,
                        &mut using_incr,
                        transfer,
                        event,
                    )?;
                    match result {
                        ReadSelNotifyResult::GotData(data) => return Ok(data),
                        ReadSelNotifyResult::IncrStarted => (),
                        ReadSelNotifyResult::EventNotRecognized => (),
                    }
                }
                // If the previous SelectionNotify event specified that the data
                // will be sent in INCR segments, each segment is transferred in
                // a PropertyNotify event.
                Event::PropertyNotify(event) => {
                    let result = self.handle_read_property_notify(
                        reader,
                        target_format,
                        using_incr,
                        &mut incr_data,
                        transfer,
                        event,
                    )?;
                    if result {
                        return Ok(ClipboardData {
                            bytes: incr_data,
                            format: target_format,
                        });
                    }
                }
                _ => log::trace!(
                    "An unexpected event arrived while reading the clipboard: {:?}",
                    event
                ),
            }
        }
        Err(Error::Transfer(TransferError::TimedOut))
    }

    fn atom_of(&self, selection: ClipboardKind) -> Atom {
        match selection {
            ClipboardKind::Clipboard => self.atoms.CLIPBOARD,
            ClipboardKind::Primary => self.atoms.PRIMARY,
            ClipboardKind::Secondary => self.atoms.SECONDARY,
        }
    }

    fn selection_of(&self, selection: ClipboardKind) -> &Selection {
        match selection {
            ClipboardKind::Clipboard => &self.clipboard,
            ClipboardKind::Primary => &self.primary,
            ClipboardKind::Secondary => &self.secondary,
        }
    }

    fn kind_of(&self, atom: Atom) -> Option<ClipboardKind> {
        match atom {
            a if a == self.atoms.CLIPBOARD => Some(ClipboardKind::Clipboard),
            a if a == self.atoms.PRIMARY => Some(ClipboardKind::Primary),
            a if a == self.atoms.SECONDARY => Some(ClipboardKind::Secondary),
            _ => None,
        }
    }

    fn is_owner(&self, selection: ClipboardKind) -> Result<bool> {
        let current = self
            .server
            .conn
            .get_selection_owner(self.atom_of(selection))
            .map_err(into_unknown)?
            .reply()
            .map_err(into_unknown)?
            .owner;

        Ok(current == self.server.win_id)
    }

    fn query_atom_name(&self, atom: x11rb::protocol::xproto::Atom) -> Result<String> {
        String::from_utf8(
            self.server
                .conn
                .get_atom_name(atom)
                .map_err(into_unknown)?
                .reply()
                .map_err(into_unknown)?
                .name,
        )
        .map_err(into_unknown)
    }

    fn atom_name(&self, atom: x11rb::protocol::xproto::Atom) -> &'static str {
        ATOM_NAME_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            match cache.entry(atom) {
                Entry::Occupied(entry) => *entry.get(),
                Entry::Vacant(entry) => {
                    let s = self
                        .query_atom_name(atom)
                        .map(|s| Box::leak(s.into_boxed_str()) as &str)
                        .unwrap_or("FAILED-TO-GET-THE-ATOM-NAME");
                    entry.insert(s);
                    s
                }
            }
        })
    }

    fn handle_read_selection_notify(
        &self,
        reader: &XContext,
        target_format: u32,
        using_incr: &mut bool,
        transfer: &mut ClipboardTransfer,
        event: SelectionNotifyEvent,
    ) -> Result<ReadSelNotifyResult> {
        // The property being set to NONE means that the `convert_selection`
        // failed.

        // According to: https://tronche.com/gui/x/icccm/sec-2.html#s-2.4
        // the target must be set to the same as what we requested.
        if event.property == NONE || event.target != target_format {
            return Err(Error::ContentNotAvailable);
        }
        if self.kind_of(event.selection).is_none() {
            log::info!(
                "Received a SelectionNotify for a selection other than CLIPBOARD, PRIMARY or SECONDARY. This is unexpected."
            );
            return Ok(ReadSelNotifyResult::EventNotRecognized);
        }
        if *using_incr {
            log::warn!("Received a SelectionNotify while already expecting INCR segments.");
            return Ok(ReadSelNotifyResult::EventNotRecognized);
        }
        // Accept any property type. The property type will typically match the format type except
        // when it is `TARGETS` in which case it is `ATOM`. `ANY` is provided to handle the case
        // where the clipboard is not convertible to the requested format. In this case
        // `reply.type_` will have format information, but `bytes` will only be non-empty if `ANY`
        // is provided.
        let property_type = AtomEnum::ANY;
        // request the selection
        let reply = reader
            .conn
            .get_property(
                true,
                event.requestor,
                event.property,
                property_type,
                0,
                transfer.remaining_bytes().div_ceil(4) as u32,
            )
            .map_err(into_unknown)?
            .reply()
            .map_err(into_unknown)?;

        if reply.bytes_after != 0 || reply.value.len() > transfer.remaining_bytes() {
            return Err(Error::Transfer(TransferError::TooLarge));
        }
        // we found something
        if reply.type_ == self.atoms.INCR {
            *using_incr = true;
            if let Some(min_data_len) = reply.value32().and_then(|mut values| values.next())
                && min_data_len as usize > transfer.remaining_bytes()
            {
                return Err(Error::Transfer(TransferError::TooLarge));
            }
            Ok(ReadSelNotifyResult::IncrStarted)
        } else {
            transfer
                .receive(reply.value.len())
                .map_err(Error::Transfer)?;
            Ok(ReadSelNotifyResult::GotData(ClipboardData {
                bytes: reply.value,
                format: reply.type_,
            }))
        }
    }

    /// Returns Ok(true) when the incr_data is ready
    fn handle_read_property_notify(
        &self,
        reader: &XContext,
        target_format: u32,
        using_incr: bool,
        incr_data: &mut Vec<u8>,
        transfer: &mut ClipboardTransfer,
        event: PropertyNotifyEvent,
    ) -> Result<bool> {
        if event.atom != self.atoms.ARBOARD_CLIPBOARD || event.state != Property::NEW_VALUE {
            return Ok(false);
        }
        if !using_incr {
            // This must mean the selection owner received our request, and is
            // now preparing the data
            return Ok(false);
        }
        let reply = reader
            .conn
            .get_property(
                true,
                event.window,
                event.atom,
                if target_format == self.atoms.TARGETS {
                    self.atoms.ATOM
                } else {
                    target_format
                },
                0,
                transfer.remaining_bytes().div_ceil(4) as u32,
            )
            .map_err(into_unknown)?
            .reply()
            .map_err(into_unknown)?;

        if reply.bytes_after != 0 {
            return Err(Error::Transfer(TransferError::TooLarge));
        }
        transfer
            .receive(reply.value.len())
            .map_err(Error::Transfer)?;
        if reply.value_len == 0 {
            // This indicates that all the data has been sent.
            return Ok(true);
        }
        incr_data.extend(reply.value);

        // Not yet complete
        Ok(false)
    }

    fn handle_selection_request(&self, event: SelectionRequestEvent) -> Result<()> {
        let selection = match self.kind_of(event.selection) {
            Some(kind) => kind,
            None => {
                log::warn!(
                    "Received a selection request to a selection other than the CLIPBOARD, PRIMARY or SECONDARY. This is unexpected."
                );
                return Ok(());
            }
        };

        let success;
        // we are asked for a list of supported conversion targets
        if event.target == self.atoms.TARGETS {
            log::trace!(
                "Handling TARGETS, dst property is {}",
                self.atom_name(event.property)
            );
            let mut targets = Vec::with_capacity(10);
            targets.push(self.atoms.TARGETS);
            targets.push(self.atoms.SAVE_TARGETS);
            let data = self.selection_of(selection).data.read();
            if let Some(data_list) = &*data {
                for data in data_list {
                    targets.push(data.format);
                    if data.format == self.atoms.UTF8_STRING {
                        // When we are storing a UTF8 string,
                        // add all equivalent formats to the supported targets
                        targets.push(self.atoms.UTF8_MIME_0);
                        targets.push(self.atoms.UTF8_MIME_1);
                    }
                }
            }
            self.server
                .conn
                .change_property32(
                    PropMode::REPLACE,
                    event.requestor,
                    event.property,
                    // TODO: change to `AtomEnum::ATOM`
                    self.atoms.ATOM,
                    &targets,
                )
                .map_err(into_unknown)?;
            self.server.conn.flush().map_err(into_unknown)?;
            success = true;
        } else {
            log::trace!("Handling request for (probably) the clipboard contents.");
            let data = self.selection_of(selection).data.read();
            // TARGETS advertises the text/plain;charset=utf-8 names for UTF8_STRING data.
            let data_format = if event.target == self.atoms.UTF8_MIME_0
                || event.target == self.atoms.UTF8_MIME_1
            {
                self.atoms.UTF8_STRING
            } else {
                event.target
            };
            if let Some(data_list) = &*data {
                success = match data_list.iter().find(|d| d.format == data_format) {
                    Some(data) => {
                        self.server
                            .conn
                            .change_property8(
                                PropMode::REPLACE,
                                event.requestor,
                                event.property,
                                event.target,
                                &data.bytes,
                            )
                            .map_err(into_unknown)?;
                        self.server.conn.flush().map_err(into_unknown)?;
                        true
                    }
                    None => false,
                };
            } else {
                // This must mean that we lost ownership of the data
                // since the other side requested the selection.
                // Let's respond with the property set to none.
                success = false;
            }
        }
        // on failure we notify the requester of it
        let property = if success {
            event.property
        } else {
            AtomEnum::NONE.into()
        };
        // tell the requestor that we finished sending data
        self.server
            .conn
            .send_event(
                false,
                event.requestor,
                EventMask::NO_EVENT,
                SelectionNotifyEvent {
                    response_type: SELECTION_NOTIFY_EVENT,
                    sequence: event.sequence,
                    time: event.time,
                    requestor: event.requestor,
                    selection: event.selection,
                    target: event.target,
                    property,
                },
            )
            .map_err(into_unknown)?;

        self.server.conn.flush().map_err(into_unknown)
    }

    fn ask_clipboard_manager_to_request_our_data(&self) -> Result<()> {
        if self.server.win_id == 0 {
            // This shouldn't really ever happen but let's just check.
            log::error!("The server's window id was 0. This is unexpected");
            return Ok(());
        }

        if !self.is_owner(ClipboardKind::Clipboard)? {
            // We are not owning the clipboard, nothing to do.
            return Ok(());
        }
        if self
            .selection_of(ClipboardKind::Clipboard)
            .data
            .read()
            .is_none()
        {
            // If we don't have any data, there's nothing to do.
            return Ok(());
        }

        // It's important that we lock the state before sending the request
        // because we don't want the request server thread to lock the state
        // after the request but before we can lock it here.
        let mut handover_state = self.handover_state.lock();

        log::trace!("Sending the data to the clipboard manager");
        self.server
            .conn
            .convert_selection(
                self.server.win_id,
                self.atoms.CLIPBOARD_MANAGER,
                self.atoms.SAVE_TARGETS,
                self.atoms.ARBOARD_CLIPBOARD,
                Time::CURRENT_TIME,
            )
            .map_err(into_unknown)?;
        self.server.conn.flush().map_err(into_unknown)?;

        *handover_state = ManagerHandoverState::InProgress;
        let max_handover_duration = Duration::from_millis(100);

        // Note that we are using a parking_lot condvar here, which doesn't wake up
        // spuriously
        let result = self
            .handover_cv
            .wait_for(&mut handover_state, max_handover_duration);

        if *handover_state == ManagerHandoverState::Finished {
            return Ok(());
        }
        if result.timed_out() {
            log::warn!(
                "Could not hand the clipboard contents over to the clipboard manager. The request timed out."
            );
            return Ok(());
        }

        Err(Error::unknown(
            "The handover was not finished and the condvar didn't time out, yet the condvar wait ended. This should be unreachable.",
        ))
    }
}

fn serve_requests(context: Arc<Inner>) -> Result<(), Box<dyn std::error::Error>> {
    fn handover_finished(clip: &Arc<Inner>, mut handover_state: MutexGuard<ManagerHandoverState>) {
        log::trace!("Finishing clipboard manager handover.");
        *handover_state = ManagerHandoverState::Finished;

        // Not sure if unlocking the mutex is necessary here but better safe than sorry.
        drop(handover_state);

        clip.handover_cv.notify_all();
    }

    log::trace!("Started serve requests thread.");

    let _guard = gpui_util::defer(|| {
        context.serve_stopped.store(true, Ordering::Relaxed);
    });

    let mut written = false;
    let mut notified = false;

    loop {
        match context.server.conn.wait_for_event().map_err(into_unknown)? {
            Event::DestroyNotify(_) => {
                // This window is being destroyed.
                log::trace!("Clipboard server window is being destroyed x_x");
                return Ok(());
            }
            Event::SelectionClear(event) => {
                // TODO: check if this works
                // Someone else has new content in the clipboard, so it is
                // notifying us that we should delete our data now.
                log::trace!("Somebody else owns the clipboard now");

                if let Some(selection) = context.kind_of(event.selection) {
                    let selection = context.selection_of(selection);
                    let mut data_guard = selection.data.write();
                    *data_guard = None;
                    *selection.explicit_text.write() = None;

                    // It is important that this mutex is locked at the time of calling
                    // `notify_all` to prevent notifications getting lost in case the sleeping
                    // thread has unlocked its `data_guard` and is just about to sleep.
                    // It is also important that the RwLock is kept write-locked for the same
                    // reason.
                    let _guard = selection.mutex.lock();
                    selection.data_changed.notify_all();
                }
            }
            Event::SelectionRequest(event) => {
                log::trace!(
                    "SelectionRequest - selection is: {}, target is {}",
                    context.atom_name(event.selection),
                    context.atom_name(event.target),
                );
                // Someone is requesting the clipboard content from us.
                context
                    .handle_selection_request(event)
                    .map_err(into_unknown)?;

                // if we are in the progress of saving to the clipboard manager
                // make sure we save that we have finished writing
                let handover_state = context.handover_state.lock();
                if *handover_state == ManagerHandoverState::InProgress {
                    // Only set written, when the actual contents were written,
                    // not just a response to what TARGETS we have.
                    if event.target != context.atoms.TARGETS {
                        log::trace!("The contents were written to the clipboard manager.");
                        written = true;
                        // if we have written and notified, make sure to notify that we are done
                        if notified {
                            handover_finished(&context, handover_state);
                        }
                    }
                }
            }
            Event::SelectionNotify(event) => {
                // We've requested the clipboard content and this is the answer.
                // Considering that this thread is not responsible for reading
                // clipboard contents, this must come from the clipboard manager
                // signaling that the data was handed over successfully.
                if event.selection != context.atoms.CLIPBOARD_MANAGER {
                    log::error!(
                        "Received a `SelectionNotify` from a selection other than the CLIPBOARD_MANAGER. This is unexpected in this thread."
                    );
                    continue;
                }
                let handover_state = context.handover_state.lock();
                if *handover_state == ManagerHandoverState::InProgress {
                    // Note that some clipboard managers send a selection notify
                    // before even sending a request for the actual contents.
                    // (That's why we use the "notified" & "written" flags)
                    log::trace!(
                        "The clipboard manager indicated that it's done requesting the contents from us."
                    );
                    notified = true;

                    // One would think that we could also finish if the property
                    // here is set 0, because that indicates failure. However
                    // this is not the case; for example on KDE plasma 5.18, we
                    // immediately get a SelectionNotify with property set to 0,
                    // but following that, we also get a valid SelectionRequest
                    // from the clipboard manager.
                    if written {
                        handover_finished(&context, handover_state);
                    }
                }
            }
            _event => {
                // May be useful for debugging but nothing else really.
                //log::trace!("Received unwanted event: {:?}", event);
            }
        }
    }
}

pub(crate) struct Clipboard {
    inner: Arc<Inner>,
}

impl Clipboard {
    pub(crate) fn new() -> Result<Self> {
        let mut global_cb = CLIPBOARD.lock();
        if let Some(global_cb) = &mut *global_cb {
            global_cb.handles += 1;
            return Ok(Self {
                inner: Arc::clone(&global_cb.inner),
            });
        }
        // At this point we know that the clipboard does not exist.
        let ctx = Arc::new(Inner::new()?);
        let join_handle = std::thread::Builder::new()
            .name("Clipboard".to_owned())
            .spawn({
                let ctx = Arc::clone(&ctx);
                move || {
                    if let Err(error) = serve_requests(ctx) {
                        log::error!("Worker thread errored with: {}", error);
                    }
                }
            })
            .unwrap();
        *global_cb = Some(GlobalClipboard {
            inner: Arc::clone(&ctx),
            server_handle: join_handle,
            handles: 1,
        });
        Ok(Self { inner: ctx })
    }

    /// Owns the selection, offering the item's file list as `text/uri-list` and
    /// `x-special/gnome-copied-files`, and its text as `UTF8_STRING`.
    pub(crate) fn set_item(
        &self,
        item: &ClipboardItem,
        selection: ClipboardKind,
        wait: WaitConfig,
    ) -> Result<()> {
        let offer = ClipboardOffer::new(item);
        let atoms = &self.inner.atoms;
        let mut data = Vec::with_capacity(3);
        if let Some(uri_list) = offer.uri_list() {
            data.push(ClipboardData {
                bytes: uri_list.as_bytes().to_vec(),
                format: atoms.URI_LIST,
            });
        }
        if let Some(gnome_copied_files) = offer.gnome_copied_files() {
            data.push(ClipboardData {
                bytes: gnome_copied_files.as_bytes().to_vec(),
                format: atoms.GNOME_COPIED_FILES,
            });
        }
        if let Some(html) = offer.html() {
            data.push(ClipboardData {
                bytes: html.as_bytes().to_vec(),
                format: atoms.HTML,
            });
        }
        data.push(ClipboardData {
            bytes: offer.text().unwrap_or_default().as_bytes().to_vec(),
            format: atoms.UTF8_STRING,
        });
        self.inner.write(
            data,
            selection,
            wait,
            item.bounded_text(crate::linux::clipboard_transfer::MAX_CLIPBOARD_BYTES)
                .ok()
                .flatten(),
        )
    }

    #[allow(unused)]
    pub(crate) fn set_image(
        &self,
        image: Image,
        selection: ClipboardKind,
        wait: WaitConfig,
    ) -> Result<()> {
        let format = self.inner.image_format_atom(image.format);
        let data = vec![ClipboardData {
            bytes: image.bytes,
            format: self.inner.atoms.PNG__MIME,
        }];
        self.inner.write(data, selection, wait, None)
    }

    pub(crate) fn get_any(&self, selection: ClipboardKind) -> Result<ClipboardItem> {
        self.inner.get_any(
            selection,
            self.read_source(selection),
            &mut ClipboardTransfer::new(CLIPBOARD_READ_TIMEOUT),
        )
    }

    /// Prepares an item read. A selection this client owns answers at once; a transfer from
    /// another owner runs on another thread.
    pub(crate) fn prepare_read(
        &self,
        selection: ClipboardKind,
    ) -> PreparedRead<std::result::Result<Option<ClipboardItem>, ClipboardReadError>> {
        self.prepare_read_from(self.read_source(selection), selection)
    }

    fn prepare_read_from(
        &self,
        source: ReadSource,
        selection: ClipboardKind,
    ) -> PreparedRead<std::result::Result<Option<ClipboardItem>, ClipboardReadError>> {
        let mut transfer = ClipboardTransfer::new(CLIPBOARD_READ_TIMEOUT);
        self.prepare(source, move |inner, source| {
            read_result(inner.get_any(selection, source, &mut transfer))
        })
    }

    /// Prepares a bounded exact text read; a transfer from another owner runs on another
    /// thread.
    pub(crate) fn prepare_text_read(
        &self,
        selection: ClipboardKind,
        max_bytes: usize,
    ) -> PreparedRead<std::result::Result<Option<String>, ClipboardReadError>> {
        self.prepare_text_read_from(
            self.read_source(selection),
            selection,
            max_bytes,
            ClipboardTransfer::new(CLIPBOARD_READ_TIMEOUT),
        )
    }

    fn prepare_text_read_from(
        &self,
        source: ReadSource,
        selection: ClipboardKind,
        max_bytes: usize,
        mut transfer: ClipboardTransfer,
    ) -> PreparedRead<std::result::Result<Option<String>, ClipboardReadError>> {
        self.prepare(source, move |inner, source| {
            read_result(inner.get_text(selection, source, max_bytes, &mut transfer))
        })
    }

    /// Answers retained contents at once and moves a transfer to another thread. The source is
    /// fixed before this call, so another client claiming the selection afterwards cannot
    /// start a transfer on the preparing thread.
    fn prepare<T: Send + 'static>(
        &self,
        source: ReadSource,
        read: impl FnOnce(&Inner, ReadSource) -> T + Send + 'static,
    ) -> PreparedRead<T> {
        match source {
            ReadSource::Retained => PreparedRead::Ready(read(&self.inner, source)),
            ReadSource::Owner => {
                let inner = Arc::clone(&self.inner);
                PreparedRead::transfer(move || read(&inner, source))
            }
        }
    }

    fn read_source(&self, selection: ClipboardKind) -> ReadSource {
        if self.is_owner(selection) {
            ReadSource::Retained
        } else {
            ReadSource::Owner
        }
    }

    pub fn is_owner(&self, selection: ClipboardKind) -> bool {
        self.inner.is_owner(selection).unwrap_or(false)
    }
}

impl Inner {
    fn image_format_atom(&self, format: ImageFormat) -> Atom {
        match format {
            ImageFormat::Png => self.atoms.PNG__MIME,
            ImageFormat::Jpeg => self.atoms.JPEG_MIME,
            ImageFormat::Webp => self.atoms.WEBP_MIME,
            ImageFormat::Gif => self.atoms.GIF__MIME,
            ImageFormat::Svg => self.atoms.SVG__MIME,
            ImageFormat::Bmp => self.atoms.BMP__MIME,
            ImageFormat::Tiff => self.atoms.TIFF_MIME,
            ImageFormat::Ico => self.atoms.ICO__MIME,
            ImageFormat::Pnm => self.atoms.PNM__MIME,
        }
    }

    fn get_any(
        &self,
        selection: ClipboardKind,
        source: ReadSource,
        transfer: &mut ClipboardTransfer,
    ) -> Result<ClipboardItem> {
        let atoms = &self.atoms;
        let image_entries = ImageFormat::iter()
            .map(|format| (self.image_format_atom(format), format))
            .collect::<Vec<_>>();
        let file_list_format_atoms = [atoms.URI_LIST, atoms.GNOME_COPIED_FILES];

        let mut format_atoms = Vec::with_capacity(
            file_list_format_atoms.len() + image_entries.len() + self.text_format_atoms().len(),
        );
        format_atoms.extend_from_slice(&file_list_format_atoms);
        format_atoms.extend_from_slice(&self.text_format_atoms());
        format_atoms.extend(image_entries.iter().map(|(atom, _)| *atom));

        let mut result = self.read(&format_atoms, selection, source, transfer)?;

        log::trace!(
            "read clipboard as format {:?}",
            self.atom_name(result.format)
        );

        if file_list_format_atoms.contains(&result.format) {
            if let Some(paths) = self.file_paths(&result) {
                let text = self
                    .read(&self.text_format_atoms(), selection, source, transfer)
                    .and_then(|data| self.decode_text(data))
                    .ok();
                return Ok(file_list_item(paths, text));
            }
            // The file list names something other than local files, so read the rest.
            result = self.read(
                &format_atoms[file_list_format_atoms.len()..],
                selection,
                source,
                transfer,
            )?;
        }

        for (format_atom, image_format) in image_entries {
            if result.format == format_atom {
                let bytes = result.bytes;
                let id = hash(&bytes);
                return Ok(ClipboardItem::new_image(&Image {
                    id,
                    format: image_format,
                    bytes,
                }));
            }
        }

        Ok(ClipboardItem::new_string(self.decode_text(result)?))
    }

    /// Reads only text representations, preserving the owner's UTF-8 bytes.
    fn get_text(
        &self,
        selection: ClipboardKind,
        source: ReadSource,
        max_bytes: usize,
        transfer: &mut ClipboardTransfer,
    ) -> Result<String> {
        let atoms = &self.atoms;
        // STRING is Latin-1, so it cannot satisfy the exact UTF-8 contract.
        let formats = [
            atoms.UTF8_STRING,
            atoms.UTF8_MIME_0,
            atoms.UTF8_MIME_1,
            atoms.TEXT_MIME_UNKNOWN,
        ];
        let data = self.read_limited(&formats, selection, source, transfer, Some(max_bytes))?;
        String::from_utf8(data.bytes).map_err(|_| Error::ConversionFailure)
    }

    fn text_format_atoms(&self) -> [Atom; 6] {
        let atoms = &self.atoms;
        [
            atoms.UTF8_STRING,
            atoms.UTF8_MIME_0,
            atoms.UTF8_MIME_1,
            atoms.STRING,
            atoms.TEXT,
            atoms.TEXT_MIME_UNKNOWN,
        ]
    }

    fn decode_text(&self, data: ClipboardData) -> Result<String> {
        if data.format == self.atoms.STRING {
            Ok(data.bytes.into_iter().map(|c| c as char).collect())
        } else {
            String::from_utf8(data.bytes).map_err(|_| Error::ConversionFailure)
        }
    }

    fn file_paths(&self, file_list: &ClipboardData) -> Option<ExternalPaths> {
        if file_list.format == self.atoms.URI_LIST {
            parse_uri_list(&file_list.bytes)
        } else {
            parse_gnome_copied_files(&file_list.bytes)
        }
    }
}

/// Classifies a failed read without its contents. A selection without a requested
/// representation has no contents.
fn read_result<T>(result: Result<T>) -> std::result::Result<Option<T>, ClipboardReadError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(Error::ContentNotAvailable) => Ok(None),
        Err(Error::Transfer(error)) => Err(error.into()),
        Err(Error::ConversionFailure) => Err(ClipboardReadError::UnsupportedContent),
        Err(_) => Err(ClipboardReadError::Unavailable),
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        // We start with locking the global guard to prevent race
        // conditions below.
        let mut global_cb = CLIPBOARD.lock();
        let Some(global) = global_cb
            .as_mut()
            .filter(|global| Arc::ptr_eq(&global.inner, &self.inner))
        else {
            return;
        };
        global.handles -= 1;
        if global.handles == 0 {
            // The last handle destroys the global object and sends the data to the clipboard
            // manager, even while a background read still shares the state.

            if let Err(e) = self.inner.ask_clipboard_manager_to_request_our_data() {
                log::error!(
                    "Could not hand the clipboard data over to the clipboard manager: {}",
                    e
                );
            }
            let global_cb = global_cb.take();
            if let Err(e) = self
                .inner
                .server
                .conn
                .destroy_window(self.inner.server.win_id)
            {
                log::error!("Failed to destroy the clipboard window. Error: {}", e);
                return;
            }
            if let Err(e) = self.inner.server.conn.flush() {
                log::error!("Failed to flush the clipboard window. Error: {}", e);
                return;
            }
            if let Some(global_cb) = global_cb
                && let Err(e) = global_cb.server_handle.join()
            {
                // Let's try extracting the error message
                let message;
                if let Some(msg) = e.downcast_ref::<&'static str>() {
                    message = Some((*msg).to_string());
                } else if let Some(msg) = e.downcast_ref::<String>() {
                    message = Some(msg.clone());
                } else {
                    message = None;
                }
                if let Some(message) = message {
                    log::error!(
                        "The clipboard server thread panicked. Panic message: '{}'",
                        message,
                    );
                } else {
                    log::error!("The clipboard server thread panicked.");
                }
            }
        }
    }
}

fn into_unknown<E: std::fmt::Display>(error: E) -> Error {
    Error::Unknown {
        description: error.to_string(),
    }
}

/// Clipboard selection
///
/// Linux has a concept of clipboard "selections" which tend to be used in different contexts. This
/// enum provides a way to get/set to a specific clipboard
///
/// See <https://specifications.freedesktop.org/clipboards-spec/clipboards-0.1.txt> for a better
/// description of the different clipboards.
#[derive(Copy, Clone, Debug)]
pub enum ClipboardKind {
    /// Typically used selection for explicit cut/copy/paste actions (ie. windows/macos like
    /// clipboard behavior)
    Clipboard,

    /// Typically used for mouse selections and/or currently selected text. Accessible via middle
    /// mouse click.
    Primary,

    /// The secondary clipboard is rarely used but theoretically available on X11.
    Secondary,
}

/// Configuration on how long to wait for a new X11 copy event is emitted.
#[derive(Default)]
pub(crate) enum WaitConfig {
    /// Waits until the given [`Instant`] has reached.
    #[allow(
        unused,
        reason = "Right now we don't wait for clipboard contents to sync on app close, but we may in the future"
    )]
    Until(Instant),

    /// Waits forever until a new event is reached.
    #[allow(unused)]
    #[allow(
        unused,
        reason = "Right now we don't wait for clipboard contents to sync on app close, but we may in the future"
    )]
    Forever,

    /// It shouldn't wait.
    #[default]
    None,
}

#[non_exhaustive]
pub enum Error {
    Transfer(TransferError),
    ConnectionFailed,
    /// The clipboard contents were not available in the requested format.
    /// This could either be due to the clipboard being empty or the clipboard contents having
    /// an incompatible format to the requested one (eg when calling `get_image` on text)
    ContentNotAvailable,

    /// The native clipboard is not accessible due to being held by an other party.
    ///
    /// This "other party" could be a different process or it could be within
    /// the same program. So for example you may get this error when trying
    /// to interact with the clipboard from multiple threads at once.
    ///
    /// Note that it's OK to have multiple `Clipboard` instances. The underlying
    /// implementation will make sure that the native clipboard is only
    /// opened for transferring data and then closed as soon as possible.
    ClipboardOccupied,

    /// The image or the text that was about the be transferred to/from the clipboard could not be
    /// converted to the appropriate format.
    ConversionFailure,

    /// Any error that doesn't fit the other error types.
    ///
    /// The `description` field is only meant to help the developer and should not be relied on as a
    /// means to identify an error case during runtime.
    Unknown {
        description: String,
    },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Transfer(error) => std::fmt::Display::fmt(error, f),
            Error::ConnectionFailed => f.write_str("Could not connect to the X11 clipboard server."),
			Error::ContentNotAvailable => f.write_str("The clipboard contents were not available in the requested format or the clipboard is empty."),
			Error::ClipboardOccupied => f.write_str("The native clipboard is not accessible due to being held by an other party."),
			Error::ConversionFailure => f.write_str("The image or the text that was about the be transferred to/from the clipboard could not be converted to the appropriate format."),
			Error::Unknown { description } => f.write_fmt(format_args!("Unknown error while interacting with the clipboard: {description}")),
		}
    }
}

impl std::error::Error for Error {}

impl std::fmt::Debug for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use Error::*;
        macro_rules! kind_to_str {
			($( $e: pat ),*) => {
				match self {
					$(
						$e => stringify!($e),
					)*
				}
			}
		}
        let name = kind_to_str!(
            Transfer(_),
            ConnectionFailed,
            ContentNotAvailable,
            ClipboardOccupied,
            ConversionFailure,
            Unknown { .. }
        );
        f.write_fmt(format_args!("{name} - \"{self}\""))
    }
}

impl Error {
    pub(crate) fn unknown<M: Into<String>>(message: M) -> Self {
        Error::Unknown {
            description: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::clipboard_transfer::MAX_CLIPBOARD_BYTES;
    use x11rb::protocol::xproto::ChangeWindowAttributesAux;

    // Keep one client alive for the native fixtures. Otherwise closing every transient owner
    // and requestor lets Xvfb reset, racing the next fixture's connection handshake.
    static DISPLAY_TEST: std::sync::LazyLock<Mutex<Arc<Inner>>> =
        std::sync::LazyLock::new(|| Mutex::new(Arc::new(Inner::new().unwrap())));

    enum Offer {
        Mixed {
            files: bool,
        },
        TextOnly {
            files: bool,
            image: bool,
            text: Option<Vec<u8>>,
        },
        Direct(usize),
        Incremental(usize),
        Trickle,
        LargeHint,
    }

    struct Owner {
        stopped: Arc<AtomicBool>,
        worker: Option<JoinHandle<()>>,
        requests: Arc<Mutex<Vec<Atom>>>,
    }

    impl Owner {
        fn new(selection: ClipboardKind, offer: Offer) -> Self {
            Self::answering_after(selection, offer, Duration::ZERO)
        }

        /// Answers each selection request only after `delay`.
        fn answering_after(selection: ClipboardKind, offer: Offer, delay: Duration) -> Self {
            let context = XContext::new().expect("private X11 display required");
            let atoms = Atoms::new(&context.conn).unwrap().reply().unwrap();
            let selection = match selection {
                ClipboardKind::Clipboard => atoms.CLIPBOARD,
                ClipboardKind::Primary => atoms.PRIMARY,
                ClipboardKind::Secondary => atoms.SECONDARY,
            };
            context
                .conn
                .set_selection_owner(context.win_id, selection, Time::CURRENT_TIME)
                .unwrap()
                .check()
                .unwrap();
            context.conn.flush().unwrap();
            let stopped = Arc::new(AtomicBool::new(false));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let worker = std::thread::spawn({
                let requests = requests.clone();
                let stopped = stopped.clone();
                move || {
                    let mut incremental: Option<(u32, Atom, Atom, usize)> = None;
                    while !stopped.load(Ordering::Relaxed) {
                        let Some(event) = context.conn.poll_for_event().unwrap() else {
                            std::thread::sleep(Duration::from_millis(1));
                            continue;
                        };
                        match event {
                            Event::SelectionRequest(event) => {
                                std::thread::sleep(delay);
                                requests.lock().push(event.target);
                                let mut property = event.property;
                                if event.target == atoms.TARGETS {
                                    let targets = match &offer {
                                        Offer::Mixed { files: true } => {
                                            vec![atoms.PNG__MIME, atoms.UTF8_STRING, atoms.URI_LIST]
                                        }
                                        Offer::Mixed { files: false } => {
                                            vec![atoms.PNG__MIME, atoms.UTF8_STRING]
                                        }
                                        Offer::TextOnly { files, image, text } => {
                                            let mut targets = Vec::new();
                                            if *files {
                                                targets.push(atoms.URI_LIST);
                                            }
                                            if *image {
                                                targets.push(atoms.PNG__MIME);
                                            }
                                            if text.is_some() {
                                                targets.push(atoms.UTF8_STRING);
                                            }
                                            targets
                                        }
                                        _ => vec![atoms.UTF8_STRING],
                                    };
                                    context
                                        .conn
                                        .change_property32(
                                            PropMode::REPLACE,
                                            event.requestor,
                                            property,
                                            atoms.ATOM,
                                            &targets,
                                        )
                                        .unwrap()
                                        .check()
                                        .unwrap();
                                } else if let Offer::TextOnly { files, image, text } = &offer {
                                    let bytes = if event.target == atoms.UTF8_STRING {
                                        text.as_deref()
                                    } else if event.target == atoms.URI_LIST && *files {
                                        Some(b"file:///tmp/fixture\r\n".as_slice())
                                    } else if event.target == atoms.PNG__MIME && *image {
                                        Some(b"image".as_slice())
                                    } else {
                                        None
                                    };
                                    if let Some(bytes) = bytes {
                                        context
                                            .conn
                                            .change_property8(
                                                PropMode::REPLACE,
                                                event.requestor,
                                                property,
                                                event.target,
                                                bytes,
                                            )
                                            .unwrap()
                                            .check()
                                            .unwrap();
                                    } else {
                                        property = NONE;
                                    }
                                } else if matches!(offer, Offer::Mixed { .. }) {
                                    let bytes: &[u8] = if event.target == atoms.UTF8_STRING {
                                        b"clipboard text"
                                    } else if event.target == atoms.URI_LIST {
                                        b"file:///tmp/clipboard-file\r\n"
                                    } else {
                                        b"image alternate"
                                    };
                                    context
                                        .conn
                                        .change_property8(
                                            PropMode::REPLACE,
                                            event.requestor,
                                            property,
                                            event.target,
                                            bytes,
                                        )
                                        .unwrap()
                                        .check()
                                        .unwrap();
                                } else if let Offer::Direct(length) = offer {
                                    context
                                        .conn
                                        .change_property8(
                                            PropMode::REPLACE,
                                            event.requestor,
                                            property,
                                            event.target,
                                            &[],
                                        )
                                        .unwrap()
                                        .check()
                                        .unwrap();
                                    let chunk = [b'x'; 64 * 1024];
                                    let mut remaining = length;
                                    while remaining > 0 {
                                        let length = remaining.min(chunk.len());
                                        context
                                            .conn
                                            .change_property8(
                                                PropMode::APPEND,
                                                event.requestor,
                                                property,
                                                event.target,
                                                &chunk[..length],
                                            )
                                            .unwrap()
                                            .check()
                                            .unwrap();
                                        remaining -= length;
                                    }
                                } else if matches!(
                                    offer,
                                    Offer::Incremental(_) | Offer::Trickle | Offer::LargeHint
                                ) {
                                    context
                                        .conn
                                        .change_window_attributes(
                                            event.requestor,
                                            &ChangeWindowAttributesAux::new()
                                                .event_mask(EventMask::PROPERTY_CHANGE),
                                        )
                                        .unwrap()
                                        .check()
                                        .unwrap();
                                    let hint = if matches!(offer, Offer::LargeHint) {
                                        u32::MAX
                                    } else {
                                        0
                                    };
                                    context
                                        .conn
                                        .change_property32(
                                            PropMode::REPLACE,
                                            event.requestor,
                                            property,
                                            atoms.INCR,
                                            &[hint],
                                        )
                                        .unwrap()
                                        .check()
                                        .unwrap();
                                    let remaining = match offer {
                                        Offer::Incremental(length) => length,
                                        _ => usize::MAX,
                                    };
                                    incremental =
                                        Some((event.requestor, property, event.target, remaining));
                                } else {
                                    property = NONE;
                                }
                                context
                                    .conn
                                    .send_event(
                                        false,
                                        event.requestor,
                                        EventMask::NO_EVENT,
                                        SelectionNotifyEvent {
                                            response_type: SELECTION_NOTIFY_EVENT,
                                            sequence: 0,
                                            time: event.time,
                                            requestor: event.requestor,
                                            selection: event.selection,
                                            target: event.target,
                                            property,
                                        },
                                    )
                                    .unwrap()
                                    .check()
                                    .unwrap();
                                context.conn.flush().unwrap();
                            }
                            Event::PropertyNotify(event) if event.state == Property::DELETE => {
                                if let Some((window, property, target, remaining)) =
                                    incremental.as_mut()
                                    && event.window == *window
                                    && event.atom == *property
                                {
                                    if matches!(offer, Offer::LargeHint) {
                                        continue;
                                    }
                                    if matches!(offer, Offer::Trickle) {
                                        std::thread::sleep(Duration::from_millis(3));
                                    }
                                    let length = (*remaining).min(8192);
                                    let result = context
                                        .conn
                                        .change_property8(
                                            PropMode::REPLACE,
                                            *window,
                                            *property,
                                            *target,
                                            &vec![b'x'; length],
                                        )
                                        .unwrap()
                                        .check();
                                    // The reader destroys its window when rejecting an owner.
                                    if result.is_err() {
                                        incremental = None;
                                        continue;
                                    }
                                    *remaining -= length;
                                    if length == 0 {
                                        incremental = None;
                                    }
                                    context.conn.flush().unwrap();
                                }
                            }
                            _ => {}
                        }
                    }
                }
            });
            Self {
                stopped,
                worker: Some(worker),
                requests,
            }
        }
    }

    impl Drop for Owner {
        fn drop(&mut self) {
            self.stopped.store(true, Ordering::Relaxed);
            self.worker.take().unwrap().join().unwrap();
        }
    }

    #[test]
    #[ignore = "requires a private X11 display"]
    fn external_mixed_offers_prefer_text_and_preserve_file_precedence() {
        let inner = DISPLAY_TEST.lock();
        let clipboard = Clipboard {
            inner: Arc::clone(&inner),
        };
        for selection in [ClipboardKind::Clipboard, ClipboardKind::Primary] {
            let owner = Owner::new(selection, Offer::Mixed { files: false });
            assert_eq!(
                clipboard.get_any(selection).unwrap().text().as_deref(),
                Some("clipboard text")
            );
            drop(owner);
            let _owner = Owner::new(selection, Offer::Mixed { files: true });
            let item = clipboard.get_any(selection).unwrap();
            assert!(matches!(
                item.entries().first(),
                Some(gpui::ClipboardEntry::ExternalPaths(_))
            ));
            assert_eq!(item.text().as_deref(), Some("clipboard text"));
        }
    }

    #[test]
    #[ignore = "requires a private X11 display"]
    fn external_text_only_reads_preserve_utf8_and_bound_each_selection() {
        let inner = DISPLAY_TEST.lock();
        let clipboard = Clipboard {
            inner: Arc::clone(&inner),
        };
        for selection in [ClipboardKind::Clipboard, ClipboardKind::Primary] {
            for (files, image, text, expected) in [
                (false, true, None, Ok(None)),
                (true, false, None, Ok(None)),
                (
                    true,
                    true,
                    Some(b"a\r\nb".to_vec()),
                    Ok(Some("a\r\nb".to_owned())),
                ),
                (
                    false,
                    false,
                    Some(vec![0xff]),
                    Err(ClipboardReadError::UnsupportedContent),
                ),
                (
                    false,
                    false,
                    Some(vec![b'x'; 1024]),
                    Ok(Some("x".repeat(1024))),
                ),
                (
                    false,
                    false,
                    Some(vec![b'x'; 1025]),
                    Err(ClipboardReadError::TooLarge),
                ),
            ] {
                let owner = Owner::new(selection, Offer::TextOnly { files, image, text });
                assert_eq!(
                    finish(clipboard.prepare_text_read(selection, 1024)),
                    expected
                );
                let requests = owner.requests.lock();
                assert!(!requests.contains(&inner.atoms.URI_LIST));
                assert!(!requests.contains(&inner.atoms.PNG__MIME));
            }
        }
    }

    #[test]
    #[ignore = "requires a private X11 display"]
    fn text_only_owned_selections_preserve_empty_text_and_limits() {
        let inner = DISPLAY_TEST.lock();
        let clipboard = Clipboard {
            inner: Arc::clone(&inner),
        };
        for selection in [ClipboardKind::Clipboard, ClipboardKind::Primary] {
            for text in ["", "a\r\nb", "é"] {
                clipboard
                    .set_item(
                        &ClipboardItem::new_string(text.into()),
                        selection,
                        WaitConfig::None,
                    )
                    .unwrap();
                // A selection this client owns answers without a transfer.
                let PreparedRead::Ready(read) = clipboard.prepare_text_read(selection, text.len())
                else {
                    panic!("an owned selection must answer at once");
                };
                assert_eq!(read, Ok(Some(text.to_owned())));
                if !text.is_empty() {
                    assert_eq!(
                        finish(clipboard.prepare_text_read(selection, text.len() - 1)),
                        Err(ClipboardReadError::TooLarge)
                    );
                }
            }
            let item = ClipboardItem {
                entries: vec![gpui::ClipboardEntry::ExternalPaths(ExternalPaths(
                    vec!["/tmp/fixture".into()].into(),
                ))],
            };
            clipboard
                .set_item(&item, selection, WaitConfig::None)
                .unwrap();
            assert_eq!(
                finish(clipboard.prepare_text_read(selection, 1024)),
                Ok(None)
            );
        }
    }

    fn read_external(inner: &Inner, offer: Offer, timeout: Duration) -> Result<ClipboardData> {
        let _owner = Owner::new(ClipboardKind::Clipboard, offer);
        inner.read(
            &[inner.atoms.UTF8_STRING],
            ClipboardKind::Clipboard,
            ReadSource::Owner,
            &mut ClipboardTransfer::new(timeout),
        )
    }

    #[test]
    #[ignore = "requires a private X11 display"]
    fn external_direct_and_incremental_transfers_are_bounded() {
        let inner = DISPLAY_TEST.lock();
        let data = read_external(
            &inner,
            Offer::Incremental(128 * 1024),
            CLIPBOARD_READ_TIMEOUT,
        )
        .unwrap();
        assert_eq!(data.bytes, vec![b'x'; 128 * 1024]);
        for offer in [
            Offer::Direct(MAX_CLIPBOARD_BYTES + 1),
            Offer::Incremental(MAX_CLIPBOARD_BYTES + 1),
            Offer::LargeHint,
        ] {
            assert!(matches!(
                read_external(&inner, offer, CLIPBOARD_READ_TIMEOUT),
                Err(Error::Transfer(TransferError::TooLarge))
            ));
        }
        // Rejected owners must not leave the client's next valid transfer unusable.
        let data = read_external(
            &inner,
            Offer::Incremental(128 * 1024),
            CLIPBOARD_READ_TIMEOUT,
        )
        .unwrap();
        assert_eq!(data.bytes, vec![b'x'; 128 * 1024]);
    }

    #[test]
    #[ignore = "requires a private X11 display"]
    fn external_trickling_owner_cannot_renew_deadline() {
        let inner = DISPLAY_TEST.lock();
        let started = Instant::now();
        assert!(matches!(
            read_external(&inner, Offer::Trickle, Duration::from_millis(100)),
            Err(Error::Transfer(TransferError::TimedOut))
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    #[ignore = "requires a private X11 display"]
    fn owned_reads_never_transfer_after_another_client_claims_the_selection() {
        let inner = DISPLAY_TEST.lock();
        let clipboard = Clipboard {
            inner: Arc::clone(&inner),
        };
        let delay = Duration::from_millis(300);
        for selection in [ClipboardKind::Clipboard, ClipboardKind::Primary] {
            clipboard
                .set_item(
                    &ClipboardItem::new_string("retained".into()),
                    selection,
                    WaitConfig::None,
                )
                .unwrap();
            let source = clipboard.read_source(selection);
            assert_eq!(source, ReadSource::Retained);

            // Another client claims the selection after the ownership check.
            let owner = Owner::answering_after(
                selection,
                Offer::TextOnly {
                    files: false,
                    image: false,
                    text: Some(b"external".to_vec()),
                },
                delay,
            );
            let started = Instant::now();
            let text = clipboard.prepare_text_read_from(
                source,
                selection,
                1024,
                ClipboardTransfer::new(CLIPBOARD_READ_TIMEOUT),
            );
            let item = clipboard.prepare_read_from(source, selection);
            assert!(started.elapsed() < delay, "preparing a read must not wait");
            let (PreparedRead::Ready(text), PreparedRead::Ready(item)) = (text, item) else {
                panic!("a read prepared for an owned selection must answer at once");
            };
            assert_eq!(text, Ok(Some("retained".to_owned())));
            assert_eq!(
                item.map(|item| item.and_then(|item| item.text())),
                Ok(Some("retained".to_owned()))
            );
            assert!(owner.requests.lock().is_empty());
        }
    }

    /// Completes a prepared read on the calling thread.
    fn finish<T>(read: PreparedRead<T>) -> T {
        match read {
            PreparedRead::Ready(value) => value,
            PreparedRead::Transfer(transfer) => transfer(),
        }
    }

    #[test]
    fn failed_reads_are_classified_without_contents() {
        assert_eq!(read_result(Ok("text")), Ok(Some("text")));
        assert_eq!(read_result::<()>(Err(Error::ContentNotAvailable)), Ok(None));
        for (error, expected) in [
            (
                Error::Transfer(TransferError::TimedOut),
                ClipboardReadError::TimedOut,
            ),
            (
                Error::Transfer(TransferError::TooLarge),
                ClipboardReadError::TooLarge,
            ),
            (
                Error::ConversionFailure,
                ClipboardReadError::UnsupportedContent,
            ),
            (Error::ConnectionFailed, ClipboardReadError::Unavailable),
            (Error::ClipboardOccupied, ClipboardReadError::Unavailable),
            (Error::unknown("detail"), ClipboardReadError::Unavailable),
        ] {
            assert_eq!(read_result::<()>(Err(error)), Err(expected));
        }
    }

    #[test]
    #[ignore = "requires a private X11 display"]
    fn slow_owner_reads_prepare_at_once_and_complete_on_another_thread() {
        let inner = DISPLAY_TEST.lock();
        let clipboard = Clipboard {
            inner: Arc::clone(&inner),
        };
        let delay = Duration::from_millis(150);
        let _owner = Owner::answering_after(
            ClipboardKind::Clipboard,
            Offer::TextOnly {
                files: false,
                image: false,
                text: Some(b"slow".to_vec()),
            },
            delay,
        );
        let started = Instant::now();
        let PreparedRead::Transfer(read) =
            clipboard.prepare_text_read(ClipboardKind::Clipboard, 1024)
        else {
            panic!("an external owner must answer through a transfer");
        };
        assert!(started.elapsed() < delay, "preparing a read must not wait");
        assert_eq!(
            std::thread::spawn(read).join().unwrap(),
            Ok(Some("slow".to_owned()))
        );
        assert!(started.elapsed() >= delay);
    }

    #[test]
    #[ignore = "requires a private X11 display"]
    fn stalled_owner_reads_end_at_the_deadline_with_a_typed_failure() {
        let inner = DISPLAY_TEST.lock();
        let clipboard = Clipboard {
            inner: Arc::clone(&inner),
        };
        let _owner = Owner::new(ClipboardKind::Clipboard, Offer::Trickle);
        let started = Instant::now();
        let read = clipboard.prepare_text_read_from(
            ReadSource::Owner,
            ClipboardKind::Clipboard,
            MAX_CLIPBOARD_BYTES,
            ClipboardTransfer::new(Duration::from_millis(100)),
        );
        assert_eq!(finish(read), Err(ClipboardReadError::TimedOut));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// A clipboard manager that records each target it is asked to convert, without answering.
    struct Manager {
        stopped: Arc<AtomicBool>,
        worker: Option<JoinHandle<()>>,
        requests: Arc<Mutex<Vec<Atom>>>,
    }

    impl Manager {
        fn new() -> Self {
            let context = XContext::new().expect("private X11 display required");
            let atoms = Atoms::new(&context.conn).unwrap().reply().unwrap();
            context
                .conn
                .set_selection_owner(context.win_id, atoms.CLIPBOARD_MANAGER, Time::CURRENT_TIME)
                .unwrap()
                .check()
                .unwrap();
            context.conn.flush().unwrap();
            let stopped = Arc::new(AtomicBool::new(false));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let worker = std::thread::spawn({
                let requests = requests.clone();
                let stopped = stopped.clone();
                move || {
                    while !stopped.load(Ordering::Relaxed) {
                        match context.conn.poll_for_event().unwrap() {
                            Some(Event::SelectionRequest(event)) => {
                                requests.lock().push(event.target);
                            }
                            Some(_) => {}
                            None => std::thread::sleep(Duration::from_millis(1)),
                        }
                    }
                }
            });
            Self {
                stopped,
                worker: Some(worker),
                requests,
            }
        }

        fn was_asked_for(&self, target: Atom, within: Duration) -> bool {
            let deadline = Instant::now() + within;
            while Instant::now() < deadline {
                if self.requests.lock().contains(&target) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            false
        }
    }

    impl Drop for Manager {
        fn drop(&mut self) {
            self.stopped.store(true, Ordering::Relaxed);
            self.worker.take().unwrap().join().unwrap();
        }
    }

    #[test]
    #[ignore = "requires a private X11 display"]
    fn the_last_clipboard_hands_over_to_the_manager_while_a_read_is_in_flight() {
        let display = DISPLAY_TEST.lock();
        let manager = Manager::new();
        let clipboard = Clipboard::new().unwrap();
        clipboard
            .set_item(
                &ClipboardItem::new_string("kept".into()),
                ClipboardKind::Clipboard,
                WaitConfig::None,
            )
            .unwrap();
        let _owner = Owner::answering_after(
            ClipboardKind::Primary,
            Offer::TextOnly {
                files: false,
                image: false,
                text: Some(b"slow".to_vec()),
            },
            Duration::from_millis(300),
        );
        let PreparedRead::Transfer(read) = clipboard.prepare_read(ClipboardKind::Primary) else {
            panic!("an external owner must answer through a transfer");
        };
        let read = std::thread::spawn(read);

        drop(clipboard);

        assert!(
            manager.was_asked_for(display.atoms.SAVE_TARGETS, Duration::from_secs(1)),
            "the clipboard manager must be asked to save the contents"
        );
        assert!(CLIPBOARD.lock().is_none());
        assert_eq!(
            read.join()
                .unwrap()
                .map(|item| item.and_then(|item| item.text())),
            Ok(Some("slow".to_owned()))
        );
    }
}
