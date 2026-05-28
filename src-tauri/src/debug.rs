use std::net::{SocketAddr, TcpStream};
use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};

use crate::config;
use crate::updater::{self, SharedUpdateState, UpdateInfo};

const SERVICE_PROBE_TIMEOUT: Duration = Duration::from_millis(200);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BuildStamp {
    Ok {
        frontend_commit: String,
        frontend_build_time: String,
        frontend_hash: String,
        app_commit: String,
        app_version: String,
    },
    Missing {
        reason: String,
    },
}

#[derive(Deserialize)]
struct RawBuildStamp {
    frontend_commit: String,
    frontend_build_time: String,
    frontend_hash: String,
    app_commit: String,
    app_version: String,
}

static BUILD_STAMP: OnceLock<BuildStamp> = OnceLock::new();

fn load_build_stamp(app: &AppHandle) -> BuildStamp {
    let resolver = app.asset_resolver();
    let Some(asset) = resolver.get("/build-stamp.json".to_string()) else {
        tracing::warn!(
            scope = "debug",
            event = "build_stamp_missing",
            reason = "asset not embedded"
        );
        return BuildStamp::Missing {
            reason: "build-stamp.json not present in embedded assets".to_string(),
        };
    };
    match serde_json::from_slice::<RawBuildStamp>(&asset.bytes) {
        Ok(raw) => BuildStamp::Ok {
            frontend_commit: raw.frontend_commit,
            frontend_build_time: raw.frontend_build_time,
            frontend_hash: raw.frontend_hash,
            app_commit: raw.app_commit,
            app_version: raw.app_version,
        },
        Err(error) => {
            tracing::warn!(
                scope = "debug",
                event = "build_stamp_parse_failed",
                %error
            );
            BuildStamp::Missing {
                reason: format!("build-stamp.json parse error: {error}"),
            }
        }
    }
}

#[tauri::command]
pub fn debug_build(app: AppHandle) -> BuildStamp {
    BUILD_STAMP
        .get_or_init(|| load_build_stamp(&app))
        .clone()
}

#[derive(Serialize)]
pub struct DebugState {
    proxy_port: u16,
    service_port: u16,
    service_alive: bool,
    auto_update: bool,
}

#[tauri::command]
pub fn debug_state(app: AppHandle) -> DebugState {
    let addr: SocketAddr = ([127, 0, 0, 1], crate::SERVICE_PORT).into();
    let service_alive = TcpStream::connect_timeout(&addr, SERVICE_PROBE_TIMEOUT).is_ok();
    let auto_update = config::load(&app).auto_update;

    DebugState {
        proxy_port: crate::PORT,
        service_port: crate::SERVICE_PORT,
        service_alive,
        auto_update,
    }
}

#[derive(Serialize)]
pub struct DebugUpdater {
    pending: Option<UpdateInfo>,
}

#[tauri::command]
pub fn debug_updater(state: State<'_, SharedUpdateState>) -> DebugUpdater {
    DebugUpdater {
        pending: updater::snapshot_pending(&state),
    }
}

#[derive(Serialize)]
pub struct DebugEvents {
    recent: Vec<serde_json::Value>,
}

#[tauri::command]
pub fn debug_events() -> DebugEvents {
    DebugEvents { recent: Vec::new() }
}

#[derive(Serialize)]
pub struct DebugLogs {
    recent: Vec<serde_json::Value>,
}

#[tauri::command]
pub fn debug_logs() -> DebugLogs {
    DebugLogs { recent: Vec::new() }
}
