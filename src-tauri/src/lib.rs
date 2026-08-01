use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::{mpsc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tauri::webview::WebviewWindowBuilder;
use tauri::{Manager, RunEvent, WebviewUrl};
use threadpool::ThreadPool;

mod chromecast;
mod config;
mod debug;
mod discord;
mod downloads;
mod proxy_security;
mod updater;

pub use updater::{snapshot_pending, UpdateState};

pub(crate) const PORT: u16 = 11480;
pub(crate) const SERVICE_PORT: u16 = 11470;
const SERVICE_TIMEOUT: Duration = Duration::from_secs(15);
const POLL_INTERVAL: Duration = Duration::from_millis(200);
const EXT_POOL_SIZE: usize = 8;
const SVC_POOL_SIZE: usize = 4;
const DOWNLOAD_POOL_SIZE: usize = 8;
const EXT_TIMEOUT: Duration = Duration::from_secs(30);
const EXT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const SVC_TIMEOUT: Duration = Duration::from_secs(60);
const SVC_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

struct ServiceProcess {
    child: Child,
    #[cfg(target_os = "windows")]
    _job: Option<windows_job::JobHandle>,
}

impl ServiceProcess {
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for ServiceProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(target_os = "windows")]
mod windows_job {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    pub struct JobHandle(HANDLE);

    unsafe impl Send for JobHandle {}
    unsafe impl Sync for JobHandle {}

    impl Drop for JobHandle {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0); }
        }
    }

    pub fn assign_child_to_kill_on_close_job(
        child: &std::process::Child,
    ) -> Option<JobHandle> {
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return None;
            }

            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

            let ok = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                CloseHandle(job);
                return None;
            }

            let ok = AssignProcessToJobObject(job, child.as_raw_handle() as _);
            if ok == 0 {
                CloseHandle(job);
                return None;
            }

            Some(JobHandle(job))
        }
    }
}

type ServiceState = Mutex<Option<ServiceProcess>>;

fn init_tracing() {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};
    debug::init_rings();
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().json())
        .with(debug::RingLayer)
        .try_init();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(target_os = "linux")]
    std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");

    init_tracing();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(Mutex::new(chromecast::CastManagerState::default()))
        .manage(updater::SharedUpdateState::default())
        .manage(discord::DiscordManager::default())
        .manage(downloads::DownloadManager::default())
        .manage(Mutex::new(None::<ServiceProcess>))
        .invoke_handler(tauri::generate_handler![
            chromecast::chromecast_discover,
            chromecast::chromecast_connect,
            chromecast::chromecast_launch,
            chromecast::chromecast_send,
            chromecast::chromecast_disconnect,
            chromecast::chromecast_get_device_name,
            updater::install_update,
            updater::check_for_updates_now,
            updater::get_pending_update,
            updater::get_auto_update_enabled,
            updater::set_auto_update_enabled,
            discord::discord_connect,
            discord::discord_disconnect,
            discord::discord_set_activity,
            discord::discord_clear_activity,
            downloads::download_start,
            downloads::download_list,
            downloads::download_playback_url,
            downloads::download_pause,
            downloads::download_resume,
            downloads::download_delete,
            debug::debug_build,
            debug::debug_state,
            debug::debug_updater,
            debug::debug_events,
            debug::debug_logs,
        ])
        .setup(|app| {
            downloads::initialize(app)?;
            start_local_server(app);

            let service = spawn_streaming_service(app);
            let spawned = service.is_some();
            *app.state::<ServiceState>().lock().unwrap() = service;

            let ready = wait_for_service();
            create_window(app)?;
            downloads::resume_pending(app.handle());

            if !spawned || !ready {
                warn_service_failed(app);
            }

            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                updater::check_for_updates(handle).await;
            });

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|app, event| {
        if let RunEvent::Exit = event {
            discord::shutdown(app);
            if let Some(mut svc) = app.state::<ServiceState>().lock().unwrap().take() {
                svc.kill();
            }
        }
    });
}

fn wait_for_service() -> bool {
    let deadline = Instant::now() + SERVICE_TIMEOUT;
    while Instant::now() < deadline {
        if TcpStream::connect(format!("127.0.0.1:{SERVICE_PORT}")).is_ok() {
            return true;
        }
        thread::sleep(POLL_INTERVAL);
    }
    eprintln!("streaming service did not start in time");
    false
}

fn create_window(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("http://localhost:{PORT}");

    #[allow(unused_mut)]
    let mut builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url.parse()?))
        .title("Stremio Horizon")
        .inner_size(1280.0, 800.0)
        .min_inner_size(900.0, 600.0)
        .initialization_script(SERVICE_WORKER_UPDATE_BRIDGE)
        .initialization_script(FULLSCREEN_BRIDGE)
        .initialization_script(FETCH_INTERCEPTOR);

    #[cfg(target_os = "windows")]
    {
        builder = builder.additional_browser_args(
            "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,msWebView2EnableTrackingPrevention"
        );
    }

    #[allow(unused_variables)]
    let window = builder.build()?;

    #[cfg(target_os = "linux")]
    {
        use webkit2gtk::{HardwareAccelerationPolicy, SettingsExt, WebViewExt};
        let _ = window.with_webview(|webview| {
            let wv = webview.inner();
            if let Some(settings) = WebViewExt::settings(&wv) {
                settings.set_hardware_acceleration_policy(HardwareAccelerationPolicy::Never);
            }
        });
    }

    Ok(())
}

// This runs before the bundled frontend, including when WebKit serves an older cached entry point.
// Once the new service worker takes control, reload into its matching precached asset set instead
// of letting the old page request hashed chunks that no longer exist in the new application.
const SERVICE_WORKER_UPDATE_BRIDGE: &str = r#"
(function() {
    if (!('serviceWorker' in navigator)) return;

    let reloading = false;
    navigator.serviceWorker.addEventListener('controllerchange', function() {
        if (reloading) return;
        reloading = true;
        window.location.reload();
    });
})();
"#;

const FULLSCREEN_BRIDGE: &str = r#"
(function() {
    const invoke = (cmd, args) => {
        const tauri = window.__TAURI_INTERNALS__;
        if (!tauri?.invoke) return Promise.resolve();
        return tauri.invoke(cmd, args);
    };

    const applyFullscreen = (value) => {
        if (value === document._tauriFullscreen) return;
        document._tauriFullscreen = value;
        document.dispatchEvent(new Event('fullscreenchange'));
    };

    // macOS animates the transition and emits a single resize part way through it, while the
    // window still reports its old state: measured from inside the webview, it answers stale
    // at 150ms and only reads correctly around 600ms. So never poll before it has settled,
    // and keep checking afterwards in case the machine is slower.
    const RESYNC_DELAYS = [600, 1200, 2000];

    // While we are driving the transition ourselves the mirror is already correct, so a poll
    // landing mid-animation could only tell us something stale and flip the toggle back.
    // Ignore answers for the length of the animation; the later polls still verify.
    const LOCAL_TRANSITION_MS = 1000;
    let trustWindowFrom = 0;

    const setFullscreen = (value) => {
        trustWindowFrom = Date.now() + LOCAL_TRANSITION_MS;
        return invoke('plugin:window|set_fullscreen', { label: 'main', value })
            .then(() => applyFullscreen(value));
    };

    // Fullscreen can also be left without going through us — the green button, Cmd+Ctrl+F,
    // the Window menu. Those resize the webview without touching the mirror above, which
    // would leave document.fullscreenElement lying to the UI, so re-read the real window
    // state on resize.
    let syncTimers = [];
    const syncFromWindow = () => {
        syncTimers.forEach(clearTimeout);
        syncTimers = RESYNC_DELAYS.map((delay) => setTimeout(() => {
            if (Date.now() < trustWindowFrom) return;
            invoke('plugin:window|is_fullscreen', { label: 'main' }).then((value) => {
                if (typeof value === 'boolean') applyFullscreen(value);
            });
        }, delay));
    };

    document._tauriFullscreen = false;
    Element.prototype.requestFullscreen = function() { return setFullscreen(true); };
    document.exitFullscreen = function() { return setFullscreen(false); };
    window.addEventListener('resize', syncFromWindow);

    Object.defineProperty(document, 'fullscreenElement', {
        get() { return document._tauriFullscreen ? document.documentElement : null; },
        configurable: true,
    });

    Object.defineProperty(document, 'fullscreenEnabled', {
        get() { return true; },
        configurable: true,
    });
})();
"#;

const FETCH_INTERCEPTOR: &str = r#"
(function() {
    const originalFetch = window.fetch;

    window.fetch = function(input, init) {
        try {
            const url = new URL(input instanceof Request ? input.url : input);
            const host = url.hostname;
            if (host !== 'localhost' && host !== '127.0.0.1' && !host.endsWith('.localhost')) {
                const rewritten = '/__ext__/' + url.href;
                if (input instanceof Request) {
                    input = new Request(rewritten, input);
                } else {
                    input = rewritten;
                }
            }
        } catch (_) {}
        return originalFetch.call(this, input, init);
    };
})();
"#;

const EXT_PREFIX: &str = "/__ext__/";

fn start_local_server(app: &mut tauri::App) {
    let app_handle = app.handle().clone();
    let resolver = app.asset_resolver();
    let fallback = resolver.get("/".to_string()).map(|a| a.bytes);
    let (tx, rx) = mpsc::channel();

    let ext_agent = ureq::AgentBuilder::new()
        .timeout(EXT_TIMEOUT)
        .timeout_connect(EXT_CONNECT_TIMEOUT)
        .try_proxy_from_env(false)
        .resolver(proxy_security::PublicResolver)
        .build();
    let svc_agent = ureq::AgentBuilder::new()
        .timeout(SVC_TIMEOUT)
        .timeout_connect(SVC_CONNECT_TIMEOUT)
        .try_proxy_from_env(false)
        .build();
    let ext_pool = ThreadPool::new(EXT_POOL_SIZE);
    let svc_pool = ThreadPool::new(SVC_POOL_SIZE);
    let download_pool = ThreadPool::new(DOWNLOAD_POOL_SIZE);

    thread::spawn(move || {
        let server = tiny_http::Server::http(format!("localhost:{PORT}"))
            .expect("unable to bind localhost server");
        let _ = tx.send(());

        for request in server.incoming_requests() {
            let raw_url = request.url().to_string();
            let path = raw_url.split('?').next().unwrap_or(&raw_url);

            if let Some(id) = path.strip_prefix(downloads::DOWNLOAD_PREFIX) {
                let app_handle = app_handle.clone();
                let id = id.to_owned();
                download_pool.execute(move || downloads::respond(&app_handle, request, &id));
                continue;
            }

            if raw_url.starts_with(EXT_PREFIX) {
                let target = match proxy_security::external_target(&raw_url, EXT_PREFIX) {
                    Ok(target) => target.to_string(),
                    Err(_) => {
                        respond_invalid_external_target(request);
                        continue;
                    }
                };
                let agent = ext_agent.clone();
                ext_pool
                    .execute(move || proxy_request(request, &target, &agent, ProxyScope::External));
                continue;
            }

            if let Some(resp) = resolve_asset(&resolver, path, fallback.as_deref()) {
                let _ = request.respond(resp);
                continue;
            }

            let service_url = format!("http://127.0.0.1:{SERVICE_PORT}{raw_url}");
            let agent = svc_agent.clone();
            svc_pool
                .execute(move || proxy_request(request, &service_url, &agent, ProxyScope::Service));
        }
    });

    let _ = rx.recv();
}

fn respond_invalid_external_target(request: tiny_http::Request) {
    let _ = request.respond(
        tiny_http::Response::from_string("Invalid or blocked external target")
            .with_status_code(400),
    );
}

fn resolve_asset(
    resolver: &tauri::AssetResolver<tauri::Wry>,
    path: &str,
    fallback: Option<&[u8]>,
) -> Option<tiny_http::Response<std::io::Cursor<Vec<u8>>>> {
    let asset = resolver.get(path.to_string())?;

    let is_root = path == "/" || path == "/index.html";
    if !is_root && fallback == Some(asset.bytes.as_slice()) {
        return None;
    }

    let mut resp = tiny_http::Response::from_data(asset.bytes);
    if let Ok(h) = header("Content-Type", &asset.mime_type) {
        resp = resp.with_header(h);
    }
    if let Some(ref csp) = asset.csp_header {
        if let Ok(h) = header("Content-Security-Policy", csp) {
            resp = resp.with_header(h);
        }
    }
    // Frontend assets are versioned by commit hash in their path, so they're safe to
    // cache forever. The root entry point must always be revalidated so a new build
    // is picked up immediately.
    let cache_control = if is_root {
        "no-cache"
    } else {
        "public, max-age=31536000, immutable"
    };
    if let Ok(h) = header("Cache-Control", cache_control) {
        resp = resp.with_header(h);
    }
    Some(resp)
}

fn header(name: &str, value: &str) -> Result<tiny_http::Header, ()> {
    tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes())
}

const SKIP_HEADERS: &[&str] = &["host", "connection", "origin", "referer"];

#[derive(Clone, Copy)]
enum ProxyScope {
    External,
    Service,
}

fn proxy_request(
    mut request: tiny_http::Request,
    url: &str,
    agent: &ureq::Agent,
    scope: ProxyScope,
) {
    let method = request.method().to_string();

    let mut proxy = agent.request(&method, url);
    for h in request.headers() {
        let name = h.field.as_str().as_str();
        if SKIP_HEADERS.iter().any(|s| s.eq_ignore_ascii_case(name)) {
            continue;
        }
        proxy = proxy.set(name, h.value.as_str());
    }

    let result = if matches!(method.as_str(), "POST" | "PUT" | "PATCH") {
        let mut body = Vec::new();
        let _ = request.as_reader().read_to_end(&mut body);
        proxy.send_bytes(&body)
    } else {
        proxy.call()
    };

    match result {
        Ok(resp) | Err(ureq::Error::Status(_, resp)) => forward_response(request, resp),
        Err(ureq::Error::Transport(e))
            if matches!(scope, ProxyScope::External)
                && proxy_security::is_blocked_destination_error(&e) =>
        {
            respond_invalid_external_target(request);
        }
        Err(ureq::Error::Transport(_)) => {
            let _ = request.respond(
                tiny_http::Response::from_string("Service unavailable").with_status_code(502),
            );
        }
    }
}

const FRAMING_HEADERS: &[&str] = &["transfer-encoding", "content-length", "connection"];

fn forward_response(request: tiny_http::Request, resp: ureq::Response) {
    let status = resp.status();
    let content_len = resp.header("content-length").and_then(|v| v.parse().ok());

    let headers: Vec<_> = resp
        .headers_names()
        .iter()
        .filter(|n| !FRAMING_HEADERS.iter().any(|f| f.eq_ignore_ascii_case(n)))
        .filter_map(|n| resp.header(n).and_then(|v| header(n, v).ok()))
        .collect();

    let _ = request.respond(tiny_http::Response::new(
        tiny_http::StatusCode(status as u16),
        headers,
        resp.into_reader(),
        content_len,
        None,
    ));
}

fn spawn_streaming_service(app: &tauri::App) -> Option<ServiceProcess> {
    let Some(dir) = find_binaries_dir(app) else {
        eprintln!("stremio binaries directory not found");
        return None;
    };

    let mut cmd = Command::new(dir.join(bin_name("stremio-runtime")));
    cmd.arg(dir.join("server.js"))
        .env("NO_CORS", "1")
        .env("FFMPEG_BIN", dir.join(bin_name("ffmpeg")))
        .env("FFPROBE_BIN", dir.join(bin_name("ffprobe")))
        .current_dir(&dir);

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }

    match cmd.spawn() {
        Ok(child) => {
            println!("server.js started from {}", dir.display());

            #[cfg(target_os = "windows")]
            let _job = windows_job::assign_child_to_kill_on_close_job(&child);
            #[cfg(target_os = "windows")]
            if _job.is_none() {
                eprintln!("failed to assign service to job object");
            }

            Some(ServiceProcess {
                child,
                #[cfg(target_os = "windows")]
                _job,
            })
        }
        Err(e) => {
            eprintln!("server.js failed to start: {e}");
            None
        }
    }
}

fn warn_service_failed(app: &tauri::App) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.eval(SERVICE_WARNING_SCRIPT);
    }
}

const SERVICE_WARNING_SCRIPT: &str = r#"
(function() {
    function show() {
        var banner = document.createElement('div');
        banner.style.cssText = 'position:fixed;top:0;left:0;right:0;z-index:999999;background:#b91c1c;color:#fff;font:14px/1.4 -apple-system,sans-serif;padding:10px 40px 10px 16px;text-align:center;';
        banner.textContent = 'The streaming service failed to start. Streaming and some features may not work.';
        var btn = document.createElement('button');
        btn.textContent = '\u00D7';
        btn.style.cssText = 'position:absolute;top:50%;right:12px;transform:translateY(-50%);background:none;border:none;color:#fff;font-size:20px;cursor:pointer;padding:0 4px;';
        btn.onclick = function() { banner.remove(); };
        banner.appendChild(btn);
        document.body.prepend(banner);
    }
    if (document.body) { show(); }
    else { document.addEventListener('DOMContentLoaded', show); }
})();
"#;

fn find_binaries_dir(app: &tauri::App) -> Option<PathBuf> {
    [
        std::env::current_exe().ok().and_then(|p| p.parent().map(|p| p.to_path_buf())),
        app.path().resource_dir().ok().map(|d| d.join("binaries")),
    ]
    .into_iter()
    .flatten()
    .find(|d| d.join("server.js").exists())
}

fn bin_name(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.into() }
}
