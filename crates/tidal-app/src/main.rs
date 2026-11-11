mod controller;
mod platform;

use std::path::PathBuf;
use std::time::Duration;

use slint::ComponentHandle;
use tidal_api::TidalService;
use tidal_audio::{AudioPlayer, TrackQueueItem};
use tidal_cache::{AudioCache, MetadataCache};
use tidal_macos::MacMediaControls;

use controller::{Ctx, QUALITY_KEY, Ui, on_ui};

slint::include_modules!();

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let args: Vec<String> = std::env::args().collect();
    // `--snapshot <dir> [--play]` runs the GUI through a scripted tour and saves PNGs.
    let snapshot_dir = args.iter().position(|a| a == "--snapshot").and_then(|i| args.get(i + 1)).map(PathBuf::from);
    if args.len() > 1 && snapshot_dir.is_none() {
        return run_cli(args).await;
    }

    tracing::info!("Starting TIDE...");
    platform::configure_backend();

    let service = TidalService::with_device_auth()?;
    let has_session = service.is_authenticated();
    let ctx = Ctx::new(service)?;

    // Media keys / Control Center arrive on a system thread; hop to the UI thread.
    *ctx.media.lock().unwrap() =
        MacMediaControls::new("TIDE", |event| on_ui(move |ui| ui.media_event(event))).ok();

    let main_window = MainWindow::new()?;
    let ui = Ui::install(&main_window, ctx);
    ui.boot(has_session);
    ui.start_token_refresh();

    let ticker = slint::Timer::default();
    let tick_ui = ui.clone();
    ticker.start(slint::TimerMode::Repeated, Duration::from_millis(250), move || tick_ui.tick());

    if let Some(dir) = snapshot_dir {
        ui.run_snapshot_tour(dir, args.iter().any(|a| a == "--play"));
    }

    main_window.run()?;
    Ok(())
}

async fn run_cli(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    let cmd = args[1].as_str();
    match cmd {
        "--help" | "-h" => {
            println!("TIDE - a pure-Rust desktop player for TIDAL (macOS)");
            println!("Usage:");
            println!("  tide                  Launch the 1:1 Tidal desktop GUI");
            println!("  tide --login          Start OAuth2 Device Code login (tidal.com/device)");
            println!("  tide --login-pkce     Start OAuth2 PKCE login flow");
            println!("  tide --status         Show current login status and cache stats");
            println!("  tide --search <query> Search Tidal catalog");
            println!("  tide --play <id>      Play track in MAX Hi-Res quality");
            println!("  tide --logout         Log out and clear session tokens");
            println!("  tide --snapshot <dir> [--play]  UI self-test: render each view to PNG");
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
                println!("Auth status:     NOT LOGGED IN (run `tide --login` to authenticate)");
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
                eprintln!("Usage: tide --search <query>");
                return Ok(());
            }
            let query = args[2..].join(" ");
            let mut svc = TidalService::with_device_auth()?;
            if !svc.load_session().await.unwrap_or(false) {
                eprintln!("You are not logged in. Run `tide --login` first.");
                return Ok(());
            }
            let api = svc.handle().ok_or("no credentials")?;
            println!("Searching Tidal for: '{}'...", query);
            let results = api.search_tracks(&query).await?;
            if results.is_empty() {
                println!("No tracks found.");
            } else {
                println!("\n{:<12} {:<32} {:<24} {:<10} {:<6}", "ID", "TITLE", "ARTIST", "QUALITY", "TIME");
                println!("{}", "-".repeat(90));
                for t in results {
                    let dur = format!("{}:{:02}", t.duration_secs / 60, t.duration_secs % 60);
                    let clip = |s: &str, n: usize| -> String {
                        if s.chars().count() > n { format!("{}...", s.chars().take(n - 3).collect::<String>()) } else { s.to_string() }
                    };
                    let (title, artist) = (clip(&t.title, 30), clip(&t.artist, 22));
                    println!("{:<12} {:<32} {:<24} {:<10} {:<6}", t.id, title, artist, t.audio_quality, dur);
                }
            }
        }
        "--play" => {
            if args.len() < 3 {
                eprintln!("Usage: tide --play <track_id>");
                return Ok(());
            }
            let track_id = &args[2];
            let mut svc = TidalService::with_device_auth()?;
            if !svc.load_session().await.unwrap_or(false) {
                eprintln!("You are not logged in. Run `tide --login` first.");
                return Ok(());
            }

            println!("Resolving track '{}'...", track_id);
            let api = svc.handle().ok_or("no credentials")?;
            let track = api.track(track_id).await?;
            println!("Found: '{}' by '{}' (Album: '{}')", track.title, track.artist, track.album);
            println!("Duration: {}:{:02}", track.duration_secs / 60, track.duration_secs % 60);

            let cache = AudioCache::new()?;
            let audio_file = if let Some(local_path) = cache.get_cached(&track.id, QUALITY_KEY) {
                println!(" Audio Cache HIT: Playing directly from local SSD (0ms network, no phone home)");
                local_path
            } else {
                println!(" Audio Cache MISS: Fetching MAX Hi-Res stream and caching to local SSD...");
                let stream_info = api.stream(&track.id).await?;
                println!(
                    "Stream quality: {} ({}-bit / {} Hz, {} segment(s))",
                    stream_info.quality, stream_info.bit_depth, stream_info.sample_rate, stream_info.urls.len()
                );
                let path = cache.fetch_or_cache(&track.id, QUALITY_KEY, &stream_info.urls).await?;
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
            if let Some(f) = player.format_info() {
                println!("Decoding {}-bit / {} Hz -> device at {} Hz", f.bits_per_sample, f.sample_rate, player.sample_rate());
            }

            let total_secs = track.duration_secs as u64;
            loop {
                let elapsed = player.elapsed();
                let secs = elapsed.as_secs();
                let pct = (secs as f64 / total_secs.max(1) as f64 * 100.0).min(100.0);
                print!("\r[Playing] {:02}:{:02} / {:02}:{:02} ({:.1}%)", secs / 60, secs % 60, total_secs / 60, total_secs % 60, pct);
                use std::io::Write;
                let _ = std::io::stdout().flush();

                if secs >= total_secs || player.is_finished() {
                    println!("\nPlayback completed!");
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        unknown => {
            eprintln!("Unknown argument '{}'. Run `tide --help` for usage.", unknown);
        }
    }
    Ok(())
}
