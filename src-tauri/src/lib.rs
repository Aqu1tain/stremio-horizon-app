use std::process::Command;
use tauri::path::BaseDirectory;
use tauri::webview::WebviewWindowBuilder;
use tauri::{Manager, WebviewUrl};

const PORT: u16 = 11480;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(target_os = "linux")]
    std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_localhost::Builder::new(PORT).build())
        .setup(|app| {
            create_window(app)?;
            spawn_service(app);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn create_window(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("http://localhost:{PORT}");
    WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url.parse()?))
        .title("Stremio Horizon")
        .inner_size(1280.0, 800.0)
        .min_inner_size(900.0, 600.0)
        .build()?;
    Ok(())
}

fn spawn_service(app: &tauri::App) {
    let name = if cfg!(windows) { "stremio-service.exe" } else { "stremio-service" };

    let path = match app.path().resolve(format!("binaries/{name}"), BaseDirectory::Resource) {
        Ok(p) if p.exists() => p,
        _ => {
            eprintln!("stremio-service binary not found in resources");
            return;
        }
    };

    let dir = path.parent().unwrap();

    match Command::new(&path).current_dir(dir).spawn() {
        Ok(_) => println!("stremio-service started from {}", dir.display()),
        Err(e) => eprintln!("stremio-service failed: {e}"),
    }
}
