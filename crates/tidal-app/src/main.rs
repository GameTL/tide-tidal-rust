use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use slint::{ComponentHandle, ModelRc, VecModel};
use tidal_api::{TidalService, TrackItem};
use tidal_audio::{AudioPlayer, TrackQueueItem};
use tidal_cache::{AudioCache, MetadataCache};
use tidal_macos::{MacMediaControls, MediaControlEvent};
use tokio::sync::Mutex as TokioMutex;

slint::include_modules!();

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        return run_cli(args).await;
    }

    tracing::info!("Starting Tidal Rust Desktop...");

    let main_window = MainWindow::new()?;
    let window_handle = main_window.as_weak();

    // Initialize Services
    let tidal_service = Arc::new(TokioMutex::new(TidalService::with_device_auth()?));
    let metadata_cache = Arc::new(MetadataCache::new()?);
    let audio_cache = Arc::new(AudioCache::new()?);
    let audio_player = Arc::new(AudioPlayer::new()?);

    // Current playlist / active track tracking
    let current_tracks = Arc::new(std::sync::Mutex::new(Vec::<TrackItem>::new()));
    let current_playing_track = Arc::new(std::sync::Mutex::new(Option::<TrackItem>::None));

    // Setup macOS Media Keys & Now Playing Integration
    let player_media = Arc::clone(&audio_player);
    let _media_controls = {
        let p = Arc::clone(&player_media);
        MacMediaControls::new("TIDAL", move |event| {
            match event {
                MediaControlEvent::Play => p.resume(),
                MediaControlEvent::Pause => p.pause(),
                MediaControlEvent::Toggle => {
                    if p.is_playing() {
                        p.pause();
                    } else {
                        p.resume();
                    }
                }
                _ => {}
            }
        }).ok()
    };
    let media_controls = Arc::new(std::sync::Mutex::new(_media_controls));

    // Check if user already has saved session
    {
        let mut svc = tidal_service.lock().await;
        if let Ok(true) = svc.load_session().await {
            tracing::info!("Restored existing Tidal session!");
            let username = "Tidal User".to_string();
            let wh = window_handle.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = wh.upgrade() {
                    ui.set_is_logged_in(true);
                    ui.set_username(username.into());
                }
            });

            // Load favorites from local SQLite cache first (0ms load!)
            let cached_favs = metadata_cache.get_favorites().unwrap_or_default();
            if !cached_favs.is_empty() {
                let mut guard = current_tracks.lock().unwrap();
                *guard = cached_favs.clone();
                let slint_tracks: Vec<TrackData> = cached_favs
                    .into_iter()
                    .map(|t| {
                        let is_cached = audio_cache.get_cached(&t.id, "HI_RES").is_some();
                        TrackData {
                            id: t.id.into(),
                            title: t.title.into(),
                            artist: t.artist.into(),
                            album: t.album.into(),
                            duration: format!("{}:{:02}", t.duration_secs / 60, t.duration_secs % 60).into(),
                            quality: t.audio_quality.into(),
                            is_cached,
                        }
                    })
                    .collect();

                let wh = window_handle.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = wh.upgrade() {
                        let model = Rc::new(VecModel::from(slint_tracks));
                        ui.set_tracks(ModelRc::from(model));
                    }
                });
            }

            // Sync favorites from Tidal API in background
            let svc_clone = Arc::clone(&tidal_service);
            let meta_clone = Arc::clone(&metadata_cache);
            let tracks_clone = Arc::clone(&current_tracks);
            let audio_cache_clone = Arc::clone(&audio_cache);
            let wh = window_handle.clone();

            tokio::spawn(async move {
                let mut svc = svc_clone.lock().await;
                if let Ok(online_tracks) = svc.get_favorite_tracks(Some(50)).await {
                    let _ = meta_clone.save_favorites(&online_tracks);
                    let mut guard = tracks_clone.lock().unwrap();
                    *guard = online_tracks.clone();

                    let slint_tracks: Vec<TrackData> = online_tracks
                        .into_iter()
                        .map(|t| {
                            let is_cached = audio_cache_clone.get_cached(&t.id, "HI_RES").is_some();
                            TrackData {
                                id: t.id.into(),
                                title: t.title.into(),
                                artist: t.artist.into(),
                                album: t.album.into(),
                                duration: format!("{}:{:02}", t.duration_secs / 60, t.duration_secs % 60).into(),
                                quality: t.audio_quality.into(),
                                is_cached,
                            }
                        })
                        .collect();

                    let wh = wh.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = wh.upgrade() {
                            let model = Rc::new(VecModel::from(slint_tracks));
                            ui.set_tracks(ModelRc::from(model));
                        }
                    });
                }
            });
        }
    }

    // Callback: Start Device Login
    {
        let svc_clone = Arc::clone(&tidal_service);
        let wh = window_handle.clone();
        main_window.on_start_device_login(move || {
            let svc = Arc::clone(&svc_clone);
            let wh = wh.clone();
            tokio::spawn(async move {
                let mut service = svc.lock().await;
                match service.start_device_auth().await {
                    Ok(auth_info) => {
                        let user_code = auth_info.user_code.clone();
                        let uri = auth_info.verification_uri_complete.clone();
                        let _ = slint::invoke_from_event_loop({
                            let wh = wh.clone();
                            let uc = user_code.clone();
                            let u = uri.clone();
                            move || {
                                if let Some(ui) = wh.upgrade() {
                                    ui.set_user_code(uc.into());
                                    ui.set_verification_url(u.into());
                                    ui.set_auth_status("Waiting for approval in browser...".into());
                                }
                            }
                        });

                        // Automatically open browser for convenience
                        let _ = open::that(&uri);

                        // Poll in background
                        match service.poll_device_auth(&auth_info.device_code, auth_info.expires_in, auth_info.interval).await {
                            Ok(user) => {
                                let _ = slint::invoke_from_event_loop({
                                    let wh = wh.clone();
                                    move || {
                                        if let Some(ui) = wh.upgrade() {
                                            ui.set_is_logged_in(true);
                                            ui.set_username(user.into());
                                            ui.set_auth_status("Successfully logged in!".into());
                                        }
                                    }
                                });
                            }
                            Err(e) => {
                                let _ = slint::invoke_from_event_loop({
                                    let wh = wh.clone();
                                    move || {
                                        if let Some(ui) = wh.upgrade() {
                                            ui.set_auth_status(format!("Auth failed: {e}").into());
                                        }
                                    }
                                });
                            }
                        }
                    }
                    Err(e) => {
                        let _ = slint::invoke_from_event_loop({
                            let wh = wh.clone();
                            move || {
                                if let Some(ui) = wh.upgrade() {
                                    ui.set_auth_status(format!("Failed to start auth: {e}").into());
                                }
                            }
                        });
                    }
                }
            });
        });
    }

    // Callback: Open Browser Login
    {
        let wh = window_handle.clone();
        main_window.on_open_browser_login(move || {
            if let Some(ui) = wh.upgrade() {
                let url = ui.get_verification_url().to_string();
                if !url.is_empty() {
                    let _ = open::that(&url);
                }
            }
        });
    }

    // Callback: Play Track
    {
        let svc_clone = Arc::clone(&tidal_service);
        let audio_cache_clone = Arc::clone(&audio_cache);
        let audio_player_clone = Arc::clone(&audio_player);
        let meta_cache_clone = Arc::clone(&metadata_cache);
        let media_controls_clone = Arc::clone(&media_controls);
        let current_playing_clone = Arc::clone(&current_playing_track);
        let tracks_clone = Arc::clone(&current_tracks);
        let wh = window_handle.clone();

        main_window.on_play_track(move |track_id_slint| {
            let track_id = track_id_slint.to_string();
            let svc = Arc::clone(&svc_clone);
            let cache = Arc::clone(&audio_cache_clone);
            let player = Arc::clone(&audio_player_clone);
            let meta = Arc::clone(&meta_cache_clone);
            let media = Arc::clone(&media_controls_clone);
            let playing_ref = Arc::clone(&current_playing_clone);
            let tracks_ref = Arc::clone(&tracks_clone);
            let wh = wh.clone();

            tokio::spawn(async move {
                // Find track info in local cache or state
                let track_item = {
                    let guard = tracks_ref.lock().unwrap();
                    guard.iter().find(|t| t.id == track_id).cloned()
                };

                let track = match track_item {
                    Some(t) => t,
                    None => {
                        let mut s = svc.lock().await;
                        match s.get_track(&track_id).await {
                            Ok(t) => t,
                            Err(e) => {
                                tracing::error!("Could not get track info: {e}");
                                return;
                            }
                        }
                    }
                };

                // Record history
                let _ = meta.record_play(&track.id);

                // Check cache first; if missing, resolve stream URL from Tidal API and cache transparently
                let audio_file = if let Some(local_path) = cache.get_cached(&track.id, "HI_RES") {
                    local_path
                } else {
                    let mut s = svc.lock().await;
                    match s.get_stream_urls(&track.id).await {
                        Ok(stream) => {
                            match cache.fetch_or_cache(&track.id, "HI_RES", &stream.stream_urls).await {
                                Ok(p) => p,
                                Err(e) => {
                                    tracing::error!("Failed to cache audio stream: {e}");
                                    return;
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!("Failed to get stream URLs: {e}");
                            return;
                        }
                    }
                };

                let is_cached = cache.get_cached(&track.id, "HI_RES").is_some();

                // Start playback in native CoreAudio engine
                let queue_item = TrackQueueItem {
                    id: track.id.clone(),
                    title: track.title.clone(),
                    artist: track.artist.clone(),
                    file_path: audio_file,
                    duration: Duration::from_secs(track.duration_secs as u64),
                };

                if let Err(e) = player.play_track(queue_item) {
                    tracing::error!("Playback error: {e}");
                    return;
                }

                // Update Now Playing in macOS Control Center
                if let Ok(mut m_guard) = media.lock() {
                    if let Some(ref mut m) = *m_guard {
                        let _ = m.set_metadata(
                            &track.title,
                            &track.artist,
                            &track.album,
                            Some(Duration::from_secs(track.duration_secs as u64)),
                            track.cover_url.as_deref(),
                        );
                        let _ = m.set_playback_state(true, Some(Duration::ZERO));
                    }
                }

                // Update UI state
                {
                    let mut cur = playing_ref.lock().unwrap();
                    *cur = Some(track.clone());
                }

                let _ = slint::invoke_from_event_loop({
                    let wh = wh.clone();
                    let t = track.title.clone();
                    let a = track.artist.clone();
                    let alb = track.album.clone();
                    let dur_str = format!("{}:{:02}", track.duration_secs / 60, track.duration_secs % 60);
                    let q = track.audio_quality.clone();
                    move || {
                        if let Some(ui) = wh.upgrade() {
                            ui.set_now_playing_title(t.into());
                            ui.set_now_playing_artist(a.into());
                            ui.set_now_playing_album(alb.into());
                            ui.set_now_playing_total(dur_str.into());
                            ui.set_now_playing_quality(q.into());
                            ui.set_is_playing(true);
                            ui.set_is_current_cached(is_cached);
                        }
                    }
                });
            });
        });
    }

    // Callback: Toggle Play / Pause
    {
        let player = Arc::clone(&audio_player);
        let media = Arc::clone(&media_controls);
        let wh = window_handle.clone();
        main_window.on_toggle_play(move || {
            if player.is_playing() {
                player.pause();
                if let Ok(mut m_guard) = media.lock() {
                    if let Some(ref mut m) = *m_guard {
                        let _ = m.set_playback_state(false, Some(player.elapsed()));
                    }
                }
                if let Some(ui) = wh.upgrade() {
                    ui.set_is_playing(false);
                }
            } else {
                player.resume();
                if let Ok(mut m_guard) = media.lock() {
                    if let Some(ref mut m) = *m_guard {
                        let _ = m.set_playback_state(true, Some(player.elapsed()));
                    }
                }
                if let Some(ui) = wh.upgrade() {
                    ui.set_is_playing(true);
                }
            }
        });
    }

    // Callback: Volume Changed
    {
        let player = Arc::clone(&audio_player);
        main_window.on_volume_changed(move |val| {
            player.set_volume(val);
        });
    }

    // Callback: Search Submitted
    {
        let svc_clone = Arc::clone(&tidal_service);
        let tracks_clone = Arc::clone(&current_tracks);
        let audio_cache_clone = Arc::clone(&audio_cache);
        let wh = window_handle.clone();

        main_window.on_search_submitted(move |query_slint| {
            let query = query_slint.to_string();
            let svc = Arc::clone(&svc_clone);
            let tracks_ref = Arc::clone(&tracks_clone);
            let cache = Arc::clone(&audio_cache_clone);
            let wh = wh.clone();

            tokio::spawn(async move {
                let mut s = svc.lock().await;
                if let Ok(results) = s.search_tracks(&query).await {
                    let mut guard = tracks_ref.lock().unwrap();
                    *guard = results.clone();

                    let slint_tracks: Vec<TrackData> = results
                        .into_iter()
                        .map(|t| {
                            let is_cached = cache.get_cached(&t.id, "HI_RES").is_some();
                            TrackData {
                                id: t.id.into(),
                                title: t.title.into(),
                                artist: t.artist.into(),
                                album: t.album.into(),
                                duration: format!("{}:{:02}", t.duration_secs / 60, t.duration_secs % 60).into(),
                                quality: t.audio_quality.into(),
                                is_cached,
                            }
                        })
                        .collect();

                    let wh = wh.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = wh.upgrade() {
                            let model = Rc::new(VecModel::from(slint_tracks));
                            ui.set_current_nav("search".into());
                            ui.set_tracks(ModelRc::from(model));
                        }
                    });
                }
            });
        });
    }

    // Callback: Switch Tab
    {
        let meta_clone = Arc::clone(&metadata_cache);
        let tracks_clone = Arc::clone(&current_tracks);
        let audio_cache_clone = Arc::clone(&audio_cache);
        let wh = window_handle.clone();

        main_window.on_switch_tab(move |tab_slint| {
            let tab = tab_slint.to_string();
            let tracks: Vec<TrackItem> = if tab == "history" {
                meta_clone.get_recently_played(50).unwrap_or_default()
            } else {
                meta_clone.get_favorites().unwrap_or_default()
            };

            let mut guard = tracks_clone.lock().unwrap();
            *guard = tracks.clone();

            let slint_tracks: Vec<TrackData> = tracks
                .into_iter()
                .map(|t| {
                    let is_cached = audio_cache_clone.get_cached(&t.id, "HI_RES").is_some();
                    TrackData {
                        id: t.id.into(),
                        title: t.title.into(),
                        artist: t.artist.into(),
                        album: t.album.into(),
                        duration: format!("{}:{:02}", t.duration_secs / 60, t.duration_secs % 60).into(),
                        quality: t.audio_quality.into(),
                        is_cached,
                    }
                })
                .collect();

            let model = Rc::new(VecModel::from(slint_tracks));
            if let Some(ui) = wh.upgrade() {
                ui.set_current_nav(tab.into());
                ui.set_tracks(ModelRc::from(model));
            }
        });
    }

    // Callback: Logout
    {
        let svc_clone = Arc::clone(&tidal_service);
        let wh = window_handle.clone();
        main_window.on_logout(move || {
            let svc = Arc::clone(&svc_clone);
            let wh = wh.clone();
            tokio::spawn(async move {
                let mut s = svc.lock().await;
                let _ = s.logout();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = wh.upgrade() {
                        ui.set_is_logged_in(false);
                        ui.set_user_code("".into());
                        ui.set_username("Not logged in".into());
                    }
                });
            });
        });
    }

    // Ticker: periodic progress update (100ms)
    {
        let player = Arc::clone(&audio_player);
        let playing_ref = Arc::clone(&current_playing_track);
        let wh = window_handle.clone();

        let timer = slint::Timer::default();
        timer.start(slint::TimerMode::Repeated, Duration::from_millis(200), move || {
            if let Some(ui) = wh.upgrade() {
                if player.is_playing() {
                    let elapsed = player.elapsed();
                    let secs = elapsed.as_secs();
                    let elapsed_str = format!("{}:{:02}", secs / 60, secs % 60);

                    let total_secs = {
                        let guard = playing_ref.lock().unwrap();
                        guard.as_ref().map(|t| t.duration_secs as u64).unwrap_or(1)
                    };

                    let progress = (secs as f32 / total_secs.max(1) as f32).clamp(0.0, 1.0);
                    ui.set_now_playing_elapsed(elapsed_str.into());
                    ui.set_now_playing_progress(progress);
                }
            }
        });

        // Run UI Event loop
        main_window.run()?;
    }

    Ok(())
}

async fn run_cli(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    let cmd = args[1].as_str();
    match cmd {
        "--help" | "-h" => {
            println!("TIDAL Rust Desktop Player (macOS)");
            println!("Usage:");
            println!("  tidal-app                  Launch the 1:1 Tidal desktop GUI");
            println!("  tidal-app --login          Start OAuth2 Device Code login (tidal.com/device)");
            println!("  tidal-app --login-pkce     Start OAuth2 PKCE login flow");
            println!("  tidal-app --status         Show current login status and cache stats");
            println!("  tidal-app --search <query> Search Tidal catalog");
            println!("  tidal-app --play <id>      Play track in MAX Hi-Res quality");
            println!("  tidal-app --logout         Log out and clear session tokens");
        }
        "--status" => {
            let session_path = tidal_api::default_session_path()?;
            let db_path = tidal_cache::default_db_path()?;
            let audio_dir = tidal_cache::default_audio_cache_dir()?;

            println!("=== Tidal Rust Status ===");
            println!("Session path:    {}", session_path.display());
            println!("Database path:   {}", db_path.display());
            println!("Audio cache dir: {}", audio_dir.display());

            let mut svc = TidalService::with_device_auth()?;
            if svc.load_session().await.unwrap_or(false) {
                println!("Auth status:     LOGGED IN");
            } else {
                println!("Auth status:     NOT LOGGED IN (run `tidal-app --login` to authenticate)");
            }

            let meta = MetadataCache::new()?;
            let favs = meta.get_favorites().unwrap_or_default();
            println!("Cached favorites: {} tracks", favs.len());

            let history = meta.get_recently_played(10).unwrap_or_default();
            println!("Recent plays:     {} tracks", history.len());

            let _audio_cache = AudioCache::new()?;
            let mut audio_count = 0;
            let mut total_bytes = 0u64;
            if let Ok(entries) = std::fs::read_dir(audio_dir) {
                for entry in entries.flatten() {
                    if let Ok(meta) = entry.metadata() {
                        if meta.is_file() {
                            audio_count += 1;
                            total_bytes += meta.len();
                        }
                    }
                }
            }
            println!("Audio on disk:    {} tracks ({:.2} MB)", audio_count, total_bytes as f64 / (1024.0 * 1024.0));
        }
        "--login" | "--login-device" => {
            println!("Starting Tidal Device Code Authorization...");
            let mut svc = TidalService::with_device_auth()?;
            let info = svc.start_device_auth().await?;
            println!("\n========================================================");
            println!("  User Code:        {}", info.user_code);
            println!("  Verification URL: {}", info.verification_uri_complete);
            println!("========================================================\n");
            println!("Opening browser for authorization...");
            let _ = open::that(&info.verification_uri_complete);
            println!("Waiting for you to authorize in browser...");
            match svc.poll_device_auth(&info.device_code, info.expires_in, info.interval).await {
                Ok(user) => {
                    println!("\n Successfully logged in as '{}'!", user);
                    println!("Session saved to {}", tidal_api::default_session_path()?.display());
                }
                Err(e) => {
                    eprintln!("Authorization failed: {e}");
                }
            }
        }
        "--login-pkce" => {
            println!("Starting Tidal PKCE Authorization...");
            let mut svc = TidalService::new()?;
            let login_url = svc.start_pkce_login()?;
            println!("\nVisit the following URL to log in:");
            println!("{}\n", login_url);
            let _ = open::that(&login_url);
            println!("After logging in, paste the full redirected URL from your browser address bar:");
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            let redirect_url = input.trim();
            match svc.finish_pkce_login(redirect_url).await {
                Ok(user) => {
                    println!("\n Successfully logged in as '{}' via PKCE!", user);
                    println!("Session saved to {}", tidal_api::default_session_path()?.display());
                }
                Err(e) => {
                    eprintln!("PKCE Login failed: {e}");
                }
            }
        }
        "--logout" => {
            let mut svc = TidalService::with_device_auth()?;
            svc.logout()?;
            println!("Logged out and cleared stored session.");
        }
        "--search" => {
            if args.len() < 3 {
                eprintln!("Usage: tidal-app --search <query>");
                return Ok(());
            }
            let query = args[2..].join(" ");
            let mut svc = TidalService::with_device_auth()?;
            if !svc.load_session().await.unwrap_or(false) {
                eprintln!("You are not logged in. Run `tidal-app --login` first.");
                return Ok(());
            }
            println!("Searching Tidal for: '{}'...", query);
            let results = svc.search_tracks(&query).await?;
            if results.is_empty() {
                println!("No tracks found.");
            } else {
                println!("\n{:<12} {:<32} {:<24} {:<10} {:<6}", "ID", "TITLE", "ARTIST", "QUALITY", "TIME");
                println!("{}", "-".repeat(90));
                for t in results {
                    let dur = format!("{}:{:02}", t.duration_secs / 60, t.duration_secs % 60);
                    let title = if t.title.len() > 30 { format!("{}...", &t.title[..27]) } else { t.title };
                    let artist = if t.artist.len() > 22 { format!("{}...", &t.artist[..19]) } else { t.artist };
                    println!("{:<12} {:<32} {:<24} {:<10} {:<6}", t.id, title, artist, t.audio_quality, dur);
                }
            }
        }
        "--play" => {
            if args.len() < 3 {
                eprintln!("Usage: tidal-app --play <track_id>");
                return Ok(());
            }
            let track_id = &args[2];
            let mut svc = TidalService::with_device_auth()?;
            if !svc.load_session().await.unwrap_or(false) {
                eprintln!("You are not logged in. Run `tidal-app --login` first.");
                return Ok(());
            }

            println!("Resolving track '{}'...", track_id);
            let track = svc.get_track(track_id).await?;
            println!("Found: '{}' by '{}' (Album: '{}')", track.title, track.artist, track.album);
            println!("Duration: {}:{:02}", track.duration_secs / 60, track.duration_secs % 60);

            let cache = AudioCache::new()?;
            let audio_file = if let Some(local_path) = cache.get_cached(&track.id, "HI_RES") {
                println!(" Audio Cache HIT: Playing directly from local SSD (0ms network, no phone home)");
                local_path
            } else {
                println!(" Audio Cache MISS: Fetching MAX Hi-Res stream and caching to local SSD...");
                let stream_info = svc.get_stream_urls(&track.id).await?;
                println!("Stream quality: {} (MIME: {:?}, Codecs: {:?})", stream_info.quality, stream_info.mime_type, stream_info.codecs);
                let path = cache.fetch_or_cache(&track.id, "HI_RES", &stream_info.stream_urls).await?;
                println!(" Cached to {}", path.display());
                path
            };

            let player = AudioPlayer::new()?;
            let queue_item = TrackQueueItem {
                id: track.id.clone(),
                title: track.title.clone(),
                artist: track.artist.clone(),
                file_path: audio_file,
                duration: Duration::from_secs(track.duration_secs as u64),
            };

            println!("\n▶ Now Playing: '{}' - '{}' [MAX Hi-Res]", track.title, track.artist);
            println!("Press Ctrl+C to stop playback.\n");

            player.play_track(queue_item)?;

            let total_secs = track.duration_secs as u64;
            loop {
                let elapsed = player.elapsed();
                let secs = elapsed.as_secs();
                let pct = (secs as f64 / total_secs.max(1) as f64 * 100.0).min(100.0);
                print!("\r[Playing] {:02}:{:02} / {:02}:{:02} ({:.1}%)", secs / 60, secs % 60, total_secs / 60, total_secs % 60, pct);
                use std::io::Write;
                let _ = std::io::stdout().flush();

                if secs >= total_secs {
                    println!("\nPlayback completed!");
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        unknown => {
            eprintln!("Unknown argument '{}'. Run `tidal-app --help` for usage.", unknown);
        }
    }
    Ok(())
}
