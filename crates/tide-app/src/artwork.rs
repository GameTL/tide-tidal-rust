//! Animated album covers: TIDAL's `videoCover`, a short silent MP4 loop.
//!
//! Three loops at most, each a muted `tide_macos::VideoPlayer` whose frames
//! replace the still: the current track's album (Now Playing and the player
//! bar share one surface), the open album page, and the album tile under the
//! pointer. Off in Settings, or off macOS, the stills stay.

use std::rc::Rc;

use slint::{ComponentHandle, Image, SharedPixelBuffer};
use tide_api::web::video_cover_url;
use tide_macos::{VideoPlayer, VideoStatus};

use super::Ui;

#[derive(Clone, Copy)]
pub(super) enum Slot {
    Now,
    Page,
    Hover,
}

const SLOTS: [Slot; 3] = [Slot::Now, Slot::Page, Slot::Hover];

impl Slot {
    /// Pixel size to fetch: about the largest the cover is drawn at.
    fn size(self) -> u32 {
        match self {
            Slot::Now | Slot::Page => 640,
            Slot::Hover => 320,
        }
    }
}

#[derive(Default)]
struct Loop {
    /// The album's `videoCover` id this slot should show (empty: none).
    uuid: String,
    player: Option<VideoPlayer>,
    /// It would not play; the still stays until another album is asked for.
    failed: bool,
}

#[derive(Default)]
pub(super) struct ArtState {
    loops: [Loop; 3],
    timer: Option<slint::Timer>,
}

impl Ui {
    fn show_motion(&self, slot: Slot, frame: Image) {
        let app = self.app();
        match slot {
            Slot::Now => app.set_np_motion(frame),
            Slot::Page => app.set_page_motion(frame),
            Slot::Hover => app.set_hover_motion(frame),
        }
    }

    /// What `slot` should animate; `None` puts its still back.
    pub(super) fn animate_art(self: &Rc<Self>, slot: Slot, uuid: Option<&str>) {
        let uuid = uuid.unwrap_or_default();
        {
            let mut art = self.art.borrow_mut();
            let l = &mut art.loops[slot as usize];
            if l.uuid == uuid {
                return;
            }
            l.uuid = uuid.to_string();
            l.player = None;
            l.failed = false;
        }
        self.show_motion(slot, Image::default());
        self.sync_art();
    }

    /// The album tile under the pointer: `key` names it, `uuid` its loop.
    pub(super) fn hover_art(self: &Rc<Self>, key: &str, uuid: &str) {
        if !uuid.is_empty() {
            self.app().set_hover_motion_key(key.into());
            self.animate_art(Slot::Hover, Some(uuid));
        } else if self.app().get_hover_motion_key() == key {
            self.app().set_hover_motion_key("".into());
            self.animate_art(Slot::Hover, None);
        }
    }

    pub(super) fn set_animated_artwork(self: &Rc<Self>, on: bool) {
        self.update_settings(|s| s.animated_artwork = on);
        self.app().set_animated_artwork(on);
        self.sync_art();
    }

    /// Open a player for every slot that wants one, drop the rest, and run
    /// the frame pump only while something plays.
    fn sync_art(self: &Rc<Self>) {
        let on = cfg!(target_os = "macos") && self.ctx.settings.read().unwrap().animated_artwork;
        let mut cleared = Vec::new();
        let mut art = self.art.borrow_mut();
        for (slot, l) in SLOTS.into_iter().zip(art.loops.iter_mut()) {
            if !on || l.uuid.is_empty() {
                if l.player.take().is_some() {
                    cleared.push(slot);
                }
            } else if l.player.is_none() && !l.failed {
                let mut player = VideoPlayer::new();
                player.set_volume(0.0);
                player.load(&video_cover_url(&l.uuid, slot.size()), 0.0);
                l.player = Some(player);
            }
        }
        let busy = art.loops.iter().any(|l| l.player.is_some());
        if !busy {
            art.timer = None;
        } else if art.timer.is_none() {
            let timer = slint::Timer::default();
            let weak = Rc::downgrade(self);
            timer.start(
                slint::TimerMode::Repeated,
                crate::platform::frame_interval(),
                move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.art_poll();
                    }
                },
            );
            art.timer = Some(timer);
        }
        drop(art);
        for slot in cleared {
            self.show_motion(slot, Image::default());
        }
    }

    /// One frame-pump tick: fresh frames out, ended loops back to the start.
    fn art_poll(self: &Rc<Self>) {
        let window = self.win.window();
        let hidden = !window.is_visible() || window.is_minimized();
        // The bar's cover moves with the music; paused, it holds still.
        let playing = self.app().get_playing();
        let mut frames = Vec::new();
        let mut failed = false;
        {
            let mut art = self.art.borrow_mut();
            for (slot, l) in SLOTS.into_iter().zip(art.loops.iter_mut()) {
                let Some(player) = l.player.as_mut() else {
                    continue;
                };
                match player.status() {
                    VideoStatus::Ended => {
                        player.seek(0.0);
                        player.play();
                    }
                    VideoStatus::Failed => {
                        tracing::warn!(uuid = l.uuid, error = ?player.error(), "animated cover failed");
                        l.player = None;
                        l.failed = true;
                        failed = true;
                        continue;
                    }
                    VideoStatus::Playing if matches!(slot, Slot::Now) && !playing => player.pause(),
                    VideoStatus::Paused if !matches!(slot, Slot::Now) || playing => player.play(),
                    _ => {}
                }
                if hidden {
                    continue;
                }
                player.pull_frame(|w, h, stride, bgra| {
                    // A fresh buffer per frame: the renderer may still hold the last one.
                    let mut buf = SharedPixelBuffer::<slint::Rgba8Pixel>::new(w, h);
                    tide_macos::bgra_to_rgba(
                        bgra,
                        w as usize,
                        h as usize,
                        stride,
                        buf.make_mut_bytes(),
                    );
                    frames.push((slot, Image::from_rgba8(buf)));
                });
            }
        }
        for (slot, frame) in frames {
            self.show_motion(slot, frame);
        }
        if failed {
            // Not from inside the pump's own tick: this may drop its timer.
            let weak = Rc::downgrade(self);
            slint::Timer::single_shot(std::time::Duration::ZERO, move || {
                if let Some(ui) = weak.upgrade() {
                    ui.sync_art();
                }
            });
        }
    }
}
