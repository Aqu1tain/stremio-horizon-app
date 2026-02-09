use std::net::TcpStream;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tauri::webview::WebviewWindowBuilder;
use tauri::{Manager, WebviewUrl};

const PORT: u16 = 11480;
const SERVICE_PORT: u16 = 11470;
const SERVICE_TIMEOUT: Duration = Duration::from_secs(15);
const POLL_INTERVAL: Duration = Duration::from_millis(200);

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(target_os = "linux")]
    std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            start_local_server(app);
            spawn_streaming_service(app);
            wait_for_service();
            create_window(app)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn wait_for_service() {
    let deadline = Instant::now() + SERVICE_TIMEOUT;
    while Instant::now() < deadline {
        if TcpStream::connect(format!("127.0.0.1:{SERVICE_PORT}")).is_ok() {
            return;
        }
        thread::sleep(POLL_INTERVAL);
    }
    eprintln!("streaming service did not start in time");
}

fn create_window(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("http://localhost:{PORT}");

    #[allow(unused_mut)]
    let mut builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url.parse()?))
        .title("Stremio Horizon")
        .inner_size(1280.0, 800.0)
        .min_inner_size(900.0, 600.0)
        .initialization_script(FULLSCREEN_BRIDGE)
        .initialization_script(FETCH_INTERCEPTOR);

    #[cfg(target_os = "windows")]
    {
        builder = builder.additional_browser_args(
            "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,msWebView2EnableTrackingPrevention"
        );
    }

    builder.build()?;
    Ok(())
}

const FULLSCREEN_BRIDGE: &str = r#"
(function() {
    const getInvoke = () => window.__TAURI_INTERNALS__?.invoke;

    const setFullscreen = (value) => {
        const invoke = getInvoke();
        if (!invoke) return Promise.resolve();
        return invoke('plugin:window|set_fullscreen', { label: 'main', value }).then(() => {
            document._tauriFullscreen = value;
            document.dispatchEvent(new Event('fullscreenchange'));
        });
    };

    document._tauriFullscreen = false;
    Element.prototype.requestFullscreen = function() { return setFullscreen(true); };
    document.exitFullscreen = function() { return setFullscreen(false); };

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
            if (host !== 'localhost' && host !== '127.0.0.1') {
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
    let resolver = app.asset_resolver();
    let fallback = resolver.get("/".to_string()).map(|a| a.bytes);
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let server = tiny_http::Server::http(format!("localhost:{PORT}"))
            .expect("unable to bind localhost server");
        let _ = tx.send(());

        for request in server.incoming_requests() {
            let raw_url = request.url().to_string();
            let path = raw_url.split('?').next().unwrap_or(&raw_url);

            if let Some(target) = path.strip_prefix(EXT_PREFIX) {
                let target = target.to_string();
                thread::spawn(move || proxy_request(request, &target));
                continue;
            }

            if let Some(resp) = resolve_asset(&resolver, path, fallback.as_deref()) {
                let _ = request.respond(resp);
                continue;
            }

            let service_url = format!("http://127.0.0.1:{SERVICE_PORT}{raw_url}");
            thread::spawn(move || proxy_request(request, &service_url));
        }
    });

    let _ = rx.recv();
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
    Some(resp)
}

fn header(name: &str, value: &str) -> Result<tiny_http::Header, ()> {
    tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes())
}

const SKIP_HEADERS: &[&str] = &["host", "connection", "origin", "referer"];

fn proxy_request(mut request: tiny_http::Request, url: &str) {
    let method = request.method().to_string();

    let mut proxy = ureq::request(&method, url);
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
        Err(ureq::Error::Transport(e)) => {
            let _ = request.respond(
                tiny_http::Response::from_string(format!("Service unavailable: {e}"))
                    .with_status_code(502),
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

fn spawn_streaming_service(app: &tauri::App) {
    let Some(dir) = find_binaries_dir(app) else {
        eprintln!("stremio binaries directory not found");
        return;
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
        Ok(_) => println!("server.js started from {}", dir.display()),
        Err(e) => eprintln!("server.js failed to start: {e}"),
    }
}

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
