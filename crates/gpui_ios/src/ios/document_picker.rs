//! `prompt_for_paths` through `UIDocumentPickerViewController`.
//!
//! The picker hands out the original files and folders (`asCopy: false`), not
//! copies in the app's inbox, so the app works on what the user picked. Such
//! a URL is security scoped; `security_scope` starts and keeps its access,
//! and makes the bookmarks that carry it over a relaunch.

use std::{cell::RefCell, path::PathBuf};

use anyhow::Result;
use futures::channel::oneshot;
use gpui::PathPromptOptions;
use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, rc::Retained,
    runtime::ProtocolObject,
};
use objc2_foundation::{NSArray, NSObject, NSObjectProtocol, NSString, NSURL};
use objc2_ui_kit::{UIDocumentPickerDelegate, UIDocumentPickerViewController, UIViewController};
use objc2_uniform_type_identifiers::UTType;

use super::security_scope;

type PathsSender = oneshot::Sender<Result<Option<Vec<PathBuf>>>>;

/// The open picker's delegate.
#[derive(Default)]
pub(super) struct DocumentPickers {
    /// UIKit holds a picker's delegate weakly, so the last one lives here
    /// until the next picker replaces it.
    delegate: RefCell<Option<Retained<PickerDelegate>>>,
}

impl DocumentPickers {
    /// Presents a picker on `presenter`; the answer arrives on `sender`:
    /// `Ok(None)` when the user cancels.
    pub(super) fn present(
        &self,
        presenter: &UIViewController,
        options: &PathPromptOptions,
        sender: PathsSender,
    ) {
        let main_thread = presenter.mtm();
        let content_types = content_types(options);
        let picker = UIDocumentPickerViewController::initForOpeningContentTypes_asCopy(
            UIDocumentPickerViewController::alloc(main_thread),
            &content_types,
            false,
        );
        picker.setAllowsMultipleSelection(options.multiple);

        let delegate = PickerDelegate::new(sender, main_thread);
        picker.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        // A picker still open answers "cancelled" before it is replaced.
        if let Some(previous) = self.delegate.replace(Some(delegate)) {
            previous.finish(Ok(None));
        }
        presenter.presentViewController_animated_completion(&picker, true, None);
    }
}

fn picked(urls: &NSArray<NSURL>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for url in urls {
        match security_scope::start_accessing(url) {
            Some(path) => paths.push(path),
            None => log::warn!("GPUI iOS: document picker returned a URL that is no file path"),
        }
    }
    paths
}

/// The UTIs the picker offers for `options`. Folders are `public.folder`;
/// files are `public.item`, which a folder conforms to as well, but the
/// picker opens a folder on a tap rather than selecting it.
fn content_types(options: &PathPromptOptions) -> Retained<NSArray<UTType>> {
    let mut identifiers = Vec::new();
    if options.directories {
        identifiers.push("public.folder");
    }
    if options.files || identifiers.is_empty() {
        identifiers.push("public.item");
    }
    let types: Vec<Retained<UTType>> = identifiers
        .into_iter()
        .filter_map(|identifier| UTType::typeWithIdentifier(&NSString::from_str(identifier)))
        .collect();
    NSArray::from_retained_slice(&types)
}

struct PickerIvars {
    sender: RefCell<Option<PathsSender>>,
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "GPUIDocumentPickerDelegate"]
    #[ivars = PickerIvars]
    struct PickerDelegate;

    unsafe impl NSObjectProtocol for PickerDelegate {}

    unsafe impl UIDocumentPickerDelegate for PickerDelegate {
        #[unsafe(method(documentPicker:didPickDocumentsAtURLs:))]
        fn did_pick(&self, _controller: &UIDocumentPickerViewController, urls: &NSArray<NSURL>) {
            self.finish(Ok(Some(picked(urls))));
        }

        #[unsafe(method(documentPickerWasCancelled:))]
        fn was_cancelled(&self, _controller: &UIDocumentPickerViewController) {
            self.finish(Ok(None));
        }
    }
);

impl PickerDelegate {
    fn new(sender: PathsSender, main_thread: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(main_thread).set_ivars(PickerIvars {
            sender: RefCell::new(Some(sender)),
        });
        unsafe { msg_send![super(this), init] }
    }

    fn finish(&self, result: Result<Option<Vec<PathBuf>>>) {
        let Some(sender) = self.ivars().sender.borrow_mut().take() else {
            return;
        };
        if sender.send(result).is_err() {
            log::debug!("GPUI iOS: document picker receiver was dropped");
        }
    }
}
