use std::io::Read;
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
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if TcpStream::connect(format!("127.0.0.1:{SERVICE_PORT}")).is_ok() {
            println!("streaming service ready on port {SERVICE_PORT}");
            return;
        }
        thread::sleep(Duration::from_millis(200));
    }
    eprintln!("streaming service did not start in time");
}

fn create_window(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("http://localhost:{PORT}");

    #[allow(unused_mut)]
    let mut builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url.parse()?))
        .title("Stremio Horizon")
        .inner_size(1280.0, 800.0)
        .min_inner_size(900.0, 600.0);

    #[cfg(target_os = "windows")]
    {
        builder = builder.additional_browser_args(
            "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,msWebView2EnableTrackingPrevention"
        );
    }

    builder.build()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Local HTTP server: serves frontend assets + proxies streaming API
// ---------------------------------------------------------------------------

fn start_local_server(app: &mut tauri::App) {
    let resolver = app.asset_resolver();
    let (tx, rx) = mpsc::channel();

    // Capture index.html bytes to detect SPA fallback
    let fallback = resolver.get("/".to_string()).map(|a| a.bytes);

    thread::spawn(move || {
        let server = tiny_http::Server::http(format!("localhost:{PORT}"))
            .expect("unable to bind localhost server");
        let _ = tx.send(()); // signal ready

        for request in server.incoming_requests() {
            let path = request
                .url()
                .split('?')
                .next()
                .unwrap_or(request.url())
                .to_string();

            // Try embedded frontend asset (skip SPA fallback for non-root paths)
            if let Some(asset) = resolver.get(path.clone()) {
                let is_root = path == "/" || path == "/index.html";
                let is_fallback =
                    !is_root && fallback.as_ref() == Some(&asset.bytes);

                if !is_fallback {
                    let mut resp = tiny_http::Response::from_data(asset.bytes);
                    if let Ok(h) = tiny_http::Header::from_bytes(
                        b"Content-Type" as &[u8],
                        asset.mime_type.as_bytes(),
                    ) {
                        resp = resp.with_header(h);
                    }
                    if let Some(csp) = asset.csp_header {
                        if let Ok(h) = tiny_http::Header::from_bytes(
                            b"Content-Security-Policy" as &[u8],
                            csp.as_bytes(),
                        ) {
                            resp = resp.with_header(h);
                        }
                    }
                    let _ = request.respond(resp);
                    continue;
                }
            }

            // Reverse-proxy to the streaming service (threaded for concurrency)
            thread::spawn(move || proxy_to_service(request));
        }
    });

    // Wait until the server is bound before creating the window
    let _ = rx.recv();
}

fn proxy_to_service(mut request: tiny_http::Request) {
    let url = format!("http://127.0.0.1:{SERVICE_PORT}{}", request.url());
    let method = request.method().to_string();

    let mut proxy = ureq::request(&method, &url);

    // Forward request headers (skip hop-by-hop / origin headers)
    for h in request.headers() {
        let name = h.field.as_str().as_str();
        match name.to_ascii_lowercase().as_str() {
            "host" | "connection" | "origin" | "referer" => continue,
            _ => proxy = proxy.set(name, h.value.as_str()),
        }
    }

    let result = if matches!(method.as_str(), "POST" | "PUT" | "PATCH") {
        let mut body = Vec::new();
        let _ = request.as_reader().read_to_end(&mut body);
        proxy.send_bytes(&body)
    } else {
        proxy.call()
    };

    match result {
        Ok(resp) => forward_response(request, resp),
        Err(ureq::Error::Status(_, resp)) => forward_response(request, resp),
        Err(ureq::Error::Transport(e)) => {
            let resp =
                tiny_http::Response::from_string(format!("Service unavailable: {e}"))
                    .with_status_code(502);
            let _ = request.respond(resp);
        }
    }
}

fn forward_response(request: tiny_http::Request, resp: ureq::Response) {
    let status = resp.status();
    let content_len = resp
        .header("content-length")
        .and_then(|v| v.parse::<usize>().ok());

    let mut headers = Vec::new();
    for name in resp.headers_names() {
        // Let tiny_http manage framing headers
        if name.eq_ignore_ascii_case("transfer-encoding")
            || name.eq_ignore_ascii_case("content-length")
            || name.eq_ignore_ascii_case("connection")
        {
            continue;
        }
        if let Some(value) = resp.header(&name) {
            if let Ok(h) = tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()) {
                headers.push(h);
            }
        }
    }

    let reader = resp.into_reader();
    let response = tiny_http::Response::new(
        tiny_http::StatusCode(status as u16),
        headers,
        reader,
        content_len,
        None,
    );
    let _ = request.respond(response);
}

// ---------------------------------------------------------------------------
// Streaming service (stremio-runtime + server.js)
// ---------------------------------------------------------------------------

fn spawn_streaming_service(app: &tauri::App) {
    let Some(dir) = find_binaries_dir(app) else {
        eprintln!("stremio binaries directory not found");
        return;
    };

    let runtime = dir.join(bin_name("stremio-runtime"));
    let server_js = dir.join("server.js");
    let ffmpeg = dir.join(bin_name("ffmpeg"));
    let ffprobe = dir.join(bin_name("ffprobe"));

    let mut cmd = Command::new(&runtime);
    cmd.arg(&server_js)
        .env("NO_CORS", "1")
        .env("FFMPEG_BIN", &ffmpeg)
        .env("FFPROBE_BIN", &ffprobe)
        .current_dir(&dir);

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }

    match cmd.spawn() {
        Ok(_) => println!("server.js started from {}", dir.display()),
        Err(e) => eprintln!("server.js failed to start: {e}"),
    }
}

fn find_binaries_dir(app: &tauri::App) -> Option<PathBuf> {
    // Dev mode: build.rs copies binaries next to the executable
    let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    if exe_dir.join("server.js").exists() {
        return Some(exe_dir);
    }

    // Production: Tauri bundles resources under a binaries/ subdirectory
    let resource_dir = app.path().resource_dir().ok()?;
    let binaries_dir = resource_dir.join("binaries");
    if binaries_dir.join("server.js").exists() {
        return Some(binaries_dir);
    }

    None
}

fn bin_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}
