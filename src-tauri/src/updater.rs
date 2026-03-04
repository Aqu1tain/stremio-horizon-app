use tauri::{AppHandle, Manager};
use tauri_plugin_updater::UpdaterExt;

use crate::config;

fn emit_event(app: &AppHandle, event: &str, payload: &str) {
    if let Some(window) = app.get_webview_window("main") {
        let json_str = serde_json::to_string(payload).unwrap_or_default();
        let js = format!(
            "window.dispatchEvent(new CustomEvent('{event}', {{ detail: JSON.parse({json_str}) }}))"
        );
        let _ = window.eval(&js);
    }
}

pub async fn check_for_updates(app: AppHandle) {
    let cfg = config::load(&app);
    if !cfg.auto_update {
        return;
    }

    let Ok(updater) = app.updater() else { return };
    let update = match updater.check().await {
        Ok(Some(update)) => update,
        _ => return,
    };

    let version = update.version.clone();
    let body = update.body.clone().unwrap_or_default();
    let payload = serde_json::json!({ "version": version, "body": body }).to_string();
    emit_event(&app, "stremio-update-available", &payload);
}

#[tauri::command]
pub async fn install_update(app: AppHandle) -> Result<(), String> {
    let updater = app.updater().map_err(|e| e.to_string())?;
    let update = updater
        .check()
        .await
        .map_err(|e| e.to_string())?
        .ok_or("no update available")?;

    let app_clone = app.clone();
    update
        .download_and_install(
            move |downloaded, total| {
                let payload = serde_json::json!({
                    "downloaded": downloaded,
                    "total": total.unwrap_or(0)
                })
                .to_string();
                emit_event(&app_clone, "stremio-update-progress", &payload);
            },
            || {},
        )
        .await
        .map_err(|e| e.to_string())?;

    app.restart();
}

#[tauri::command]
pub fn get_auto_update_enabled(app: AppHandle) -> bool {
    config::load(&app).auto_update
}

#[tauri::command]
pub fn set_auto_update_enabled(app: AppHandle, enabled: bool) {
    let mut cfg = config::load(&app);
    cfg.auto_update = enabled;
    config::save(&app, &cfg);
}
