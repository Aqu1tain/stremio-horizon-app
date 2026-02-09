use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_cors_fetch::init())
        .setup(|app| {
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
                    .env("NO_CORS", "1")
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
