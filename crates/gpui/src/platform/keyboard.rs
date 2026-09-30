use collections::HashMap;

use crate::{KeybindingKeystroke, Keystroke, SharedString};

/// A trait for platform-specific keyboard layouts
pub trait PlatformKeyboardLayout {
    /// Get the keyboard layout ID, which should be unique to the layout
    fn id(&self) -> &str;
    /// Get the keyboard layout display name
    fn name(&self) -> &str;
    /// The `(base, shifted)` key pairs of the active layout, spelled as [`Keystroke::key`] is
    /// spelled for key-down events with Control held, without and with Shift.
    ///
    /// A pair is listed when both keys are single characters that differ and the shifted key has
    /// no case, so GPUI reports it without the Shift modifier (Shift+1 arrives as `!` on a US
    /// layout). Each base key appears at most once. Returns `None` when the platform does not
    /// report the table.
    fn shift_pairs(&self) -> Option<&[(SharedString, SharedString)]> {
        None
    }
}

/// A trait for platform-specific keyboard mappings
pub trait PlatformKeyboardMapper {
    /// Map a key equivalent to its platform-specific representation
    fn map_key_equivalent(
        &self,
        keystroke: Keystroke,
        use_key_equivalents: bool,
    ) -> KeybindingKeystroke;
    /// Get the key equivalents for the current keyboard layout,
    /// only used on macOS
    fn get_key_equivalents(&self) -> Option<&HashMap<char, char>>;
}

/// A dummy implementation of the platform keyboard mapper
pub struct DummyKeyboardMapper;

impl PlatformKeyboardMapper for DummyKeyboardMapper {
    fn map_key_equivalent(
        &self,
        keystroke: Keystroke,
        _use_key_equivalents: bool,
    ) -> KeybindingKeystroke {
        KeybindingKeystroke::from_keystroke(keystroke)
    }

    fn get_key_equivalents(&self) -> Option<&HashMap<char, char>> {
        None
    }
}
