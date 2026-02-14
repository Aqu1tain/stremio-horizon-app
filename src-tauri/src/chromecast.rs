use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Mutex;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use native_tls::TlsConnector;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

const HEARTBEAT_NS: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
const CONNECTION_NS: &str = "urn:x-cast:com.google.cast.tp.connection";
const RECEIVER_NS: &str = "urn:x-cast:com.google.cast.receiver";
const STREMIO_NS: &str = "urn:x-cast:com.stremio";

const SENDER_ID: &str = "sender-0";
const RECEIVER_ID: &str = "receiver-0";

const READ_TIMEOUT: Duration = Duration::from_millis(500);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const DISCOVERY_DURATION: Duration = Duration::from_secs(5);

// --- Protobuf encoding/decoding ---

fn encode_varint(buf: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            buf.push(byte);
            return;
        }
        buf.push(byte | 0x80);
    }
}

fn decode_varint(data: &[u8], pos: usize) -> Result<(u64, usize), String> {
    let mut result: u64 = 0;
    let mut shift = 0;
    let mut i = pos;
    loop {
        if i >= data.len() {
            return Err("varint: unexpected end of data".into());
        }
        let byte = data[i];
        result |= ((byte & 0x7F) as u64) << shift;
        i += 1;
        if byte & 0x80 == 0 {
            return Ok((result, i));
        }
        shift += 7;
        if shift >= 64 {
            return Err("varint: too many bytes".into());
        }
    }
}

fn encode_varint_field(buf: &mut Vec<u8>, field: u32, value: u64) {
    encode_varint(buf, ((field as u64) << 3) | 0);
    encode_varint(buf, value);
}

fn encode_bytes_field(buf: &mut Vec<u8>, field: u32, value: &[u8]) {
    encode_varint(buf, ((field as u64) << 3) | 2);
    encode_varint(buf, value.len() as u64);
    buf.extend_from_slice(value);
}

fn encode_cast_message(source: &str, dest: &str, namespace: &str, payload: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    encode_varint_field(&mut buf, 1, 0); // protocol_version = CASTV2_1_0
    encode_bytes_field(&mut buf, 2, source.as_bytes());
    encode_bytes_field(&mut buf, 3, dest.as_bytes());
    encode_bytes_field(&mut buf, 4, namespace.as_bytes());
    encode_varint_field(&mut buf, 5, 0); // payload_type = STRING
    encode_bytes_field(&mut buf, 6, payload.as_bytes());
    buf
}

#[derive(Debug)]
#[allow(dead_code)]
struct CastMessage {
    source_id: String,
    destination_id: String,
    namespace: String,
    payload_utf8: Option<String>,
}

fn decode_cast_message(data: &[u8]) -> Result<CastMessage, String> {
    let mut pos = 0;
    let mut source_id = String::new();
    let mut destination_id = String::new();
    let mut namespace = String::new();
    let mut payload_utf8 = None;

    while pos < data.len() {
        let (tag, new_pos) = decode_varint(data, pos)?;
        pos = new_pos;
        let field_num = (tag >> 3) as u32;
        let wire_type = (tag & 0x7) as u32;

        match wire_type {
            0 => {
                let (_, new_pos) = decode_varint(data, pos)?;
                pos = new_pos;
            }
            2 => {
                let (len, new_pos) = decode_varint(data, pos)?;
                let end = new_pos + len as usize;
                if end > data.len() {
                    return Err("protobuf: data truncated".into());
                }
                let bytes = &data[new_pos..end];
                match field_num {
                    2 => source_id = String::from_utf8_lossy(bytes).into(),
                    3 => destination_id = String::from_utf8_lossy(bytes).into(),
                    4 => namespace = String::from_utf8_lossy(bytes).into(),
                    6 => payload_utf8 = Some(String::from_utf8_lossy(bytes).into()),
                    _ => {}
                }
                pos = end;
            }
            _ => return Err(format!("protobuf: unsupported wire type {wire_type}")),
        }
    }

    Ok(CastMessage {
        source_id,
        destination_id,
        namespace,
        payload_utf8,
    })
}

// --- Cast connection ---

struct CastConnection {
    stream: native_tls::TlsStream<TcpStream>,
    request_id: u32,
}

impl CastConnection {
    fn connect(host: &str, port: u16) -> Result<Self, String> {
        let tcp = TcpStream::connect((host, port)).map_err(|e| format!("tcp connect: {e}"))?;
        tcp.set_read_timeout(Some(READ_TIMEOUT)).ok();
        tcp.set_nodelay(true).ok();

        let connector = TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .map_err(|e| format!("tls build: {e}"))?;

        let stream = connector
            .connect(host, tcp)
            .map_err(|e| format!("tls connect: {e}"))?;

        Ok(Self {
            stream,
            request_id: 0,
        })
    }

    fn next_request_id(&mut self) -> u32 {
        self.request_id += 1;
        self.request_id
    }

    fn send(&mut self, source: &str, dest: &str, namespace: &str, payload: &str) -> Result<(), String> {
        let msg = encode_cast_message(source, dest, namespace, payload);
        let len = (msg.len() as u32).to_be_bytes();
        self.stream.write_all(&len).map_err(|e| format!("send len: {e}"))?;
        self.stream.write_all(&msg).map_err(|e| format!("send msg: {e}"))?;
        Ok(())
    }

    fn receive(&mut self) -> Result<Option<CastMessage>, String> {
        let mut len_buf = [0u8; 4];
        match self.stream.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(ref e) if is_timeout(e) => return Ok(None),
            Err(e) => return Err(format!("recv len: {e}")),
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        self.stream.read_exact(&mut buf).map_err(|e| format!("recv msg: {e}"))?;
        decode_cast_message(&buf).map(Some)
    }

    fn open_connection(&mut self, dest: &str) -> Result<(), String> {
        let payload = r#"{"type":"CONNECT"}"#;
        self.send(SENDER_ID, dest, CONNECTION_NS, payload)
    }

    fn close_connection(&mut self, dest: &str) -> Result<(), String> {
        let payload = r#"{"type":"CLOSE"}"#;
        self.send(SENDER_ID, dest, CONNECTION_NS, payload)
    }

    fn ping(&mut self) -> Result<(), String> {
        self.send(SENDER_ID, RECEIVER_ID, HEARTBEAT_NS, r#"{"type":"PING"}"#)
    }

    fn pong(&mut self) -> Result<(), String> {
        self.send(SENDER_ID, RECEIVER_ID, HEARTBEAT_NS, r#"{"type":"PONG"}"#)
    }

    fn launch_app(&mut self, app_id: &str) -> Result<u32, String> {
        let req_id = self.next_request_id();
        let payload = format!(r#"{{"type":"LAUNCH","appId":"{app_id}","requestId":{req_id}}}"#);
        self.send(SENDER_ID, RECEIVER_ID, RECEIVER_NS, &payload)?;
        Ok(req_id)
    }

    fn stop_app(&mut self, session_id: &str) -> Result<(), String> {
        let req_id = self.next_request_id();
        let payload = format!(
            r#"{{"type":"STOP","sessionId":"{session_id}","requestId":{req_id}}}"#
        );
        self.send(SENDER_ID, RECEIVER_ID, RECEIVER_NS, &payload)
    }

    fn send_custom(&mut self, transport_id: &str, message: &str) -> Result<(), String> {
        self.send(SENDER_ID, transport_id, STREMIO_NS, message)
    }
}

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

// --- Cast session thread ---

enum CastCommand {
    Launch {
        app_id: String,
        reply: Sender<Result<SessionInfo, String>>,
    },
    Send {
        message: String,
        reply: Sender<Result<(), String>>,
    },
    Disconnect,
}

#[derive(Clone, Serialize)]
pub struct SessionInfo {
    session_id: String,
    transport_id: String,
}

fn emit_event(app: &AppHandle, event: &str, payload: &str) {
    if let Some(window) = app.get_webview_window("main") {
        let json_str = serde_json::to_string(payload).unwrap_or_default();
        let js = format!(
            "window.dispatchEvent(new CustomEvent('{event}', {{ detail: JSON.parse({json_str}) }}))"
        );
        let _ = window.eval(&js);
    }
}

fn emit_state(app: &AppHandle, cast_state: &str, session_state: &str) {
    emit_event(
        app,
        "chromecast:state-changed",
        &format!(r#"{{"castState":"{cast_state}","sessionState":"{session_state}"}}"#),
    );
}

fn cast_session_loop(
    mut conn: CastConnection,
    cmd_rx: Receiver<CastCommand>,
    app: AppHandle,
    device_name: String,
) {
    if let Err(e) = conn.open_connection(RECEIVER_ID) {
        eprintln!("chromecast: open connection failed: {e}");
        return;
    }

    emit_state(&app, "NOT_CONNECTED", "NO_SESSION");

    let mut session: Option<SessionInfo> = None;
    let mut last_ping = Instant::now();
    let mut running = true;

    while running {
        // Receive messages from the Chromecast
        match conn.receive() {
            Ok(Some(msg)) => {
                handle_cast_message(&mut conn, &msg, &mut session, &app, &device_name);
            }
            Ok(None) => {} // timeout
            Err(e) => {
                eprintln!("chromecast: receive error: {e}");
                break;
            }
        }

        // Process commands
        loop {
            match cmd_rx.try_recv() {
                Ok(CastCommand::Launch { app_id, reply }) => {
                    let result = handle_launch(&mut conn, &app_id);
                    if result.is_ok() {
                        emit_state(&app, "CONNECTING", "SESSION_STARTING");
                    }
                    // Wait for RECEIVER_STATUS with session info
                    let session_result = if result.is_ok() {
                        wait_for_session(&mut conn, &mut session, &app, &device_name)
                    } else {
                        Err(result.unwrap_err())
                    };
                    let _ = reply.send(session_result);
                }
                Ok(CastCommand::Send { message, reply }) => {
                    let result = if let Some(ref s) = session {
                        conn.send_custom(&s.transport_id, &message)
                    } else {
                        Err("no active session".into())
                    };
                    let _ = reply.send(result);
                }
                Ok(CastCommand::Disconnect) => {
                    if let Some(ref s) = session {
                        let _ = conn.stop_app(&s.session_id);
                        let _ = conn.close_connection(&s.transport_id);
                    }
                    let _ = conn.close_connection(RECEIVER_ID);
                    session = None;
                    emit_state(&app, "NOT_CONNECTED", "SESSION_ENDED");
                    running = false;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    running = false;
                    break;
                }
            }
        }

        // Heartbeat
        if last_ping.elapsed() >= HEARTBEAT_INTERVAL {
            if conn.ping().is_err() {
                break;
            }
            last_ping = Instant::now();
        }
    }

    emit_state(&app, "NOT_CONNECTED", "SESSION_ENDED");
}

fn handle_cast_message(
    conn: &mut CastConnection,
    msg: &CastMessage,
    session: &mut Option<SessionInfo>,
    app: &AppHandle,
    _device_name: &str,
) {
    let payload = match &msg.payload_utf8 {
        Some(p) => p,
        None => return,
    };

    match msg.namespace.as_str() {
        HEARTBEAT_NS => {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(payload) {
                if json.get("type").and_then(|t| t.as_str()) == Some("PING") {
                    let _ = conn.pong();
                }
            }
        }
        RECEIVER_NS => {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(payload) {
                if json.get("type").and_then(|t| t.as_str()) == Some("RECEIVER_STATUS") {
                    if let Some(apps) = json
                        .get("status")
                        .and_then(|s| s.get("applications"))
                        .and_then(|a| a.as_array())
                    {
                        if let Some(app_info) = apps.first() {
                            let transport_id = app_info
                                .get("transportId")
                                .and_then(|t| t.as_str())
                                .unwrap_or_default()
                                .to_string();
                            let session_id = app_info
                                .get("sessionId")
                                .and_then(|s| s.as_str())
                                .unwrap_or_default()
                                .to_string();

                            if session.is_none() && !transport_id.is_empty() {
                                let info = SessionInfo {
                                    session_id,
                                    transport_id,
                                };
                                *session = Some(info);
                            }
                        }
                    }
                }
            }
        }
        STREMIO_NS => {
            emit_event(app, "chromecast:message", payload);
        }
        _ => {}
    }
}

fn handle_launch(conn: &mut CastConnection, app_id: &str) -> Result<u32, String> {
    conn.launch_app(app_id)
}

fn wait_for_session(
    conn: &mut CastConnection,
    session: &mut Option<SessionInfo>,
    app: &AppHandle,
    device_name: &str,
) -> Result<SessionInfo, String> {
    let deadline = Instant::now() + Duration::from_secs(30);

    while Instant::now() < deadline {
        match conn.receive() {
            Ok(Some(msg)) => {
                handle_cast_message(conn, &msg, session, app, device_name);
                if let Some(ref info) = session {
                    // Connect to the app's transport
                    conn.open_connection(&info.transport_id)?;
                    emit_state(app, "CONNECTED", "SESSION_STARTED");
                    return Ok(info.clone());
                }
            }
            Ok(None) => continue,
            Err(e) => return Err(e),
        }
    }

    Err("timeout waiting for session".into())
}

// --- Tauri state ---

#[derive(Serialize, Deserialize, Clone)]
pub struct DeviceInfo {
    pub name: String,
    pub host: String,
    pub port: u16,
}

pub struct CastManagerState {
    cmd_tx: Option<Sender<CastCommand>>,
    thread_handle: Option<JoinHandle<()>>,
    device_name: Option<String>,
}

impl Default for CastManagerState {
    fn default() -> Self {
        Self {
            cmd_tx: None,
            thread_handle: None,
            device_name: None,
        }
    }
}

pub type CastManager = Mutex<CastManagerState>;

// --- Tauri commands ---

#[tauri::command]
pub fn chromecast_discover() -> Result<Vec<DeviceInfo>, String> {
    let mdns = mdns_sd::ServiceDaemon::new().map_err(|e| format!("mdns init: {e}"))?;
    let receiver = mdns
        .browse("_googlecast._tcp.local.")
        .map_err(|e| format!("mdns browse: {e}"))?;

    let mut devices = Vec::new();
    let deadline = Instant::now() + DISCOVERY_DURATION;

    while Instant::now() < deadline {
        let timeout = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(timeout) {
            Ok(mdns_sd::ServiceEvent::ServiceResolved(info)) => {
                let name = info
                    .get_property_val_str("fn")
                    .unwrap_or(info.get_fullname())
                    .to_string();
                if let Some(addr) = info.get_addresses().iter().next() {
                    devices.push(DeviceInfo {
                        name,
                        host: addr.to_string(),
                        port: info.get_port(),
                    });
                }
            }
            Err(_) => break,
            _ => continue,
        }
    }

    let _ = mdns.shutdown();
    Ok(devices)
}

#[tauri::command]
pub fn chromecast_connect(
    host: String,
    port: u16,
    name: String,
    app_handle: AppHandle,
    state: State<'_, CastManager>,
) -> Result<(), String> {
    let mut mgr = state.lock().map_err(|e| e.to_string())?;

    // Disconnect existing session
    if let Some(tx) = mgr.cmd_tx.take() {
        let _ = tx.send(CastCommand::Disconnect);
    }
    if let Some(handle) = mgr.thread_handle.take() {
        let _ = handle.join();
    }

    let conn = CastConnection::connect(&host, port)?;

    let (cmd_tx, cmd_rx) = mpsc::channel();
    let device_name = name.clone();
    let app = app_handle.clone();

    let handle = thread::spawn(move || {
        cast_session_loop(conn, cmd_rx, app, device_name);
    });

    mgr.cmd_tx = Some(cmd_tx);
    mgr.thread_handle = Some(handle);
    mgr.device_name = Some(name);

    Ok(())
}

#[tauri::command]
pub fn chromecast_launch(
    app_id: String,
    state: State<'_, CastManager>,
) -> Result<SessionInfo, String> {
    let mgr = state.lock().map_err(|e| e.to_string())?;
    let tx = mgr.cmd_tx.as_ref().ok_or("not connected")?;

    let (reply_tx, reply_rx) = mpsc::channel();
    tx.send(CastCommand::Launch {
        app_id,
        reply: reply_tx,
    })
    .map_err(|_| "cast thread gone")?;

    drop(mgr); // release lock while waiting
    reply_rx
        .recv_timeout(Duration::from_secs(35))
        .map_err(|_| "launch timeout")?
}

#[tauri::command]
pub fn chromecast_send(
    message: String,
    state: State<'_, CastManager>,
) -> Result<(), String> {
    let mgr = state.lock().map_err(|e| e.to_string())?;
    let tx = mgr.cmd_tx.as_ref().ok_or("not connected")?;

    let (reply_tx, reply_rx) = mpsc::channel();
    tx.send(CastCommand::Send {
        message,
        reply: reply_tx,
    })
    .map_err(|_| "cast thread gone")?;

    drop(mgr);
    reply_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| "send timeout")?
}

#[tauri::command]
pub fn chromecast_disconnect(state: State<'_, CastManager>) -> Result<(), String> {
    let mut mgr = state.lock().map_err(|e| e.to_string())?;

    if let Some(tx) = mgr.cmd_tx.take() {
        let _ = tx.send(CastCommand::Disconnect);
    }
    if let Some(handle) = mgr.thread_handle.take() {
        let _ = handle.join();
    }
    mgr.device_name = None;

    Ok(())
}

#[tauri::command]
pub fn chromecast_get_device_name(state: State<'_, CastManager>) -> Result<Option<String>, String> {
    let mgr = state.lock().map_err(|e| e.to_string())?;
    Ok(mgr.device_name.clone())
}
