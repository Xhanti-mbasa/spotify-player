use super::*;
use wiremock::{
    matchers::{method, path, query_param},
    Mock, MockServer, ResponseTemplate,
};

async fn client(server: &MockServer) -> AppClient {
    let api = rspotify::AuthCodePkceSpotify::with_config(
        rspotify::Credentials::new_pkce("test-client"),
        rspotify::OAuth::default(),
        rspotify::Config {
            api_base_url: format!("{}/v1", server.uri()),
            ..Default::default()
        },
    );
    *api.get_token().lock().await.unwrap() = Some(rspotify::Token {
        access_token: "test-token".into(),
        expires_in: chrono::Duration::hours(1),
        expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
        scopes: HashSet::new(),
        refresh_token: None,
    });
    AppClient {
        http: reqwest::Client::new(),
        playback_control: Arc::new(tokio::sync::Mutex::new(())),
        last_player_command: Arc::new(parking_lot::Mutex::new(None)),
        playback_refresh_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        playback_poll: Arc::new(tokio::sync::Mutex::new(())),
        playback_refresh_timer: Arc::new(parking_lot::Mutex::new(None)),
        spotify: Arc::new(spotify::Spotify::new()),
        auth_config: AuthConfig::default(),
        api_client: WebApiClient::new(api, None),
        #[cfg(feature = "streaming")]
        stream_conn: Arc::new(Mutex::new(None)),
        #[cfg(feature = "streaming")]
        user_requested_playback: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    }
}

fn metadata(is_playing: bool) -> PlaybackMetadata {
    PlaybackMetadata {
        device_name: "Laptop".into(),
        device_id: Some("laptop".into()),
        volume: Some(50),
        is_playing,
        repeat_state: rspotify::model::RepeatState::Off,
        shuffle_state: false,
        mute_state: None,
    }
}

#[tokio::test]
async fn explicit_resume_and_pause_are_not_skipped_by_cached_state() {
    let server = MockServer::start().await;
    for endpoint in ["play", "pause"] {
        Mock::given(method("PUT"))
            .and(path(format!("/v1/me/player/{endpoint}")))
            .and(query_param("device_id", "laptop"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
    }
    let client = client(&server).await;
    let resumed = client
        .handle_player_request(PlayerRequest::Resume, Some(metadata(true)))
        .await
        .unwrap()
        .unwrap();
    assert!(resumed.is_playing);
    let paused = client
        .handle_player_request(PlayerRequest::Pause, Some(metadata(false)))
        .await
        .unwrap()
        .unwrap();
    assert!(!paused.is_playing);
}

#[tokio::test]
async fn repeated_toggles_alternate_requested_state() {
    let server = MockServer::start().await;
    for endpoint in ["pause", "play"] {
        Mock::given(method("PUT"))
            .and(path(format!("/v1/me/player/{endpoint}")))
            .respond_with(ResponseTemplate::new(204))
            .expect(2)
            .mount(&server)
            .await;
    }
    let client = client(&server).await;
    let mut playback = Some(metadata(true));
    for expected in [false, true, false, true] {
        playback = client
            .handle_player_request(PlayerRequest::ResumePause, playback)
            .await
            .unwrap();
        assert_eq!(playback.as_ref().unwrap().is_playing, expected);
    }
}

#[tokio::test]
async fn selected_track_without_cached_playback_targets_discovered_device() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/me/player/devices"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"devices": [{
                "id": "laptop", "is_active": true, "is_private_session": false,
                "is_restricted": false, "name": "Laptop", "type": "Computer",
                "volume_percent": 50, "supports_volume": true
            }]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    for endpoint in ["play", "shuffle"] {
        Mock::given(method("PUT"))
            .and(path(format!("/v1/me/player/{endpoint}")))
            .and(query_param("device_id", "laptop"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
    }
    let client = client(&server).await;
    let id = TrackId::from_id("6t7WriKgVszATnrdBKSUAf")
        .unwrap()
        .into_static();
    let playback = client
        .handle_player_request(
            PlayerRequest::StartPlayback(Playback::URIs(vec![id.into()], None), None),
            None,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(playback.device_id.as_deref(), Some("laptop"));
    assert!(playback.is_playing);
}

#[tokio::test]
async fn command_failure_is_returned_and_next_command_can_succeed() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/v1/me/player/pause"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/v1/me/player/play"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let client = client(&server).await;
    assert!(client
        .handle_player_request(PlayerRequest::Pause, Some(metadata(true)))
        .await
        .is_err());
    assert!(
        client
            .handle_player_request(PlayerRequest::Resume, Some(metadata(false)))
            .await
            .unwrap()
            .unwrap()
            .is_playing
    );
}
