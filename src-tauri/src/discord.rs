use discord_rich_presence::{
    activity::{Activity, ActivityType, Assets, Timestamps},
    DiscordIpc, DiscordIpcClient,
};
use serde::Deserialize;
use std::sync::Mutex;
use tauri::{AppHandle, Manager, State};

// This is the existing Stremio Discord application. Activity::name overrides the
// displayed activity name so the fork is identified as Stremio Horizon.
const DISCORD_CLIENT_ID: &str = "997798118185771059";

#[derive(Default)]
pub struct DiscordManager {
    client: Mutex<Option<DiscordIpcClient>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscordActivityRequest {
    state: String,
    details: String,
    image: Option<String>,
    start_timestamp: Option<i64>,
    end_timestamp: Option<i64>,
}

impl DiscordManager {
    fn connect(&self) -> Result<bool, String> {
        let mut client = self
            .client
            .lock()
            .map_err(|_| "Discord state is unavailable")?;
        if client.is_some() {
            return Ok(true);
        }

        let mut next = DiscordIpcClient::new(DISCORD_CLIENT_ID);
        next.connect()
            .map_err(|error| format!("failed to connect to Discord: {error}"))?;
        *client = Some(next);
        Ok(true)
    }

    fn disconnect(&self) {
        let Ok(mut client) = self.client.lock() else {
            return;
        };
        if let Some(mut connected) = client.take() {
            let _ = connected.clear_activity();
            let _ = connected.close();
        }
    }

    fn clear_activity(&self) -> Result<(), String> {
        self.with_client(|client| client.clear_activity())
    }

    fn set_activity(&self, request: &DiscordActivityRequest) -> Result<(), String> {
        self.with_client(|client| {
            let mut activity = Activity::new()
                .name("Stremio Horizon")
                .activity_type(ActivityType::Watching)
                .state(request.state.trim())
                .details(request.details.trim());

            if let Some(image) = request
                .image
                .as_deref()
                .filter(|image| !image.trim().is_empty())
            {
                activity = activity.assets(
                    Assets::new()
                        .large_image(image.trim())
                        .large_text(request.details.trim()),
                );
            }

            if request.start_timestamp.is_some() || request.end_timestamp.is_some() {
                let mut timestamps = Timestamps::new();
                if let Some(start) = request.start_timestamp {
                    timestamps = timestamps.start(start);
                }
                if let Some(end) = request.end_timestamp {
                    timestamps = timestamps.end(end);
                }
                activity = activity.timestamps(timestamps);
            }

            client.set_activity(activity)
        })
    }

    fn with_client<T>(
        &self,
        operation: impl FnOnce(&mut DiscordIpcClient) -> Result<T, discord_rich_presence::error::Error>,
    ) -> Result<T, String> {
        let mut guard = self
            .client
            .lock()
            .map_err(|_| "Discord state is unavailable")?;
        let Some(client) = guard.as_mut() else {
            return Err("Discord is not connected".to_owned());
        };

        match operation(client) {
            Ok(value) => Ok(value),
            Err(error) => {
                let _ = client.close();
                *guard = None;
                Err(format!("Discord RPC failed: {error}"))
            }
        }
    }
}

#[tauri::command]
pub fn discord_connect(manager: State<'_, DiscordManager>) -> Result<bool, String> {
    manager.connect()
}

#[tauri::command]
pub fn discord_disconnect(manager: State<'_, DiscordManager>) {
    manager.disconnect();
}

#[tauri::command]
pub fn discord_set_activity(
    manager: State<'_, DiscordManager>,
    activity: DiscordActivityRequest,
) -> Result<(), String> {
    manager.set_activity(&activity)
}

#[tauri::command]
pub fn discord_clear_activity(manager: State<'_, DiscordManager>) -> Result<(), String> {
    manager.clear_activity()
}

pub fn shutdown(app: &AppHandle) {
    app.state::<DiscordManager>().disconnect();
}

#[cfg(test)]
mod tests {
    use super::DiscordActivityRequest;

    #[test]
    fn deserializes_frontend_activity_payload() {
        let request: DiscordActivityRequest = serde_json::from_value(serde_json::json!({
            "state": "Watching",
            "details": "Hunter x Hunter · S1:E57",
            "image": "https://example.com/episode.jpg",
            "startTimestamp": 1_700_000_000,
            "endTimestamp": 1_700_001_200
        }))
        .unwrap();

        assert_eq!(request.state, "Watching");
        assert_eq!(request.details, "Hunter x Hunter · S1:E57");
        assert_eq!(
            request.image.as_deref(),
            Some("https://example.com/episode.jpg")
        );
        assert_eq!(request.start_timestamp, Some(1_700_000_000));
        assert_eq!(request.end_timestamp, Some(1_700_001_200));
    }
}
