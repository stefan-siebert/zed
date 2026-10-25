//! Keystrokes for ⌘⇧ on keys that make no letter.
//!
//! With Command held, `UIKey.characters` ignores Shift: measured on the
//! iPadOS 27.0 simulator with a German layout, ⌘⇧1 reports "1", ⌘⇧7 "7" and
//! ⌘⇧ß "/", where the layout makes "!", "/" and "?". Under Control the
//! characters were right ("ctrl-?" for ⌃⇧ß). iOS has no API that translates a
//! key through the layout, as `gpui_macos` does with `UCKeyTranslate`, but
//! UIKit's key-command matching knows more of it.
//!
//! So such a press goes to UIKit before GPUI (`keyboard.rs`). The view
//! controller offers a hidden key command for every ASCII character that is
//! no letter, with Command and with Command-Control; the one UIKit matches
//! names the character the keystroke is made from. Measured as above:
//! ⌘⇧1 → "!", ⌘⇧9 → ")", ⌘⇧+ → "*", but ⌘⇧ß → "/" and nothing for ⌘⇧7 —
//! the order of the commands and their automatic localization changed none
//! of it. Without a match the keystroke keeps the shift ("cmd-shift-7"), so
//! it cannot fire a binding of the unshifted key.
//!
//! The commands can be performed only while such a press waits
//! (`canPerformAction:withSender:`), so they never take a key from the menu
//! bar or the system otherwise. One that ran took the press from the menu
//! bar, so a menu command with the same shortcut runs when GPUI leaves the
//! keystroke (`AppMenus::perform_shortcut`). The system's own commands, such
//! as Help's ⌘? menu search, do not get it.

use std::cell::OnceCell;

use objc2::{MainThreadMarker, rc::Retained, runtime::Sel, sel};
use objc2_foundation::{NSArray, NSObjectProtocol, NSString};
use objc2_ui_kit::{UIKeyCommand, UIKeyModifierFlags, UIMenuElementRepeatBehavior, UIMenuLeaf};

/// The action of every hidden key command.
pub(super) fn action() -> Sel {
    sel!(gpuiShiftedKey:)
}

/// The hidden key commands, made once.
pub(super) fn key_commands(main_thread: MainThreadMarker) -> Retained<NSArray<UIKeyCommand>> {
    thread_local! {
        static COMMANDS: OnceCell<Retained<NSArray<UIKeyCommand>>> = const { OnceCell::new() };
    }
    COMMANDS.with(|commands| commands.get_or_init(|| make(main_thread)).clone())
}

fn make(main_thread: MainThreadMarker) -> Retained<NSArray<UIKeyCommand>> {
    let modifiers = [
        UIKeyModifierFlags::Command,
        UIKeyModifierFlags::Command | UIKeyModifierFlags::Control,
    ];
    let mut commands = Vec::new();
    for character in ('!'..='~').filter(|character| !character.is_ascii_alphabetic()) {
        let input = NSString::from_str(character.encode_utf8(&mut [0; 4]));
        for flags in modifiers {
            // No title: the command shows in no menu and no shortcut overlay.
            let command = unsafe {
                UIKeyCommand::keyCommandWithInput_modifierFlags_action(
                    &input,
                    flags,
                    action(),
                    main_thread,
                )
            };
            // As for the menu bar's commands; it made no difference here.
            command.setAllowsAutomaticLocalization(false);
            // A held key repeats in GPUI (`keyboard.rs`), not here; iPadOS 17+.
            if command.respondsToSelector(sel!(setRepeatBehavior:)) {
                command.setRepeatBehavior(UIMenuElementRepeatBehavior::NonRepeatable);
            }
            commands.push(command);
        }
    }
    NSArray::from_retained_slice(&commands)
}
