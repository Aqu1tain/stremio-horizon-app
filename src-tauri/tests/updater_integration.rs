//! Level B integration tests for the updater module.
//!
//! Scope today: assertions that exercise the public surface without requiring
//! an `AppHandle` and without any network. Tests that need full IPC + HTTP
//! mocking of `tauri-plugin-updater` are deferred until the updater endpoint
//! can be redirected at runtime through our public API.

use std::sync::Mutex;
use stremio_horizon_app_lib::{snapshot_pending, UpdateState};

#[test]
fn snapshot_pending_is_none_on_default_state() {
    let state = Mutex::new(UpdateState::default());
    assert!(snapshot_pending(&state).is_none());
}
