//! Native regression fixture for key-equivalent offers and exact-once key-down fallback.
//! Run with `cargo run -p gpui_macos --example key_equivalents --features font-kit`.

#[cfg(target_os = "macos")]
fn main() {
    use cocoa::{
        appkit::{NSEventModifierFlags, NSEventType},
        base::{BOOL, NO, YES, id, nil},
        foundation::NSPoint,
    };
    use gpui::{
        Application, Context, FocusHandle, KeyDownEvent, Render, Window, WindowOptions, div,
        prelude::*,
    };
    use objc::{class, msg_send, sel, sel_impl};
    use objc2::{rc::Retained, runtime::AnyObject};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::{cell::RefCell, rc::Rc, time::Duration};

    struct Fixture {
        focus: FocusHandle,
        events: Rc<RefCell<Vec<(bool, KeyDownEvent)>>>,
    }
    impl Render for Fixture {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .track_focus(&self.focus)
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                    let offering = window.is_offering_key_equivalent();
                    this.events.borrow_mut().push((offering, event.clone()));
                    if !offering || event.keystroke.key == "x" {
                        cx.stop_propagation();
                    }
                }))
        }
    }

    Application::with_platform(Rc::new(gpui_macos::MacPlatform::new(false))).run(|cx| {
        let events = Rc::new(RefCell::new(Vec::new()));
        let recorded = events.clone();
        let handle = cx.open_window(WindowOptions::default(), |window, cx| cx.new(|cx| {
            let focus = cx.focus_handle();
            focus.focus(window, cx);
            Fixture { focus, events: recorded }
        })).expect("native fixture window");
        cx.activate(true);
        cx.spawn(async move |cx| {
            cx.background_executor().timer(Duration::from_millis(100)).await;
            let view = handle.update(cx, |_, window, _| {
                let handle = HasWindowHandle::window_handle(window).expect("native handle");
                let RawWindowHandle::AppKit(native) = handle.as_raw() else { panic!("AppKit handle") };
                native.ns_view.as_ptr() as id
            }).expect("fixture is open");
            // Native messages run outside a GPUI update because their callbacks update the window.
            unsafe {
                let window: id = msg_send![view, window];
                let number: isize = msg_send![window, windowNumber];
                let make_event = |text: &str, code: u16, repeat: BOOL, flags: NSEventModifierFlags| {
                    let text_string = objc2_foundation::NSString::from_str(text);
                    let text = Retained::as_ptr(&text_string) as id;
                    let event: id = msg_send![class!(NSEvent),
                        keyEventWithType: NSEventType::NSKeyDown
                        location: NSPoint::new(0., 0.)
                        modifierFlags: flags timestamp: 0.0f64 windowNumber: number context: nil
                        characters: text charactersIgnoringModifiers: text isARepeat: repeat keyCode: code];
                    Retained::retain(event.cast::<AnyObject>()).expect("native key event")
                };
                let offer = |event: &Retained<AnyObject>| -> BOOL {
                    msg_send![view, performKeyEquivalent: Retained::as_ptr(event) as id]
                };
                let down = |event: &Retained<AnyObject>| {
                    let _: () = msg_send![view, keyDown: Retained::as_ptr(event) as id];
                };
                let first = make_event("y", 16, NO, NSEventModifierFlags::NSCommandKeyMask);
                assert_eq!(offer(&first), NO);
                assert_eq!(events.borrow().len(), 1);
                assert!(events.borrow()[0].0);
                down(&first);
                down(&first);
                down(&first);
                assert_eq!(events.borrow().len(), 2, "the same native event is delivered once");
                assert!(!events.borrow()[1].0);

                // Identical keys and zero timestamps still identify distinct native events.
                let second = make_event("y", 16, YES, NSEventModifierFlags::NSCommandKeyMask);
                assert_eq!(offer(&second), NO);
                down(&second);
                assert_eq!(events.borrow().len(), 4);
                assert!(events.borrow()[3].1.is_held);

                let bound = make_event("x", 7, NO, NSEventModifierFlags::NSCommandKeyMask);
                assert_eq!(offer(&bound), YES);
                down(&bound);
                assert_eq!(events.borrow().len(), 5, "a view action has already consumed the key");

                let function = make_event("e", 14, NO, NSEventModifierFlags::NSFunctionKeyMask);
                assert_eq!(offer(&function), NO, "Fn equivalents reach native menu arbitration");
                assert_eq!(events.borrow().len(), 6);
                down(&function);
                assert_eq!(events.borrow().len(), 7);
                assert!(!events.borrow()[6].0);

                // A real native menu can claim a remapped shortcut after the view declines it.
                let target: id = msg_send![class!(NSMutableArray), array];
                let menu: id = msg_send![class!(NSMenu), new];
                let _: () = msg_send![menu, setAutoenablesItems: NO];
                let text_string = objc2_foundation::NSString::from_str(" ");
                let text = Retained::as_ptr(&text_string) as id;
                let item: id = msg_send![class!(NSMenuItem), alloc];
                let item: id = msg_send![item, initWithTitle: text action: sel!(addObject:) keyEquivalent: text];
                let flags = NSEventModifierFlags::NSCommandKeyMask | NSEventModifierFlags::NSControlKeyMask;
                let _: () = msg_send![item, setKeyEquivalentModifierMask: flags];
                let _: () = msg_send![item, setTarget: target];
                let _: () = msg_send![menu, addItem: item];
                let remapped = make_event(" ", 49, NO, flags);
                assert_eq!(offer(&remapped), NO);
                let claimed: BOOL = msg_send![menu, performKeyEquivalent: Retained::as_ptr(&remapped) as id];
                assert_eq!(claimed, YES);
                let actions: usize = msg_send![target, count];
                assert_eq!(actions, 1);
                assert_eq!(events.borrow().len(), 8, "the native menu claim produces no raw key down");
                let _: () = msg_send![item, release];
                let _: () = msg_send![menu, release];

                let arrow = make_event("\u{f702}", 123, YES, NSEventModifierFlags::empty());
                down(&arrow);
                assert_eq!(events.borrow().len(), 9);
                assert!(events.borrow()[8].1.is_held, "editing-selector fallback preserves repeat metadata");

                let option_arrow = make_event("\u{f702}", 123, YES, NSEventModifierFlags::NSAlternateKeyMask);
                assert_eq!(offer(&option_arrow), NO, "editing selectors retain the native offer phase");
                assert_eq!(events.borrow().len(), 10);
                assert!(events.borrow()[9].0);
                down(&option_arrow);
                assert_eq!(events.borrow().len(), 11);
                assert!(!events.borrow()[10].0);
                assert!(events.borrow()[10].1.is_held);

                let escape = make_event("\u{1b}", 53, NO, NSEventModifierFlags::empty());
                down(&escape);
                assert_eq!(events.borrow().len(), 12);
                assert_eq!(events.borrow()[11].1.keystroke.key, "escape");
                assert!(!events.borrow()[11].0);
            }
            // Drive AppKit's real cancellation route, including currentEvent ownership.
            events.borrow_mut().clear();
            unsafe {
                let window: id = msg_send![view, window];
                let number: isize = msg_send![window, windowNumber];
                let text_string = objc2_foundation::NSString::from_str(".");
                let text = Retained::as_ptr(&text_string) as id;
                let event: id = msg_send![class!(NSEvent),
                    keyEventWithType: NSEventType::NSKeyDown
                    location: NSPoint::new(0., 0.)
                    modifierFlags: NSEventModifierFlags::NSCommandKeyMask
                    timestamp: 0.0f64 windowNumber: number context: nil
                    characters: text charactersIgnoringModifiers: text isARepeat: NO keyCode: 47u16];
                let app: id = msg_send![class!(NSApplication), sharedApplication];
                let _: () = msg_send![app, postEvent: event atStart: NO];
            }
            cx.background_executor().timer(Duration::from_millis(100)).await;
            {
                let events = events.borrow();
                assert_eq!(events.len(), 2, "Command-Period has one offer and one ordinary delivery");
                assert!(events[0].0);
                assert!(!events[1].0);
                assert_eq!(events[1].1.keystroke.key, ".");
                assert!(events[1].1.keystroke.modifiers.platform);
            }
            println!("native key-equivalent regression fixture passed");
            cx.update(|cx| cx.quit());
        }).detach();
    });
}

#[cfg(not(target_os = "macos"))]
fn main() {}
