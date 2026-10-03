//! Dead-key and Compose sequences through XKB's compose tables, shared by the Wayland and X11
//! clients. Sequence text is shown as marked text and committed through the input handler, the
//! way an input method commits text, so it reaches text input even when the focused view handles
//! key-down events itself.

use gpui::Keystroke;
use xkbcommon::xkb::{self, Keysym, compose};

use crate::linux::keystroke_underlying_dead_key;

/// A change to the focused input handler's text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ComposeText {
    /// Replace the marked text, or the selection, with this text and mark it.
    Mark(String),
    /// Replace the marked text, or the selection, with this text.
    Insert(String),
    /// Leave the current text in place but no longer marked.
    Unmark,
    /// Remove the marked text.
    DeleteMarked,
}

/// The result of feeding one key press to a compose state.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct ComposeStep {
    /// The text changes to apply, in order, before dispatching `key_down`.
    pub text: Vec<ComposeText>,
    /// The key-down event to dispatch, if the key still acts as a key.
    pub key_down: Option<Keystroke>,
    /// The marked text after this step.
    pub pre_edit: Option<String>,
}

/// Feeds the press of `keysym`, translated as `keystroke`, to `compose`. `pre_edit` is the
/// marked text of the sequence in progress.
pub(super) fn feed_compose(
    compose: &mut compose::State,
    keysym: Keysym,
    mut keystroke: Keystroke,
    pre_edit: Option<String>,
) -> ComposeStep {
    compose.feed(keysym);
    match compose.status() {
        compose::Status::Composing => {
            let pre_edit = compose
                .utf8()
                .or_else(|| keystroke_underlying_dead_key(keysym))
                .unwrap_or_default();
            ComposeStep {
                text: vec![ComposeText::Mark(pre_edit.clone())],
                key_down: None,
                pre_edit: Some(pre_edit),
            }
        }
        compose::Status::Composed => match compose.utf8() {
            // The key completes the sequence, so its own key-down would repeat the text.
            Some(text) => ComposeStep {
                text: vec![ComposeText::Insert(text), ComposeText::Unmark],
                key_down: None,
                pre_edit: None,
            },
            None => {
                if let Some(keysym) = compose.keysym() {
                    keystroke.key = xkb::keysym_get_name(keysym);
                }
                keystroke.key_char = None;
                ComposeStep {
                    text: vec![ComposeText::DeleteMarked],
                    key_down: Some(keystroke),
                    pre_edit: None,
                }
            }
        },
        compose::Status::Cancelled => {
            let mut text = Vec::new();
            if let Some(pre_edit) = pre_edit {
                text.push(ComposeText::Insert(pre_edit));
            }
            // The key that broke the sequence may start the next one.
            compose.feed(keysym);
            let pre_edit = (compose.status() == compose::Status::Composing)
                .then(|| {
                    compose
                        .utf8()
                        .or_else(|| keystroke_underlying_dead_key(keysym))
                })
                .flatten();
            match &pre_edit {
                Some(pre_edit) => text.push(ComposeText::Mark(pre_edit.clone())),
                None => text.push(ComposeText::Unmark),
            }
            ComposeStep {
                key_down: pre_edit.is_none().then_some(keystroke),
                text,
                pre_edit,
            }
        }
        compose::Status::Nothing => ComposeStep {
            text: Vec::new(),
            key_down: Some(keystroke),
            pre_edit,
        },
    }
}

/// Physical keys used for composed text stay consumed until release, even if focus moves.
#[derive(Default)]
pub(super) struct ComposeKeys {
    consumed: Vec<u32>,
}

impl ComposeKeys {
    pub fn press(&mut self, keycode: u32, repeated: bool, step: &ComposeStep) {
        if !repeated {
            self.consumed.retain(|&key| key != keycode);
        }
        if (step.pre_edit.is_some() || step.key_down.is_none()) && !self.consumed.contains(&keycode)
        {
            self.consumed.push(keycode);
        }
    }

    #[cfg(feature = "x11")]
    pub fn consumes(&self, keycode: u32) -> bool {
        self.consumed.contains(&keycode)
    }

    pub fn release(&mut self, keycode: u32) -> bool {
        if let Some(index) = self.consumed.iter().position(|&key| key == keycode) {
            self.consumed.swap_remove(index);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMPOSE_TABLE: &str = r#"
<dead_acute> <e> : "é" eacute
<dead_acute> <dead_acute> : "´" acute
<Multi_key> <o> <c> : "©" copyright
<dead_grave> <F1> : "" F1
"#;

    fn compose_state() -> compose::State {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let table = compose::Table::new_from_buffer(
            &context,
            COMPOSE_TABLE,
            "C",
            compose::FORMAT_TEXT_V1,
            compose::COMPILE_NO_FLAGS,
        )
        .expect("test compose table should compile");
        compose::State::new(&table, compose::STATE_NO_FLAGS)
    }

    fn keystroke(key: &str, key_char: Option<&str>) -> Keystroke {
        Keystroke {
            modifiers: Default::default(),
            key: key.into(),
            key_char: key_char.map(Into::into),
        }
    }

    #[test]
    fn plain_keys_pass_through() {
        let mut compose = compose_state();
        let step = feed_compose(&mut compose, Keysym::a, keystroke("a", Some("a")), None);
        assert_eq!(
            step,
            ComposeStep {
                text: Vec::new(),
                key_down: Some(keystroke("a", Some("a"))),
                pre_edit: None,
            }
        );
    }

    #[test]
    fn dead_key_marks_its_text() {
        let mut compose = compose_state();
        let step = feed_compose(
            &mut compose,
            Keysym::dead_acute,
            keystroke("dead_acute", None),
            None,
        );
        assert_eq!(step.text, [ComposeText::Mark("´".into())]);
        assert_eq!(step.key_down, None);
        assert_eq!(step.pre_edit.as_deref(), Some("´"));
    }

    #[test]
    fn composed_text_is_committed_through_the_input_handler_without_a_key_down() {
        let mut compose = compose_state();
        let step = feed_compose(
            &mut compose,
            Keysym::dead_acute,
            keystroke("dead_acute", None),
            None,
        );
        let step = feed_compose(
            &mut compose,
            Keysym::e,
            keystroke("e", Some("e")),
            step.pre_edit,
        );
        assert_eq!(
            step,
            ComposeStep {
                text: vec![ComposeText::Insert("é".into()), ComposeText::Unmark],
                key_down: None,
                pre_edit: None,
            }
        );
    }

    #[test]
    fn multi_key_sequences_commit_their_text() {
        let mut compose = compose_state();
        let mut pre_edit = None;
        for (keysym, key) in [(Keysym::Multi_key, "multi_key"), (Keysym::o, "o")] {
            let step = feed_compose(&mut compose, keysym, keystroke(key, None), pre_edit);
            pre_edit = step.pre_edit;
        }
        let step = feed_compose(&mut compose, Keysym::c, keystroke("c", Some("c")), pre_edit);
        assert_eq!(
            step.text,
            [ComposeText::Insert("©".into()), ComposeText::Unmark]
        );
        assert_eq!(step.key_down, None);
    }

    #[test]
    fn cancelled_sequence_commits_the_dead_key_and_unmarks_before_the_key() {
        let mut compose = compose_state();
        let step = feed_compose(
            &mut compose,
            Keysym::dead_acute,
            keystroke("dead_acute", None),
            None,
        );
        let step = feed_compose(
            &mut compose,
            Keysym::q,
            keystroke("q", Some("q")),
            step.pre_edit,
        );
        assert_eq!(
            step,
            ComposeStep {
                text: vec![ComposeText::Insert("´".into()), ComposeText::Unmark],
                key_down: Some(keystroke("q", Some("q"))),
                pre_edit: None,
            }
        );
    }

    #[test]
    fn cancelling_dead_key_starts_the_next_sequence() {
        let mut compose = compose_state();
        let step = feed_compose(
            &mut compose,
            Keysym::dead_acute,
            keystroke("dead_acute", None),
            None,
        );
        let step = feed_compose(
            &mut compose,
            Keysym::dead_grave,
            keystroke("dead_grave", None),
            step.pre_edit,
        );
        assert_eq!(
            step.text,
            [
                ComposeText::Insert("´".into()),
                ComposeText::Mark("`".into())
            ]
        );
        assert_eq!(step.pre_edit.as_deref(), Some("`"));
    }

    #[test]
    fn sequence_without_text_clears_the_marked_text() {
        let mut compose = compose_state();
        let step = feed_compose(
            &mut compose,
            Keysym::dead_grave,
            keystroke("dead_grave", None),
            None,
        );
        let step = feed_compose(
            &mut compose,
            Keysym::F1,
            keystroke("f1", None),
            step.pre_edit,
        );
        assert_eq!(step.text, [ComposeText::DeleteMarked]);
        assert_eq!(step.key_down, Some(keystroke("F1", None)));
        assert_eq!(step.pre_edit, None);
    }
    #[test]
    fn consumed_keys_suppress_releases_after_commit_and_focus_loss() {
        let mut compose = compose_state();
        let mut keys = ComposeKeys::default();
        let step = feed_compose(
            &mut compose,
            Keysym::dead_acute,
            keystroke("dead_acute", None),
            None,
        );
        keys.press(48, false, &step);
        assert_eq!(step.key_down, None);
        assert!(keys.release(48));
        let step = feed_compose(
            &mut compose,
            Keysym::e,
            keystroke("e", Some("e")),
            step.pre_edit,
        );
        keys.press(26, false, &step);
        assert_eq!(
            step.text,
            [ComposeText::Insert("é".into()), ComposeText::Unmark]
        );
        // Focus loss cancels composition, but must not turn its held key into an ordinary release.
        compose.reset();
        assert!(keys.release(26));
        assert!(!keys.release(26));
        let step = feed_compose(&mut compose, Keysym::e, keystroke("e", Some("e")), None);
        keys.press(26, false, &step);
        assert!(!keys.release(26));
    }

    #[test]
    fn fresh_press_retires_consumption_when_release_was_delivered_outside_the_app() {
        let mut compose = compose_state();
        let mut keys = ComposeKeys::default();
        let step = feed_compose(
            &mut compose,
            Keysym::dead_acute,
            keystroke("dead_acute", None),
            None,
        );
        keys.press(48, false, &step);
        compose.reset();
        let step = feed_compose(&mut compose, Keysym::a, keystroke("a", Some("a")), None);
        keys.press(48, false, &step);
        assert!(!keys.release(48));
    }
}
