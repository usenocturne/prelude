use prelude_media::UrlVideoPlayer;

use super::{Player, PlayerFactory, PlayerStatus};
use crate::media_runtime::MediaController;

#[derive(Debug)]
pub(crate) struct GstPlayerFactory {
    media: MediaController,
}

impl GstPlayerFactory {
    pub(crate) fn new(media: MediaController) -> Self {
        Self { media }
    }
}

impl PlayerFactory for GstPlayerFactory {
    fn start(&self, url: &str, start_seconds: f64, muted: bool) -> Result<Box<dyn Player>, String> {
        let outputs = self.media.begin_video_playback()?;
        match UrlVideoPlayer::start(url, start_seconds, muted, outputs.display, outputs.audio) {
            Ok(player) => Ok(Box::new(GstPlayer {
                player: Some(player),
                media: self.media.clone(),
            })),
            Err(error) => {
                if let Err(resume) = self.media.end_video_playback() {
                    tracing::warn!(resume, "CarPlay screen decoding did not resume");
                }
                Err(error.to_string())
            }
        }
    }
}

struct GstPlayer {
    player: Option<UrlVideoPlayer>,
    media: MediaController,
}

impl Player for GstPlayer {
    fn set_rate(&mut self, rate: f64) {
        if let Some(player) = &self.player {
            player.set_playing(rate > 0.0);
        }
    }

    fn set_muted(&mut self, muted: bool) {
        if let Some(player) = &self.player {
            player.set_muted(muted);
        }
    }

    fn status(&self) -> PlayerStatus {
        let Some(player) = &self.player else {
            return PlayerStatus::default();
        };
        let status = player.status();
        PlayerStatus {
            ready: status.ready,
            playing: status.playing,
            ended: status.ended,
            failed: status.failed,
            position: status.position_seconds,
            duration: status.duration_seconds,
            buffered_percent: status.buffered_percent,
        }
    }
}

impl Drop for GstPlayer {
    fn drop(&mut self) {
        drop(self.player.take());
        if let Err(error) = self.media.end_video_playback() {
            tracing::warn!(error, "CarPlay screen decoding did not resume");
        }
    }
}
