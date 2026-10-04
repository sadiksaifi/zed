use super::{TitlebarDoubleClickAction, xdg_desktop_portal::Event};
use gpui::{MouseButton, WindowButton, WindowButtonLayout};
use std::{
    collections::HashMap,
    fs::OpenOptions,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

const CONFIG_SIZE_LIMIT: u64 = 64 * 1024;

pub(crate) fn is_kde() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .split(':')
        .any(|name| name.eq_ignore_ascii_case("KDE") || name.eq_ignore_ascii_case("PLASMA"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KdeWindowSettings {
    pub(crate) layout: WindowButtonLayout,
    pub(crate) double: TitlebarDoubleClickAction,
    pub(crate) middle: TitlebarDoubleClickAction,
    pub(crate) right: TitlebarDoubleClickAction,
}
impl Default for KdeWindowSettings {
    fn default() -> Self {
        Self {
            layout: WindowButtonLayout::linux_default(),
            double: TitlebarDoubleClickAction::ToggleMaximize,
            middle: TitlebarDoubleClickAction::Lower,
            right: TitlebarDoubleClickAction::Menu,
        }
    }
}
impl KdeWindowSettings {
    pub(crate) fn parse(contents: &str) -> Self {
        let mut result = Self::default();
        let mut group = "";
        let mut values = HashMap::new();
        for line in contents.lines().map(str::trim) {
            if line.starts_with('[') && line.ends_with(']') {
                group = &line[1..line.len() - 1];
            } else if let Some((key, value)) = line.split_once('=') {
                values.insert((group, key.trim()), value.trim());
            }
        }
        if let Some(value) = values.get(&("org.kde.kdecoration2", "ButtonsOnLeft")) {
            result.layout.left = kde_buttons(value);
        }
        if let Some(value) = values.get(&("org.kde.kdecoration2", "ButtonsOnRight")) {
            result.layout.right = kde_buttons(value);
        }
        // A control is never rendered twice, including malformed cross-side duplicates.
        for right in &mut result.layout.right {
            if right.is_some() && result.layout.left.contains(right) {
                *right = None;
            }
        }
        for (group, key, action) in [
            ("Windows", "TitlebarDoubleClickCommand", &mut result.double),
            (
                "MouseBindings",
                "CommandActiveTitlebar2",
                &mut result.middle,
            ),
            ("MouseBindings", "CommandActiveTitlebar3", &mut result.right),
        ] {
            if let Some(value) = values.get(&(group, key)) {
                *action = kde_action(value);
            }
        }
        result
    }
    pub(crate) fn events(&self) -> [Event; 4] {
        let side = |buttons: [Option<WindowButton>; 3]| {
            buttons
                .into_iter()
                .flatten()
                .map(|button| match button {
                    WindowButton::Close => "close",
                    WindowButton::Minimize => "minimize",
                    WindowButton::Maximize => "maximize",
                })
                .collect::<Vec<_>>()
                .join(",")
        };
        [
            Event::ButtonLayout(format!(
                "{}:{}",
                side(self.layout.left),
                side(self.layout.right)
            )),
            Event::TitlebarClickAction(MouseButton::Left, self.double),
            Event::TitlebarClickAction(MouseButton::Middle, self.middle),
            Event::TitlebarClickAction(MouseButton::Right, self.right),
        ]
    }
}
fn kde_buttons(value: &str) -> [Option<WindowButton>; 3] {
    let mut buttons = [None; 3];
    let mut count = 0;
    for symbol in value.chars() {
        let button = match symbol {
            'X' => WindowButton::Close,
            'I' => WindowButton::Minimize,
            'A' => WindowButton::Maximize,
            _ => continue,
        };
        if !buttons.contains(&Some(button))
            && let Some(slot) = buttons.get_mut(count)
        {
            *slot = Some(button);
            count += 1;
        }
    }
    buttons
}
fn kde_action(value: &str) -> TitlebarDoubleClickAction {
    match value {
        "Maximize" => TitlebarDoubleClickAction::ToggleMaximize,
        "Minimize" => TitlebarDoubleClickAction::Minimize,
        "Operations menu" => TitlebarDoubleClickAction::Menu,
        "Lower" => TitlebarDoubleClickAction::Lower,
        _ => TitlebarDoubleClickAction::None,
    }
}
pub(crate) fn config_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|root| root.join("kwinrc"))
}
pub(crate) fn read() -> KdeWindowSettings {
    config_path()
        .and_then(|path| read_config(&path))
        .map_or_else(KdeWindowSettings::default, |contents| {
            KdeWindowSettings::parse(&contents)
        })
}

/// Reads a regular configuration file of at most [`CONFIG_SIZE_LIMIT`] bytes.
///
/// The nonblocking open keeps a FIFO without a writer, or a device, from blocking the open, and
/// the check on the opened handle rejects it before any read can wait for data. The standard
/// library opens every file close-on-exec.
fn read_config(path: &Path) -> Option<String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut contents = String::new();
    file.take(CONFIG_SIZE_LIMIT + 1)
        .read_to_string(&mut contents)
        .ok()?;
    (contents.len() as u64 <= CONFIG_SIZE_LIMIT).then_some(contents)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn kde_window_settings_preserve_sides_order_and_configured_actions() {
        let settings = KdeWindowSettings::parse(
            "[org.kde.kdecoration2]\nButtonsOnLeft=XIA\nButtonsOnRight=M\n[Windows]\nTitlebarDoubleClickCommand=Minimize\n[MouseBindings]\nCommandActiveTitlebar2=Nothing\nCommandActiveTitlebar3=Maximize\n",
        );
        assert_eq!(
            settings.layout.left,
            [
                Some(WindowButton::Close),
                Some(WindowButton::Minimize),
                Some(WindowButton::Maximize)
            ]
        );
        assert_eq!(settings.layout.right, [None; 3]);
        assert_eq!(settings.double, TitlebarDoubleClickAction::Minimize);
        assert_eq!(settings.middle, TitlebarDoubleClickAction::None);
        assert_eq!(settings.right, TitlebarDoubleClickAction::ToggleMaximize);
        assert!(
            matches!(&settings.events()[0], Event::ButtonLayout(layout) if layout == "close,minimize,maximize:")
        );
    }
    #[test]
    fn kde_window_settings_handle_live_reordering_empty_sides_and_duplicates() {
        let before = KdeWindowSettings::default();
        let after = KdeWindowSettings::parse(
            "[org.kde.kdecoration2]\nButtonsOnLeft=XXI\nButtonsOnRight=XA\n",
        );
        assert_ne!(before, after);
        assert_eq!(
            after.layout.left,
            [
                Some(WindowButton::Close),
                Some(WindowButton::Minimize),
                None
            ]
        );
        assert_eq!(
            after.layout.right,
            [None, Some(WindowButton::Maximize), None]
        );
        let empty =
            KdeWindowSettings::parse("[org.kde.kdecoration2]\nButtonsOnLeft=\nButtonsOnRight=\n");
        assert_eq!(
            empty.layout,
            WindowButtonLayout {
                left: [None; 3],
                right: [None; 3]
            }
        );
        assert_eq!(kde_action("Shade"), TitlebarDoubleClickAction::None);
    }

    #[test]
    fn kde_config_reads_only_bounded_regular_files() {
        let directory = tempfile::tempdir().unwrap();

        let regular = directory.path().join("kwinrc");
        std::fs::write(&regular, "[Windows]\n").unwrap();
        assert_eq!(read_config(&regular).as_deref(), Some("[Windows]\n"));

        let oversized = directory.path().join("oversized");
        std::fs::write(&oversized, vec![b'#'; CONFIG_SIZE_LIMIT as usize + 1]).unwrap();
        assert_eq!(read_config(&oversized), None);

        assert_eq!(read_config(directory.path()), None);
        assert_eq!(read_config(&directory.path().join("missing")), None);
    }

    #[test]
    fn kde_config_rejects_a_fifo_without_a_writer() {
        let directory = tempfile::tempdir().unwrap();
        let fifo = directory.path().join("kwinrc");
        let fifo_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);

        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || sender.send(read_config(&fifo)));
        let result = receiver.recv_timeout(std::time::Duration::from_secs(5));
        if result.is_err() {
            // Unblock the reader so the test process can exit.
            std::fs::OpenOptions::new()
                .write(true)
                .open(directory.path().join("kwinrc"))
                .ok();
        }
        assert_eq!(result, Ok(None));
    }
}
