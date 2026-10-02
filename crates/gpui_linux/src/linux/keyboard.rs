use std::sync::Arc;

use gpui::{PlatformKeyboardLayout, SharedString};

#[derive(Clone)]
pub(crate) struct LinuxKeyboardLayout {
    name: SharedString,
    shift_pairs: Option<Arc<[(SharedString, SharedString)]>>,
}

impl PlatformKeyboardLayout for LinuxKeyboardLayout {
    fn id(&self) -> &str {
        &self.name
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn shift_pairs(&self) -> Option<&[(SharedString, SharedString)]> {
        self.shift_pairs.as_deref()
    }
}

impl LinuxKeyboardLayout {
    pub(crate) fn new(name: SharedString) -> Self {
        Self {
            name,
            shift_pairs: None,
        }
    }
}

#[cfg(any(feature = "wayland", feature = "x11"))]
mod xkb_layout {
    use std::sync::Arc;

    use collections::HashSet;
    use gpui::{Modifiers, SharedString};
    use xkbcommon::xkb;

    use super::LinuxKeyboardLayout;
    use crate::linux::keystroke_from_xkb;

    impl LinuxKeyboardLayout {
        /// Describes the layout that is active in `state`.
        pub(crate) fn from_xkb(state: &xkb::State) -> Self {
            let keymap = state.get_keymap();
            let layout = state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE);
            Self {
                name: keymap.layout_get_name(layout).to_string().into(),
                shift_pairs: Some(shift_pairs(&keymap, layout)),
            }
        }
    }

    /// Computes the keys GPUI dispatches for Control and Control+Shift on every key of `layout`
    /// by translating each key exactly as key events are translated.
    fn shift_pairs(
        keymap: &xkb::Keymap,
        layout: xkb::LayoutIndex,
    ) -> Arc<[(SharedString, SharedString)]> {
        let translation = ShiftTranslation::new(keymap, layout);
        let mut bases = HashSet::default();
        let mut pairs = Vec::new();
        for keycode in keycodes(keymap) {
            if keymap.key_get_syms_by_level(keycode, layout, 0).is_empty() {
                continue;
            }
            let (base, shifted) = translation.keys(keycode);
            if is_shift_pair(&base, &shifted) && bases.insert(base.clone()) {
                pairs.push((base.into(), shifted.into()));
            }
        }
        pairs.into()
    }

    struct ShiftTranslation {
        base: xkb::State,
        shifted: xkb::State,
    }

    impl ShiftTranslation {
        fn new(keymap: &xkb::Keymap, layout: xkb::LayoutIndex) -> Self {
            let control = mod_mask(keymap, xkb::MOD_NAME_CTRL);
            let shift = mod_mask(keymap, xkb::MOD_NAME_SHIFT);
            Self {
                base: layout_state(keymap, layout, control),
                shifted: layout_state(keymap, layout, control | shift),
            }
        }

        fn keys(&self, keycode: xkb::Keycode) -> (String, String) {
            (
                keystroke_from_xkb(&self.base, Modifiers::control(), keycode).key,
                keystroke_from_xkb(&self.shifted, Modifiers::control_shift(), keycode).key,
            )
        }
    }

    fn keycodes(keymap: &xkb::Keymap) -> impl Iterator<Item = xkb::Keycode> {
        (keymap.min_keycode().raw()..=keymap.max_keycode().raw()).map(xkb::Keycode::new)
    }

    /// Whether GPUI dispatches `shifted` for Shift plus the key that dispatches `base`, with Shift
    /// folded into the key.
    fn is_shift_pair(base: &str, shifted: &str) -> bool {
        let (Some(_), Some(shifted_char)) = (single_char(base), single_char(shifted)) else {
            return false;
        };
        base != shifted
            && !shifted_char.is_control()
            && shifted_char.to_lowercase().eq(shifted_char.to_uppercase())
    }

    fn single_char(key: &str) -> Option<char> {
        let mut chars = key.chars();
        let first = chars.next()?;
        chars.next().is_none().then_some(first)
    }

    fn layout_state(
        keymap: &xkb::Keymap,
        layout: xkb::LayoutIndex,
        depressed_mods: xkb::ModMask,
    ) -> xkb::State {
        let mut state = xkb::State::new(keymap);
        state.update_mask(depressed_mods, 0, 0, 0, 0, layout);
        state
    }

    fn mod_mask(keymap: &xkb::Keymap, name: &str) -> xkb::ModMask {
        let index = keymap.mod_get_index(name);
        if index == xkb::MOD_INVALID {
            0
        } else {
            1 << index
        }
    }

    #[cfg(test)]
    mod tests {
        use gpui::PlatformKeyboardLayout;

        use super::*;

        fn keymap(layouts: &str) -> xkb::Keymap {
            // These fixtures compile layout names from local files, unlike server keymaps.
            let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
            xkb::Keymap::new_from_names(
                &context,
                "",
                "pc105",
                layouts,
                "",
                None,
                xkb::COMPILE_NO_FLAGS,
            )
            .expect("test keymap should compile")
        }

        fn layout(keymap: &xkb::Keymap, group: xkb::LayoutIndex) -> LinuxKeyboardLayout {
            let mut state = xkb::State::new(keymap);
            state.update_mask(0, 0, 0, 0, 0, group);
            LinuxKeyboardLayout::from_xkb(&state)
        }

        fn pairs(layout: &LinuxKeyboardLayout) -> Vec<(String, String)> {
            layout
                .shift_pairs()
                .expect("an xkb layout reports shift pairs")
                .iter()
                .map(|(base, shifted)| (base.to_string(), shifted.to_string()))
                .collect()
        }

        fn shifted<'a>(pairs: &'a [(String, String)], base: &str) -> Option<&'a str> {
            pairs
                .iter()
                .find(|(candidate, _)| candidate == base)
                .map(|(_, shifted)| shifted.as_str())
        }

        /// Every pair must be what key-down events carry for the key that dispatches the base.
        fn assert_pairs_match_key_events(keymap: &xkb::Keymap, group: xkb::LayoutIndex) {
            let layout = layout(keymap, group);
            let pairs = pairs(&layout);
            assert!(!pairs.is_empty());

            let translation = ShiftTranslation::new(keymap, group);
            for (base, shifted) in &pairs {
                let keycode = keycodes(keymap)
                    .find(|keycode| translation.keys(*keycode).0 == *base)
                    .expect("a key dispatches every base");
                let keystroke =
                    keystroke_from_xkb(&translation.shifted, Modifiers::control_shift(), keycode);
                assert_eq!(keystroke.key, *shifted, "Shift+{base:?}");
                assert_eq!(keystroke.modifiers, Modifiers::control(), "Shift+{base:?}");
            }
        }

        #[test]
        fn us_layout_pairs_digits_and_symbols() {
            let keymap = keymap("us");
            let layout = layout(&keymap, 0);
            assert_eq!(layout.name(), "English (US)");
            let pairs = pairs(&layout);
            for (base, expected) in [
                ("1", "!"),
                ("2", "@"),
                ("3", "#"),
                ("4", "$"),
                ("5", "%"),
                ("6", "^"),
                ("7", "&"),
                ("8", "*"),
                ("9", "("),
                ("0", ")"),
                ("-", "_"),
                ("=", "+"),
                ("[", "{"),
                ("]", "}"),
                ("\\", "|"),
                (";", ":"),
                ("'", "\""),
                ("`", "~"),
                (",", "<"),
                (".", ">"),
                ("/", "?"),
            ] {
                assert_eq!(shifted(&pairs, base), Some(expected), "us {base:?}");
            }
            assert_eq!(shifted(&pairs, "a"), None);
            assert_pairs_match_key_events(&keymap, 0);
        }

        #[test]
        fn german_layout_pairs_follow_the_layout() {
            let keymap = keymap("de");
            let pairs = pairs(&layout(&keymap, 0));
            for (base, expected) in [
                ("1", "!"),
                ("2", "\""),
                ("3", "§"),
                ("7", "/"),
                ("0", "="),
                ("+", "*"),
                ("#", "'"),
                (",", ";"),
                (".", ":"),
                ("-", "_"),
                ("<", ">"),
            ] {
                assert_eq!(shifted(&pairs, base), Some(expected), "de {base:?}");
            }
            assert_pairs_match_key_events(&keymap, 0);
        }

        #[test]
        fn latin_letters_do_not_alias_shifted_punctuation_shortcuts() {
            let keymap = keymap("de");
            let translation = ShiftTranslation::new(&keymap, 0);
            for (position, expected) in [("AD11", "ü"), ("AC10", "ö"), ("AC11", "ä")] {
                let keycode = keymap.key_by_name(position).unwrap();
                let ordinary = keystroke_from_xkb(&translation.base, Modifiers::control(), keycode);
                assert_eq!(ordinary.key, expected, "Control+{expected}");
                let shifted =
                    keystroke_from_xkb(&translation.shifted, Modifiers::control_shift(), keycode);
                assert_eq!(shifted.key, expected, "Control+Shift+{expected}");
                assert_eq!(shifted.modifiers, Modifiers::control_shift());
            }
            // Control+ö must remain distinct from the Settings shortcut Control+Shift+comma.
            let comma = keymap.key_by_name("AB08").unwrap();
            let settings =
                keystroke_from_xkb(&translation.shifted, Modifiers::control_shift(), comma);
            assert_eq!(settings.key, ";");
            assert_eq!(settings.modifiers, Modifiers::control());
        }

        #[test]
        fn french_layout_pairs_shift_to_digits() {
            let keymap = keymap("fr");
            let pairs = pairs(&layout(&keymap, 0));
            for (base, expected) in [
                ("&", "1"),
                ("é", "2"),
                ("\"", "3"),
                ("'", "4"),
                ("(", "5"),
                ("-", "6"),
                ("è", "7"),
                ("_", "8"),
                ("ç", "9"),
                ("à", "0"),
                ("=", "+"),
                (",", "?"),
                (";", "."),
                (":", "/"),
            ] {
                assert_eq!(shifted(&pairs, base), Some(expected), "fr {base:?}");
            }
            assert_pairs_match_key_events(&keymap, 0);
        }

        #[test]
        fn pairs_follow_the_active_group() {
            let keymap = keymap("us,de");
            assert_eq!(shifted(&pairs(&layout(&keymap, 0)), "2"), Some("@"));
            assert_eq!(shifted(&pairs(&layout(&keymap, 1)), "2"), Some("\""));
            assert_pairs_match_key_events(&keymap, 1);
        }

        #[test]
        fn non_latin_layouts_pair_the_same_keys_that_key_events_carry() {
            let keymap = keymap("ru");
            let pairs = pairs(&layout(&keymap, 0));
            assert_eq!(shifted(&pairs, "1"), Some("!"));
            assert_eq!(shifted(&pairs, ";"), Some(":"));
            assert_pairs_match_key_events(&keymap, 0);
        }
    }
}
