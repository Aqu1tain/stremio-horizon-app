use std::fs;
use std::env;
use std::path::PathBuf;

fn main() {
    let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let out_dir = PathBuf::from("target").join(&profile);

    let companions = ["stremio-service", "server.js", "stremio-runtime", "ffmpeg", "ffprobe", "package.json"];
    let binaries = PathBuf::from("binaries");

    for file in &companions {
        let src = binaries.join(file);
        if src.exists() {
            let dst = out_dir.join(file);
            let _ = fs::create_dir_all(&out_dir);
            let _ = fs::copy(&src, &dst);
            println!("cargo:rerun-if-changed=binaries/{file}");
        }
    }

    tauri_build::build()
}
