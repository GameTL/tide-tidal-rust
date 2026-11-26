# TIDE's patch to i-slint-backend-winit 1.18.1

This is the crate as published on crates.io, with the change below, marked
`TIDE patch` in the source. The crate keeps Slint's licence (see `LICENSES/`).
When Slint is upgraded, replace this directory with the new release and
re-apply it, or drop the patch if upstream has fixed it.

1. `frame_throttle.rs` – the timer-based frame throttle (Windows, X11) divides
   by the monitor's refresh rate and only falls back to 60 Hz when there is
   none. Wine and some virtual or remote displays report 0 Hz, which panicked
   on the first redraw. A rate of 0 now falls back to 60 Hz as well.
