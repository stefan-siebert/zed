//! Hardware keyboard: UIKit presses become GPUI key events.
//!
//! UIKit reports a hardware key once when it goes down and once when it comes
//! up (`pressesBegan`/`pressesEnded`), and never again while it is held:
//! measured on the iPadOS 27.0 simulator, a ↓ held for two seconds was one
//! `began` and one `ended`. GPUI expects the auto-repeats of a held key as
//! `KeyDownEvent`s with `is_held`, so they are made here — except for a key
//! that went on to UIKit's text system, which repeats typed text by itself.
//!
//! Modifier keys arrive as presses of their own, with empty characters; they
//! become `ModifiersChanged` events. Keys that type nothing are named by their
//! HID usage, with the names `gpui_macos` gives them, so one keymap serves
//! both; everything else by the characters UIKit reports for the user's layout.

use std::{
    cell::{Cell, RefCell},
    rc::Weak,
    time::Duration,
};

use gpui::{
    Capslock, KeyDownEvent, KeyUpEvent, Keystroke, Modifiers, ModifiersChangedEvent, PlatformInput,
    Task,
};
use objc2::MainThreadMarker;
use objc2_foundation::NSSet;
use objc2_ui_kit::{UIKey, UIKeyModifierFlags, UIPress};

use super::window::IosWindowState;

/// iOS keeps its key-repeat timing in an accessibility setting no API reads.
/// These are the defaults GPUI's other platforms ship with.
const KEY_REPEAT_DELAY: Duration = Duration::from_millis(500);
const KEY_REPEAT_INTERVAL: Duration = Duration::from_millis(33);

/// The phase of a press, as UIKit reports it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PressPhase {
    Began,
    Ended,
    Cancelled,
}

/// Per-window keyboard state.
#[derive(Default)]
pub(super) struct HardwareKeyboard {
    /// One bit per modifier key (HID usages 0xE0–0xE7) currently held, so
    /// releasing one Shift while the other is down keeps Shift on.
    held_modifiers: Cell<u8>,
    capslock: Cell<bool>,
    repeat: RefCell<Option<KeyRepeat>>,
    /// Key codes whose `began` went on to UIKit, which therefore expects
    /// their `ended` too.
    forwarded: RefCell<Vec<isize>>,
    /// A press whose character only UIKit's key-command matching knows
    /// (`shifted_keys.rs`), with the keystroke UIKit's characters make.
    waiting: RefCell<Option<(isize, Keystroke)>>,
    /// A key whose keystroke a key command named, so its release names the
    /// same keystroke.
    named: RefCell<Option<(isize, Keystroke)>>,
}

struct KeyRepeat {
    key_code: isize,
    _task: Task<()>,
}

impl HardwareKeyboard {
    pub(super) fn capslock(&self) -> Capslock {
        Capslock {
            on: self.capslock.get(),
        }
    }

    /// Ends the repeat of a held key, e.g. when the window loses focus and
    /// will not see its release.
    pub(super) fn stop_repeat(&self) {
        self.repeat.borrow_mut().take();
    }

    /// Whether a press waits for a hidden key command (`shifted_keys.rs`).
    pub(super) fn waits_for_key_command(&self) -> bool {
        self.waiting.borrow().is_some()
    }
}

/// Delivers `presses` to the window. A key GPUI leaves unhandled goes on to
/// UIKit: from the text input view to its text system, which types it, and
/// from either view to the menu bar's key commands, which run only for a
/// press that reaches UIKit — measured on iPadOS 27.0, `pressesBegan` comes
/// first, as AppKit's `performKeyEquivalent:` lets GPUI go before the menu.
/// Returns whether the caller must pass `presses` to `super` for that.
/// `text_system` says the caller is the text input view: UIKit then repeats
/// a key it was passed, and GPUI must not repeat it as well.
pub(super) fn handle_presses(
    window: &Weak<IosWindowState>,
    presses: &NSSet<UIPress>,
    phase: PressPhase,
    text_system: bool,
    main_thread: MainThreadMarker,
) -> bool {
    let Some(state) = window.upgrade() else {
        return true;
    };
    let mut forward = false;
    for press in presses {
        let Some(key) = press.key(main_thread) else {
            forward = true;
            continue;
        };
        forward |= handle_key(window, &state, &key, phase, text_system);
    }
    forward
}

fn handle_key(
    window: &Weak<IosWindowState>,
    state: &IosWindowState,
    key: &UIKey,
    phase: PressPhase,
    text_system: bool,
) -> bool {
    let keyboard = state.hardware_keyboard();
    let key_code = key.keyCode().0;
    let flags = key.modifierFlags();
    keyboard
        .capslock
        .set(flags.contains(UIKeyModifierFlags::AlphaShift));

    if let Some(bit) = modifier_bit(key_code) {
        let held = keyboard.held_modifiers.get();
        let held = if phase == PressPhase::Began {
            held | bit
        } else {
            held & !bit
        };
        keyboard.held_modifiers.set(held);
        let modifiers = held_modifiers(held);
        state.set_modifiers(modifiers);
        state.dispatch_input(PlatformInput::ModifiersChanged(ModifiersChangedEvent {
            modifiers,
            capslock: keyboard.capslock(),
        }));
        // Modifiers type nothing, but UIKit tracks them for key commands,
        // shift-selection and the like.
        return true;
    }

    let Some((keystroke, unreliable)) = keystroke(key, flags) else {
        return true;
    };
    state.set_modifiers(keystroke.modifiers);

    match phase {
        PressPhase::Began if unreliable => {
            wait_for_key_command(window, state, key_code, keystroke);
            true
        }
        PressPhase::Began => {
            let handled = state.dispatch_input(PlatformInput::KeyDown(KeyDownEvent {
                keystroke: keystroke.clone(),
                is_held: false,
                prefer_character_input: false,
            }));
            let forward = !handled;
            if forward {
                keyboard.forwarded.borrow_mut().push(key_code);
            }
            if forward && text_system {
                keyboard.stop_repeat();
            } else {
                start_repeat(window, state, key_code, keystroke);
            }
            forward
        }
        PressPhase::Ended | PressPhase::Cancelled => {
            let waiting = keyboard
                .waiting
                .borrow()
                .as_ref()
                .is_some_and(|(code, _)| *code == key_code);
            if waiting {
                key_command_ran(window, None);
            }
            let named = keyboard
                .named
                .borrow_mut()
                .take_if(|(code, _)| *code == key_code);
            let keystroke = named.map_or(keystroke, |(_, named)| named);
            let repeating = keyboard
                .repeat
                .borrow()
                .as_ref()
                .is_some_and(|repeat| repeat.key_code == key_code);
            if repeating {
                keyboard.stop_repeat();
            }
            state.dispatch_input(PlatformInput::KeyUp(KeyUpEvent { keystroke }));
            let mut forwarded = keyboard.forwarded.borrow_mut();
            match forwarded.iter().position(|code| *code == key_code) {
                Some(index) => {
                    forwarded.swap_remove(index);
                    true
                }
                None => false,
            }
        }
    }
}

/// Sends a press to UIKit first, whose hidden key commands know the
/// character it makes (`shifted_keys.rs`). A command that matches runs
/// while UIKit handles the press and calls [`key_command_ran`]; whatever
/// still waits afterwards goes to GPUI as `keystroke`.
fn wait_for_key_command(
    window: &Weak<IosWindowState>,
    state: &IosWindowState,
    key_code: isize,
    keystroke: Keystroke,
) {
    let keyboard = state.hardware_keyboard();
    keyboard.stop_repeat();
    keyboard.forwarded.borrow_mut().push(key_code);
    if let Some((previous, _)) = keyboard.waiting.replace(Some((key_code, keystroke))) {
        log::debug!("GPUI iOS: key {previous:#x} still waited for a key command");
    }
    match state.executors() {
        Some((foreground, _)) => {
            let window = window.clone();
            foreground
                .spawn(async move { key_command_ran(&window, None) })
                .detach();
        }
        None => key_command_ran(window, None),
    }
}

/// Ends the wait of a press for its key command: `input` is the character
/// of the command that matched, `None` when none did.
pub(super) fn key_command_ran(window: &Weak<IosWindowState>, input: Option<String>) {
    let Some(state) = window.upgrade() else {
        return;
    };
    let keyboard = state.hardware_keyboard();
    let Some((key_code, mut keystroke)) = keyboard.waiting.take() else {
        return;
    };
    let named = input.is_some();
    if let Some(input) = input {
        // The character carries the shift, as for every other key.
        keystroke.key = input;
        keystroke.modifiers.shift = false;
        keyboard.named.replace(Some((key_code, keystroke.clone())));
    }
    let handled = state.dispatch_input(PlatformInput::KeyDown(KeyDownEvent {
        keystroke: keystroke.clone(),
        is_held: false,
        prefer_character_input: false,
    }));
    // The key command took the press from the menu bar, so a menu command
    // with this shortcut gets it next, as a key GPUI leaves does elsewhere.
    if !handled
        && named
        && let Some(platform) = super::application::current_platform()
    {
        platform.menus.perform_shortcut(&keystroke);
    }
    start_repeat(window, &state, key_code, keystroke);
}

/// Repeats `keystroke` as held key-downs until its key comes up or another
/// key goes down, as a held key does on every desktop platform.
fn start_repeat(
    window: &Weak<IosWindowState>,
    state: &IosWindowState,
    key_code: isize,
    keystroke: Keystroke,
) {
    let keyboard = state.hardware_keyboard();
    let Some((foreground, background)) = state.executors() else {
        keyboard.stop_repeat();
        return;
    };
    let window = window.clone();
    let task = foreground.spawn(async move {
        background.timer(KEY_REPEAT_DELAY).await;
        loop {
            let Some(state) = window.upgrade() else {
                return;
            };
            state.dispatch_input(PlatformInput::KeyDown(KeyDownEvent {
                keystroke: keystroke.clone(),
                is_held: true,
                prefer_character_input: false,
            }));
            drop(state);
            background.timer(KEY_REPEAT_INTERVAL).await;
        }
    });
    keyboard.repeat.replace(Some(KeyRepeat {
        key_code,
        _task: task,
    }));
}

/// The bit of `held_modifiers` for a modifier key: left/right Control,
/// Shift, Option and Command are HID usages 0xE0–0xE7.
fn modifier_bit(key_code: isize) -> Option<u8> {
    (0xE0..=0xE7)
        .contains(&key_code)
        .then(|| 1 << (key_code - 0xE0))
}

fn held_modifiers(held: u8) -> Modifiers {
    // Bits 0–3 are the left keys, 4–7 the right ones, in the same order.
    let either = |bit: u8| held & (bit | bit << 4) != 0;
    Modifiers {
        control: either(1 << 0),
        shift: either(1 << 1),
        alt: either(1 << 2),
        platform: either(1 << 3),
        function: false,
    }
}

pub(super) fn modifiers(flags: UIKeyModifierFlags) -> Modifiers {
    Modifiers {
        control: flags.contains(UIKeyModifierFlags::Control),
        alt: flags.contains(UIKeyModifierFlags::Alternate),
        shift: flags.contains(UIKeyModifierFlags::Shift),
        platform: flags.contains(UIKeyModifierFlags::Command),
        function: false,
    }
}

/// The keystroke a key makes, or `None` for a key GPUI has no name for, and
/// whether its key is unreliable: shift with Command on a key that makes no
/// letter, whose character UIKit reports wrongly (`shifted_keys.rs`).
fn keystroke(key: &UIKey, flags: UIKeyModifierFlags) -> Option<(Keystroke, bool)> {
    let mut modifiers = modifiers(flags);
    let characters = key.characters().to_string();
    let types_text = !modifiers.control && !modifiers.platform;
    let text = |fixed: &str| types_text.then(|| fixed.to_string());
    let mut unreliable = false;

    let (key_name, key_char) = match key.keyCode().0 {
        0x28 | 0x58 => ("enter".to_string(), text("\n")),
        0x29 => ("escape".to_string(), None),
        0x2A => ("backspace".to_string(), None),
        0x2B => ("tab".to_string(), text("\t")),
        0x2C => ("space".to_string(), text(" ")),
        code @ 0x3A..=0x45 => (format!("f{}", code - 0x3A + 1), None),
        code @ 0x68..=0x73 => (format!("f{}", code - 0x68 + 13), None),
        0x49 => ("insert".to_string(), None),
        0x4A => ("home".to_string(), None),
        0x4B => ("pageup".to_string(), None),
        0x4C => ("delete".to_string(), None),
        0x4D => ("end".to_string(), None),
        0x4E => ("pagedown".to_string(), None),
        0x4F => ("right".to_string(), None),
        0x50 => ("left".to_string(), None),
        0x51 => ("down".to_string(), None),
        0x52 => ("up".to_string(), None),
        // The keypad's arithmetic keys, named apart from the main row as on
        // macOS and Linux; they still type their character.
        code @ (0x54 | 0x55 | 0x56 | 0x57 | 0x63) => {
            let name = match code {
                0x54 => "divide",
                0x55 => "multiply",
                0x56 => "subtract",
                0x57 => "add",
                _ => "decimal",
            };
            (name.to_string(), typed(&characters, types_text))
        }
        // ⌘. is iPadOS's Escape — Apple's keyboards for the iPad have none
        // — and UIKit makes it one itself: the "." key, the Command taken
        // off, the characters `UIKeyInputEscape` (measured on iPadOS 27.0).
        _ if key.charactersIgnoringModifiers().to_string() == "UIKeyInputEscape" => {
            ("escape".to_string(), None)
        }
        _ => {
            let ignoring = key.charactersIgnoringModifiers().to_string();
            // Keys without a name above report UIKit constants such as
            // "UIKeyInputPageUp" rather than text.
            if ignoring.is_empty()
                || ignoring.chars().any(char::is_control)
                || ignoring.starts_with("UIKeyInput")
            {
                return None;
            }
            // As on macOS: shift with a letter stays "shift-a"; shift with
            // anything else names the character it makes ("!" for shift-1),
            // and the shift is used up by it.
            let key_name = if !modifiers.shift
                || ignoring
                    .chars()
                    .all(|character| character.is_ascii_lowercase())
                || modifiers.alt
            {
                ignoring
            } else if modifiers.platform {
                // Until a key command names the character, the keystroke
                // keeps the shift rather than take UIKit's character, which
                // may be the unshifted one ("cmd-7" for ⌘⇧7).
                unreliable = true;
                ignoring
            } else {
                match typed(&characters, true) {
                    Some(shifted) => {
                        modifiers.shift = false;
                        shifted
                    }
                    None => ignoring,
                }
            };
            (key_name, typed(&characters, types_text))
        }
    };

    Some((
        Keystroke {
            modifiers,
            key: key_name,
            key_char,
        },
        unreliable,
    ))
}

/// `characters` as typed text, when there is any.
fn typed(characters: &str, types_text: bool) -> Option<String> {
    (types_text
        && !characters.is_empty()
        && !characters.chars().any(char::is_control)
        && !characters.starts_with("UIKeyInput"))
    .then(|| characters.to_string())
}
