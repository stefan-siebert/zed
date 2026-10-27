//! Files dropped from other apps, as GPUI file drops.
//!
//! A `UIDropInteraction` on the Metal view takes drags from other apps (the
//! Files app, Mail, Photos, …). Unlike AppKit, UIKit hands out no paths while
//! the drag hovers: the items load only once they are dropped, and then
//! asynchronously. So the drag enters GPUI with no paths, which lets drop
//! targets show themselves, and on the drop its items load; once all have,
//! the drag enters again with their paths and is submitted where it was
//! dropped.
//!
//! A drag lifted from this window (`drag.rs`) enters it as GPUI's own drag
//! again (`restore_platform_drag`), so a drop here is GPUI's drop of what was
//! dragged, with nothing to load. A drag from another window of the app is
//! loaded like one from another app.
//!
//! An item is loaded in place where its provider allows it — the original
//! file, whose security scope `security_scope` starts — and otherwise as
//! the provider's copy, which UIKit deletes when its handler returns and is
//! therefore moved to a directory of its own under the app's temporary
//! directory first. Measured on the iPadOS 27.0 simulator: a folder from the
//! Files app arrives in place (its File Provider storage), a photo from
//! Photos as a copy.

use std::{
    path::{Path, PathBuf},
    rc::Weak,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, anyhow};
use block2::RcBlock;
use futures::channel::oneshot;
use gpui::{ExternalPaths, FileDropEvent, Pixels, PlatformInput, Point, px};
use objc2::{
    MainThreadMarker, MainThreadOnly,
    rc::Retained,
    runtime::{Bool, ProtocolObject},
};
use objc2_foundation::{
    NSArray, NSError, NSItemProvider, NSItemProviderFileOptions, NSString, NSTemporaryDirectory,
    NSURL,
};
use objc2_ui_kit::{
    UIDragDropSession, UIDropInteraction, UIDropInteractionDelegate, UIDropOperation,
    UIDropProposal, UIDropSession, UIInteraction, UIView,
};
use smallvec::SmallVec;

use super::{security_scope, window::IosWindowState};

/// Adds the drop interaction to `view`, which keeps it.
pub(super) fn install(
    view: &UIView,
    delegate: &ProtocolObject<dyn UIDropInteractionDelegate>,
    main_thread: MainThreadMarker,
) {
    let interaction =
        UIDropInteraction::initWithDelegate(UIDropInteraction::alloc(main_thread), delegate);
    view.addInteraction(ProtocolObject::<dyn UIInteraction>::from_ref(&*interaction));
}

/// Whether to take part in `session`: drags that carry anything at all.
pub(super) fn can_handle(session: &ProtocolObject<dyn UIDropSession>) -> bool {
    session.hasItemsConformingToTypeIdentifiers(&NSArray::from_retained_slice(&[
        NSString::from_str("public.item"),
    ]))
}

/// Whether `session` is the drag lifted from `window`, which GPUI drops itself.
fn is_own(window: &IosWindowState, session: &ProtocolObject<dyn UIDropSession>) -> bool {
    window.is_drag_source() && session.localDragSession().is_some()
}

pub(super) fn entered(
    window: &IosWindowState,
    session: &ProtocolObject<dyn UIDropSession>,
    view: &UIView,
) {
    window.dispatch_input(PlatformInput::FileDrop(FileDropEvent::Entered {
        position: position(session, view),
        paths: ExternalPaths(SmallVec::new()),
    }));
}

/// Moves the drag; the proposal copies, as `gpui_macos` does for a drag
/// from another app. The window's own drag proposes a copy too: what its drop
/// does is GPUI's to decide, and a move proposal is ignored for a drag that
/// allows no move (`sessionAllowsMoveOperation`) — `performDrop` never
/// came (measured).
pub(super) fn updated(
    window: &IosWindowState,
    session: &ProtocolObject<dyn UIDropSession>,
    view: &UIView,
) -> Retained<UIDropProposal> {
    window.dispatch_input(PlatformInput::FileDrop(FileDropEvent::Pending {
        position: position(session, view),
    }));
    UIDropProposal::initWithDropOperation(UIDropProposal::alloc(view.mtm()), UIDropOperation::Copy)
}

pub(super) fn exited(window: &IosWindowState) {
    window.dispatch_input(PlatformInput::FileDrop(FileDropEvent::Exited));
}

/// Loads the dropped items and, once all have loaded, submits them at the
/// drop position.
pub(super) fn perform(
    window: &Weak<IosWindowState>,
    session: &ProtocolObject<dyn UIDropSession>,
    view: &UIView,
) {
    let position = position(session, view);
    if let Some(window) = window.upgrade()
        && is_own(&window, session)
    {
        window.dispatch_input(PlatformInput::FileDrop(FileDropEvent::Submit { position }));
        return;
    }
    let receivers: Vec<_> = session
        .items()
        .iter()
        .map(|item| load(&item.itemProvider()))
        .collect();
    let Some((foreground, _)) = window.upgrade().and_then(|window| window.executors()) else {
        return;
    };
    let window = window.clone();
    foreground
        .spawn(async move {
            let mut paths = SmallVec::new();
            for receiver in receivers {
                match receiver.await {
                    Ok(Ok(Loaded::InPlace(url))) => {
                        paths.extend(security_scope::start_accessing(url.0));
                    }
                    Ok(Ok(Loaded::Copied(path))) => paths.push(path),
                    Ok(Err(error)) => log::warn!("GPUI iOS: a dropped item: {error:#}"),
                    Err(_) => log::warn!("GPUI iOS: a dropped item never loaded"),
                }
            }
            let Some(window) = window.upgrade() else {
                return;
            };
            // The drag that entered without paths makes way for one with them.
            window.dispatch_input(PlatformInput::FileDrop(FileDropEvent::Exited));
            if paths.is_empty() {
                return;
            }
            window.dispatch_input(PlatformInput::FileDrop(FileDropEvent::Entered {
                position,
                paths: ExternalPaths(paths),
            }));
            window.dispatch_input(PlatformInput::FileDrop(FileDropEvent::Submit { position }));
            window.dispatch_input(PlatformInput::FileDrop(FileDropEvent::Exited));
        })
        .detach();
}

fn position(session: &ProtocolObject<dyn UIDropSession>, view: &UIView) -> Point<Pixels> {
    let location = session.locationInView(view);
    Point::new(px(location.x as f32), px(location.y as f32))
}

enum Loaded {
    /// The original, still to be accessed through its security scope.
    InPlace(SendUrl),
    /// A copy the provider made, moved to a directory of the app's own.
    Copied(PathBuf),
}

/// A URL handed from the provider's queue to the main thread.
struct SendUrl(Retained<NSURL>);

// SAFETY: NSURL is immutable, and Foundation documents immutable objects as
// safe to use from any thread.
unsafe impl Send for SendUrl {}

/// Starts loading one item; the answer arrives on the returned receiver.
fn load(provider: &NSItemProvider) -> oneshot::Receiver<Result<Loaded>> {
    let (sender, receiver) = oneshot::channel();
    // A type registered for opening in place first: the Files app lists a
    // private one before `public.folder` (measured), and a copy is all the
    // first type yields.
    let type_identifier = provider
        .registeredTypeIdentifiersWithFileOptions(NSItemProviderFileOptions::OpenInPlace)
        .firstObject()
        .or_else(|| provider.registeredTypeIdentifiers().firstObject());
    let Some(type_identifier) = type_identifier else {
        sender
            .send(Err(anyhow!("an item with no type")))
            .unwrap_or_else(|_| log::debug!("GPUI iOS: drop receiver dropped"));
        return receiver;
    };
    let suggested_name = provider.suggestedName().map(|name| name.to_string());
    // The handler is `Fn` and runs once, on a queue of the provider's.
    let sender = Mutex::new(Some(sender));
    let handler = RcBlock::new(
        move |url: *mut NSURL, in_place: Bool, error: *mut NSError| {
            let result = loaded(url, in_place, error, suggested_name.as_deref());
            let sender = sender.lock().ok().and_then(|mut sender| sender.take());
            if let Some(sender) = sender
                && sender.send(result).is_err()
            {
                log::debug!("GPUI iOS: drop receiver dropped");
            }
        },
    );
    // SAFETY: the handler matches the declared block type.
    let _progress = unsafe {
        provider.loadInPlaceFileRepresentationForTypeIdentifier_completionHandler(
            &type_identifier,
            &handler,
        )
    };
    receiver
}

/// What the provider's handler received, made into a `Loaded`. Runs on the
/// provider's queue, before UIKit deletes a copy.
fn loaded(
    url: *mut NSURL,
    in_place: Bool,
    error: *mut NSError,
    suggested_name: Option<&str>,
) -> Result<Loaded> {
    // SAFETY: UIKit passes either null or a valid object for the call.
    let url = unsafe { Retained::retain(url) };
    let Some(url) = url else {
        let error = unsafe { Retained::retain(error) };
        return Err(match error {
            Some(error) => anyhow!("{}", error.localizedDescription()),
            None => anyhow!("no file and no error"),
        });
    };
    if in_place.as_bool() {
        return Ok(Loaded::InPlace(SendUrl(url)));
    }
    let source = url.to_file_path().context("the copy is no file path")?;
    Ok(Loaded::Copied(keep(&source, suggested_name)?))
}

/// Moves the provider's copy at `source` into a new directory of its own,
/// keeping its name, or the provider's suggested one.
fn keep(source: &Path, suggested_name: Option<&str>) -> Result<PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let directory = PathBuf::from(NSTemporaryDirectory().to_string())
        .join("GPUIDrops")
        .join(format!("{stamp}-{}", NEXT.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("creating {}", directory.display()))?;
    let name = source
        .file_name()
        .map(|name| name.to_os_string())
        .or_else(|| suggested_name.map(Into::into))
        .context("the copy has no name")?;
    let target = directory.join(name);
    std::fs::rename(source, &target)
        .with_context(|| format!("moving {} to {}", source.display(), target.display()))?;
    Ok(target)
}
