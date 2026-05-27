use std::collections::VecDeque;
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

use crate::config;
use crate::updater::{self, SharedUpdateState, UpdateInfo};

const SERVICE_PROBE_TIMEOUT: Duration = Duration::from_millis(200);
const RING_CAPACITY: usize = 200;

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

// -------- ring buffers --------

struct RingBuffer<T> {
    items: VecDeque<T>,
    capacity: usize,
}

impl<T> RingBuffer<T> {
    fn new(capacity: usize) -> Self {
        Self {
            items: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn push(&mut self, item: T) {
        if self.items.len() == self.capacity {
            self.items.pop_front();
        }
        self.items.push_back(item);
    }

    fn snapshot(&self) -> Vec<T>
    where
        T: Clone,
    {
        self.items.iter().cloned().collect()
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct EventRecord {
    timestamp_ms: u64,
    name: String,
    payload: serde_json::Value,
}

#[derive(Clone, Debug, Serialize)]
pub struct LogRecord {
    timestamp_ms: u64,
    level: String,
    scope: Option<String>,
    event: Option<String>,
    fields: serde_json::Map<String, serde_json::Value>,
}

static EVENT_RING: OnceLock<Arc<Mutex<RingBuffer<EventRecord>>>> = OnceLock::new();
static LOG_RING: OnceLock<Arc<Mutex<RingBuffer<LogRecord>>>> = OnceLock::new();

pub fn init_rings() {
    let _ = EVENT_RING.set(Arc::new(Mutex::new(RingBuffer::new(RING_CAPACITY))));
    let _ = LOG_RING.set(Arc::new(Mutex::new(RingBuffer::new(RING_CAPACITY))));
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn record_event<T: serde::Serialize>(name: &str, payload: &T) {
    let payload = serde_json::to_value(payload).unwrap_or(serde_json::Value::Null);
    let record = EventRecord {
        timestamp_ms: now_ms(),
        name: name.to_string(),
        payload,
    };
    if let Some(ring) = EVENT_RING.get() {
        if let Ok(mut guard) = ring.lock() {
            guard.push(record);
        }
    }
}

#[derive(Default)]
struct JsonVisitor {
    scope: Option<String>,
    event: Option<String>,
    fields: serde_json::Map<String, serde_json::Value>,
}

impl JsonVisitor {
    fn assign(&mut self, field: &Field, value: serde_json::Value) {
        match field.name() {
            "scope" => self.scope = value.as_str().map(String::from),
            "event" => self.event = value.as_str().map(String::from),
            other => {
                self.fields.insert(other.to_string(), value);
            }
        }
    }
}

impl Visit for JsonVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.assign(field, serde_json::Value::String(value.to_string()));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.assign(field, serde_json::Value::Number(value.into()));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.assign(field, serde_json::Value::Number(value.into()));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.assign(field, serde_json::Value::Bool(value));
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.assign(field, serde_json::Value::String(format!("{value:?}")));
    }
}

pub struct RingLayer;

impl<S: Subscriber> Layer<S> for RingLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = JsonVisitor::default();
        event.record(&mut visitor);
        let record = LogRecord {
            timestamp_ms: now_ms(),
            level: event.metadata().level().to_string(),
            scope: visitor.scope,
            event: visitor.event,
            fields: visitor.fields,
        };
        if let Some(ring) = LOG_RING.get() {
            if let Ok(mut guard) = ring.lock() {
                guard.push(record);
            }
        }
    }
}

#[derive(Serialize)]
pub struct DebugEvents {
    recent: Vec<EventRecord>,
}

#[tauri::command]
pub fn debug_events() -> DebugEvents {
    let recent = EVENT_RING
        .get()
        .and_then(|r| r.lock().ok().map(|g| g.snapshot()))
        .unwrap_or_default();
    DebugEvents { recent }
}

#[derive(Serialize)]
pub struct DebugLogs {
    recent: Vec<LogRecord>,
}

#[tauri::command]
pub fn debug_logs() -> DebugLogs {
    let recent = LOG_RING
        .get()
        .and_then(|r| r.lock().ok().map(|g| g.snapshot()))
        .unwrap_or_default();
    DebugLogs { recent }
}
