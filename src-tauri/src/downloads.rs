use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};
use tiny_http::{Header, Method, Request, Response, StatusCode};
use url::Url;

use crate::proxy_security;
use crate::updater;
use crate::SERVICE_PORT;

pub const DOWNLOAD_PREFIX: &str = "/__downloads__/";
const INDEX_FILE: &str = "index.json";
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
const MAX_REDIRECTS: usize = 5;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_THUMBNAIL_BYTES: u64 = 10 * 1024 * 1024;
const MAX_HLS_PLAYLIST_BYTES: u64 = 5 * 1024 * 1024;
const MAX_HLS_RESOURCES: usize = 100_000;
const MAX_HLS_DEPTH: usize = 5;
const MAX_MP4_PROBE_BYTES: u64 = 2 * 1024 * 1024;
const MIN_OFFLINE_VIDEO_DURATION_SECONDS: f64 = 30.0;
static DOWNLOAD_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DownloadStatus {
    Queued,
    Downloading,
    Paused,
    Completed,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadItem {
    pub id: String,
    pub title: String,
    pub subtitle: Option<String>,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub content_id: Option<String>,
    #[serde(default)]
    pub video_id: Option<String>,
    #[serde(default)]
    pub season: Option<u32>,
    #[serde(default)]
    pub episode: Option<u32>,
    #[serde(default)]
    pub description: Option<String>,
    pub source_name: Option<String>,
    #[serde(default)]
    pub source_thumbnail_url: Option<String>,
    #[serde(default)]
    pub thumbnail_storage_name: Option<String>,
    pub source_url: String,
    pub file_name: String,
    pub storage_name: String,
    #[serde(default)]
    pub hls_directory: Option<String>,
    pub status: DownloadStatus,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub created_at: u64,
    pub updated_at: u64,
    pub error: Option<String>,
    pub playback_url: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadView {
    pub id: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub content_type: Option<String>,
    pub content_id: Option<String>,
    pub video_id: Option<String>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub description: Option<String>,
    pub source_name: Option<String>,
    pub thumbnail_url: Option<String>,
    pub file_name: String,
    pub status: DownloadStatus,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub created_at: u64,
    pub updated_at: u64,
    pub error: Option<String>,
    pub playback_url: String,
}

impl From<&DownloadItem> for DownloadView {
    fn from(item: &DownloadItem) -> Self {
        Self {
            id: item.id.clone(),
            title: item.title.clone(),
            subtitle: item.subtitle.clone(),
            content_type: item.content_type.clone(),
            content_id: item.content_id.clone(),
            video_id: item.video_id.clone(),
            season: item.season,
            episode: item.episode,
            description: item.description.clone(),
            source_name: item.source_name.clone(),
            thumbnail_url: item
                .thumbnail_storage_name
                .as_ref()
                .map(|_| thumbnail_url(&item.id))
                .or_else(|| item.source_thumbnail_url.clone()),
            file_name: item.file_name.clone(),
            status: item.status,
            downloaded_bytes: item.downloaded_bytes,
            total_bytes: item.total_bytes,
            created_at: item.created_at,
            updated_at: item.updated_at,
            error: item.error.clone(),
            playback_url: item.playback_url.clone(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadRequest {
    pub url: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub content_type: Option<String>,
    pub content_id: Option<String>,
    pub video_id: Option<String>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub description: Option<String>,
    pub source_name: Option<String>,
    pub thumbnail_url: Option<String>,
    pub file_name: Option<String>,
}

#[derive(Default)]
pub struct DownloadManager {
    items: Mutex<HashMap<String, DownloadItem>>,
    active: Mutex<HashMap<String, Arc<AtomicBool>>>,
    persistence: Mutex<()>,
}

pub fn initialize(app: &mut tauri::App) -> Result<(), String> {
    let items = load_items(app.handle())?;
    let manager = app.state::<DownloadManager>();
    let mut changed = false;
    {
        let mut state = manager
            .items
            .lock()
            .map_err(|error| format!("failed to initialize downloads: {error}"))?;

        for mut item in items {
            if matches!(
                item.status,
                DownloadStatus::Downloading | DownloadStatus::Queued
            ) {
                item.status = DownloadStatus::Queued;
            }
            if item.status == DownloadStatus::Completed {
                if let Some(error) = completed_video_problem(app.handle(), &item)? {
                    item.status = DownloadStatus::Failed;
                    item.error = Some(error);
                    changed = true;
                }
            }
            state.insert(item.id.clone(), item);
        }
    }
    if changed {
        persist(app.handle())?;
    }
    Ok(())
}

pub fn resume_pending(app: &AppHandle) {
    let _ = start_next_worker(app.clone());
}

#[tauri::command]
pub fn download_list(state: State<'_, DownloadManager>) -> Result<Vec<DownloadView>, String> {
    let mut items = state
        .items
        .lock()
        .map_err(|error| format!("failed to read downloads: {error}"))?
        .values()
        .cloned()
        .collect::<Vec<_>>();
    items.sort_by(|left, right| right.created_at.cmp(&left.created_at));
    Ok(items.iter().map(DownloadView::from).collect())
}

#[tauri::command]
pub fn download_playback_url(
    app: AppHandle,
    state: State<'_, DownloadManager>,
    id: String,
) -> Result<String, String> {
    let item = state
        .items
        .lock()
        .map_err(|error| format!("failed to read download: {error}"))?
        .get(&id)
        .filter(|item| item.status == DownloadStatus::Completed)
        .cloned()
        .ok_or_else(|| "completed download not found".to_owned())?;
    let path = download_directory(&app)?.join(&item.storage_name);
    if !path.is_file() {
        return Err("download file not found".to_owned());
    }
    service_playback_url(&id, &path)
}

#[tauri::command]
pub fn download_start(
    app: AppHandle,
    state: State<'_, DownloadManager>,
    request: DownloadRequest,
) -> Result<DownloadView, String> {
    validate_target(&Url::parse(&request.url).map_err(|_| "invalid download URL")?)?;
    if let Some(thumbnail_url) = request.thumbnail_url.as_deref() {
        validate_target(&Url::parse(thumbnail_url).map_err(|_| "invalid thumbnail URL")?)?;
    }

    let existing = {
        let items = state
            .items
            .lock()
            .map_err(|error| format!("failed to inspect downloads: {error}"))?;
        items
            .values()
            .find(|item| item.source_url == request.url)
            .cloned()
    };
    if let Some(existing) = existing {
        if matches!(
            existing.status,
            DownloadStatus::Failed | DownloadStatus::Paused
        ) {
            return download_resume(app, state, existing.id.clone());
        }
        return Ok(DownloadView::from(&existing));
    }

    let id = next_id();
    let now = now_millis();
    let requested_file_name = request
        .file_name
        .filter(|name| !name.trim().is_empty())
        .or_else(|| file_name_from_url(&request.url))
        .unwrap_or_else(|| "video".to_owned());
    let file_name = sanitize_file_name(&requested_file_name);
    let storage_name = storage_name(&id, &file_name);
    let playback_url = playback_url(&id, &storage_name);
    let item = DownloadItem {
        id: id.clone(),
        title: non_empty(request.title, "Downloaded video"),
        subtitle: clean_optional(request.subtitle),
        content_type: clean_optional(request.content_type),
        content_id: clean_optional(request.content_id),
        video_id: clean_optional(request.video_id),
        season: request.season,
        episode: request.episode,
        description: clean_optional(request.description),
        source_name: clean_optional(request.source_name),
        source_thumbnail_url: clean_optional(request.thumbnail_url),
        thumbnail_storage_name: None,
        source_url: request.url,
        file_name,
        storage_name,
        hls_directory: None,
        status: DownloadStatus::Queued,
        downloaded_bytes: 0,
        total_bytes: None,
        created_at: now,
        updated_at: now,
        error: None,
        playback_url,
    };

    state
        .items
        .lock()
        .map_err(|error| format!("failed to add download: {error}"))?
        .insert(id.clone(), item.clone());
    persist(&app)?;
    emit_changed(&app, &item);
    start_next_worker(app)?;
    Ok(DownloadView::from(&item))
}

#[tauri::command]
pub fn download_pause(
    app: AppHandle,
    state: State<'_, DownloadManager>,
    id: String,
) -> Result<DownloadView, String> {
    if let Some(cancel) = state
        .active
        .lock()
        .map_err(|error| format!("failed to pause download: {error}"))?
        .get(&id)
    {
        cancel.store(true, Ordering::Relaxed);
    }

    update_item(&app, &id, |item| {
        if item.status != DownloadStatus::Completed {
            item.status = DownloadStatus::Paused;
            item.error = None;
        }
    })
    .map(|item| DownloadView::from(&item))
}

#[tauri::command]
pub fn download_resume(
    app: AppHandle,
    state: State<'_, DownloadManager>,
    id: String,
) -> Result<DownloadView, String> {
    let item = {
        let mut items = state
            .items
            .lock()
            .map_err(|error| format!("failed to resume download: {error}"))?;
        let item = items.get_mut(&id).ok_or("download not found")?;
        if item.status == DownloadStatus::Completed {
            return Ok(DownloadView::from(&*item));
        }
        item.status = DownloadStatus::Queued;
        item.error = None;
        item.updated_at = now_millis();
        item.clone()
    };
    persist(&app)?;
    emit_changed(&app, &item);
    start_next_worker(app)?;
    Ok(DownloadView::from(&item))
}

#[tauri::command]
pub fn download_delete(
    app: AppHandle,
    state: State<'_, DownloadManager>,
    id: String,
) -> Result<(), String> {
    if state
        .active
        .lock()
        .map_err(|error| format!("failed to delete download: {error}"))?
        .contains_key(&id)
    {
        return Err("pause the download before deleting it".to_owned());
    }

    let item = state
        .items
        .lock()
        .map_err(|error| format!("failed to delete download: {error}"))?
        .remove(&id)
        .ok_or("download not found")?;
    let directory = download_directory(&app)?;
    remove_if_exists(&directory.join(&item.storage_name))?;
    if let Some(thumbnail_storage_name) = item.thumbnail_storage_name {
        remove_if_exists(&directory.join(thumbnail_storage_name))?;
    }
    remove_if_exists(&directory.join(format!("{}.part", item.id)))?;
    remove_dir_if_exists(&directory.join(format!("{}.hls", item.id)))?;
    remove_dir_if_exists(&directory.join(format!("{}.hls.part", item.id)))?;
    persist(&app)?;
    updater::emit_event(
        &app,
        "stremio-download-removed",
        &serde_json::json!({ "id": id }),
    );
    Ok(())
}

fn next_queued_id(items: &HashMap<String, DownloadItem>) -> Option<String> {
    items
        .values()
        .filter(|item| item.status == DownloadStatus::Queued)
        .min_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.cmp(&right.id))
        })
        .map(|item| item.id.clone())
}

fn start_next_worker(app: AppHandle) -> Result<(), String> {
    let (id, cancel) = {
        let state = app.state::<DownloadManager>();
        let mut active = state
            .active
            .lock()
            .map_err(|error| format!("failed to schedule download: {error}"))?;
        if !active.is_empty() {
            return Ok(());
        }
        let id = {
            let items = state
                .items
                .lock()
                .map_err(|error| format!("failed to inspect download queue: {error}"))?;
            next_queued_id(&items)
        };
        let Some(id) = id else {
            return Ok(());
        };
        let cancel = Arc::new(AtomicBool::new(false));
        active.insert(id.clone(), cancel.clone());
        (id, cancel)
    };

    if let Err(error) = update_item(&app, &id, |item| {
        item.status = DownloadStatus::Downloading;
        item.error = None;
    }) {
        if let Ok(mut active) = app.state::<DownloadManager>().active.lock() {
            active.remove(&id);
        }
        return Err(error);
    }

    thread::spawn(move || {
        let result = run_download(&app, &id, &cancel);
        if let Err(error) = result {
            let _ = update_item(&app, &id, |item| {
                if item.status != DownloadStatus::Paused {
                    item.status = DownloadStatus::Failed;
                    item.error = Some(error.clone());
                }
            });
        }
        if let Ok(mut active) = app.state::<DownloadManager>().active.lock() {
            active.remove(&id);
        }
        let _ = start_next_worker(app);
    });
    Ok(())
}

fn run_download(app: &AppHandle, id: &str, cancel: &AtomicBool) -> Result<(), String> {
    let item = get_item(app, id)?;
    let directory = download_directory(app)?;
    fs::create_dir_all(&directory)
        .map_err(|error| format!("failed to create download directory: {error}"))?;
    if !cancel.load(Ordering::Relaxed) {
        let _ = cache_thumbnail(app, id);
    }
    let partial_path = directory.join(format!("{id}.part"));
    let existing = partial_path
        .metadata()
        .map(|metadata| metadata.len())
        .unwrap_or(0);

    let mut response = fetch(&item.source_url, existing)?;
    if response.status() == 416 && existing > 0 {
        remove_if_exists(&partial_path)?;
        response = fetch(&item.source_url, 0)?;
    }
    if !matches!(response.status(), 200 | 206) {
        return Err(format!(
            "download server returned HTTP {}",
            response.status()
        ));
    }

    let content_type = response
        .header("content-type")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if content_type.contains("text/html") {
        return Err("this source does not expose a downloadable video file".to_owned());
    }

    let resolved_file_name = resolved_file_name(&item, &response, &content_type);
    if is_hls_content_type(&content_type) {
        remove_if_exists(&partial_path)?;
        return run_hls_download(app, id, cancel, response, &resolved_file_name);
    }
    let resolved_source_url =
        Url::parse(response.get_url()).map_err(|_| "invalid resolved download URL".to_owned())?;
    remove_dir_if_exists(&directory.join(format!("{id}.hls.part")))?;

    let resumed = response.status() == 206 && existing > 0;
    let start = if resumed { existing } else { 0 };
    let response_length = response
        .header("content-length")
        .and_then(|value| value.parse::<u64>().ok());
    let total = content_range_total(response.header("content-range"))
        .or_else(|| response_length.map(|length| start.saturating_add(length)));
    let resolved_storage_name = storage_name(id, &resolved_file_name);

    let mut file = if resumed {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&partial_path)
    } else {
        File::create(&partial_path)
    }
    .map_err(|error| format!("failed to open partial download: {error}"))?;

    update_item(app, id, |item| {
        item.file_name = resolved_file_name.clone();
        item.storage_name = resolved_storage_name.clone();
        item.hls_directory = None;
        item.playback_url = playback_url(id, &resolved_storage_name);
        item.downloaded_bytes = start;
        item.total_bytes = total;
    })?;

    let mut reader = response.into_reader();
    let mut buffer = [0_u8; 64 * 1024];
    let mut downloaded = start;
    let mut last_progress = Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed) {
            update_item(app, id, |item| {
                item.status = DownloadStatus::Paused;
                item.downloaded_bytes = downloaded;
            })?;
            return Ok(());
        }

        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("download interrupted: {error}"))?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])
            .map_err(|error| format!("failed to write download: {error}"))?;
        downloaded = downloaded.saturating_add(read as u64);

        if last_progress.elapsed() >= PROGRESS_INTERVAL {
            update_item(app, id, |item| item.downloaded_bytes = downloaded)?;
            last_progress = Instant::now();
        }
    }
    file.flush()
        .map_err(|error| format!("failed to flush download: {error}"))?;
    drop(file);

    if total.is_some_and(|expected| downloaded < expected) {
        return Err("the connection ended before the file was complete".to_owned());
    }

    if is_playlist_file(&resolved_file_name) && file_starts_with_hls_playlist(&partial_path)? {
        let playlist = fs::read_to_string(&partial_path)
            .map_err(|error| format!("failed to read HLS playlist: {error}"))?;
        remove_if_exists(&partial_path)?;
        return run_hls_download_contents(
            app,
            id,
            cancel,
            resolved_source_url,
            playlist,
            &resolved_file_name,
        );
    }

    let final_file_name = sniffed_file_name(&resolved_file_name, &partial_path)?;
    if Path::new(&final_file_name)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("mp4"))
    {
        if let Some(duration) = mp4_duration_seconds(&partial_path)? {
            if duration > 0.0 && duration < MIN_OFFLINE_VIDEO_DURATION_SECONDS {
                remove_if_exists(&partial_path)?;
                return Err(short_preview_error(duration));
            }
        }
    }
    let final_storage_name = storage_name(id, &final_file_name);
    if final_storage_name != resolved_storage_name {
        update_item(app, id, |item| {
            item.file_name = final_file_name.clone();
            item.storage_name = final_storage_name.clone();
            item.playback_url = playback_url(id, &final_storage_name);
        })?;
    }

    let final_path = directory.join(&final_storage_name);
    remove_if_exists(&final_path)?;
    fs::rename(&partial_path, &final_path)
        .map_err(|error| format!("failed to finalize download: {error}"))?;
    update_item(app, id, |item| {
        item.status = DownloadStatus::Completed;
        item.downloaded_bytes = downloaded;
        item.total_bytes = Some(downloaded);
        item.error = None;
    })?;
    Ok(())
}

struct HlsProgress {
    bytes: u64,
    resources: usize,
    last_update: Instant,
}

fn run_hls_download(
    app: &AppHandle,
    id: &str,
    cancel: &AtomicBool,
    response: ureq::Response,
    resolved_file_name: &str,
) -> Result<(), String> {
    let (playlist_url, playlist) = read_hls_playlist(response)?;
    run_hls_download_contents(app, id, cancel, playlist_url, playlist, resolved_file_name)
}

fn run_hls_download_contents(
    app: &AppHandle,
    id: &str,
    cancel: &AtomicBool,
    playlist_url: Url,
    playlist: String,
    resolved_file_name: &str,
) -> Result<(), String> {
    let directory = download_directory(app)?;
    let partial_name = format!("{id}.hls.part");
    let final_name = format!("{id}.hls");
    let partial_directory = directory.join(&partial_name);
    let final_directory = directory.join(&final_name);
    fs::create_dir_all(&partial_directory)
        .map_err(|error| format!("failed to create HLS download directory: {error}"))?;

    let downloaded = directory_size(&partial_directory)?;
    let mut progress = HlsProgress {
        bytes: downloaded,
        resources: 0,
        last_update: Instant::now(),
    };
    let display_file_name = Path::new(resolved_file_name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .map(|stem| format!("{}.m3u8", sanitize_file_name(stem)))
        .unwrap_or_else(|| "offline-video.m3u8".to_owned());
    let storage_name = format!("{final_name}/index.m3u8");

    update_item(app, id, |item| {
        item.file_name = display_file_name.clone();
        item.storage_name = storage_name.clone();
        item.hls_directory = Some(final_name.clone());
        item.playback_url = playback_url(id, &storage_name);
        item.downloaded_bytes = downloaded;
        item.total_bytes = None;
    })?;

    write_hls_playlist(
        app,
        id,
        cancel,
        &playlist_url,
        &playlist,
        &partial_directory,
        "index.m3u8",
        0,
        &mut progress,
    )?;
    pause_hls_if_requested(app, id, cancel, progress.bytes)?;

    remove_dir_if_exists(&final_directory)?;
    fs::rename(&partial_directory, &final_directory)
        .map_err(|error| format!("failed to finalize HLS download: {error}"))?;
    update_item(app, id, |item| {
        item.status = DownloadStatus::Completed;
        item.downloaded_bytes = progress.bytes;
        item.total_bytes = Some(progress.bytes);
        item.error = None;
    })?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_hls_playlist(
    app: &AppHandle,
    id: &str,
    cancel: &AtomicBool,
    playlist_url: &Url,
    playlist: &str,
    directory: &Path,
    output_name: &str,
    depth: usize,
    progress: &mut HlsProgress,
) -> Result<(), String> {
    if depth > MAX_HLS_DEPTH {
        return Err("HLS playlist nesting is too deep".to_owned());
    }
    if !playlist.trim_start().starts_with("#EXTM3U") {
        return Err("the HLS playlist is invalid".to_owned());
    }
    pause_hls_if_requested(app, id, cancel, progress.bytes)?;

    if let Some((stream_info, variant_uri)) = select_hls_variant(playlist) {
        let audio_group = hls_attribute(&stream_info, "AUDIO");
        let subtitle_group = hls_attribute(&stream_info, "SUBTITLES");
        let mut output = String::from("#EXTM3U\n");
        for line in playlist.lines().filter(|line| {
            line.starts_with("#EXT-X-VERSION:") || *line == "#EXT-X-INDEPENDENT-SEGMENTS"
        }) {
            output.push_str(line);
            output.push('\n');
        }

        let mut rendition_index = 0_usize;
        for line in playlist
            .lines()
            .filter(|line| line.starts_with("#EXT-X-MEDIA:"))
        {
            let group_id = hls_attribute(line, "GROUP-ID");
            let selected = group_id.as_ref().is_some_and(|group| {
                audio_group.as_ref() == Some(group) || subtitle_group.as_ref() == Some(group)
            });
            if !selected {
                continue;
            }
            let rewritten = if let Some(uri) = hls_attribute(line, "URI") {
                let local_name = format!("rendition-{rendition_index}.m3u8");
                rendition_index += 1;
                download_hls_playlist(
                    app,
                    id,
                    cancel,
                    playlist_url,
                    &uri,
                    directory,
                    &local_name,
                    depth + 1,
                    progress,
                )?;
                replace_hls_uri(line, &local_name)?
            } else {
                line.to_owned()
            };
            output.push_str(&rewritten);
            output.push('\n');
        }

        download_hls_playlist(
            app,
            id,
            cancel,
            playlist_url,
            &variant_uri,
            directory,
            "video.m3u8",
            depth + 1,
            progress,
        )?;
        output.push_str(&stream_info);
        output.push('\n');
        output.push_str("video.m3u8\n");
        return write_text_file(&directory.join(output_name), &output);
    }

    let mut output = String::with_capacity(playlist.len());
    for line in playlist.lines() {
        pause_hls_if_requested(app, id, cancel, progress.bytes)?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            output.push('\n');
            continue;
        }
        if !trimmed.starts_with('#') {
            let resource_url = playlist_url
                .join(trimmed)
                .map_err(|_| "invalid HLS segment URL".to_owned())?;
            let local_name = hls_resource_name(&resource_url);
            download_hls_resource(
                app,
                id,
                cancel,
                &resource_url,
                directory,
                &local_name,
                progress,
            )?;
            output.push_str(&local_name);
            output.push('\n');
            continue;
        }

        if hls_attribute(trimmed, "URI").is_some()
            && (trimmed.starts_with("#EXT-X-KEY:")
                || trimmed.starts_with("#EXT-X-MAP:")
                || trimmed.starts_with("#EXT-X-SESSION-KEY:")
                || trimmed.starts_with("#EXT-X-PART:")
                || trimmed.starts_with("#EXT-X-PRELOAD-HINT:"))
        {
            let uri = hls_attribute(trimmed, "URI").expect("checked HLS URI");
            let resource_url = playlist_url
                .join(&uri)
                .map_err(|_| "invalid HLS resource URL".to_owned())?;
            let local_name = hls_resource_name(&resource_url);
            download_hls_resource(
                app,
                id,
                cancel,
                &resource_url,
                directory,
                &local_name,
                progress,
            )?;
            output.push_str(&replace_hls_uri(trimmed, &local_name)?);
            output.push('\n');
            continue;
        }

        if trimmed.starts_with("#EXT-X-RENDITION-REPORT:") {
            continue;
        }
        output.push_str(trimmed);
        output.push('\n');
    }
    write_text_file(&directory.join(output_name), &output)
}

#[allow(clippy::too_many_arguments)]
fn download_hls_playlist(
    app: &AppHandle,
    id: &str,
    cancel: &AtomicBool,
    base_url: &Url,
    raw_url: &str,
    directory: &Path,
    output_name: &str,
    depth: usize,
    progress: &mut HlsProgress,
) -> Result<(), String> {
    let url = base_url
        .join(raw_url)
        .map_err(|_| "invalid nested HLS playlist URL".to_owned())?;
    let response = fetch(url.as_str(), 0)?;
    if response.status() != 200 {
        return Err(format!(
            "HLS playlist server returned HTTP {}",
            response.status()
        ));
    }
    let (resolved_url, playlist) = read_hls_playlist(response)?;
    write_hls_playlist(
        app,
        id,
        cancel,
        &resolved_url,
        &playlist,
        directory,
        output_name,
        depth,
        progress,
    )
}

fn read_hls_playlist(response: ureq::Response) -> Result<(Url, String), String> {
    let resolved_url = Url::parse(response.get_url())
        .map_err(|_| "invalid resolved HLS playlist URL".to_owned())?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_HLS_PLAYLIST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("failed to read HLS playlist: {error}"))?;
    if bytes.len() as u64 > MAX_HLS_PLAYLIST_BYTES {
        return Err("HLS playlist is too large".to_owned());
    }
    let playlist =
        String::from_utf8(bytes).map_err(|_| "HLS playlist is not valid UTF-8".to_owned())?;
    if !playlist.trim_start().starts_with("#EXTM3U") {
        return Err("the source is not a valid HLS playlist".to_owned());
    }
    Ok((resolved_url, playlist))
}

fn download_hls_resource(
    app: &AppHandle,
    id: &str,
    cancel: &AtomicBool,
    url: &Url,
    directory: &Path,
    local_name: &str,
    progress: &mut HlsProgress,
) -> Result<(), String> {
    progress.resources += 1;
    if progress.resources > MAX_HLS_RESOURCES {
        return Err("HLS playlist contains too many resources".to_owned());
    }
    let final_path = directory.join(local_name);
    if final_path.exists() {
        return Ok(());
    }
    let partial_path = directory.join(format!("{local_name}.part"));
    let mut existing = partial_path
        .metadata()
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let mut response = fetch(url.as_str(), existing)?;
    if response.status() == 416 && existing > 0 {
        progress.bytes = progress.bytes.saturating_sub(existing);
        remove_if_exists(&partial_path)?;
        existing = 0;
        response = fetch(url.as_str(), 0)?;
    }
    if !matches!(response.status(), 200 | 206) {
        return Err(format!(
            "HLS resource server returned HTTP {}",
            response.status()
        ));
    }
    let resumed = response.status() == 206 && existing > 0;
    if !resumed && existing > 0 {
        progress.bytes = progress.bytes.saturating_sub(existing);
        existing = 0;
    }
    let expected = response
        .header("content-length")
        .and_then(|value| value.parse::<u64>().ok())
        .map(|length| existing.saturating_add(length));
    let mut file = if resumed {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&partial_path)
    } else {
        File::create(&partial_path)
    }
    .map_err(|error| format!("failed to open HLS resource: {error}"))?;

    let mut reader = response.into_reader();
    let mut buffer = [0_u8; 64 * 1024];
    let mut resource_bytes = existing;
    loop {
        pause_hls_if_requested(app, id, cancel, progress.bytes)?;
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("HLS download interrupted: {error}"))?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])
            .map_err(|error| format!("failed to write HLS resource: {error}"))?;
        resource_bytes = resource_bytes.saturating_add(read as u64);
        progress.bytes = progress.bytes.saturating_add(read as u64);
        if progress.last_update.elapsed() >= PROGRESS_INTERVAL {
            update_item(app, id, |item| item.downloaded_bytes = progress.bytes)?;
            progress.last_update = Instant::now();
        }
    }
    file.flush()
        .map_err(|error| format!("failed to flush HLS resource: {error}"))?;
    drop(file);
    if expected.is_some_and(|total| resource_bytes < total) {
        return Err("the HLS resource ended before it was complete".to_owned());
    }
    fs::rename(&partial_path, &final_path)
        .map_err(|error| format!("failed to finalize HLS resource: {error}"))
}

fn pause_hls_if_requested(
    app: &AppHandle,
    id: &str,
    cancel: &AtomicBool,
    downloaded: u64,
) -> Result<(), String> {
    if !cancel.load(Ordering::Relaxed) {
        return Ok(());
    }
    update_item(app, id, |item| {
        item.status = DownloadStatus::Paused;
        item.downloaded_bytes = downloaded;
    })?;
    Err("download paused".to_owned())
}

fn select_hls_variant(playlist: &str) -> Option<(String, String)> {
    let lines = playlist.lines().collect::<Vec<_>>();
    let mut selected: Option<(u64, String, String)> = None;
    for (index, line) in lines.iter().enumerate() {
        if !line.starts_with("#EXT-X-STREAM-INF:") {
            continue;
        }
        let uri = lines[index + 1..]
            .iter()
            .map(|line| line.trim())
            .find(|line| !line.is_empty() && !line.starts_with('#'))?;
        let bandwidth = hls_attribute(line, "BANDWIDTH")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        if selected
            .as_ref()
            .is_none_or(|(current, _, _)| bandwidth > *current)
        {
            selected = Some((bandwidth, (*line).to_owned(), uri.to_owned()));
        }
    }
    selected.map(|(_, line, uri)| (line, uri))
}

fn hls_attribute(line: &str, name: &str) -> Option<String> {
    let attributes = line.split_once(':')?.1;
    let mut quoted = false;
    let mut start = 0_usize;
    for (index, character) in attributes
        .char_indices()
        .chain(std::iter::once((attributes.len(), ',')))
    {
        if character == '"' {
            quoted = !quoted;
        }
        if character != ',' || quoted {
            continue;
        }
        let attribute = attributes[start..index].trim();
        start = index + 1;
        let (attribute_name, value) = attribute.split_once('=')?;
        if attribute_name.trim() != name {
            continue;
        }
        let value = value.trim();
        if let Some(value) = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
        {
            return Some(value.to_owned());
        }
        return Some(value.to_owned());
    }
    None
}

fn replace_hls_uri(line: &str, local_name: &str) -> Result<String, String> {
    let marker = "URI=\"";
    let start = line
        .find(marker)
        .map(|index| index + marker.len())
        .ok_or_else(|| "invalid HLS URI attribute".to_owned())?;
    let end = line[start..]
        .find('"')
        .map(|index| start + index)
        .ok_or_else(|| "invalid HLS URI attribute".to_owned())?;
    Ok(format!("{}{}{}", &line[..start], local_name, &line[end..]))
}

fn hls_resource_name(url: &Url) -> String {
    let hash = url
        .as_str()
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        });
    let extension = Path::new(url.path())
        .extension()
        .and_then(|extension| extension.to_str())
        .filter(|extension| {
            !extension.is_empty()
                && extension.len() <= 12
                && extension
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric())
        })
        .map(|extension| format!(".{extension}"))
        .unwrap_or_default();
    format!("asset-{hash:016x}{extension}")
}

fn directory_size(path: &Path) -> Result<u64, String> {
    let mut total = 0_u64;
    for entry in
        fs::read_dir(path).map_err(|error| format!("failed to inspect HLS download: {error}"))?
    {
        let entry = entry.map_err(|error| format!("failed to inspect HLS download: {error}"))?;
        if entry
            .file_type()
            .map_err(|error| format!("failed to inspect HLS download: {error}"))?
            .is_file()
        {
            total = total.saturating_add(
                entry
                    .metadata()
                    .map_err(|error| format!("failed to inspect HLS download: {error}"))?
                    .len(),
            );
        }
    }
    Ok(total)
}

fn write_text_file(path: &Path, contents: &str) -> Result<(), String> {
    fs::write(path, contents).map_err(|error| format!("failed to save HLS playlist: {error}"))
}

fn fetch(url: &str, offset: u64) -> Result<ureq::Response, String> {
    let public_agent = ureq::AgentBuilder::new()
        .timeout(DOWNLOAD_TIMEOUT)
        .timeout_connect(CONNECT_TIMEOUT)
        .redirects(0)
        .try_proxy_from_env(false)
        .resolver(proxy_security::PublicResolver)
        .build();
    let local_agent = ureq::AgentBuilder::new()
        .timeout(DOWNLOAD_TIMEOUT)
        .timeout_connect(CONNECT_TIMEOUT)
        .redirects(0)
        .try_proxy_from_env(false)
        .build();
    let mut current = Url::parse(url).map_err(|_| "invalid download URL")?;

    for _ in 0..=MAX_REDIRECTS {
        validate_target(&current)?;
        let agent = if is_local_service_target(&current) {
            &local_agent
        } else {
            &public_agent
        };
        let mut request = agent
            .get(current.as_str())
            .set("Accept-Encoding", "identity")
            .set("User-Agent", "Stremio Horizon");
        if offset > 0 {
            request = request.set("Range", &format!("bytes={offset}-"));
        }
        let response = match request.call() {
            Ok(response) | Err(ureq::Error::Status(_, response)) => response,
            Err(ureq::Error::Transport(error)) => {
                return Err(if proxy_security::is_blocked_destination_error(&error) {
                    "download destination is not public".to_owned()
                } else {
                    format!("download request failed: {:?}", error.kind())
                });
            }
        };

        if matches!(response.status(), 301 | 302 | 303 | 307 | 308) {
            let location = response
                .header("location")
                .ok_or("download redirect has no location")?;
            current = current
                .join(location)
                .map_err(|_| "invalid download redirect")?;
            continue;
        }
        return Ok(response);
    }
    Err("too many download redirects".to_owned())
}

fn cache_thumbnail(app: &AppHandle, id: &str) -> Result<(), String> {
    let item = get_item(app, id)?;
    if item.thumbnail_storage_name.is_some() {
        return Ok(());
    }
    let source_url = match item.source_thumbnail_url {
        Some(source_url) => source_url,
        None => return Ok(()),
    };
    let response = fetch(&source_url, 0)?;
    if response.status() != 200 {
        return Err(format!(
            "thumbnail server returned HTTP {}",
            response.status()
        ));
    }
    let content_type = response
        .header("content-type")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let extension = thumbnail_extension(&content_type)
        .ok_or_else(|| "thumbnail response is not a supported image".to_owned())?;
    if response
        .header("content-length")
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > MAX_THUMBNAIL_BYTES)
    {
        return Err("thumbnail is too large".to_owned());
    }

    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_THUMBNAIL_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("failed to download thumbnail: {error}"))?;
    if bytes.len() as u64 > MAX_THUMBNAIL_BYTES {
        return Err("thumbnail is too large".to_owned());
    }
    let storage_name = format!("{id}.thumbnail{extension}");
    let path = download_directory(app)?.join(&storage_name);
    fs::write(&path, bytes).map_err(|error| format!("failed to save thumbnail: {error}"))?;
    update_item(app, id, |item| {
        item.thumbnail_storage_name = Some(storage_name);
    })?;
    Ok(())
}

fn validate_target(url: &Url) -> Result<(), String> {
    if is_local_service_target(url) {
        return Ok(());
    }
    proxy_security::validate_url(url).map_err(|_| "download destination is not public".to_owned())
}

fn is_local_service_target(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && matches!(url.host_str(), Some("127.0.0.1") | Some("localhost"))
        && url.port_or_known_default() == Some(SERVICE_PORT)
        && url.username().is_empty()
        && url.password().is_none()
}

fn update_item(
    app: &AppHandle,
    id: &str,
    update: impl FnOnce(&mut DownloadItem),
) -> Result<DownloadItem, String> {
    let item = {
        let state = app.state::<DownloadManager>();
        let mut items = state
            .items
            .lock()
            .map_err(|error| format!("failed to update download: {error}"))?;
        let item = items.get_mut(id).ok_or("download not found")?;
        update(item);
        item.updated_at = now_millis();
        item.clone()
    };
    persist(app)?;
    emit_changed(app, &item);
    Ok(item)
}

fn get_item(app: &AppHandle, id: &str) -> Result<DownloadItem, String> {
    app.state::<DownloadManager>()
        .items
        .lock()
        .map_err(|error| format!("failed to read download: {error}"))?
        .get(id)
        .cloned()
        .ok_or_else(|| "download not found".to_owned())
}

fn emit_changed(app: &AppHandle, item: &DownloadItem) {
    updater::emit_event(app, "stremio-download-changed", &DownloadView::from(item));
}

fn load_items(app: &AppHandle) -> Result<Vec<DownloadItem>, String> {
    let path = index_path(app)?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes =
        fs::read(&path).map_err(|error| format!("failed to read download index: {error}"))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("failed to parse download index: {error}"))
}

fn persist(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<DownloadManager>();
    let _persistence = state
        .persistence
        .lock()
        .map_err(|error| format!("failed to lock download index: {error}"))?;
    let mut items = state
        .items
        .lock()
        .map_err(|error| format!("failed to persist downloads: {error}"))?
        .values()
        .cloned()
        .collect::<Vec<_>>();
    items.sort_by(|left, right| right.created_at.cmp(&left.created_at));
    let path = index_path(app)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create download directory: {error}"))?;
    }
    let bytes = serde_json::to_vec_pretty(&items)
        .map_err(|error| format!("failed to serialize downloads: {error}"))?;
    let temporary_path = path.with_extension("json.tmp");
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary_path)
        .map_err(|error| format!("failed to open download index: {error}"))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("failed to write download index: {error}"))?;
    drop(file);
    #[cfg(windows)]
    remove_if_exists(&path)?;
    fs::rename(&temporary_path, &path)
        .map_err(|error| format!("failed to replace download index: {error}"))
}

fn index_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(download_directory(app)?.join(INDEX_FILE))
}

fn download_directory(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|path| path.join("downloads"))
        .map_err(|error| format!("failed to resolve download directory: {error}"))
}

pub fn respond(app: &AppHandle, request: Request, path: &str) {
    let (id, requested) = path.split_once('/').unwrap_or((path, ""));
    if let Err((request, status, message)) = respond_inner(app, request, id, requested) {
        let _ = request.respond(Response::from_string(message).with_status_code(status));
    }
}

fn respond_inner(
    app: &AppHandle,
    request: Request,
    id: &str,
    requested: &str,
) -> Result<(), (Request, u16, String)> {
    let thumbnail = requested == "thumbnail";
    let item = match get_item(app, id) {
        Ok(item) if thumbnail || item.status == DownloadStatus::Completed => item,
        _ => return Err((request, 404, "download not found".to_owned())),
    };
    let storage_name = if thumbnail {
        match item.thumbnail_storage_name.as_deref() {
            Some(storage_name) => storage_name.to_owned(),
            None => return Err((request, 404, "thumbnail not found".to_owned())),
        }
    } else if let Some(hls_directory) = item.hls_directory.as_deref() {
        let prefix = format!("{hls_directory}/");
        let relative = match requested.strip_prefix(&prefix) {
            Some(relative)
                if !relative.is_empty()
                    && !relative.contains('/')
                    && !relative.contains('\\')
                    && !matches!(relative, "." | "..") =>
            {
                relative
            }
            _ => return Err((request, 404, "download file not found".to_owned())),
        };
        format!("{hls_directory}/{relative}")
    } else {
        if requested != item.storage_name {
            return Err((request, 404, "download file not found".to_owned()));
        }
        item.storage_name.clone()
    };
    let path = match download_directory(app) {
        Ok(directory) => directory.join(&storage_name),
        Err(error) => return Err((request, 500, error)),
    };
    let length = match path.metadata() {
        Ok(metadata) => metadata.len(),
        Err(_) => return Err((request, 404, "download file not found".to_owned())),
    };
    let range_header = request
        .headers()
        .iter()
        .find(|header| header.field.as_str().as_str().eq_ignore_ascii_case("range"))
        .map(|header| header.value.as_str().to_owned());
    let range = match range_header {
        Some(raw) => match parse_range(&raw, length) {
            Some(range) => Some(range),
            None => return Err((request, 416, "invalid byte range".to_owned())),
        },
        None => None,
    };
    let (start, end, status) = range
        .map(|(start, end)| (start, end, StatusCode(206)))
        .unwrap_or((0, length.saturating_sub(1), StatusCode(200)));
    let response_length = if length == 0 { 0 } else { end - start + 1 };
    let response_length_usize = match usize::try_from(response_length) {
        Ok(length) => length,
        Err(_) => {
            return Err((
                request,
                500,
                "download is too large for this platform".to_owned(),
            ))
        }
    };
    let mut headers = vec![
        header("Accept-Ranges", "bytes"),
        header(
            "Cache-Control",
            if thumbnail {
                "private, max-age=31536000, immutable"
            } else {
                "no-store"
            },
        ),
        header("Content-Type", mime_for_file(&storage_name)),
    ];
    if status.0 == 206 {
        headers.push(header(
            "Content-Range",
            &format!("bytes {start}-{end}/{length}"),
        ));
    }

    if request.method() == &Method::Head {
        let _ = request.respond(Response::new(
            status,
            headers,
            io::empty(),
            Some(response_length_usize),
            None,
        ));
        return Ok(());
    }

    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(error) => return Err((request, 500, format!("failed to open download: {error}"))),
    };
    if let Err(error) = file.seek(SeekFrom::Start(start)) {
        return Err((request, 500, format!("failed to seek download: {error}")));
    }
    let _ = request.respond(Response::new(
        status,
        headers,
        file.take(response_length),
        Some(response_length_usize),
        None,
    ));
    Ok(())
}

fn parse_range(value: &str, length: u64) -> Option<(u64, u64)> {
    let value = value.strip_prefix("bytes=")?;
    if value.contains(',') || length == 0 {
        return None;
    }
    let (start, end) = value.split_once('-')?;
    if start.is_empty() {
        let suffix = end.parse::<u64>().ok()?.min(length);
        return (suffix > 0).then_some((length - suffix, length - 1));
    }
    let start = start.parse::<u64>().ok()?;
    if start >= length {
        return None;
    }
    let end = if end.is_empty() {
        length - 1
    } else {
        end.parse::<u64>().ok()?.min(length - 1)
    };
    (start <= end).then_some((start, end))
}

fn content_range_total(value: Option<&str>) -> Option<u64> {
    value?.rsplit_once('/')?.1.parse().ok()
}

fn resolved_file_name(
    item: &DownloadItem,
    response: &ureq::Response,
    content_type: &str,
) -> String {
    let from_header = response
        .header("content-disposition")
        .and_then(content_disposition_file_name)
        .map(sanitize_file_name);
    let mut file_name = from_header.unwrap_or_else(|| item.file_name.clone());
    if Path::new(&file_name).extension().is_none() {
        if let Some(extension) = extension_for_content_type(content_type) {
            file_name.push_str(extension);
        }
    }
    file_name
}

fn content_disposition_file_name(value: &str) -> Option<&str> {
    value.split(';').find_map(|part| {
        let part = part.trim();
        part.strip_prefix("filename=")
            .or_else(|| part.strip_prefix("filename*=UTF-8''"))
            .map(|name| name.trim_matches('"'))
            .filter(|name| !name.is_empty())
    })
}

fn extension_for_content_type(content_type: &str) -> Option<&'static str> {
    if content_type.contains("video/mp4") {
        Some(".mp4")
    } else if content_type.contains("matroska") {
        Some(".mkv")
    } else if content_type.contains("video/webm") {
        Some(".webm")
    } else if content_type.contains("quicktime") {
        Some(".mov")
    } else if content_type.contains("video/x-msvideo") {
        Some(".avi")
    } else if content_type.contains("mp2t") {
        Some(".ts")
    } else {
        None
    }
}

fn thumbnail_extension(content_type: &str) -> Option<&'static str> {
    if content_type.contains("image/jpeg") {
        Some(".jpg")
    } else if content_type.contains("image/png") {
        Some(".png")
    } else if content_type.contains("image/webp") {
        Some(".webp")
    } else if content_type.contains("image/avif") {
        Some(".avif")
    } else {
        None
    }
}

fn mime_for_file(file_name: &str) -> &'static str {
    match Path::new(file_name)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "ts" | "m2ts" => "video/mp2t",
        "m3u" | "m3u8" => "application/vnd.apple.mpegurl",
        "m4s" | "mp4s" => "video/iso.segment",
        "aac" => "audio/aac",
        "vtt" => "text/vtt",
        "key" => "application/octet-stream",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "avif" => "image/avif",
        _ => "application/octet-stream",
    }
}

fn is_hls_content_type(content_type: &str) -> bool {
    content_type.to_ascii_lowercase().contains("mpegurl")
}

fn is_playlist_file(file_name: &str) -> bool {
    matches!(
        Path::new(file_name)
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "m3u" | "m3u8"
    )
}

fn file_starts_with_hls_playlist(path: &Path) -> Result<bool, String> {
    let mut file = File::open(path)
        .map_err(|error| format!("failed to inspect downloaded source: {error}"))?;
    let mut prefix = [0_u8; 64];
    let length = file
        .read(&mut prefix)
        .map_err(|error| format!("failed to inspect downloaded source: {error}"))?;
    Ok(String::from_utf8_lossy(&prefix[..length])
        .trim_start_matches(['\u{feff}', ' ', '\t', '\r', '\n'])
        .starts_with("#EXTM3U"))
}

fn sniffed_file_name(file_name: &str, path: &Path) -> Result<String, String> {
    let mut file =
        File::open(path).map_err(|error| format!("failed to inspect downloaded video: {error}"))?;
    let mut prefix = [0_u8; 16];
    let length = file
        .read(&mut prefix)
        .map_err(|error| format!("failed to inspect downloaded video: {error}"))?;
    let extension = if length >= 8 && &prefix[4..8] == b"ftyp" {
        Some("mp4")
    } else if length >= 4 && prefix[..4] == [0x1a, 0x45, 0xdf, 0xa3] {
        Some("mkv")
    } else if length >= 1 && prefix[0] == 0x47 {
        Some("ts")
    } else {
        None
    };
    let Some(extension) = extension else {
        return Ok(file_name.to_owned());
    };
    if Path::new(file_name)
        .extension()
        .and_then(|current| current.to_str())
        .is_some_and(|current| current.eq_ignore_ascii_case(extension))
    {
        return Ok(file_name.to_owned());
    }
    let mut corrected = PathBuf::from(file_name);
    corrected.set_extension(extension);
    Ok(sanitize_file_name(&corrected.to_string_lossy()))
}

fn mp4_duration_seconds(path: &Path) -> Result<Option<f64>, String> {
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|error| format!("failed to inspect downloaded video: {error}"))?
        .take(MAX_MP4_PROBE_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("failed to inspect downloaded video: {error}"))?;
    for position in 0..bytes.len().saturating_sub(32) {
        if &bytes[position..position + 4] != b"mvhd" {
            continue;
        }
        let version = bytes[position + 4];
        let (timescale, duration) = if version == 0 {
            (
                read_be_u32(&bytes, position + 16).map(u64::from),
                read_be_u32(&bytes, position + 20).map(u64::from),
            )
        } else if version == 1 {
            (
                read_be_u32(&bytes, position + 24).map(u64::from),
                read_be_u64(&bytes, position + 28),
            )
        } else {
            continue;
        };
        if let (Some(timescale), Some(duration)) = (timescale, duration) {
            if timescale > 0 {
                return Ok(Some(duration as f64 / timescale as f64));
            }
        }
    }
    Ok(None)
}

fn completed_video_problem(app: &AppHandle, item: &DownloadItem) -> Result<Option<String>, String> {
    if !Path::new(&item.file_name)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("mp4"))
    {
        return Ok(None);
    }
    let path = download_directory(app)?.join(&item.storage_name);
    Ok(mp4_duration_seconds(&path)?
        .filter(|duration| *duration > 0.0 && *duration < MIN_OFFLINE_VIDEO_DURATION_SECONDS)
        .map(short_preview_error))
}

fn short_preview_error(duration: f64) -> String {
    format!(
        "the source only provided a {}-second preview; retry when it is ready or choose another stream",
        duration.ceil() as u64
    )
}

fn read_be_u32(bytes: &[u8], start: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        bytes.get(start..start + 4)?.try_into().ok()?,
    ))
}

fn read_be_u64(bytes: &[u8], start: usize) -> Option<u64> {
    Some(u64::from_be_bytes(
        bytes.get(start..start + 8)?.try_into().ok()?,
    ))
}

fn file_name_from_url(raw: &str) -> Option<String> {
    Url::parse(raw)
        .ok()?
        .path_segments()?
        .next_back()
        .filter(|segment| !segment.is_empty())
        .map(ToOwned::to_owned)
}

fn sanitize_file_name(value: &str) -> String {
    let cleaned = value
        .chars()
        .map(|character| match character {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\n' | '\r' | '\t' => '_',
            character if character.is_control() => '_',
            character => character,
        })
        .collect::<String>();
    let cleaned = cleaned.trim().trim_matches('.');
    if cleaned.is_empty() {
        "video".to_owned()
    } else {
        cleaned.chars().take(180).collect()
    }
}

fn storage_name(id: &str, file_name: &str) -> String {
    let extension = Path::new(file_name)
        .extension()
        .and_then(|extension| extension.to_str())
        .filter(|extension| !extension.is_empty())
        .map(|extension| format!(".{}", sanitize_file_name(extension)))
        .unwrap_or_default();
    format!("{id}{extension}")
}

fn playback_url(id: &str, storage_name: &str) -> String {
    format!(
        "http://localhost:{}{DOWNLOAD_PREFIX}{id}/{storage_name}",
        crate::PORT
    )
}

fn local_file_url(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    if path.starts_with('/') {
        format!("file://{path}")
    } else {
        format!("file:///{path}")
    }
}

fn service_playback_url(id: &str, path: &Path) -> Result<String, String> {
    let mut url = Url::parse(&format!(
        "http://127.0.0.1:{SERVICE_PORT}/hlsv2/offline-{id}/master.m3u8"
    ))
    .map_err(|_| "failed to build offline playback URL".to_owned())?;
    url.query_pairs_mut()
        .append_pair("mediaURL", &local_file_url(path))
        .append_pair("forceTranscoding", "1")
        .append_pair("videoCodecs", "h264")
        .append_pair("audioCodecs", "aac")
        .append_pair("maxAudioChannels", "2");
    Ok(url.to_string())
}

fn thumbnail_url(id: &str) -> String {
    format!(
        "http://localhost:{}{DOWNLOAD_PREFIX}{id}/thumbnail",
        crate::PORT
    )
}

fn non_empty(value: String, fallback: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        fallback.to_owned()
    } else {
        value.to_owned()
    }
}

fn clean_optional(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim().to_owned();
        (!value.is_empty()).then_some(value)
    })
}

fn next_id() -> String {
    format!(
        "{}-{}",
        now_millis(),
        DOWNLOAD_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn remove_if_exists(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("failed to delete {}: {error}", path.display())),
    }
}

fn remove_dir_if_exists(path: &Path) -> Result<(), String> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("failed to delete {}: {error}", path.display())),
    }
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid HTTP header")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queued_item(id: &str, created_at: u64, status: DownloadStatus) -> DownloadItem {
        DownloadItem {
            id: id.to_owned(),
            title: "Title".to_owned(),
            subtitle: None,
            content_type: None,
            content_id: None,
            video_id: None,
            season: None,
            episode: None,
            description: None,
            source_name: None,
            source_thumbnail_url: None,
            thumbnail_storage_name: None,
            source_url: "https://example.com/video.mp4".to_owned(),
            file_name: "video.mp4".to_owned(),
            storage_name: format!("{id}.mp4"),
            hls_directory: None,
            status,
            downloaded_bytes: 0,
            total_bytes: None,
            created_at,
            updated_at: created_at,
            error: None,
            playback_url: String::new(),
        }
    }

    #[test]
    fn download_queue_is_fifo_and_skips_non_queued_items() {
        let items = HashMap::from([
            (
                "newer".to_owned(),
                queued_item("newer", 20, DownloadStatus::Queued),
            ),
            (
                "active".to_owned(),
                queued_item("active", 5, DownloadStatus::Downloading),
            ),
            (
                "older".to_owned(),
                queued_item("older", 10, DownloadStatus::Queued),
            ),
        ]);

        assert_eq!(next_queued_id(&items).as_deref(), Some("older"));
    }

    #[test]
    fn sanitizes_file_names_for_all_desktop_platforms() {
        assert_eq!(
            sanitize_file_name("../Season 1/Episode: 2?.mkv"),
            "_Season 1_Episode_ 2_.mkv"
        );
        assert_eq!(sanitize_file_name("..."), "video");
    }

    #[test]
    fn parses_http_byte_ranges() {
        assert_eq!(parse_range("bytes=10-19", 100), Some((10, 19)));
        assert_eq!(parse_range("bytes=90-", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=-10", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=100-", 100), None);
        assert_eq!(parse_range("bytes=0-1,4-5", 100), None);
    }

    #[test]
    fn keeps_spaces_literal_in_service_file_urls() {
        let url = local_file_url(Path::new("/Users/example/Application Support/video.mp4"));
        assert_eq!(
            url,
            "file:///Users/example/Application Support/video.mp4"
        );
        assert!(!url.contains("%20"));
    }

    #[test]
    fn builds_single_encoded_service_playback_urls() {
        let url = service_playback_url(
            "download-1",
            Path::new("/Users/example/Application Support/video.mp4"),
        )
        .unwrap();
        assert!(!url.contains("%2520"));
        let parsed = Url::parse(&url).unwrap();
        assert_eq!(
            parsed
                .query_pairs()
                .find(|(key, _)| key == "mediaURL")
                .map(|(_, value)| value.into_owned()),
            Some("file:///Users/example/Application Support/video.mp4".to_owned())
        );
    }

    #[test]
    fn only_allows_the_bundled_service_on_loopback() {
        assert!(validate_target(&Url::parse("http://127.0.0.1:11470/file").unwrap()).is_ok());
        assert!(validate_target(&Url::parse("http://localhost:11470/file").unwrap()).is_ok());
        assert!(validate_target(&Url::parse("http://127.0.0.1:9999/file").unwrap()).is_err());
        assert!(validate_target(&Url::parse("http://192.168.1.2/file").unwrap()).is_err());
        assert!(validate_target(&Url::parse("https://example.com/video.mp4").unwrap()).is_ok());
    }

    #[test]
    fn detects_hls_playlist_files_and_content_types() {
        assert!(is_playlist_file("movie.m3u"));
        assert!(is_playlist_file("movie.M3U8"));
        assert!(!is_playlist_file("movie.mkv"));
        assert!(is_hls_content_type("application/vnd.apple.mpegurl"));
        assert!(is_hls_content_type("application/x-mpegURL; charset=utf-8"));
        assert!(!is_hls_content_type("video/mp4"));
    }

    #[test]
    fn selects_the_highest_bandwidth_hls_variant() {
        let playlist = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=500000\nlow.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=1500000,AUDIO=\"main\"\nhigh.m3u8\n";
        assert_eq!(
            select_hls_variant(playlist),
            Some((
                "#EXT-X-STREAM-INF:BANDWIDTH=1500000,AUDIO=\"main\"".to_owned(),
                "high.m3u8".to_owned()
            ))
        );
    }

    #[test]
    fn rewrites_hls_resource_uris_without_leaking_remote_urls() {
        let line = "#EXT-X-KEY:METHOD=AES-128,URI=\"https://cdn.example/key\",IV=0x12";
        assert_eq!(
            replace_hls_uri(line, "asset.key").unwrap(),
            "#EXT-X-KEY:METHOD=AES-128,URI=\"asset.key\",IV=0x12"
        );
        assert_eq!(hls_attribute(line, "METHOD"), Some("AES-128".to_owned()));
        assert_eq!(
            hls_attribute(line, "URI"),
            Some("https://cdn.example/key".to_owned())
        );
    }

    #[test]
    fn sniffs_mislabeled_mp4_and_real_hls_sources() {
        let path = std::env::temp_dir().join(format!("horizon-download-sniff-{}", next_id()));
        let mut mp4 = vec![0_u8; 64];
        mp4[4..8].copy_from_slice(b"ftyp");
        mp4[20..24].copy_from_slice(b"mvhd");
        mp4[36..40].copy_from_slice(&1_000_u32.to_be_bytes());
        mp4[40..44].copy_from_slice(&8_000_u32.to_be_bytes());
        fs::write(&path, mp4).unwrap();
        assert_eq!(
            sniffed_file_name("playlist.m3u", &path).unwrap(),
            "playlist.mp4"
        );
        assert_eq!(mp4_duration_seconds(&path).unwrap(), Some(8.0));
        assert!(!file_starts_with_hls_playlist(&path).unwrap());

        fs::write(&path, b"\xef\xbb\xbf\n#EXTM3U\n#EXT-X-ENDLIST\n").unwrap();
        assert!(file_starts_with_hls_playlist(&path).unwrap());
        remove_if_exists(&path).unwrap();
    }

    #[test]
    fn maps_supported_thumbnail_content_types() {
        assert_eq!(thumbnail_extension("image/jpeg"), Some(".jpg"));
        assert_eq!(
            thumbnail_extension("image/webp; charset=binary"),
            Some(".webp")
        );
        assert_eq!(thumbnail_extension("text/html"), None);
    }
}
