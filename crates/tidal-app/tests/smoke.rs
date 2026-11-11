use tidal_api::{TidalService, TrackItem};
use tidal_cache::{AudioCache, MetadataCache};
use tidal_audio::AudioPlayer;

#[tokio::test]
async fn test_services_initialization() {
    let svc = TidalService::with_device_auth();
    assert!(svc.is_ok(), "TidalService must initialize successfully");

    let meta = MetadataCache::in_memory();
    assert!(meta.is_ok(), "MetadataCache must initialize in-memory");
    let meta = meta.unwrap();

    let track = TrackItem {
        id: "777777".to_string(),
        title: "Test Hi-Res Song".to_string(),
        artist: "Test Artist".to_string(),
        artist_id: Some("111".to_string()),
        album: "Test Album".to_string(),
        album_id: Some("222".to_string()),
        duration_secs: 215,
        cover_url: Some("https://resources.tidal.com/images/test/640x640.jpg".to_string()),
        audio_quality: "HI_RES".to_string(),
        ..Default::default()
    };

    meta.save_favorites(&[track.clone()]).unwrap();
    let favs = meta.get_favorites().unwrap();
    assert_eq!(favs.len(), 1);
    assert_eq!(favs[0].title, "Test Hi-Res Song");

    let temp_cache_dir = std::env::temp_dir().join("tidal_test_smoke_cache");
    let audio_cache = AudioCache::with_dir(temp_cache_dir.clone(), 10 * 1024 * 1024).unwrap();
    let path = audio_cache.file_path("777777", "HI_RES");
    assert!(path.to_str().unwrap().contains("777777_HI_RES.audio"));

    // Verify AudioPlayer CoreAudio initialization
    let player = AudioPlayer::new();
    assert!(player.is_ok(), "AudioPlayer must initialize default output device");

    let _ = std::fs::remove_dir_all(&temp_cache_dir);
}
