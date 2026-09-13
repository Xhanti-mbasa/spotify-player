use std::time::{Duration, Instant};

use anyhow::Context;
use rspotify::model::Id;
use tracing::Instrument;

use crate::{
    config,
    state::{ContextId, ContextPageType, ContextPageUIState, PageState, PlayableId, SharedState},
};

use crate::utils::map_join;

use super::ClientRequest;

struct PlayerEventHandlerState {
    ended_playable_uri: Option<String>,
    last_get_context: Instant,
    last_playback_refresh: Instant,
    last_queue_refresh: Option<(String, Instant)>,
}

/// Interval between background session-validity checks.
const SESSION_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const EVENT_INTERVAL: Duration = Duration::from_millis(100);
const CONTEXT_REFRESH_THROTTLE: Duration = Duration::from_secs(5);
const QUEUE_REFRESH_THROTTLE: Duration = Duration::from_secs(5);

fn handle_playback_change_event(
    state: &SharedState,
    client_pub: &flume::Sender<ClientRequest>,
    handler_state: &mut PlayerEventHandlerState,
) -> anyhow::Result<()> {
    let player = state.player.read();
    let (playback, id, duration) = match (
        player.buffered_playback.as_ref(),
        player.currently_playing(),
    ) {
        (Some(playback), Some(rspotify::model::PlayableItem::Track(track))) => (
            playback,
            PlayableId::Track(track.id.clone().expect("null track_id")),
            track.duration,
        ),
        (Some(playback), Some(rspotify::model::PlayableItem::Episode(episode))) => (
            playback,
            PlayableId::Episode(episode.id.clone()),
            episode.duration,
        ),
        _ => return Ok(()),
    };
    let playable_uri = id.uri();

    let playback_ended = player
        .playback_progress()
        .is_some_and(|progress| progress >= duration && playback.is_playing);
    if playback_ended && handler_state.ended_playable_uri.as_deref() != Some(&playable_uri) {
        client_pub.send(ClientRequest::GetCurrentPlayback)?;
        handler_state.ended_playable_uri = Some(playable_uri.clone());
    } else if !playback_ended {
        handler_state.ended_playable_uri = None;
    }

    let queue_needs_refresh = player.queue.as_ref().is_none_or(|queue| {
        queue
            .currently_playing
            .as_ref()
            .is_none_or(|queue_item| queue_item.id().expect("null track_id") != id)
    });
    if queue_needs_refresh {
        let should_refresh =
            handler_state
                .last_queue_refresh
                .as_ref()
                .is_none_or(|(last_uri, timer)| {
                    last_uri != &playable_uri || timer.elapsed() >= QUEUE_REFRESH_THROTTLE
                });
        if should_refresh {
            handler_state.last_queue_refresh = Some((playable_uri, Instant::now()));
            client_pub.send(ClientRequest::GetCurrentUserQueue)?;
        }
    } else if !queue_needs_refresh {
        handler_state.last_queue_refresh = None;
    }

    Ok(())
}

fn handle_page_change_event(
    state: &SharedState,
    client_pub: &flume::Sender<ClientRequest>,
    handler_state: &mut PlayerEventHandlerState,
) -> anyhow::Result<()> {
    match state.ui.lock().current_page_mut() {
        PageState::Context {
            id,
            context_page_type,
            state: page_state,
        } => {
            let expected_id = match context_page_type {
                ContextPageType::Browsing(context_id) => Some(context_id.clone()),
                ContextPageType::CurrentPlaying => state.player.read().playing_context_id(),
            };

            let new_id = if *id == expected_id {
                false
            } else {
                // update the context state and request new data when moving to a new context page
                tracing::info!("Current context ID ({:?}) is different from the expected ID ({:?}), update the context state", id, expected_id);

                *id = expected_id;

                // update the UI page state based on the context's type
                match id {
                    Some(id) => {
                        *page_state = Some(match id {
                            ContextId::Album(_) => ContextPageUIState::new_album(),
                            ContextId::Artist(_) => ContextPageUIState::new_artist(),
                            ContextId::Playlist(_) => ContextPageUIState::new_playlist(),
                            ContextId::Tracks(_) => ContextPageUIState::new_tracks(),
                            ContextId::Show(_) => ContextPageUIState::new_show(),
                        });
                    }
                    None => {
                        *page_state = None;
                    }
                }
                true
            };

            // request new context's data if not found in memory
            // To avoid making too many requests, only request if context id is changed
            // or it's been a while since the last request.
            if let Some(id) = id {
                if !matches!(id, ContextId::Tracks(_))
                    && !state.data.read().caches.context.contains_key(&id.uri())
                    && (new_id
                        || handler_state.last_get_context.elapsed() > CONTEXT_REFRESH_THROTTLE)
                {
                    client_pub.send(ClientRequest::GetContext(id.clone()))?;
                    handler_state.last_get_context = Instant::now();
                }
            }
        }

        PageState::Lyrics {
            track_uri,
            track,
            artists,
        } => {
            if let Some(rspotify::model::PlayableItem::Track(current_track)) =
                state.player.read().currently_playing()
            {
                if current_track.name != *track {
                    if let Some(id) = &current_track.id {
                        tracing::info!("Currently playing track \"{}\" is different from the track \"{track}\" shown up in the lyrics page. Fetching new track's lyrics...", current_track.name);
                        track.clone_from(&current_track.name);
                        *artists = map_join(&current_track.artists, |a| &a.name, ", ");
                        *track_uri = id.uri();
                        client_pub.send(ClientRequest::GetLyrics {
                            track_id: id.clone_static(),
                        })?;
                    }
                }
            }
        }
        _ => {}
    }

    Ok(())
}

fn handle_player_event(
    state: &SharedState,
    client_pub: &flume::Sender<ClientRequest>,
    handler_state: &mut PlayerEventHandlerState,
) -> anyhow::Result<()> {
    handle_page_change_event(state, client_pub, handler_state)
        .context("handle page change event")?;
    handle_playback_change_event(state, client_pub, handler_state)
        .context("handle playback change event")?;

    Ok(())
}

/// Runs request dispatch, playback refreshes, and session recovery from one event loop.
pub async fn run(
    state: &SharedState,
    client: &super::AppClient,
    client_pub: &flume::Sender<ClientRequest>,
    client_sub: &flume::Receiver<ClientRequest>,
) {
    let configs = config::get_config();
    let playback_refresh_duration =
        Duration::from_millis(configs.app_config.playback_refresh_duration_in_ms.max(1000));
    let mut handler_state = PlayerEventHandlerState {
        last_get_context: Instant::now(),
        last_playback_refresh: Instant::now(),
        ended_playable_uri: None,
        last_queue_refresh: None,
    };
    let mut events = tokio::time::interval(EVENT_INTERVAL);
    let mut sessions = tokio::time::interval(SESSION_CHECK_INTERVAL);
    events.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    sessions.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut deferred = None;

    loop {
        let request = if deferred.is_some() {
            deferred.take()
        } else {
            tokio::select! {
                request = client_sub.recv_async() => request.ok(),
                _ = sessions.tick() => {
                    if let Err(err) = client.check_valid_session(state).await {
                        tracing::error!("Failed to check/reconnect the client's session: {err:#}");
                    }
                    continue;
                }
                _ = events.tick() => {
                    if configs.app_config.playback_refresh_duration_in_ms > 0
                        && handler_state.last_playback_refresh.elapsed() >= playback_refresh_duration
                    {
                        if client_pub.send(ClientRequest::GetCurrentPlayback).is_err() {
                            return;
                        }
                        handler_state.last_playback_refresh = Instant::now();
                    }
                    if let Err(err) = handle_player_event(state, client_pub, &mut handler_state) {
                        tracing::error!("Failed to handle player event: {err:#}");
                    }
                    continue;
                }
            }
        };

        let Some(request) = request else { return };
        let request = match request {
            ClientRequest::Player(mut request) => {
                if matches!(&request, super::PlayerRequest::Volume(_)) {
                    while let Ok(next) = client_sub.try_recv() {
                        match next {
                            ClientRequest::Player(next @ super::PlayerRequest::Volume(_)) => {
                                request = next
                            }
                            next => {
                                deferred = Some(next);
                                break;
                            }
                        }
                    }
                }
                if let Err(err) = client.handle_state_player_request(state, request).await {
                    tracing::error!("Failed to handle player request: {err:#}");
                }
                continue;
            }
            request => request,
        };

        let state = state.clone();
        let client = client.clone();
        let span = tracing::info_span!("client_request", request = ?request);
        tokio::spawn(
            async move {
                if let Err(err) = client.handle_request(&state, request).await {
                    tracing::error!("Failed to handle client request: {err:#}");
                }
            }
            .instrument(span),
        );
    }
}
