use tauri::Manager;
use tauri::WebviewUrl;
use tauri::webview::WebviewWindowBuilder;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let port = portpicker::pick_unused_port().expect("failed to find open port");

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_localhost::Builder::new(port).build())
        .setup(move |app| {
            let url = format!("http://localhost:{port}");
            WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url.parse().unwrap()))
                .title("Stremio Horizon")
                .inner_size(1280.0, 800.0)
                .min_inner_size(900.0, 600.0)
                .build()?;

            let exe_dir = std::env::current_exe()?
                .parent()
                .unwrap()
                .to_path_buf();

            let service_name = if cfg!(windows) {
                "stremio-service.exe"
            } else {
                "stremio-service"
            };

            let service_dir = if exe_dir.join(service_name).exists() {
                Some(exe_dir)
            } else {
                app.path()
                    .resource_dir()
                    .ok()
                    .filter(|d| d.join(service_name).exists())
            };

            if let Some(dir) = service_dir {
                match std::process::Command::new(dir.join(service_name))
                    .current_dir(&dir)
                    .spawn()
                {
                    Ok(_) => println!("stremio-service started"),
                    Err(e) => eprintln!("stremio-service not bundled or already running: {e}"),
                }
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
