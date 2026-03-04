use serde::{Deserialize, Serialize};
use std::fs;
use tauri::{AppHandle, Manager};

const CONFIG_FILE: &str = "config.json";

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_true")]
    pub auto_update: bool,
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self { auto_update: true }
    }
}

pub fn load(app: &AppHandle) -> Config {
    let Ok(dir) = app.path().app_config_dir() else {
        return Config::default();
    };
    let path = dir.join(CONFIG_FILE);
    fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(app: &AppHandle, config: &Config) {
    let Ok(dir) = app.path().app_config_dir() else {
        return;
    };
    let _ = fs::create_dir_all(&dir);
    let path = dir.join(CONFIG_FILE);
    if let Ok(json) = serde_json::to_string_pretty(config) {
        let _ = fs::write(path, json);
    }
}
