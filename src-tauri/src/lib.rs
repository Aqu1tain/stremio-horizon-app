use tauri_plugin_shell::ShellExt;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .setup(|app| {
            if let Ok(sidecar) = app.shell().sidecar("stremio-service") {
                match sidecar.spawn() {
                    Ok((_rx, _child)) => println!("stremio-service started"),
                    Err(e) => eprintln!("stremio-service not bundled, assuming external: {e}"),
                }
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
