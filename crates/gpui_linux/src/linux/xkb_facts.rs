//! The XKB facts behind a key event that GPUI's portable keystrokes leave out, shared by the
//! Wayland and X11 clients.

use gpui::{Capslock, ModifierRole, Modifiers, ModifiersChangedEvent, NativeKeyEvent};
use xkbcommon::xkb::{self, KeyDirection, Keycode, Keysym};

/// XKB keycodes are evdev scancodes offset by 8.
const EVDEV_KEYCODE_OFFSET: u32 = 8;

/// `XKB_CONSUMED_MODE_GTK` from `xkbcommon.h`.
const XKB_CONSUMED_MODE_GTK: std::ffi::c_int = 1;

#[link(name = "xkbcommon")]
unsafe extern "C" {
    /// Available since libxkbcommon 0.7.0; the `xkbcommon` crate does not wrap it.
    fn xkb_state_key_get_consumed_mods2(
        state: *mut xkb::ffi::xkb_state,
        key: xkb::ffi::xkb_keycode_t,
        mode: std::ffi::c_int,
    ) -> xkb::ffi::xkb_mod_mask_t;
}

/// The facts for a non-modifier key translated with `state`, which must be the state the key's
/// keystroke was translated with.
pub(super) fn native_key_event(state: &xkb::State, keycode: Keycode) -> NativeKeyEvent {
    let keymap = state.get_keymap();
    // XKB reports every modifier that would change the key's level, active or not.
    let consumed = unsafe {
        xkb_state_key_get_consumed_mods2(state.get_raw_ptr(), keycode.raw(), XKB_CONSUMED_MODE_GTK)
    } & state.serialize_mods(xkb::STATE_MODS_EFFECTIVE);
    NativeKeyEvent {
        scancode: evdev_scancode(keycode),
        modifiers: effective_modifiers(state),
        consumed: modifiers_from_mask(&keymap, consumed),
        unshifted: unshifted_char(state, keycode),
        caps_lock: state.mod_name_is_active(xkb::MOD_NAME_CAPS, xkb::STATE_MODS_EFFECTIVE),
        num_lock: state.mod_name_is_active(xkb::MOD_NAME_NUM, xkb::STATE_MODS_EFFECTIVE),
        modifier_key: None,
        modifier_role: None,
    }
}

/// The facts for a modifier key's own press or release. `state` is the state before the
/// transition; the reported modifiers and locks are the state after it.
///
/// A press is applied to `state` exactly as XKB applies it. A release removes the modifiers the
/// key sets while held unless another key in `held_keys` still contributes them. Lock changes
/// that XKB applies on release arrive with the next aggregate modifier state.
pub(super) fn modifier_key_event(
    state: &xkb::State,
    keycode: Keycode,
    pressed: bool,
    held_keys: impl Iterator<Item = Keycode>,
) -> NativeKeyEvent {
    let after = state_after_modifier_key(state, keycode, pressed, held_keys);
    let scancode = evdev_scancode(keycode);
    NativeKeyEvent {
        scancode,
        modifiers: effective_modifiers(&after),
        consumed: Modifiers::none(),
        unshifted: None,
        caps_lock: after.mod_name_is_active(xkb::MOD_NAME_CAPS, xkb::STATE_MODS_EFFECTIVE),
        num_lock: after.mod_name_is_active(xkb::MOD_NAME_NUM, xkb::STATE_MODS_EFFECTIVE),
        modifier_key: Some((scancode, pressed)),
        modifier_role: modifier_role(state, keycode),
    }
}

/// The modifier key `keycode` acts as in `state`'s layout. XKB options can bind a modifier to any
/// key, and the keysym need not name it: `caps:ctrl_modifier` keeps `Caps_Lock` but sets Control.
/// So the single portable modifier the key sets while held decides the role, and the keysym
/// decides its side. A key that sets none can still name a lock.
fn modifier_role(state: &xkb::State, keycode: Keycode) -> Option<ModifierRole> {
    let held = modifiers_from_mask(
        &state.get_keymap(),
        modifiers_set_while_held(state, std::iter::once(keycode)),
    );
    let keysym = state.key_get_one_sym(keycode);
    let right = matches!(
        keysym,
        Keysym::Shift_R
            | Keysym::Control_R
            | Keysym::Alt_R
            | Keysym::Meta_R
            | Keysym::Super_R
            | Keysym::Hyper_R
    );
    let sided = |left, right_role| Some(if right { right_role } else { left });
    match (held.shift, held.control, held.alt, held.platform) {
        (true, false, false, false) => sided(ModifierRole::ShiftLeft, ModifierRole::ShiftRight),
        (false, true, false, false) => sided(ModifierRole::ControlLeft, ModifierRole::ControlRight),
        (false, false, true, false) => sided(ModifierRole::AltLeft, ModifierRole::AltRight),
        (false, false, false, true) => {
            sided(ModifierRole::PlatformLeft, ModifierRole::PlatformRight)
        }
        _ => match keysym {
            Keysym::Caps_Lock => Some(ModifierRole::CapsLock),
            Keysym::Num_Lock => Some(ModifierRole::NumLock),
            _ => None,
        },
    }
}

/// The modifiers that holding `keys` alone sets in the layout active in `state`.
fn modifiers_set_while_held(
    state: &xkb::State,
    keys: impl Iterator<Item = Keycode>,
) -> xkb::ModMask {
    let mut held = xkb::State::new(&state.get_keymap());
    held.update_mask(
        0,
        0,
        0,
        0,
        0,
        state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE),
    );
    for key in keys {
        held.update_key(key, KeyDirection::Down);
    }
    held.serialize_mods(xkb::STATE_MODS_DEPRESSED)
}

/// The modifiers-changed event that carries a modifier key's [`modifier_key_event`].
pub(super) fn modifier_key_changed_event(native: &NativeKeyEvent) -> ModifiersChangedEvent {
    ModifiersChangedEvent {
        modifiers: native.modifiers,
        capslock: Capslock {
            on: native.caps_lock,
        },
    }
}

fn state_after_modifier_key(
    state: &xkb::State,
    keycode: Keycode,
    pressed: bool,
    held_keys: impl Iterator<Item = Keycode>,
) -> xkb::State {
    let keymap = state.get_keymap();
    let mut depressed_mods = state.serialize_mods(xkb::STATE_MODS_DEPRESSED);
    if !pressed {
        let released_mods = modifiers_set_while_held(state, std::iter::once(keycode));
        let held_mods =
            modifiers_set_while_held(state, held_keys.filter(|&held_key| held_key != keycode));
        // The aggregate mask has no press counts. Releasing one Shift/Control/Alt key
        // must not clear the bit while another physical key still contributes it.
        depressed_mods &= !(released_mods & !held_mods);
    }

    let mut after = xkb::State::new(&keymap);
    after.update_mask(
        depressed_mods,
        state.serialize_mods(xkb::STATE_MODS_LATCHED),
        state.serialize_mods(xkb::STATE_MODS_LOCKED),
        state.serialize_layout(xkb::STATE_LAYOUT_DEPRESSED),
        state.serialize_layout(xkb::STATE_LAYOUT_LATCHED),
        state.serialize_layout(xkb::STATE_LAYOUT_LOCKED),
    );
    if pressed {
        after.update_key(keycode, KeyDirection::Down);
    }
    after
}

fn evdev_scancode(keycode: Keycode) -> u16 {
    u16::try_from(keycode.raw().saturating_sub(EVDEV_KEYCODE_OFFSET)).unwrap_or(u16::MAX)
}

fn effective_modifiers(state: &xkb::State) -> Modifiers {
    modifiers_from_mask(
        &state.get_keymap(),
        state.serialize_mods(xkb::STATE_MODS_EFFECTIVE),
    )
}

fn modifiers_from_mask(keymap: &xkb::Keymap, mask: xkb::ModMask) -> Modifiers {
    let is_set = |name: &str| {
        let index = keymap.mod_get_index(name);
        index != xkb::MOD_INVALID && index < xkb::ModMask::BITS && mask & (1 << index) != 0
    };
    Modifiers {
        control: is_set(xkb::MOD_NAME_CTRL),
        alt: is_set(xkb::MOD_NAME_ALT),
        shift: is_set(xkb::MOD_NAME_SHIFT),
        platform: is_set(xkb::MOD_NAME_LOGO),
        function: false,
    }
}

/// The character at the key's first shift level in the layout active for the key.
fn unshifted_char(state: &xkb::State, keycode: Keycode) -> Option<char> {
    let layout = state.key_get_layout(keycode);
    let keymap = state.get_keymap();
    let keysym = *keymap.key_get_syms_by_level(keycode, layout, 0).first()?;
    char_for_keysym(keysym)
}

fn char_for_keysym(keysym: Keysym) -> Option<char> {
    match xkb::keysym_to_utf32(keysym) {
        0 => None,
        codepoint => char::from_u32(codepoint),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keymap(layouts: &str, options: Option<&str>) -> xkb::Keymap {
        // These fixtures compile layout names from local files, unlike server keymaps.
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        xkb::Keymap::new_from_names(
            &context,
            "",
            "pc105",
            layouts,
            "",
            options.map(str::to_string),
            xkb::COMPILE_NO_FLAGS,
        )
        .expect("test keymap should compile")
    }

    fn key(keymap: &xkb::Keymap, name: &str) -> Keycode {
        keymap.key_by_name(name).expect("test key should exist")
    }

    fn press(state: &mut xkb::State, keycode: Keycode) {
        state.update_key(keycode, KeyDirection::Down);
    }

    fn release(state: &mut xkb::State, keycode: Keycode) {
        state.update_key(keycode, KeyDirection::Up);
    }

    #[test]
    fn plain_key_reports_scancode_and_unshifted_character() {
        let keymap = keymap("us", None);
        let state = xkb::State::new(&keymap);
        let native = native_key_event(&state, key(&keymap, "AC01"));
        assert_eq!(native.scancode, 30);
        assert_eq!(native.unshifted, Some('a'));
        assert_eq!(native.modifiers, Modifiers::none());
        assert_eq!(native.consumed, Modifiers::none());
        assert!(!native.caps_lock);
        assert!(!native.num_lock);
        assert_eq!(native.modifier_key, None);
    }

    #[test]
    fn shift_is_consumed_by_symbols_and_reported_unfolded() {
        let keymap = keymap("us", None);
        let mut state = xkb::State::new(&keymap);
        press(&mut state, key(&keymap, "LFSH"));
        press(&mut state, key(&keymap, "LCTL"));

        let native = native_key_event(&state, key(&keymap, "AE01"));
        assert_eq!(native.scancode, 2);
        assert_eq!(native.unshifted, Some('1'));
        assert_eq!(native.modifiers, Modifiers::control_shift());
        assert_eq!(native.consumed, Modifiers::shift());
    }

    #[test]
    fn altgr_text_consumes_no_portable_modifier() {
        let keymap = keymap("de", None);
        let mut state = xkb::State::new(&keymap);
        press(&mut state, key(&keymap, "RALT"));

        let q = key(&keymap, "AD01");
        assert_eq!(state.key_get_utf8(q), "@");
        let native = native_key_event(&state, q);
        assert_eq!(native.unshifted, Some('q'));
        assert_eq!(native.modifiers, Modifiers::none());
        assert_eq!(native.consumed, Modifiers::none());
    }

    #[test]
    fn unshifted_character_follows_the_active_layout() {
        let keymap = keymap("us,ru", None);
        let mut state = xkb::State::new(&keymap);
        state.update_mask(0, 0, 0, 0, 0, 1);
        let native = native_key_event(&state, key(&keymap, "AD01"));
        assert_eq!(native.unshifted, Some('й'));
    }

    #[test]
    fn locks_are_reported() {
        let keymap = keymap("us", None);
        let mut state = xkb::State::new(&keymap);
        for lock in ["CAPS", "NMLK"] {
            press(&mut state, key(&keymap, lock));
            release(&mut state, key(&keymap, lock));
        }
        let native = native_key_event(&state, key(&keymap, "AC01"));
        assert!(native.caps_lock);
        assert!(native.num_lock);
    }

    #[test]
    fn modifier_keys_report_sides_and_the_state_after_the_transition() {
        let keymap = keymap("us", None);
        let mut state = xkb::State::new(&keymap);
        let left_shift = key(&keymap, "LFSH");
        let right_control = key(&keymap, "RCTL");

        let pressed = modifier_key_event(&state, left_shift, true, std::iter::empty());
        assert_eq!(pressed.scancode, 42);
        assert_eq!(pressed.modifier_key, Some((42, true)));
        assert_eq!(pressed.modifiers, Modifiers::shift());
        press(&mut state, left_shift);

        let pressed = modifier_key_event(&state, right_control, true, [left_shift].into_iter());
        assert_eq!(pressed.modifier_key, Some((97, true)));
        assert_eq!(pressed.modifiers, Modifiers::control_shift());
        press(&mut state, right_control);

        let released = modifier_key_event(&state, left_shift, false, [right_control].into_iter());
        assert_eq!(released.modifier_key, Some((42, false)));
        assert_eq!(released.modifiers, Modifiers::control());

        let event = modifier_key_changed_event(&released);
        assert_eq!(event.modifiers, Modifiers::control());
        assert!(!event.capslock.on);
    }

    #[test]
    fn modifier_key_release_follows_a_state_without_key_history() {
        // X11 key events carry only the modifier mask from before the event.
        let keymap = keymap("us", None);
        let super_key = key(&keymap, "LWIN");
        let mut key_alone = xkb::State::new(&keymap);
        press(&mut key_alone, super_key);
        let mut state = xkb::State::new(&keymap);
        state.update_mask(
            key_alone.serialize_mods(xkb::STATE_MODS_DEPRESSED),
            0,
            0,
            0,
            0,
            0,
        );
        assert_eq!(
            effective_modifiers(&state),
            Modifiers {
                platform: true,
                ..Modifiers::none()
            }
        );

        let released = modifier_key_event(&state, super_key, false, std::iter::empty());
        assert_eq!(released.scancode, 125);
        assert_eq!(released.modifiers, Modifiers::none());
    }

    #[test]
    fn caps_lock_press_reports_the_lock() {
        let keymap = keymap("us", None);
        let state = xkb::State::new(&keymap);
        let pressed = modifier_key_event(&state, key(&keymap, "CAPS"), true, std::iter::empty());
        assert_eq!(pressed.scancode, 58);
        assert!(pressed.caps_lock);
        assert!(modifier_key_changed_event(&pressed).capslock.on);
    }

    fn role(keymap: &xkb::Keymap, name: &str) -> Option<ModifierRole> {
        let state = xkb::State::new(keymap);
        let native = modifier_key_event(&state, key(keymap, name), true, std::iter::empty());
        native.modifier_role
    }

    #[test]
    fn modifier_keys_report_their_usual_roles() {
        let us = keymap("us", None);
        for (name, expected) in [
            ("LFSH", Some(ModifierRole::ShiftLeft)),
            ("RTSH", Some(ModifierRole::ShiftRight)),
            ("LCTL", Some(ModifierRole::ControlLeft)),
            ("RCTL", Some(ModifierRole::ControlRight)),
            ("LALT", Some(ModifierRole::AltLeft)),
            ("RALT", Some(ModifierRole::AltRight)),
            ("LWIN", Some(ModifierRole::PlatformLeft)),
            ("RWIN", Some(ModifierRole::PlatformRight)),
            ("CAPS", Some(ModifierRole::CapsLock)),
            ("NMLK", Some(ModifierRole::NumLock)),
        ] {
            assert_eq!(role(&us, name), expected, "{name}");
        }
        // AltGr shifts levels and acts as no portable modifier key.
        assert_eq!(role(&keymap("de", None), "RALT"), None);
        let state = xkb::State::new(&us);
        assert_eq!(
            native_key_event(&state, key(&us, "AC01")).modifier_role,
            None
        );
    }

    #[test]
    fn remapped_modifier_keys_report_the_role_the_keymap_gives_them() {
        for (options, name, expected) in [
            // The keysym stays Caps_Lock while the key sets Control.
            ("caps:ctrl_modifier", "CAPS", ModifierRole::ControlLeft),
            ("ctrl:nocaps", "CAPS", ModifierRole::ControlLeft),
            ("ctrl:swapcaps", "CAPS", ModifierRole::ControlLeft),
            ("ctrl:swapcaps", "LCTL", ModifierRole::CapsLock),
            ("ctrl:swap_lalt_lctl", "LALT", ModifierRole::ControlLeft),
            ("ctrl:swap_lalt_lctl", "LCTL", ModifierRole::AltLeft),
            ("ctrl:swap_ralt_rctl", "RALT", ModifierRole::ControlRight),
            ("ctrl:swap_ralt_rctl", "RCTL", ModifierRole::AltRight),
            ("altwin:swap_alt_win", "LALT", ModifierRole::PlatformLeft),
            ("altwin:swap_alt_win", "LWIN", ModifierRole::AltLeft),
        ] {
            let keymap = keymap("us", Some(options));
            assert_eq!(role(&keymap, name), Some(expected), "{options} {name}");
        }
    }

    #[test]
    fn caps_lock_as_control_sets_and_releases_control() {
        let keymap = keymap("us", Some("caps:ctrl_modifier"));
        let caps = key(&keymap, "CAPS");
        let left_control = key(&keymap, "LCTL");
        let mut state = xkb::State::new(&keymap);

        let pressed = modifier_key_event(&state, caps, true, std::iter::empty());
        assert_eq!(pressed.modifier_key, Some((58, true)));
        assert_eq!(pressed.modifiers, Modifiers::control());
        assert!(!pressed.caps_lock);
        press(&mut state, caps);
        // Shortcuts and terminal input see Control+C.
        let c = key(&keymap, "AB03");
        assert_eq!(native_key_event(&state, c).modifiers, Modifiers::control());
        let modifiers = crate::linux::modifiers_from_xkb(&state);
        assert_eq!(modifiers, Modifiers::control());
        let keystroke = crate::linux::keystroke_from_xkb(&state, modifiers, c);
        assert_eq!(keystroke.key, "c");
        assert_eq!(keystroke.modifiers, Modifiers::control());
        press(&mut state, left_control);

        // Control stays set while the ordinary Control key still holds it.
        let released = modifier_key_event(&state, caps, false, [left_control].into_iter());
        assert_eq!(released.modifier_role, Some(ModifierRole::ControlLeft));
        assert_eq!(released.modifiers, Modifiers::control());
        release(&mut state, caps);
        let released = modifier_key_event(&state, left_control, false, std::iter::empty());
        assert_eq!(released.modifiers, Modifiers::none());
        assert!(!released.caps_lock);
    }

    #[test]
    fn releasing_one_modifier_side_preserves_the_other_held_side() {
        let keymap = keymap("us", None);
        for names in [
            ["LFSH", "RTSH"],
            ["LCTL", "RCTL"],
            ["LALT", "RALT"],
            ["LWIN", "RWIN"],
        ] {
            for reverse_press in [false, true] {
                for reverse_release in [false, true] {
                    let mut keys = names.map(|name| key(&keymap, name));
                    if reverse_press {
                        keys.reverse();
                    }
                    let mut state = xkb::State::new(&keymap);
                    for lock in ["CAPS", "NMLK"] {
                        let lock = key(&keymap, lock);
                        press(&mut state, lock);
                        release(&mut state, lock);
                    }
                    let mut transitions = vec![(keys[0], true), (keys[1], true)];
                    if reverse_release {
                        keys.reverse();
                    }
                    transitions.extend([(keys[0], false), (keys[1], false)]);
                    let mut held_keys = Vec::new();
                    for (keycode, pressed) in transitions {
                        if pressed {
                            held_keys.push(keycode);
                        } else {
                            held_keys.retain(|&held_key| held_key != keycode);
                        }
                        // Backend states arrive as aggregate masks, without XKB's local
                        // per-key press counts.
                        let mut aggregate = xkb::State::new(&keymap);
                        aggregate.update_mask(
                            state.serialize_mods(xkb::STATE_MODS_DEPRESSED),
                            state.serialize_mods(xkb::STATE_MODS_LATCHED),
                            state.serialize_mods(xkb::STATE_MODS_LOCKED),
                            state.serialize_layout(xkb::STATE_LAYOUT_DEPRESSED),
                            state.serialize_layout(xkb::STATE_LAYOUT_LATCHED),
                            state.serialize_layout(xkb::STATE_LAYOUT_LOCKED),
                        );
                        let reported = modifier_key_event(
                            &aggregate,
                            keycode,
                            pressed,
                            held_keys.iter().copied(),
                        );
                        state.update_key(
                            keycode,
                            if pressed {
                                KeyDirection::Down
                            } else {
                                KeyDirection::Up
                            },
                        );
                        assert_eq!(
                            reported.modifiers,
                            effective_modifiers(&state),
                            "{names:?}: key={keycode:?}, pressed={pressed}"
                        );
                        assert_eq!(
                            reported.modifier_key,
                            Some((evdev_scancode(keycode), pressed))
                        );
                        assert!(reported.caps_lock);
                        assert!(reported.num_lock);
                    }
                }
            }
        }
    }
}
