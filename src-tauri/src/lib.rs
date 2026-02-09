use std::path::PathBuf;
use std::process::Command;
use tauri::webview::WebviewWindowBuilder;
use tauri::{Manager, WebviewUrl};

const PORT: u16 = 11480;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
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
    let name = service_name();

    let Some(dir) = find_service_dir(app, &name) else {
        return;
    };

    match Command::new(dir.join(&name)).current_dir(&dir).spawn() {
        Ok(_) => println!("stremio-service started"),
        Err(e) => eprintln!("stremio-service failed: {e}"),
    }
}

fn find_service_dir(app: &tauri::App, name: &str) -> Option<PathBuf> {
    let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();

    if exe_dir.join(name).exists() {
        return Some(exe_dir);
    }

    app.path()
        .resource_dir()
        .ok()
        .filter(|d| d.join(name).exists())
}

fn service_name() -> &'static str {
    if cfg!(windows) {
        "stremio-service.exe"
    } else {
        "stremio-service"
    }
}
