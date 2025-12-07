use anyhow::anyhow;
use discord_rich_presence::activity::Activity;
use discord_rich_presence::{activity, DiscordIpc, DiscordIpcClient};
use std::time::{SystemTime, UNIX_EPOCH};
pub struct PresenceProvider {
    pub client: DiscordIpcClient,
    pub activity: Activity<'static>,
}

impl PresenceProvider {
    pub fn try_init() -> anyhow::Result<Self> {
        let mut client = DiscordIpcClient::new("914720093701832724");
        client.connect().map_err(|e| {
            error!("{}", e);
            anyhow!("Failed to connect to RPC endpoint!")
        })?;
        let assets = activity::Assets::new()
            .large_image("cogmind_logo")
            .small_image("go_treads")
            .large_text("Cogmind b13")
            .small_text("Treads enjoyer");
        let mut buttons = Vec::new();
        let start = SystemTime::now();
        let start_time = start.duration_since(UNIX_EPOCH)?;
        let timestamp = activity::Timestamps::new().start(start_time.as_secs() as i64);
        buttons.push(activity::Button::new(
            "Visit Site",
            "https://gridsagegames.com/cogmind",
        ));
        let payload = Activity::new()
            .assets(assets)
            .details("Playing b13")
            .buttons(buttons)
            .timestamps(timestamp);
        Ok(Self {
            client,
            activity: payload,
        })
    }
}
