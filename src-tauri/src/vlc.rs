use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use tauri::AppHandle;
use url::Url;

use crate::updater::emit_event;

const STATUS_POLL_INTERVAL: Duration = Duration::from_secs(1);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const HTTP_TIMEOUT: Duration = Duration::from_secs(2);
const WATCHED_THRESHOLD: f64 = 0.9;

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VlcPlaybackRequest {
    session_id: String,
    url: String,
    start_time_ms: u64,
    #[serde(default)]
    audio_language: Option<String>,
    #[serde(default)]
    subtitle_language: Option<String>,
    #[serde(default)]
    subtitle_url: Option<String>,
    #[serde(default = "subtitles_enabled_by_default")]
    subtitles_enabled: bool,
}

fn subtitles_enabled_by_default() -> bool {
    true
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VlcPlaybackProgress {
    session_id: String,
    position_ms: u64,
    duration_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VlcPlaybackResult {
    session_id: String,
    position_ms: u64,
    duration_ms: u64,
    completed: bool,
}

#[derive(Debug, Default, Deserialize)]
struct VlcStatus {
    #[serde(default)]
    time: f64,
    #[serde(default)]
    length: f64,
    #[serde(default)]
    state: String,
}

#[tauri::command]
pub fn vlc_available() -> bool {
    find_vlc_binary().is_some()
}

#[tauri::command]
pub async fn vlc_play(
    app: AppHandle,
    request: VlcPlaybackRequest,
) -> Result<VlcPlaybackResult, String> {
    validate_stream_url(&request.url)?;
    tauri::async_runtime::spawn_blocking(move || play_blocking(app, request))
        .await
        .map_err(|error| format!("VLC playback task failed: {error}"))?
}

fn play_blocking(app: AppHandle, request: VlcPlaybackRequest) -> Result<VlcPlaybackResult, String> {
    let binary = find_vlc_binary().ok_or_else(|| {
        "VLC was not found. Install VLC in its default location, then try again.".to_owned()
    })?;
    let preference_args = playback_preference_args(&request)?;
    let port = available_port()?;
    let password = format!(
        "horizon-{}-{}",
        std::process::id(),
        NEXT_SESSION.fetch_add(1, Ordering::Relaxed)
    );
    let status_url = format!("http://127.0.0.1:{port}/requests/status.json");
    let authorization = format!("Basic {}", BASE64_STANDARD.encode(format!(":{password}")));

    let mut command = Command::new(&binary);
    command
        .arg("--extraintf")
        .arg("http")
        .arg("--http-host")
        .arg("127.0.0.1")
        .arg("--http-port")
        .arg(port.to_string())
        .arg("--http-password")
        .arg(&password)
        .arg("--no-video-title-show")
        .arg("--no-media-library")
        .arg("--play-and-exit")
        .arg(format!("--start-time={}", request.start_time_ms / 1000));

    for argument in preference_args {
        command.arg(argument);
    }

    command
        .arg(&request.url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to launch VLC at {}: {error}", binary.display()))?;

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(HTTP_TIMEOUT)
        .timeout_read(HTTP_TIMEOUT)
        .timeout_write(HTTP_TIMEOUT)
        .build();
    let startup_deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut api_ready = false;
    let mut playback_started = false;
    let mut last_position_ms = request.start_time_ms;
    let mut last_duration_ms = 0;
    let mut ended = false;

    loop {
        if let Some(status) = fetch_status(&agent, &status_url, &authorization) {
            api_ready = true;
            let position_ms = seconds_to_milliseconds(status.time);
            let duration_ms = seconds_to_milliseconds(status.length);
            if duration_ms > 0 {
                last_duration_ms = duration_ms;
            }
            if position_ms > 0 || last_position_ms == 0 {
                last_position_ms = position_ms;
            }
            ended |= status.state == "ended";
            playback_started |= matches!(
                status.state.as_str(),
                "opening" | "buffering" | "playing" | "paused"
            ) || duration_ms > 0;

            emit_event(
                &app,
                "vlc-playback-progress",
                &VlcPlaybackProgress {
                    session_id: request.session_id.clone(),
                    position_ms: last_position_ms,
                    duration_ms: last_duration_ms,
                },
            );

            if playback_started && status.state == "stopped" {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }

        if child
            .try_wait()
            .map_err(|error| format!("failed to read VLC process state: {error}"))?
            .is_some()
        {
            break;
        }

        if !playback_started && Instant::now() >= startup_deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(if api_ready {
                "VLC opened, but the stream did not start playing.".to_owned()
            } else {
                "VLC started, but Horizon could not connect to its local playback status API."
                    .to_owned()
            });
        }

        thread::sleep(STATUS_POLL_INTERVAL);
    }

    if !api_ready {
        return Err("VLC closed before playback could start.".to_owned());
    }

    Ok(VlcPlaybackResult {
        session_id: request.session_id,
        position_ms: last_position_ms,
        duration_ms: last_duration_ms,
        completed: ended || playback_is_complete(last_position_ms, last_duration_ms),
    })
}

fn playback_preference_args(request: &VlcPlaybackRequest) -> Result<Vec<String>, String> {
    let mut args = Vec::new();

    if let Some(audio_language) = sanitized_language_list(request.audio_language.as_deref()) {
        args.push(format!("--audio-language={audio_language}"));
    }

    if request.subtitles_enabled {
        if let Some(subtitle_language) =
            sanitized_language_list(request.subtitle_language.as_deref())
        {
            args.push(format!("--sub-language={subtitle_language}"));
        }

        if let Some(subtitle_url) = request.subtitle_url.as_deref() {
            validate_http_url(subtitle_url, "VLC subtitle URL")?;
            args.push(format!("--sub-file={subtitle_url}"));
        }
    } else {
        args.push("--sub-track=-1".to_owned());
        args.push("--no-sub-autodetect-file".to_owned());
    }

    Ok(args)
}

fn sanitized_language_list(raw: Option<&str>) -> Option<String> {
    let languages = raw?
        .split(',')
        .map(str::trim)
        .filter(|language| {
            !language.is_empty()
                && language.len() <= 16
                && language
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-')
        })
        .take(8)
        .fold(Vec::<&str>::new(), |mut result, language| {
            if !result.contains(&language) {
                result.push(language);
            }
            result
        });

    (!languages.is_empty()).then(|| languages.join(","))
}

fn fetch_status(agent: &ureq::Agent, url: &str, authorization: &str) -> Option<VlcStatus> {
    let response = agent
        .get(url)
        .set("Authorization", authorization)
        .call()
        .ok()?;
    serde_json::from_reader(response.into_reader()).ok()
}

fn seconds_to_milliseconds(seconds: f64) -> u64 {
    if !seconds.is_finite() || seconds <= 0.0 {
        0
    } else {
        (seconds * 1000.0).round() as u64
    }
}

fn playback_is_complete(position_ms: u64, duration_ms: u64) -> bool {
    duration_ms > 0 && position_ms as f64 / duration_ms as f64 >= WATCHED_THRESHOLD
}

fn validate_stream_url(raw_url: &str) -> Result<(), String> {
    validate_http_url(raw_url, "VLC stream URL")
}

fn validate_http_url(raw_url: &str, label: &str) -> Result<(), String> {
    let url = Url::parse(raw_url).map_err(|_| format!("invalid {label}"))?;
    if matches!(url.scheme(), "http" | "https") {
        Ok(())
    } else {
        Err(format!("{label} only accepts HTTP or HTTPS URLs"))
    }
}

fn available_port() -> Result<u16, String> {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .map_err(|error| format!("failed to reserve a local VLC control port: {error}"))
}

fn find_vlc_binary() -> Option<PathBuf> {
    if let Some(custom_path) = std::env::var_os("VLC_PATH").map(PathBuf::from) {
        if custom_path.is_file() {
            return Some(custom_path);
        }
    }

    for candidate in platform_candidates() {
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    find_in_path(if cfg!(target_os = "windows") {
        "vlc.exe"
    } else {
        "vlc"
    })
}

fn find_in_path(binary_name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|path| path.join(binary_name))
        .find(|candidate| candidate.is_file())
}

fn platform_candidates() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        return vec![PathBuf::from("/Applications/VLC.app/Contents/MacOS/VLC")];
    }

    #[cfg(target_os = "windows")]
    {
        return ["ProgramFiles", "ProgramFiles(x86)"]
            .into_iter()
            .filter_map(std::env::var_os)
            .map(PathBuf::from)
            .map(|path| path.join("VideoLAN").join("VLC").join("vlc.exe"))
            .collect();
    }

    #[cfg(target_os = "linux")]
    {
        vec![
            PathBuf::from("/usr/bin/vlc"),
            PathBuf::from("/usr/local/bin/vlc"),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playback_request() -> VlcPlaybackRequest {
        VlcPlaybackRequest {
            session_id: "test".to_owned(),
            url: "https://example.com/video.mp4".to_owned(),
            start_time_ms: 0,
            audio_language: None,
            subtitle_language: None,
            subtitle_url: None,
            subtitles_enabled: true,
        }
    }

    #[test]
    fn considers_ninety_percent_complete() {
        assert!(playback_is_complete(90_000, 100_000));
        assert!(!playback_is_complete(89_999, 100_000));
    }

    #[test]
    fn rejects_unknown_or_local_file_schemes() {
        assert!(validate_stream_url("https://example.com/video.mp4").is_ok());
        assert!(validate_stream_url("http://127.0.0.1:11470/video.mp4").is_ok());
        assert!(validate_stream_url("file:///tmp/video.mp4").is_err());
        assert!(validate_stream_url("javascript:alert(1)").is_err());
    }

    #[test]
    fn converts_status_seconds_to_core_milliseconds() {
        assert_eq!(seconds_to_milliseconds(12.345), 12_345);
        assert_eq!(seconds_to_milliseconds(f64::NAN), 0);
        assert_eq!(seconds_to_milliseconds(-1.0), 0);
    }

    #[test]
    fn passes_language_preferences_and_remote_subtitles_to_vlc() {
        let request = VlcPlaybackRequest {
            audio_language: Some("fre,eng".to_owned()),
            subtitle_language: Some("fre,eng".to_owned()),
            subtitle_url: Some("https://example.com/subtitle.srt".to_owned()),
            ..playback_request()
        };

        assert_eq!(
            playback_preference_args(&request).unwrap(),
            vec![
                "--audio-language=fre,eng",
                "--sub-language=fre,eng",
                "--sub-file=https://example.com/subtitle.srt",
            ]
        );
    }

    #[test]
    fn disables_subtitles_when_the_app_preference_is_off() {
        let request = VlcPlaybackRequest {
            subtitles_enabled: false,
            subtitle_language: Some("fre".to_owned()),
            subtitle_url: Some("https://example.com/subtitle.srt".to_owned()),
            ..playback_request()
        };

        assert_eq!(
            playback_preference_args(&request).unwrap(),
            vec!["--sub-track=-1", "--no-sub-autodetect-file"]
        );
    }

    #[test]
    fn rejects_non_http_subtitle_urls() {
        let request = VlcPlaybackRequest {
            subtitle_url: Some("file:///tmp/subtitle.srt".to_owned()),
            ..playback_request()
        };

        assert!(playback_preference_args(&request).is_err());
    }

    #[test]
    fn ignores_invalid_language_preferences() {
        assert_eq!(
            sanitized_language_list(Some("fre, --sub-file=/tmp/bad,eng,fre")),
            Some("fre,eng".to_owned())
        );
    }

    #[test]
    fn locates_the_default_macos_vlc_installation_when_present() {
        if cfg!(target_os = "macos")
            && std::path::Path::new("/Applications/VLC.app/Contents/MacOS/VLC").is_file()
        {
            assert_eq!(
                find_vlc_binary(),
                Some(PathBuf::from("/Applications/VLC.app/Contents/MacOS/VLC"))
            );
        }
    }
}
