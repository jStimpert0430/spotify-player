use crate::config;
use crate::event;
use crate::prelude::*;
use crate::state;

/// A spotify client
pub struct Client {
    spotify: Spotify,
    http: reqwest::Client,
    oauth: SpotifyOAuth,
}

impl Client {
    /// returns the new `Client`
    pub fn new(oauth: SpotifyOAuth) -> Self {
        Self {
            spotify: Spotify::default(),
            http: reqwest::Client::new(),
            oauth,
        }
    }

    /// handles a client event
    pub async fn handle_event(
        &mut self,
        state: &state::SharedState,
        event: event::Event,
    ) -> Result<()> {
        log::info!("handle event: {:?}", event);
        match event {
            event::Event::RefreshToken => {
                state.write().unwrap().auth_token_expires_at = self.refresh_token().await?;
            }
            event::Event::NextTrack => {
                self.next_track().await?;
            }
            event::Event::PreviousTrack => {
                self.previous_track().await?;
            }
            event::Event::ResumePause => {
                let state = state.read().unwrap();
                self.toggle_playing_state(&state).await?;
            }
            event::Event::Shuffle => {
                let state = state.read().unwrap();
                self.toggle_shuffle(&state).await?;
            }
            event::Event::Repeat => {
                let state = state.read().unwrap();
                self.cycle_repeat(&state).await?;
            }
            event::Event::Quit => {
                state.write().unwrap().is_running = false;
            }
            event::Event::GetPlaylist(playlist_id) => {
                if let Some(ref playlist) = state.read().unwrap().current_playlist {
                    // avoid getting the same playlist more than once
                    if playlist.id == playlist_id {
                        return Ok(());
                    }
                }
                // get the playlist
                let playlist = self.get_playlist(&playlist_id).await?;
                state.write().unwrap().current_playlist = Some(playlist);
                // get the playlist's track
                let tracks = self
                    .get_current_playlist_tracks(&state.read().unwrap())
                    .await?;
                // filter tracks that are either unaccessible or deleted from album
                let tracks: Vec<_> = tracks.into_iter().filter(|t| t.track.is_some()).collect();
                // update the state (UI) of the `playlist_tracks_widget`
                if !tracks.is_empty() {
                    state
                        .write()
                        .unwrap()
                        .ui_context_tracks_table_state
                        .select(Some(0));
                }
                state.write().unwrap().current_playlist_tracks = tracks;
            }
            event::Event::SelectNextTrack => {
                let mut state = state.write().unwrap();
                if let Some(id) = state.ui_context_tracks_table_state.selected() {
                    if id + 1 < state.get_context_filtered_tracks().len() {
                        state.ui_context_tracks_table_state.select(Some(id + 1));
                    }
                }
            }
            event::Event::SelectPreviousTrack => {
                let mut state = state.write().unwrap();
                if let Some(id) = state.ui_context_tracks_table_state.selected() {
                    if id > 0 {
                        state.ui_context_tracks_table_state.select(Some(id - 1));
                    }
                }
            }
            event::Event::PlaySelectedTrack => {
                let state = state.read().unwrap();
                if let (Some(id), Some(playback)) = (
                    state.ui_context_tracks_table_state.selected(),
                    state.current_playback_context.as_ref(),
                ) {
                    if let Some(ref context) = playback.context {
                        self.play_track_with_context(
                            context.uri.clone(),
                            state.get_context_filtered_tracks()[id].uri.clone(),
                        )
                        .await?;
                    }
                }
            }
            event::Event::SearchTrackInContext => {
                let mut state = state.write().unwrap();
                if let Some(ref query) = state.context_search_state.query {
                    let mut query = query.clone();
                    query.remove(0);

                    log::info!("search tracks in context with query {}", query);
                    state.context_search_state.tracks = state
                        .get_context_tracks()
                        .into_iter()
                        .filter(|&t| {
                            let desc = state::get_track_description(t).to_string();
                            desc.to_lowercase().contains(&query)
                        })
                        .cloned()
                        .collect();

                    // update ui selection
                    let id = if state.context_search_state.tracks.is_empty() {
                        None
                    } else {
                        Some(0)
                    };
                    state.ui_context_tracks_table_state.select(id);
                    log::info!(
                        "after search, context_search_state.tracks = {:?}",
                        state.context_search_state.tracks
                    );
                }
            }
            event::Event::SortPlaylistTracks(order) => {
                state.write().unwrap().sort_playlist_tracks(order);
            }
        }
        Ok(())
    }

    /// refreshes the client's authentication token, returns
    /// the token's `expires_at` time.
    pub async fn refresh_token(&mut self) -> Result<std::time::SystemTime> {
        let token = match get_token(&mut self.oauth).await {
            Some(token) => token,
            None => return Err(anyhow!("auth failed")),
        };

        let expires_at = token
            .expires_at
            .expect("got `None` for token's `expires_at`");
        self.spotify = Self::get_spotify_client(token);
        Ok(
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(expires_at as u64)
                - std::time::Duration::from_secs(10),
        )
    }

    // client functions

    /// starts a track given a playback context
    pub async fn play_track_with_context(
        &self,
        context_uri: String,
        track_uri: String,
    ) -> Result<()> {
        Self::handle_rspotify_result(
            self.spotify
                .start_playback(
                    None,
                    Some(context_uri),
                    None,
                    offset::for_uri(track_uri),
                    None,
                )
                .await,
        )
    }

    /// returns a list of tracks in the current playlist
    pub async fn get_current_playlist_tracks(
        &self,
        state: &RwLockReadGuard<'_, state::State>,
    ) -> Result<Vec<playlist::PlaylistTrack>> {
        let mut tracks: Vec<playlist::PlaylistTrack> = vec![];
        if let Some(ref playlist) = state.current_playlist {
            tracks = playlist.tracks.items.clone();
            let mut next = playlist.tracks.next.clone();
            while let Some(url) = next {
                let mut paged_tracks = self
                    .internal_call::<page::Page<playlist::PlaylistTrack>>(&url)
                    .await?;
                tracks.append(&mut paged_tracks.items);
                next = paged_tracks.next;
            }
        }
        Ok(tracks)
    }

    /// Returns a playlist given its id
    pub async fn get_playlist(&self, playlist_id: &str) -> Result<playlist::FullPlaylist> {
        Self::handle_rspotify_result(self.spotify.playlist(playlist_id, None, None).await)
    }

    /// cycles through the repeat state of the current playback
    pub async fn cycle_repeat(&self, state: &RwLockReadGuard<'_, state::State>) -> Result<()> {
        let state = Self::get_current_playback_state(&state)?;
        let next_repeat_state = match state.repeat_state {
            RepeatState::Off => RepeatState::Track,
            RepeatState::Track => RepeatState::Context,
            RepeatState::Context => RepeatState::Off,
        };
        Self::handle_rspotify_result(self.spotify.repeat(next_repeat_state, None).await)
    }

    /// toggles the shuffle state of the current playback
    pub async fn toggle_shuffle(&self, state: &RwLockReadGuard<'_, state::State>) -> Result<()> {
        let state = Self::get_current_playback_state(&state)?;
        Self::handle_rspotify_result(self.spotify.shuffle(!state.shuffle_state, None).await)
    }

    /// toggles the current playing state (pause/resume a track)
    pub async fn toggle_playing_state(
        &self,
        state: &RwLockReadGuard<'_, state::State>,
    ) -> Result<()> {
        let state = Self::get_current_playback_state(&state)?;
        if state.is_playing {
            self.pause_track().await
        } else {
            self.resume_track().await
        }
    }

    /// resumes a previously paused/played track
    pub async fn resume_track(&self) -> Result<()> {
        Self::handle_rspotify_result(
            self.spotify
                .start_playback(None, None, None, None, None)
                .await,
        )
    }

    /// pauses currently playing track
    pub async fn pause_track(&self) -> Result<()> {
        Self::handle_rspotify_result(self.spotify.pause_playback(None).await)
    }

    /// skips to the next track
    pub async fn next_track(&self) -> Result<()> {
        Self::handle_rspotify_result(self.spotify.next_track(None).await)
    }

    /// skips to the previous track
    pub async fn previous_track(&self) -> Result<()> {
        Self::handle_rspotify_result(self.spotify.previous_track(None).await)
    }

    /// returns the current playing context
    pub async fn get_current_playback(&self) -> Result<Option<context::CurrentlyPlaybackContext>> {
        Self::handle_rspotify_result(self.spotify.current_playback(None, None).await)
    }

    // helper functions

    async fn get_auth_token(&self) -> String {
        format!(
            "Bearer {}",
            self.spotify
                .client_credentials_manager
                .as_ref()
                .expect("client credentials manager is `None`")
                .get_access_token()
                .await
        )
    }

    async fn internal_call<T>(&self, url: &str) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        Ok(self
            .http
            .get(url)
            .header(reqwest::header::AUTHORIZATION, self.get_auth_token().await)
            .send()
            .await?
            .json::<T>()
            .await?)
    }

    /// builds a spotify client from an authentication token
    fn get_spotify_client(token: TokenInfo) -> Spotify {
        let client_credential = SpotifyClientCredentials::default()
            .token_info(token)
            .build();
        Spotify::default()
            .client_credentials_manager(client_credential)
            .build()
    }

    /// converts a `rspotify` result format into `anyhow` compatible result format
    fn handle_rspotify_result<T, E: fmt::Display>(result: std::result::Result<T, E>) -> Result<T> {
        match result {
            Ok(data) => Ok(data),
            Err(err) => Err(anyhow!(format!("{}", err))),
        }
    }

    /// gets the current playing state from the application state
    fn get_current_playback_state<'a>(
        state: &'a RwLockReadGuard<'a, state::State>,
    ) -> Result<&'a context::CurrentlyPlaybackContext> {
        match state.current_playback_context {
            Some(ref state) => Ok(state),
            None => Err(anyhow!("unable to get the currently playing context")),
        }
    }
}

/// starts the client's event watcher
pub async fn start_watcher(
    state: state::SharedState,
    mut client: Client,
    recv: mpsc::Receiver<event::Event>,
) -> Result<()> {
    state.write().unwrap().auth_token_expires_at = client.refresh_token().await?;
    state.write().unwrap().current_playback_context = client.get_current_playback().await?;
    let mut last_refresh = std::time::SystemTime::now();
    loop {
        if let Ok(event) = recv.try_recv() {
            client.handle_event(&state, event).await?;
        }
        if std::time::SystemTime::now() > last_refresh + config::PLAYBACK_REFRESH_DURACTION {
            // `config::REFRESH_DURATION` passes since the last refresh, get the
            // current playback context again
            log::info!("refresh the current playback context...");
            state.write().unwrap().current_playback_context = client.get_current_playback().await?;
            last_refresh = std::time::SystemTime::now()
        }
    }
}