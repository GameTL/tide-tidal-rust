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
6. `glass.rs` (new), `lib.rs` (`pub mod glass`), `itemrenderer.rs`
   (`draw_rectangle`, `draw_border_rectangle`) and `wgpu_30_surface.rs`
   (`init_with_parts`) – a rectangle can be glass, after Apple's Liquid Glass:
   what is already drawn behind it shows through blurred and dimmed towards a
   tint, bent a little along its free edges, with a light rim. Slint 1.18.1
   has no backdrop filter (no such element or property in
   `i-slint-compiler`'s `builtin_elements.rs` / `typeregister.rs`, no hook in
   `i-slint-core`'s `ItemRenderer`), so the
   rectangle asks with a marker brush as its `background`:
   `@conic-gradient(#474c5301 0deg, <tint> 360deg)`. The renderer copies
   what is drawn so far under the rectangle (plus the blur's reach), blurs it
   at a quarter of the size and draws it back through an SkSL shader. No
   texture is the size of the window. Skia's own route, a `save_layer` with a
   backdrop filter, kept 14.3 MB of GPU textures for a 1440x96 bar at 2x
   (each layer is rounded up to a power of two); this keeps 4.3 MB and costs
   about 0.3 ms a frame on an M-series GPU (`glass_cost_on_metal`). Other
   renderers would draw the gradient: the app uses the marker only after
   `glass::set_available(true)` (TIDE's player bar, `App.glass`).
   - The backdrop is read back from the window's buffer. `set_available`
     before the window exists makes the surface ask for `COPY_SRC`; without
     it wgpu marks the Metal layer `framebufferOnly`, which Metal does not
     promise can be read (Apple silicon lets it through; do not rely on it).
   - The Metal surface repaints the whole window every frame
     (`use_partial_rendering` is false), so the blurred backdrop is never
     stale. If partial rendering is ever turned on, a dirty region that
     touches a glass rectangle must grow to all of it plus the blur radius.
   - A glass rectangle inside an `opacity` group or a cached `Layer` would
     see the window's content, not the group's: keep it out of those.
   - Unit tests in `glass.rs` (`cargo test -p i-slint-renderer-skia glass`),
     one of them against a real Metal drawable with no window; the app's
     `tests/glass_render.rs` draws the whole window with this renderer.
7. `tilt.rs` (new), `lib.rs` (`mod tilt`) and `itemrenderer.rs`
   (`draw_rectangle`, `draw_border_rectangle`) – a rectangle can turn its
   children in perspective, like a card leaning back: Now Playing's artwork
   tilts away from the pointer. Slint 1.18.1 transforms are 2D only
   (`transform-rotation`, `transform-scale-x/y` in `i-slint-core`'s
   `Transform` item), so the rectangle asks with a marker brush as its
   `background`:
   `@conic-gradient(at <yaw> <pitch>, #3d711701 0deg, #3d711701 360deg)`.
   The gradient's centre (`at`, two lengths) carries the angles in degrees
   as floats; the colour stops are only 8 bits. The rectangle draws nothing
   and `concat`s a 3x3 projective matrix (rotation about its centre, eye at
   2.5x its larger side) onto the canvas; the item's own save/restore in
   `render_item_children` scopes it to the children, so a drop shadow among
   them tilts too. At rest (both angles under 0.01°) the canvas is left
   untouched, so a still pointer costs nothing and the art stays
   pixel-aligned. Hit-testing is not projected (it does not need to be: the
   pointer area is the flat art). Other renderers draw the all but
   transparent gradient and the children flat.
   - Unit tests in `tilt.rs` (`cargo test -p i-slint-renderer-skia tilt`);
     the app's `tests/now_playing_render.rs` draws the tilted art.
8. `fade.rs` (new), `lib.rs` (`mod fade`) and `itemrenderer.rs`
   (`draw_rectangle`, `draw_border_rectangle`, `draw_text`, `restore_state`) –
   a rectangle can softly fade its children along its vertical or horizontal
   edges instead of hard-clipping them (Now Playing's Lyrics/Queue lists
   vertically and their sliding column container horizontally). Slint 1.18.1
   has no alpha-mask layer item, so the rectangle asks with a marker brush as
   its `background`:
   - `@conic-gradient(at <top> <bottom>, #46414401 0deg, #46414401 360deg)` (vertical)
   - `@conic-gradient(at <left> <right>, #46414801 0deg, #46414801 360deg)` (horizontal)
   The gradient's centre carries the two fade band sizes in logical lengths.
   When both bands are zero, nothing is allocated; otherwise children draw into
   a `save_layer` bounded to the rectangle itself and `restore_state` applies a
   smoothstep linear gradient with `BlendMode::DstIn` before compositing. Text
   drawn inside an active fade mask skips integer-pixel origin snapping so
   subpixel-positioned glyphs glide smoothly at 120 Hz. Other renderers draw
   the all but transparent gradient and clip children normally.
   - Unit tests in `fade.rs` (`cargo test -p i-slint-renderer-skia fade`);
     the app's `tests/now_playing_render.rs` verifies scrolled queue and side edge fades.
