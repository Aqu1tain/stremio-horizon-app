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
        // On Windows, binaries have .exe extension
        let src = if cfg!(windows) && !name.contains('.') {
            binaries.join(format!("{name}.exe"))
        } else {
            binaries.join(name)
        };

        if !src.exists() {
            continue;
        }
        let dst = target.join(src.file_name().unwrap());
        let _ = std::fs::copy(&src, dst);
        println!("cargo:rerun-if-changed={}", src.display());
    }
}
