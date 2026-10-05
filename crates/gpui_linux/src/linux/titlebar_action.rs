/// What the desktop does when the user double-clicks a window's titlebar.
///
/// Read from the `org.gnome.desktop.wm.preferences` `action-double-click-titlebar` setting, which
/// GNOME and other desktops expose through the XDG settings portal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TitlebarDoubleClickAction {
    /// Maximize the window, or restore it when it is maximized.
    #[default]
    ToggleMaximize,
    /// Maximize the window's width only, or restore it when it is maximized horizontally.
    ///
    /// Platforms that cannot maximize along one axis maximize the whole window.
    ToggleMaximizeHorizontally,
    /// Maximize the window's height only, or restore it when it is maximized vertically.
    ///
    /// Platforms that cannot maximize along one axis maximize the whole window.
    ToggleMaximizeVertically,
    /// Minimize the window.
    Minimize,
    /// Show the window menu at the pointer.
    Menu,
    /// Move the window below the other windows.
    Lower,
    /// Do nothing.
    None,
}

impl TitlebarDoubleClickAction {
    /// Parses an `action-double-click-titlebar` value.
    ///
    /// Actions GPUI cannot perform, such as shading, and unknown values do nothing.
    pub(crate) fn parse(value: &str) -> Self {
        match value.trim() {
            "toggle-maximize" => Self::ToggleMaximize,
            "toggle-maximize-horizontally" => Self::ToggleMaximizeHorizontally,
            "toggle-maximize-vertically" => Self::ToggleMaximizeVertically,
            "minimize" => Self::Minimize,
            "menu" => Self::Menu,
            "lower" => Self::Lower,
            _ => Self::None,
        }
    }

    /// Returns the action to perform for a window, dropping actions the window does not allow.
    pub(crate) fn for_window(self, is_resizable: bool, is_minimizable: bool) -> Self {
        match self {
            Self::ToggleMaximize
            | Self::ToggleMaximizeHorizontally
            | Self::ToggleMaximizeVertically
                if !is_resizable =>
            {
                Self::None
            }
            Self::Minimize if !is_minimizable => Self::None,
            action => action,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TitlebarDoubleClickAction;

    #[test]
    fn parses_desktop_actions() {
        let cases = [
            ("toggle-maximize", TitlebarDoubleClickAction::ToggleMaximize),
            (
                "toggle-maximize-horizontally",
                TitlebarDoubleClickAction::ToggleMaximizeHorizontally,
            ),
            (
                "toggle-maximize-vertically",
                TitlebarDoubleClickAction::ToggleMaximizeVertically,
            ),
            ("minimize", TitlebarDoubleClickAction::Minimize),
            ("menu", TitlebarDoubleClickAction::Menu),
            ("lower", TitlebarDoubleClickAction::Lower),
            ("none", TitlebarDoubleClickAction::None),
            (" minimize ", TitlebarDoubleClickAction::Minimize),
        ];
        for (value, action) in cases {
            assert_eq!(TitlebarDoubleClickAction::parse(value), action, "{value}");
        }
    }

    #[test]
    fn unsupported_and_unknown_actions_do_nothing() {
        for value in ["toggle-shade", "", "maximize", "Toggle-Maximize"] {
            assert_eq!(
                TitlebarDoubleClickAction::parse(value),
                TitlebarDoubleClickAction::None,
                "{value}"
            );
        }
    }

    #[test]
    fn defaults_to_toggle_maximize() {
        assert_eq!(
            TitlebarDoubleClickAction::default(),
            TitlebarDoubleClickAction::ToggleMaximize
        );
    }

    #[test]
    fn drops_actions_the_window_does_not_allow() {
        use TitlebarDoubleClickAction::*;

        for action in [
            ToggleMaximize,
            ToggleMaximizeHorizontally,
            ToggleMaximizeVertically,
        ] {
            assert_eq!(action.for_window(false, true), None);
            assert_eq!(action.for_window(true, false), action);
        }
        assert_eq!(Minimize.for_window(true, false), None);
        assert_eq!(Minimize.for_window(false, true), Minimize);
        for action in [Menu, Lower, None] {
            assert_eq!(action.for_window(false, false), action);
        }
    }
}
