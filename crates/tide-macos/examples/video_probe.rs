//! `cargo run -p tide-macos --example video_probe -- <hls-url>`: play a video
//! headless for a few seconds and report the frames AVFoundation decodes.
#[cfg(target_os = "macos")]
fn main() {
    use std::time::Instant;
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopDefaultMode: *const std::ffi::c_void;
        fn CFRunLoopRunInMode(mode: *const std::ffi::c_void, secs: f64, ret: u8) -> i32;
    }
    let url = std::env::args()
        .nth(1)
        .expect("usage: video_probe <hls-url>");
    let mut player = tide_macos::VideoPlayer::new();
    player.set_volume(0.0);
    player.load(&url, 5.0);
    let start = Instant::now();
    let (mut frames, mut size, mut buf) = (0, (0, 0), Vec::new());
    while start.elapsed().as_secs_f64() < 6.0 {
        unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.01, 0) };
        let t = Instant::now();
        if player.pull_frame(|w, h, stride, bgra| {
            buf.resize((w * h * 4) as usize, 0);
            tide_macos::bgra_to_rgba(bgra, w as usize, h as usize, stride, &mut buf);
            size = (w, h);
        }) {
            frames += 1;
            if frames % 30 == 1 {
                println!(
                    "frame {frames} {size:?} copy {:?} pos {:.2}",
                    t.elapsed(),
                    player.position()
                );
            }
        }
    }
    println!(
        "status {:?} frames {frames} size {size:?} duration {:.1} error {:?}",
        player.status(),
        player.duration(),
        player.error()
    );
    println!("downloads {:?}", player.download_stats());
    println!("media {:?}", player.media_info());
}

#[cfg(not(target_os = "macos"))]
fn main() {}
