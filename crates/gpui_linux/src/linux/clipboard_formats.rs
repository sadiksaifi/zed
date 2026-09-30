//! Clipboard formats shared by the Wayland and X11 clipboards.
//!
//! File managers publish copied files as `text/uri-list` (RFC 2483) and, on GNOME and
//! GTK, as `x-special/gnome-copied-files`, which prefixes the URIs with a `copy` or
//! `cut` line. GPUI represents both as [`ClipboardEntry::ExternalPaths`].

use std::{
    ffi::OsString,
    os::unix::ffi::{OsStrExt as _, OsStringExt as _},
    path::{Path, PathBuf},
};

use gpui::{ClipboardEntry, ClipboardItem, ClipboardString, ExternalPaths};
use smallvec::SmallVec;

/// The freedesktop.org file list format.
pub(crate) const URI_LIST_MIME_TYPE: &str = "text/uri-list";
/// The GNOME file list format, read by Nautilus and other GTK file managers on paste.
pub(crate) const GNOME_COPIED_FILES_MIME_TYPE: &str = "x-special/gnome-copied-files";

/// Parses a `text/uri-list` body into local paths.
///
/// Returns `None` unless the list names at least one file and every URI names a local
/// file: a `file` URI with an empty or `localhost` host. A list that also names remote
/// resources is left to the text representation instead of being pasted partially.
pub(crate) fn parse_uri_list(bytes: &[u8]) -> Option<ExternalPaths> {
    let uris = bytes
        .split(|&byte| byte == b'\n')
        .map(|line| line.trim_ascii())
        .filter(|line| !line.is_empty() && !line.starts_with(b"#"));
    parse_file_uris(uris)
}

/// Parses an `x-special/gnome-copied-files` body into local paths.
///
/// The first line is the operation, `copy` or `cut`, and every following line is a URI.
/// The same local-file rules as [`parse_uri_list`] apply.
pub(crate) fn parse_gnome_copied_files(bytes: &[u8]) -> Option<ExternalPaths> {
    let mut lines = bytes
        .split(|&byte| byte == b'\n')
        .map(|line| line.trim_ascii());
    let operation = lines.next()?;
    if operation != b"copy" && operation != b"cut" {
        return None;
    }
    parse_file_uris(lines.filter(|line| !line.is_empty()))
}

fn parse_file_uris<'a>(uris: impl Iterator<Item = &'a [u8]>) -> Option<ExternalPaths> {
    let paths = uris
        .map(parse_file_uri)
        .collect::<Option<SmallVec<[PathBuf; 2]>>>()?;
    (!paths.is_empty()).then_some(ExternalPaths(paths))
}

/// Converts a local `file` URI into a path, decoding percent escapes into raw path bytes.
fn parse_file_uri(uri: &[u8]) -> Option<PathBuf> {
    const SCHEME: &[u8] = b"file:";
    if uri.len() < SCHEME.len() || !uri[..SCHEME.len()].eq_ignore_ascii_case(SCHEME) {
        return None;
    }
    let rest = &uri[SCHEME.len()..];
    let path = match rest.strip_prefix(b"//") {
        Some(authority_and_path) => {
            let path_start = authority_and_path.iter().position(|&byte| byte == b'/')?;
            let host = &authority_and_path[..path_start];
            if !host.is_empty() && !host.eq_ignore_ascii_case(b"localhost") {
                return None;
            }
            &authority_and_path[path_start..]
        }
        None if rest.starts_with(b"/") => rest,
        None => return None,
    };
    // A query or fragment would make the URI name something other than the file.
    if path.iter().any(|&byte| byte == b'?' || byte == b'#') {
        return None;
    }
    let path = percent_decode(path)?;
    if path.contains(&0) {
        return None;
    }
    Some(PathBuf::from(OsString::from_vec(path)))
}

fn percent_decode(input: &[u8]) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(input.len());
    let mut bytes = input.iter();
    while let Some(&byte) = bytes.next() {
        if byte == b'%' {
            let high = hex_value(*bytes.next()?)?;
            let low = hex_value(*bytes.next()?)?;
            output.push(high << 4 | low);
        } else {
            output.push(byte);
        }
    }
    Some(output)
}

fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

/// Encodes an absolute path as a `file` URI, percent-encoding every byte outside the
/// URI path characters. Returns `None` for a relative path.
pub(crate) fn file_uri(path: &Path) -> Option<String> {
    if !path.is_absolute() {
        return None;
    }
    let mut uri = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        if is_uri_path_byte(byte) {
            uri.push(char::from(byte));
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    Some(uri)
}

/// Unreserved characters, sub-delimiters, `:`, `@` and the `/` segment separator.
fn is_uri_path_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/".contains(&byte)
}

/// Encodes paths as a `text/uri-list` body, one CRLF-terminated URI per absolute path.
pub(crate) fn uri_list<'a>(paths: impl IntoIterator<Item = &'a Path>) -> String {
    paths
        .into_iter()
        .filter_map(file_uri)
        .fold(String::new(), |mut list, uri| {
            list.push_str(&uri);
            list.push_str("\r\n");
            list
        })
}

/// Encodes paths as an `x-special/gnome-copied-files` body for a copy operation.
fn gnome_copied_files<'a>(paths: impl IntoIterator<Item = &'a Path>) -> String {
    paths
        .into_iter()
        .filter_map(file_uri)
        .fold(String::from("copy"), |mut list, uri| {
            list.push('\n');
            list.push_str(&uri);
            list
        })
}

/// Builds the item read from a selection that offered a file list, keeping the offered
/// text so that text consumers can still paste it.
pub(crate) fn file_list_item(paths: ExternalPaths, text: Option<String>) -> ClipboardItem {
    let text = text.unwrap_or_else(|| paths_text(paths.paths()));
    ClipboardItem {
        entries: vec![
            ClipboardEntry::ExternalPaths(paths),
            ClipboardEntry::String(ClipboardString::new(text)),
        ],
    }
}

/// One path per line, as file managers offer copied files as text.
fn paths_text(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The representations that GPUI offers to other programs for a [`ClipboardItem`].
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ClipboardOffer {
    text: Option<String>,
    uri_list: Option<String>,
    gnome_copied_files: Option<String>,
}

impl ClipboardOffer {
    pub(crate) fn new(item: &ClipboardItem) -> Self {
        let mut text = String::new();
        let mut paths = Vec::new();
        for entry in item.entries() {
            match entry {
                ClipboardEntry::String(string) => text.push_str(string.text()),
                ClipboardEntry::ExternalPaths(external_paths) => {
                    paths.extend(external_paths.paths().iter().cloned())
                }
                ClipboardEntry::Image(_) => {}
            }
        }
        if text.is_empty() && !paths.is_empty() {
            text = paths_text(&paths);
        }

        let file_paths = || paths.iter().map(PathBuf::as_path);
        let uri_list = Some(uri_list(file_paths())).filter(|list| !list.is_empty());
        let gnome_copied_files = uri_list.is_some().then(|| gnome_copied_files(file_paths()));

        Self {
            text: Some(text).filter(|text| !text.is_empty()),
            uri_list,
            gnome_copied_files,
        }
    }

    /// The plain text representation.
    pub(crate) fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }

    /// The `text/uri-list` representation, present when the item carries local paths.
    pub(crate) fn uri_list(&self) -> Option<&str> {
        self.uri_list.as_deref()
    }

    /// The `x-special/gnome-copied-files` representation, present with [`Self::uri_list`].
    pub(crate) fn gnome_copied_files(&self) -> Option<&str> {
        self.gnome_copied_files.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn paths(paths: &[&str]) -> ExternalPaths {
        ExternalPaths(paths.iter().map(PathBuf::from).collect())
    }

    #[test]
    fn parses_uri_list_with_comments_and_crlf() {
        let list = b"# copied by a file manager\r\nfile:///tmp/first\r\nfile:///tmp/nested%20directory\r\n";

        assert_eq!(
            parse_uri_list(list),
            Some(paths(&["/tmp/first", "/tmp/nested directory"]))
        );
    }

    #[test]
    fn accepts_localhost_and_authorityless_file_uris() {
        assert_eq!(
            parse_uri_list(b"file://localhost/tmp/a\nFILE://LOCALHOST/tmp/b\nfile:/tmp/c"),
            Some(paths(&["/tmp/a", "/tmp/b", "/tmp/c"]))
        );
    }

    #[test]
    fn decodes_percent_escapes_into_raw_path_bytes() {
        let parsed = parse_uri_list(b"file:///tmp/caf%C3%A9/%FF%fe").unwrap();

        assert_eq!(
            parsed.paths()[0].as_os_str(),
            OsStr::from_bytes(b"/tmp/caf\xC3\xA9/\xFF\xFE")
        );
    }

    #[test]
    fn rejects_lists_naming_anything_other_than_local_files() {
        for list in [
            &b"file:///tmp/a\nhttps://example.com/a"[..],
            b"file://remote-host/tmp/a",
            b"sftp://host/tmp/a",
            b"file:relative",
            b"file:///tmp/a?query",
            b"file:///tmp/a#fragment",
            b"file:///tmp/%2",
            b"file:///tmp/%zz",
            b"file:///tmp/nul%00byte",
            b"",
            b"# only a comment\r\n",
        ] {
            assert_eq!(
                parse_uri_list(list),
                None,
                "{:?}",
                String::from_utf8_lossy(list)
            );
        }
    }

    #[test]
    fn parses_gnome_copied_files_after_the_operation_line() {
        assert_eq!(
            parse_gnome_copied_files(b"copy\nfile:///tmp/a\nfile:///tmp/b%20c"),
            Some(paths(&["/tmp/a", "/tmp/b c"]))
        );
        assert_eq!(
            parse_gnome_copied_files(b"cut\nfile:///tmp/a\n"),
            Some(paths(&["/tmp/a"]))
        );
        assert_eq!(parse_gnome_copied_files(b"file:///tmp/a"), None);
        assert_eq!(parse_gnome_copied_files(b"copy\n"), None);
    }

    #[test]
    fn encodes_uri_list_for_absolute_paths() {
        let list = uri_list([
            Path::new("/tmp/first"),
            Path::new("relative/path"),
            Path::new("/tmp/nested directory/100%#?"),
        ]);

        assert_eq!(
            list,
            "file:///tmp/first\r\nfile:///tmp/nested%20directory/100%25%23%3F\r\n"
        );
        assert!(uri_list([Path::new("relative")]).is_empty());
    }

    #[test]
    fn encodes_non_utf8_paths_losslessly() {
        let path = Path::new(OsStr::from_bytes(b"/tmp/\xFF name"));
        let uri = file_uri(path).unwrap();

        assert_eq!(uri, "file:///tmp/%FF%20name");
        assert_eq!(
            parse_uri_list(uri.as_bytes()),
            Some(ExternalPaths([path.to_path_buf()].into_iter().collect()))
        );
    }

    #[test]
    fn offers_file_lists_and_path_text_for_external_paths() {
        let item = ClipboardItem {
            entries: vec![ClipboardEntry::ExternalPaths(paths(&[
                "/tmp/a b", "/tmp/c",
            ]))],
        };
        let offer = ClipboardOffer::new(&item);

        assert_eq!(offer.text(), Some("/tmp/a b\n/tmp/c"));
        assert_eq!(
            offer.uri_list(),
            Some("file:///tmp/a%20b\r\nfile:///tmp/c\r\n")
        );
        assert_eq!(
            offer.gnome_copied_files(),
            Some("copy\nfile:///tmp/a%20b\nfile:///tmp/c")
        );
        assert_eq!(
            parse_gnome_copied_files(offer.gnome_copied_files().unwrap().as_bytes()),
            Some(paths(&["/tmp/a b", "/tmp/c"]))
        );
    }

    #[test]
    fn offers_string_entries_as_the_text_of_a_file_list() {
        let item = file_list_item(paths(&["/tmp/a"]), Some("a".to_string()));
        let offer = ClipboardOffer::new(&item);

        assert_eq!(offer.text(), Some("a"));
        assert_eq!(offer.uri_list(), Some("file:///tmp/a\r\n"));
    }

    #[test]
    fn offers_only_text_for_strings() {
        let offer = ClipboardOffer::new(&ClipboardItem::new_string("text".to_string()));

        assert_eq!(
            offer,
            ClipboardOffer {
                text: Some("text".to_string()),
                uri_list: None,
                gnome_copied_files: None,
            }
        );
        assert_eq!(
            ClipboardOffer::new(&ClipboardItem { entries: vec![] }),
            ClipboardOffer::default()
        );
    }

    #[test]
    fn reads_file_lists_with_the_offered_text_or_one_path_per_line() {
        assert_eq!(
            file_list_item(paths(&["/tmp/a", "/tmp/b"]), None).entries(),
            &[
                ClipboardEntry::ExternalPaths(paths(&["/tmp/a", "/tmp/b"])),
                ClipboardEntry::String(ClipboardString::new("/tmp/a\n/tmp/b".to_string())),
            ]
        );
        assert_eq!(
            file_list_item(paths(&["/tmp/a"]), Some("offered".to_string())).entries()[1],
            ClipboardEntry::String(ClipboardString::new("offered".to_string()))
        );
    }
}
