use crate::{App, Global};
use std::time::SystemTime;

/// What the hot-reload client has seen since the app started.
///
/// For tools that drive a running app: an inspector can wait until
/// `patches_received` moves past the count it read before an edit, then
/// read whether the patch applied, instead of tailing the dev server's log.
/// Present only in a build that compiles hot reloading in (a debug build
/// with the `hot-reload` feature); see [`App::hot_reload_status`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HotReloadStatus {
    /// Connected to a `dx` development server right now.
    pub connected: bool,
    /// Patches that arrived for this process, applied or not.
    pub patches_received: u64,
    /// Patches that were applied.
    pub patches_applied: u64,
    /// When the last patch was applied.
    pub last_applied_at: Option<SystemTime>,
    /// Why the last patch failed to apply, why the connection ended, or why
    /// there never was one. Cleared by the next applied patch.
    pub last_error: Option<String>,
}

impl Global for HotReloadStatus {}

// Filled only by the hot-reload client, which a build without it lacks.
#[cfg(any(gpui_hot_reload, test))]
impl HotReloadStatus {
    pub(crate) fn applied(&mut self, at: SystemTime) {
        self.patches_received += 1;
        self.patches_applied += 1;
        self.last_applied_at = Some(at);
        self.last_error = None;
    }

    pub(crate) fn failed(&mut self, error: String) {
        self.patches_received += 1;
        self.last_error = Some(error);
    }

    pub(crate) fn ended(&mut self, error: Option<String>) {
        self.connected = false;
        if error.is_some() {
            self.last_error = error;
        }
    }
}

impl App {
    /// The hot-reload client's state, or `None` in a build without hot
    /// reloading — a release build, or one without the `hot-reload` feature.
    pub fn hot_reload_status(&self) -> Option<&HotReloadStatus> {
        self.try_global::<HotReloadStatus>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_applied_patch_clears_the_last_failure() {
        let mut status = HotReloadStatus::default();
        status.failed("symbol missing".into());
        assert_eq!(status.patches_received, 1);
        assert_eq!(status.patches_applied, 0);
        assert_eq!(status.last_error.as_deref(), Some("symbol missing"));

        let at = SystemTime::UNIX_EPOCH;
        status.applied(at);
        assert_eq!(status.patches_received, 2);
        assert_eq!(status.patches_applied, 1);
        assert_eq!(status.last_applied_at, Some(at));
        assert_eq!(status.last_error, None);
    }

    #[test]
    fn a_closed_connection_keeps_the_counts() {
        let mut status = HotReloadStatus {
            connected: true,
            ..Default::default()
        };
        status.applied(SystemTime::UNIX_EPOCH);
        status.ended(None);
        assert!(!status.connected);
        assert_eq!(status.patches_applied, 1);
        assert_eq!(status.last_error, None);

        status.ended(Some("dev server went away".into()));
        assert_eq!(status.last_error.as_deref(), Some("dev server went away"));
    }
}
