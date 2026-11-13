# TIDE's patch to i-slint-renderer-skia 1.18.1

This is the crate as published on crates.io, with the changes below. Each is
marked `TIDE patch` in the source. The crate keeps Slint's licence (see
`LICENSES/`). When Slint is upgraded, replace this directory with the new
release and re-apply them, or drop the patch if upstream has fixed both.

1. `font_cache.rs` – fonts are handed to Skia without a copy. Upstream calls
   `Data::new_copy` on the whole font file; system fonts are memory-mapped and
   Apple Color Emoji is about 190 MB, so drawing one emoji cost that much heap.
2. `lib.rs` (`pub mod memory`) and `wgpu_30_surface.rs` – the GPU resource
   cache gets a budget, resources no frame has used for a while are freed, the
   cache size is reported, and the window can run on two drawables instead of
   three. Upstream leaves the cache at Skia's default 256 MB and never trims it.
