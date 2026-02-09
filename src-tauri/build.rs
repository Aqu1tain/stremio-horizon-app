use std::path::PathBuf;

const COMPANIONS: &[&str] = &[
    "stremio-service",
    "server.js",
    "stremio-runtime",
    "ffmpeg",
    "ffprobe",
    "package.json",
];

fn main() {
    copy_companions_to_target();
    tauri_build::build();
}

fn copy_companions_to_target() {
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".into());
    let target = PathBuf::from("target").join(profile);
    let binaries = PathBuf::from("binaries");

    let _ = std::fs::create_dir_all(&target);

    for name in COMPANIONS {
        let src = binaries.join(name);
        if !src.exists() {
            continue;
        }
        let _ = std::fs::copy(&src, target.join(name));
        println!("cargo:rerun-if-changed=binaries/{name}");
    }
}
