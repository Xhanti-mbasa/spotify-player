use super::model::{
    AlbumId, ArtistId, ContextId, Device, PlaybackMetadata, PlaylistId, ShowId, TracksId,
};
use super::queue::CustomQueue;

#[derive(Debug)]
pub struct PendingVolume {
    volume: u8,
    device_id: Option<String>,
    requested_at: std::time::Instant,
}

/// Player state
#[derive(Default, Debug)]
pub struct PlayerState {
    pub devices: Vec<Device>,
    pub pending_volume: Option<PendingVolume>,

    pub playback: Option<rspotify::model::CurrentPlaybackContext>,
    pub playback_last_updated_time: Option<std::time::Instant>,
    /// A buffered state to speedup the feedback of playback metadata update to user
    // Related issue: https://github.com/aome510/spotify-player/issues/109
    pub buffered_playback: Option<PlaybackMetadata>,

    pub queue: Option<rspotify::model::CurrentUserQueue>,

    /// The currently playing Tracks context (for contexts not tracked by Spotify's playback, e.g. liked/top tracks)
    pub currently_playing_tracks_id: Option<TracksId>,

    /// App-managed custom queue for full playlist/album playback.
    /// Active when the integrated librespot player is streaming and the user
    /// started playback from a track-table context.
    pub custom_queue: Option<CustomQueue>,
}

impl PlayerState {
    pub fn change_volume(&mut self, offset: i32) -> Option<u8> {
        let playback = self.buffered_playback.as_mut()?;
        let volume = (i64::from(playback.volume?) + i64::from(offset)).clamp(0, 100) as u8;
        if playback.volume == Some(u32::from(volume)) {
            return None;
        }
        playback.volume = Some(u32::from(volume));
        playback.mute_state = None;
        self.pending_volume = Some(PendingVolume {
            volume,
            device_id: playback.device_id.clone(),
            requested_at: std::time::Instant::now(),
        });
        Some(volume)
    }

    pub fn reconcile_pending_volume(&mut self) {
        let Some(pending) = &self.pending_volume else {
            return;
        };
        let Some(playback) = self.buffered_playback.as_mut() else {
            return;
        };
        if playback.device_id != pending.device_id
            || pending.requested_at.elapsed() >= std::time::Duration::from_secs(2)
        {
            self.pending_volume = None;
        } else {
            playback.volume = Some(u32::from(pending.volume));
        }
    }

    /// Get the current playback
    ///
    /// # Note
    /// Because playback metadata stored inside the player state is buffered,
    /// the returned playback is estimated based on the available data.
    pub fn current_playback(&self) -> Option<rspotify::model::CurrentPlaybackContext> {
        let mut playback = self.playback.clone()?;

        playback.progress = self.playback_progress();

        // update the playback's metadata based on the `buffered_playback` metadata
        if let Some(ref p) = self.buffered_playback {
            playback.device.name.clone_from(&p.device_name);
            playback.device.id.clone_from(&p.device_id);
            playback.is_playing = p.is_playing;
            playback.device.volume_percent = p.volume;
            playback.repeat_state = p.repeat_state;
            playback.shuffle_state = p.shuffle_state;
        }

        Some(playback)
    }

    pub fn currently_playing(&self) -> Option<&rspotify::model::PlayableItem> {
        self.playback.as_ref().and_then(|p| p.item.as_ref())
    }

    pub fn playback_progress(&self) -> Option<chrono::Duration> {
        let playback = self.playback.as_ref()?;
        let progress = playback.progress?;
        let is_playing = self
            .buffered_playback
            .as_ref()
            .map_or(playback.is_playing, |p| p.is_playing);
        let elapsed = if is_playing {
            self.playback_last_updated_time
                .and_then(|t| chrono::Duration::from_std(t.elapsed()).ok())
                .unwrap_or_default()
        } else {
            chrono::Duration::zero()
        };
        Some(progress + elapsed)
    }

    pub fn playing_context_id(&self) -> Option<ContextId> {
        match self.playback {
            Some(ref playback) => match playback.context {
                Some(ref context) => {
                    let uri = crate::utils::parse_uri(&context.uri);
                    match context._type {
                        rspotify::model::Type::Playlist => Some(ContextId::Playlist(
                            PlaylistId::from_uri(&uri).ok()?.into_static(),
                        )),
                        rspotify::model::Type::Album => Some(ContextId::Album(
                            AlbumId::from_uri(&uri).ok()?.into_static(),
                        )),
                        rspotify::model::Type::Artist => Some(ContextId::Artist(
                            ArtistId::from_uri(&uri).ok()?.into_static(),
                        )),
                        rspotify::model::Type::Show => {
                            Some(ContextId::Show(ShowId::from_uri(&uri).ok()?.into_static()))
                        }
                        _ => None,
                    }
                }
                None => self
                    .custom_queue
                    .as_ref()
                    .and_then(|q| q.source_context().cloned())
                    .or_else(|| {
                        self.currently_playing_tracks_id
                            .clone()
                            .map(ContextId::Tracks)
                    }),
            },
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn player(volume: u32) -> PlayerState {
        PlayerState {
            buffered_playback: Some(PlaybackMetadata {
                device_name: "test".into(),
                device_id: Some("laptop".into()),
                volume: Some(volume),
                is_playing: true,
                repeat_state: rspotify::model::RepeatState::Off,
                shuffle_state: false,
                mute_state: None,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn repeated_volume_keys_accumulate_and_clamp() {
        let mut p = player(5);
        assert_eq!(p.change_volume(-10), Some(0));
        assert_eq!(p.change_volume(-1), None);
        assert_eq!(p.change_volume(5), Some(5));
        assert_eq!(p.change_volume(5), Some(10));
        assert_eq!(p.change_volume(i32::MAX), Some(100));
        assert_eq!(p.change_volume(1), None);
        assert_eq!(p.change_volume(i32::MIN), Some(0));
    }

    #[test]
    fn stale_volume_cannot_overwrite_pending_keys() {
        let mut p = player(50);
        p.change_volume(5);
        p.change_volume(5);
        p.buffered_playback.as_mut().unwrap().volume = Some(55);
        p.reconcile_pending_volume();
        assert_eq!(p.buffered_playback.as_ref().unwrap().volume, Some(60));
    }

    #[test]
    fn volume_intent_does_not_cross_devices_or_persist_forever() {
        let mut p = player(50);
        p.change_volume(5);
        p.buffered_playback.as_mut().unwrap().device_id = Some("phone".into());
        p.buffered_playback.as_mut().unwrap().volume = Some(20);
        p.reconcile_pending_volume();
        assert!(p.pending_volume.is_none());
        assert_eq!(p.buffered_playback.as_ref().unwrap().volume, Some(20));
        p.change_volume(5);
        p.pending_volume.as_mut().unwrap().requested_at =
            std::time::Instant::now() - std::time::Duration::from_secs(3);
        p.buffered_playback.as_mut().unwrap().volume = Some(20);
        p.reconcile_pending_volume();
        assert!(p.pending_volume.is_none());
        assert_eq!(p.buffered_playback.as_ref().unwrap().volume, Some(20));
    }

    fn snapshot(progress: serde_json::Value) -> rspotify::model::CurrentPlaybackContext {
        serde_json::from_value(serde_json::json!({
            "device": {"id": "laptop", "is_active": true, "is_private_session": false,
                "is_restricted": false, "name": "test", "type": "Computer", "volume_percent": 50},
            "repeat_state": "off", "shuffle_state": false, "context": null,
            "timestamp": 0, "progress_ms": progress, "is_playing": true,
            "item": null, "currently_playing_type": "track", "actions": {"disallows": {}}
        }))
        .unwrap()
    }

    #[test]
    fn buffered_pause_freezes_progress_even_when_raw_snapshot_says_playing() {
        let mut p = player(50);
        p.playback = Some(snapshot(serde_json::json!(1000)));
        p.playback_last_updated_time =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(10));
        p.buffered_playback.as_mut().unwrap().is_playing = false;
        assert_eq!(
            p.playback_progress(),
            Some(chrono::Duration::milliseconds(1000))
        );
        assert_eq!(
            p.current_playback().unwrap().progress,
            p.playback_progress()
        );
    }

    #[test]
    fn absent_api_progress_or_timestamp_does_not_panic() {
        let mut p = player(50);
        p.playback = Some(snapshot(serde_json::Value::Null));
        assert_eq!(p.playback_progress(), None);
        p.playback = Some(snapshot(serde_json::json!(1000)));
        assert_eq!(
            p.playback_progress(),
            Some(chrono::Duration::milliseconds(1000))
        );
    }
}
