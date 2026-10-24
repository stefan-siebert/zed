//! The iPad menu bar: GPUI's application menus in `UIMainMenuSystem`.
//!
//! iPadOS 26 shows a menu bar for apps that build one through the main menu
//! system; earlier versions have none, and `set_menus` only records the menus
//! there. The system's File, Edit, Format and View menus are removed, and the
//! first GPUI menu fills the application menu, as it does on macOS. Window
//! and Help stay: the system fills Window with its window commands and keeps
//! Help, with the menu search, even when asked to remove it (measured on
//! iPadOS 27.0). A GPUI menu that names either — "Window"/"Help", as
//! `gpui_macos` matches "Window", or the system menu's own, localized title —
//! goes into it rather than beside it: Window at the end, Help at the start.
//! The other menus follow the application menu in order.
//!
//! Every item is a `UICommand` (a `UIKeyCommand` when the action has a
//! one-keystroke binding, which shows the shortcut) whose selector the GPUI
//! view controller implements; the command's property list is the action's
//! index. UIKit looks the selector up along the responder chain, which runs
//! through that view controller whenever a GPUI view is first responder.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::ptr::NonNull;
use std::sync::OnceLock;

use block2::RcBlock;
use gpui::{Action, KeyContext, Keymap, Keystroke, Menu, MenuItem, OwnedMenu};
use objc2::{
    MainThreadMarker,
    rc::Retained,
    runtime::{AnyClass, ProtocolObject},
    sel,
};
use objc2_foundation::{NSArray, NSCopying, NSNumber, NSString};
use objc2_ui_kit::{
    UICommand, UIKeyCommand, UIKeyInputDelete, UIKeyInputDownArrow, UIKeyInputEnd,
    UIKeyInputEscape, UIKeyInputF2, UIKeyInputF3, UIKeyInputF4, UIKeyInputF5, UIKeyInputF6,
    UIKeyInputF7, UIKeyInputF8, UIKeyInputF9, UIKeyInputF10, UIKeyInputF11, UIKeyInputF12,
    UIKeyInputHome, UIKeyInputLeftArrow, UIKeyInputPageDown, UIKeyInputPageUp,
    UIKeyInputRightArrow, UIKeyInputUpArrow, UIKeyModifierFlags, UIMainMenuSystem,
    UIMainMenuSystemConfiguration, UIMenu, UIMenuApplication, UIMenuBuilder, UIMenuEdit,
    UIMenuElement, UIMenuElementAttributes, UIMenuElementState, UIMenuFile, UIMenuFormat,
    UIMenuHelp, UIMenuOptions, UIMenuRoot, UIMenuView, UIMenuWindow,
};

use super::CallbackSlot;

unsafe extern "C" {
    // Declared twice in UIResponder.h, so objc2-ui-kit leaves it out.
    static UIKeyInputF1: &'static NSString;
}

/// The menus GPUI asked for, as UIKit menus, and the callbacks that carry a
/// chosen command back to GPUI.
#[derive(Default)]
pub(super) struct AppMenus {
    owned: RefCell<Option<Vec<OwnedMenu>>>,
    built: RefCell<Option<BuiltMenus>>,
    /// Each command's action and whether the menu declared it disabled;
    /// indexed by the command's property list.
    actions: RefCell<Vec<(Box<dyn Action>, bool)>>,
    configured: Cell<bool>,
    pub(super) command: CallbackSlot<Box<dyn FnMut(&dyn Action)>>,
    pub(super) validate: CallbackSlot<Box<dyn FnMut(&dyn Action) -> bool>>,
    pub(super) will_open: CallbackSlot<Box<dyn FnMut()>>,
}

struct BuiltMenus {
    application: Vec<Retained<UIMenuElement>>,
    /// The other menus, by title; whether one joins a system menu is decided
    /// while building, where the system menus' titles are known.
    others: Vec<(String, Vec<Retained<UIMenuElement>>)>,
}

impl AppMenus {
    pub(super) fn set(&self, menus: Vec<Menu>, keymap: &Keymap) {
        self.install(&menus, keymap);
        self.owned
            .replace(Some(menus.into_iter().map(Menu::owned).collect()));
    }

    fn install(&self, menus: &[Menu], keymap: &Keymap) {
        let Some(main_thread) = MainThreadMarker::new() else {
            log::warn!("GPUI iOS: set_menus off the main thread");
            return;
        };
        if AnyClass::get(c"UIMainMenuSystem").is_none() {
            log::debug!("GPUI iOS: no main menu system before iPadOS 26");
            return;
        }

        let mut collected = Collected::default();
        let mut built = BuiltMenus {
            application: Vec::new(),
            others: Vec::new(),
        };
        for (index, menu) in menus.iter().enumerate() {
            let children = elements(
                &menu.items,
                menu.disabled,
                &mut collected,
                keymap,
                main_thread,
            );
            if index == 0 {
                built.application = children;
            } else {
                built.others.push((menu.name.to_string(), children));
            }
        }
        self.actions.replace(collected.actions);
        self.built.replace(Some(built));

        let system = UIMainMenuSystem::sharedSystem(main_thread);
        if self.configured.replace(true) {
            system.setNeedsRebuild();
        } else {
            let configuration = UIMainMenuSystemConfiguration::new(main_thread);
            let handler = RcBlock::new(|builder: NonNull<ProtocolObject<dyn UIMenuBuilder>>| {
                if let Some(platform) = super::application::current_platform() {
                    platform.menus.build(unsafe { builder.as_ref() });
                }
            });
            system.setBuildConfiguration_buildHandler(&configuration, Some(&handler));
        }
    }

    pub(super) fn get(&self) -> Option<Vec<OwnedMenu>> {
        self.owned.borrow().clone()
    }

    fn build(&self, builder: &ProtocolObject<dyn UIMenuBuilder>) {
        let built = self.built.borrow();
        let Some(built) = built.as_ref() else {
            return;
        };
        let Some(main_thread) = MainThreadMarker::new() else {
            return;
        };
        let (root_menu, application_menu, window_menu, help_menu, removed) = unsafe {
            (
                UIMenuRoot,
                UIMenuApplication,
                UIMenuWindow,
                UIMenuHelp,
                [UIMenuFile, UIMenuEdit, UIMenuFormat, UIMenuView],
            )
        };
        for identifier in removed {
            builder.removeMenuForIdentifier(identifier);
        }
        // What the system menus that stay still bind, e.g. ⌘? for Help's menu
        // search. UIKit asks for unique shortcuts ("to avoid undefined
        // behavior"); a clash with a system menu did not throw (measured),
        // unlike two GPUI commands in one menu, but is avoided all the same.
        let mut taken = HashSet::new();
        if let Some(root) = builder.menuForIdentifier(root_menu) {
            for menu in root.children().iter() {
                let replaced = menu
                    .downcast_ref::<UIMenu>()
                    .is_some_and(|menu| menu.identifier().isEqualToString(application_menu));
                if !replaced {
                    collect_shortcuts(&menu, &mut taken);
                }
            }
        }
        let joins = |name: &str, english: &str, identifier| {
            name == english
                || builder
                    .menuForIdentifier(identifier)
                    .is_some_and(|menu| menu.title().to_string() == name)
        };
        // The block keeps the array alive while UIKit holds the +0 result.
        let application = NSArray::from_retained_slice(&without_shortcuts(
            &built.application,
            &taken,
            main_thread,
        ));
        let replace =
            RcBlock::new(move |_: NonNull<NSArray<UIMenuElement>>| NonNull::from(&*application));
        unsafe {
            builder
                .replaceChildrenOfMenuForIdentifier_fromChildrenBlock(application_menu, &replace);
        }
        // Inserting each after the application menu, last first, keeps their order.
        for (name, children) in built.others.iter().rev() {
            let children =
                NSArray::from_retained_slice(&without_shortcuts(children, &taken, main_thread));
            if joins(name, "Window", window_menu) {
                builder.insertElements_atEndOfMenuForIdentifier(&children, window_menu);
            } else if joins(name, "Help", help_menu) {
                builder.insertElements_atStartOfMenuForIdentifier(&children, help_menu);
            } else {
                let menu = UIMenu::menuWithTitle_children(
                    &NSString::from_str(name),
                    &children,
                    main_thread,
                );
                builder.insertSiblingMenu_afterMenuForIdentifier(&menu, application_menu);
            }
        }
    }

    /// Runs the action of a chosen command.
    pub(super) fn perform(&self, command: &UICommand) {
        let Some(action) = self.action(command).map(|(action, _)| action) else {
            return;
        };
        if self.command.with(|callback| callback(&*action)).is_none() {
            log::debug!("GPUI iOS: menu command chosen with no handler");
        }
    }

    /// Enables or disables a command before the menu shows it.
    pub(super) fn validate(&self, command: &UICommand) {
        let Some((action, disabled)) = self.action(command) else {
            return;
        };
        let enabled = !disabled
            && self
                .validate
                .with(|callback| callback(&*action))
                .unwrap_or(true);
        let attributes = command.attributes();
        command.setAttributes(if enabled {
            attributes - UIMenuElementAttributes::Disabled
        } else {
            attributes | UIMenuElementAttributes::Disabled
        });
    }

    fn action(&self, command: &UICommand) -> Option<(Box<dyn Action>, bool)> {
        let index = command
            .propertyList()?
            .downcast::<NSNumber>()
            .ok()?
            .as_usize();
        // Cloned, so the callback may call set_menus.
        self.actions
            .borrow()
            .get(index)
            .map(|(action, disabled)| (action.boxed_clone(), *disabled))
    }
}

/// What building the menus gathers on the way.
#[derive(Default)]
struct Collected {
    /// Each command's action and whether it is disabled, by index.
    actions: Vec<(Box<dyn Action>, bool)>,
    /// Shortcuts already given to a command: when two commands in one menu
    /// share one, which GPUI allows (one action twice), UIKit throws
    /// `NSInvalidArgumentException` ("Inserted elements contain duplicates")
    /// at the next key press (measured). Kept across menus too.
    shortcuts: HashSet<(String, isize)>,
}

/// `items` as menu elements: runs between separators become inline menus,
/// since a UIKit menu has no separator item.
fn elements(
    items: &[MenuItem],
    disabled: bool,
    collected: &mut Collected,
    keymap: &Keymap,
    main_thread: MainThreadMarker,
) -> Vec<Retained<UIMenuElement>> {
    let mut groups: Vec<Vec<Retained<UIMenuElement>>> = vec![Vec::new()];
    for item in items {
        match item {
            MenuItem::Separator => groups.push(Vec::new()),
            MenuItem::Action {
                name,
                action,
                checked,
                disabled: item_disabled,
                ..
            } => {
                let command = command(name, action.as_ref(), collected, keymap, main_thread);
                collected
                    .actions
                    .push((action.boxed_clone(), disabled || *item_disabled));
                if disabled || *item_disabled {
                    command.setAttributes(UIMenuElementAttributes::Disabled);
                }
                if *checked {
                    command.setState(UIMenuElementState::On);
                }
                if let Some(group) = groups.last_mut() {
                    group.push(Retained::into_super(command));
                }
            }
            MenuItem::Submenu(menu) => {
                let children = elements(
                    &menu.items,
                    disabled || menu.disabled,
                    collected,
                    keymap,
                    main_thread,
                );
                let submenu = UIMenu::menuWithTitle_children(
                    &NSString::from_str(&menu.name),
                    &NSArray::from_retained_slice(&children),
                    main_thread,
                );
                if let Some(group) = groups.last_mut() {
                    group.push(Retained::into_super(submenu));
                }
            }
            // The Services menu and its kind exist only on macOS.
            MenuItem::SystemMenu(_) => {}
        }
    }
    groups.retain(|group| !group.is_empty());
    if groups.len() <= 1 {
        return groups.pop().unwrap_or_default();
    }
    groups
        .into_iter()
        .map(|group| {
            Retained::into_super(UIMenu::menuWithTitle_image_identifier_options_children(
                &NSString::new(),
                None,
                None,
                UIMenuOptions::DisplayInline,
                &NSArray::from_retained_slice(&group),
                main_thread,
            ))
        })
        .collect()
}

/// Adds the shortcut of every key command in `element` to `shortcuts`.
fn collect_shortcuts(element: &UIMenuElement, shortcuts: &mut HashSet<(String, isize)>) {
    if let Some(menu) = element.downcast_ref::<UIMenu>() {
        for child in menu.children().iter() {
            collect_shortcuts(&child, shortcuts);
        }
    } else if let Some(command) = element.downcast_ref::<UIKeyCommand>()
        && let Some(input) = command.input()
    {
        shortcuts.insert((input.to_string(), command.modifierFlags().bits()));
    }
}

/// `elements` with every key command whose shortcut is `taken` turned into a
/// plain command: same title, action and state, no shortcut shown. The key
/// still reaches GPUI's keymap first, as every key does.
fn without_shortcuts(
    elements: &[Retained<UIMenuElement>],
    taken: &HashSet<(String, isize)>,
    main_thread: MainThreadMarker,
) -> Vec<Retained<UIMenuElement>> {
    elements
        .iter()
        .map(|element| {
            if let Some(menu) = element.downcast_ref::<UIMenu>() {
                let children = without_shortcuts(&menu.children().to_vec(), taken, main_thread);
                return Retained::into_super(
                    menu.menuByReplacingChildren(&NSArray::from_retained_slice(&children)),
                );
            }
            let Some(command) = element.downcast_ref::<UIKeyCommand>() else {
                return element.clone();
            };
            let Some(input) = command.input() else {
                return element.clone();
            };
            if !taken.contains(&(input.to_string(), command.modifierFlags().bits())) {
                return element.clone();
            }
            log::warn!(
                "GPUI iOS: menu item {:?} loses its shortcut to a system menu",
                command.title().to_string()
            );
            let plain = unsafe {
                UICommand::commandWithTitle_image_action_propertyList(
                    &command.title(),
                    None,
                    sel!(gpuiMenuCommand:),
                    command.propertyList().as_deref(),
                    main_thread,
                )
            };
            plain.setAttributes(command.attributes());
            plain.setState(command.state());
            Retained::into_super(plain)
        })
        .collect()
}

/// The command for one action item; its index is the next in `collected`.
fn command(
    name: &str,
    action: &dyn Action,
    collected: &mut Collected,
    keymap: &Keymap,
    main_thread: MainThreadMarker,
) -> Retained<UICommand> {
    let title = NSString::from_str(name);
    let index = NSNumber::new_usize(collected.actions.len());
    let selector = sel!(gpuiMenuCommand:);
    let shortcut = shortcut(action, keymap).filter(|(input, modifiers)| {
        collected
            .shortcuts
            .insert((input.to_string(), modifiers.bits()))
    });
    if let Some((input, modifiers)) = shortcut {
        let command = unsafe {
            UIKeyCommand::commandWithTitle_image_action_input_modifierFlags_propertyList(
                &title,
                None,
                selector,
                &input,
                modifiers,
                Some(&index),
                main_thread,
            )
        };
        // GPUI names the keystroke the user's layout makes; UIKit must not
        // move it to another key.
        command.setAllowsAutomaticLocalization(false);
        Retained::into_super(command)
    } else {
        unsafe {
            UICommand::commandWithTitle_image_action_propertyList(
                &title,
                None,
                selector,
                Some(&index),
                main_thread,
            )
        }
    }
}

/// The keystroke a menu shows for `action`, chosen as `gpui_macos` does: the
/// first binding whose context matches a default one, else the first.
fn shortcut(
    action: &dyn Action,
    keymap: &Keymap,
) -> Option<(Retained<NSString>, UIKeyModifierFlags)> {
    static DEFAULT_CONTEXT: OnceLock<Vec<KeyContext>> = OnceLock::new();
    let default_context = DEFAULT_CONTEXT.get_or_init(|| {
        let mut workspace_context = KeyContext::new_with_defaults();
        workspace_context.add("Workspace");
        let mut pane_context = KeyContext::new_with_defaults();
        pane_context.add("Pane");
        let mut editor_context = KeyContext::new_with_defaults();
        editor_context.add("Editor");
        pane_context.extend(&editor_context);
        workspace_context.extend(&pane_context);
        vec![workspace_context]
    });
    let mut bindings = keymap.bindings_for_action(action).peekable();
    let first = *bindings.peek()?;
    let binding = bindings
        .find(|binding| {
            binding
                .predicate()
                .is_none_or(|predicate| predicate.eval(default_context))
        })
        .unwrap_or(first);
    let [keystroke] = binding.keystrokes() else {
        return None;
    };
    let keystroke = keystroke.inner();
    Some((key_input(keystroke)?, modifier_flags(keystroke)))
}

fn modifier_flags(keystroke: &Keystroke) -> UIKeyModifierFlags {
    let modifiers = &keystroke.modifiers;
    let mut flags = UIKeyModifierFlags::empty();
    for (on, flag) in [
        (modifiers.platform, UIKeyModifierFlags::Command),
        (modifiers.control, UIKeyModifierFlags::Control),
        (modifiers.alt, UIKeyModifierFlags::Alternate),
        (modifiers.shift, UIKeyModifierFlags::Shift),
    ] {
        if on {
            flags |= flag;
        }
    }
    flags
}

/// The `UIKeyCommand` input for a GPUI key name; the inverse of the names
/// `keyboard.rs` gives.
fn key_input(keystroke: &Keystroke) -> Option<Retained<NSString>> {
    let fixed: Option<&'static NSString> = match keystroke.key.as_str() {
        "up" => Some(unsafe { UIKeyInputUpArrow }),
        "down" => Some(unsafe { UIKeyInputDownArrow }),
        "left" => Some(unsafe { UIKeyInputLeftArrow }),
        "right" => Some(unsafe { UIKeyInputRightArrow }),
        "escape" => Some(unsafe { UIKeyInputEscape }),
        "pageup" => Some(unsafe { UIKeyInputPageUp }),
        "pagedown" => Some(unsafe { UIKeyInputPageDown }),
        "home" => Some(unsafe { UIKeyInputHome }),
        "end" => Some(unsafe { UIKeyInputEnd }),
        "delete" => Some(unsafe { UIKeyInputDelete }),
        "f1" => Some(unsafe { UIKeyInputF1 }),
        "f2" => Some(unsafe { UIKeyInputF2 }),
        "f3" => Some(unsafe { UIKeyInputF3 }),
        "f4" => Some(unsafe { UIKeyInputF4 }),
        "f5" => Some(unsafe { UIKeyInputF5 }),
        "f6" => Some(unsafe { UIKeyInputF6 }),
        "f7" => Some(unsafe { UIKeyInputF7 }),
        "f8" => Some(unsafe { UIKeyInputF8 }),
        "f9" => Some(unsafe { UIKeyInputF9 }),
        "f10" => Some(unsafe { UIKeyInputF10 }),
        "f11" => Some(unsafe { UIKeyInputF11 }),
        "f12" => Some(unsafe { UIKeyInputF12 }),
        _ => None,
    };
    if let Some(fixed) = fixed {
        return Some(fixed.copy());
    }
    let text = match keystroke.key.as_str() {
        "enter" => "\r",
        "tab" => "\t",
        "space" => " ",
        "backspace" => "\u{8}",
        key if key.chars().count() == 1 => key,
        // f13 and up, keypad names and the like have no input string.
        _ => return None,
    };
    Some(NSString::from_str(text))
}
