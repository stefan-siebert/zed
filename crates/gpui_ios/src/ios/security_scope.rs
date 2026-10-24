//! Access to files outside the app's container, and bookmarks that keep it.
//!
//! A URL the document picker hands out is security scoped: the app may read
//! it only between `startAccessingSecurityScopedResource` and the matching
//! stop. GPUI passes paths, not URLs, so access is started here and kept for
//! the rest of the process, and the URL is remembered by its path.
//!
//! Access ends with the process. An app that wants a folder again after a
//! relaunch asks [`bookmark`] for bookmark data while it still has access,
//! stores those bytes itself, and hands them to [`resolve_bookmark`] at the
//! next start. Apple's pattern ("Providing access to directories") makes the
//! bookmark from the picked URL itself with `minimalBookmark`; [`bookmark`]
//! does that whenever this process picked the path.

use std::{
    cell::RefCell,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, anyhow};
use objc2::{MainThreadMarker, rc::Retained, runtime::Bool};
use objc2_foundation::{
    NSData, NSURL, NSURLBookmarkCreationOptions, NSURLBookmarkResolutionOptions,
};

thread_local! {
    /// Every URL whose access was started, by its path. Only the main thread
    /// touches it: the picker answers there, and the public functions
    /// refuse other threads.
    static ACCESSED: RefCell<Vec<(PathBuf, Retained<NSURL>)>> = const { RefCell::new(Vec::new()) };
}

/// Starts access to `url` and keeps it for the process. Returns the URL's
/// path, or `None` for a URL that is no file path.
pub(super) fn start_accessing(url: Retained<NSURL>) -> Option<PathBuf> {
    let path = url.to_file_path()?;
    // False for a URL that needs no scope, e.g. one inside the app's own
    // container; it is remembered all the same, so `bookmark` finds it.
    if !unsafe { url.startAccessingSecurityScopedResource() } {
        log::debug!(
            "GPUI iOS: {} needs no security scope, or was refused one",
            path.display()
        );
    }
    ACCESSED.with_borrow_mut(|accessed| {
        if !accessed.iter().any(|(known, _)| *known == path) {
            accessed.push((path.clone(), url));
        }
    });
    Some(path)
}

/// Bookmark data for `path`, to be stored by the app and passed to
/// [`resolve_bookmark`] after a relaunch. Made from the URL the document
/// picker returned when this process picked `path` (or resolved it from a
/// bookmark), else from a plain file URL. Main thread only.
pub fn bookmark(path: &Path) -> Result<Vec<u8>> {
    main_thread()?;
    let url = ACCESSED
        .with_borrow(|accessed| {
            accessed
                .iter()
                .find(|(known, _)| known == path)
                .map(|(_, url)| url.clone())
        })
        .or_else(|| NSURL::from_file_path(path))
        .with_context(|| format!("{} is not a file path", path.display()))?;
    let data = url
        .bookmarkDataWithOptions_includingResourceValuesForKeys_relativeToURL_error(
            NSURLBookmarkCreationOptions::MinimalBookmark,
            None,
            None,
        )
        .map_err(|error| {
            anyhow!(
                "bookmark for {}: {}",
                path.display(),
                error.localizedDescription()
            )
        })?;
    Ok(data.to_vec())
}

/// What [`resolve_bookmark`] found.
#[derive(Debug, Clone)]
pub struct ResolvedBookmark {
    /// Where the bookmarked item is now; it may have moved or been renamed.
    pub path: PathBuf,
    /// Fresh bookmark data when the stored data was stale. The app replaces
    /// what it stored with these bytes.
    pub renewed: Option<Vec<u8>>,
}

/// Resolves bookmark data from [`bookmark`] and starts access to the item for
/// the rest of the process. Fails when the item is gone or the user withdrew
/// the app's access in Settings. Main thread only.
pub fn resolve_bookmark(data: &[u8]) -> Result<ResolvedBookmark> {
    main_thread()?;
    let data = NSData::with_bytes(data);
    let mut is_stale = Bool::NO;
    let url = unsafe {
        NSURL::URLByResolvingBookmarkData_options_relativeToURL_bookmarkDataIsStale_error(
            &data,
            NSURLBookmarkResolutionOptions::WithoutUI,
            None,
            &mut is_stale,
        )
    }
    .map_err(|error| anyhow!("resolving a bookmark: {}", error.localizedDescription()))?;
    let path = start_accessing(url).context("a bookmark resolved to no file path")?;
    let renewed = if is_stale.as_bool() {
        // Made from the resolved URL, which is in ACCESSED now.
        Some(bookmark(&path)?)
    } else {
        None
    };
    Ok(ResolvedBookmark { path, renewed })
}

fn main_thread() -> Result<MainThreadMarker> {
    MainThreadMarker::new().context("security-scoped bookmarks are main-thread only")
}
