use std::sync::Mutex;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::config;

#[derive(Clone, Debug, serde::Serialize)]
pub struct UpdateInfo {
    version: String,
    body: String,
}

#[derive(Default)]
pub struct UpdateState {
    pending: Option<PendingUpdate>,
}

struct PendingUpdate {
    info: UpdateInfo,
    update: Update,
}

impl From<&Update> for UpdateInfo {
    fn from(update: &Update) -> Self {
        Self {
            version: update.version.clone(),
            body: update.body.clone().unwrap_or_default(),
        }
    }
}

pub type SharedUpdateState = Mutex<UpdateState>;

fn emit_event<T: serde::Serialize>(app: &AppHandle, event: &str, payload: &T) {
    if let Some(window) = app.get_webview_window("main") {
        let Ok(event_json) = serde_json::to_string(event) else {
            tracing::warn!(
                scope = "updater",
                event = "serialize_event_name_failed",
                name = %event
            );
            return;
        };
        let Ok(payload_json) = serde_json::to_string(payload) else {
            tracing::warn!(
                scope = "updater",
                event = "serialize_event_payload_failed",
                name = %event
            );
            return;
        };
        let js = format!(
            "window.dispatchEvent(new CustomEvent({event_json}, {{ detail: {payload_json} }}))"
        );
        if let Err(error) = window.eval(&js) {
            tracing::warn!(
                scope = "updater",
                event = "emit_event_failed",
                name = %event,
                %error
            );
        }
    }
}

async fn fetch_update(app: &AppHandle) -> Result<Option<Update>, String> {
    let updater = match app.updater() {
        Ok(updater) => updater,
        Err(error) => return Err(format!("failed to initialize updater: {error}")),
    };
    updater
        .check()
        .await
        .map_err(|error| format!("failed to check for updates: {error}"))
}

fn store_update(app: &AppHandle, update: Update) -> Result<UpdateInfo, String> {
    let update_info = UpdateInfo::from(&update);

    let update_state = app.state::<SharedUpdateState>();
    let mut update_state = update_state
        .lock()
        .map_err(|error| format!("failed to store pending update state: {error}"))?;
    update_state.pending = Some(PendingUpdate {
        info: update_info.clone(),
        update,
    });

    Ok(update_info)
}

async fn check_for_update(app: &AppHandle) -> Result<Option<UpdateInfo>, String> {
    let Some(update) = fetch_update(app).await? else {
        return Ok(None);
    };
    store_update(app, update).map(Some)
}

pub async fn check_for_updates(app: AppHandle) {
    let cfg = config::load(&app);
    if !cfg.auto_update {
        return;
    }

    let update_info = match check_for_update(&app).await {
        Ok(Some(update_info)) => update_info,
        Ok(None) => return,
        Err(error) => {
            tracing::error!(
                scope = "updater",
                event = "check_for_updates_failed",
                %error
            );
            return;
        }
    };

    emit_event(&app, "stremio-update-available", &update_info);
}

#[tauri::command]
pub async fn check_for_updates_now(app: AppHandle) -> Result<Option<UpdateInfo>, String> {
    check_for_update(&app).await
}

#[tauri::command]
pub async fn install_update(app: AppHandle) -> Result<(), String> {
    let update = {
        let update_state = app.state::<SharedUpdateState>();
        let update_state = update_state
            .lock()
            .map_err(|error| format!("failed to read pending update state: {error}"))?;
        update_state
            .pending
            .as_ref()
            .map(|pending| pending.update.clone())
    };

    let update = match update {
        Some(update) => update,
        None => fetch_update(&app).await?.ok_or("no update available")?,
    };

    let app_clone = app.clone();
    let mut downloaded_total = 0usize;
    update
        .download_and_install(
            move |downloaded, total| {
                downloaded_total = downloaded_total.saturating_add(downloaded);
                let payload = serde_json::json!({
                    "downloaded": downloaded_total,
                    "total": total.unwrap_or(0)
                });
                emit_event(&app_clone, "stremio-update-progress", &payload);
            },
            {
                let app = app.clone();
                move || {
                    let payload = serde_json::json!({
                        "downloaded": 1,
                        "total": 1
                    });
                    emit_event(&app, "stremio-update-progress", &payload);
                }
            },
        )
        .await
        .map_err(|e| e.to_string())?;

    if let Ok(mut update_state) = app.state::<SharedUpdateState>().lock() {
        update_state.pending = None;
    }

    app.restart();
}

#[tauri::command]
pub fn get_pending_update(update_state: State<'_, SharedUpdateState>) -> Option<UpdateInfo> {
    update_state
        .lock()
        .ok()
        .and_then(|state| state.pending.as_ref().map(|pending| pending.info.clone()))
}

#[tauri::command]
pub fn get_auto_update_enabled(app: AppHandle) -> bool {
    config::load(&app).auto_update
}

#[tauri::command]
pub fn set_auto_update_enabled(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut cfg = config::load(&app);
    cfg.auto_update = enabled;
    config::save(&app, &cfg).map_err(|e| {
        tracing::error!(
            scope = "updater",
            event = "persist_auto_update_failed",
            error = %e
        );
        e
    })
}

pub fn snapshot_pending(state: &SharedUpdateState) -> Option<UpdateInfo> {
    state
        .lock()
        .ok()
        .and_then(|s| s.pending.as_ref().map(|pending| pending.info.clone()))
}
