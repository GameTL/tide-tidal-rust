# TIDE's patch to i-slint-renderer-skia 1.18.1

This is the crate as published on crates.io, with the changes below. Each is
marked `TIDE patch` in the source. The crate keeps Slint's licence (see
`LICENSES/`). When Slint is upgraded, replace this directory with the new
release and re-apply them, or drop the patch if upstream has fixed them.

1. `font_cache.rs` – fonts are handed to Skia without a copy. Upstream calls
   `Data::new_copy` on the whole font file; system fonts are memory-mapped and
   Apple Color Emoji is about 190 MB, so drawing one emoji cost that much heap.
2. `lib.rs` (`pub mod memory`) and `wgpu_30_surface.rs` – the GPU resource
   cache gets a budget, resources no frame has used for a while are freed, the
   cache size is reported, and the window can run on two drawables instead of
   three. Upstream leaves the cache at Skia's default 256 MB and never trims it.
3. `itemrenderer.rs` (`visit_opacity`) – a group with `opacity` gets an
   unbounded `save_layer`, i.e. an offscreen texture the size of the whole
   window, every frame. Two cases skip it:
   - a fully transparent group (alpha rounds to 0) is not drawn at all;
   - a group whose only child is a `Layer` with `cache-rendering-hint: true`
     is drawn by blending that cached, item-sized image with the alpha, which
     already is the group's opacity.
   Now Playing's fades set the hint while they animate; without this the
   expand rendered several window-sized layers per frame.
4. `lib.rs` (`internal_render_with_post_callback`) – `TIDE_FRAME_LOG=1` prints
   each frame's wall time, total render+present time and scene-building time
   to stderr. Pair with `TIDE --snapshot <dir>` and `TIDE_TOUR=now-playing`.
5. `lib.rs` (`memory::frames_rendered`) – a count of submitted frames, so the
   app can hold its window-open animation until there is content to show.
