// TIDE patch: this whole file. See TIDE-PATCH.md, item 8.

//! A vertical edge-fade mask for a rectangle's children: content smoothly
//! dissolves into transparency along the top and/or bottom edge instead of
//! hard-clipping against the viewport boundary (used by Now Playing's Queue).
//!
//! Slint 1.18.1 has no alpha-mask layer item, so a rectangle asks for the mask
//! with a marker brush as its `background`:
//!
//! ```slint
//! background: @conic-gradient(at <top> <bottom>, #46414401 0deg, #46414401 360deg);
//! ```
//!
//! The gradient's centre carries the top and bottom fade band heights in
//! logical lengths. When both bands are zero, nothing is allocated and children
//! draw directly as usual. Otherwise the children draw into a `save_layer`
//! bounded to the rectangle itself (never window-sized), and on restore a
//! smoothstep vertical alpha gradient is applied with `BlendMode::DstIn`.
//!
//! Other renderers draw the marker as an all but transparent gradient and clip
//! children normally.

use i_slint_core::Brush;
use skia_safe::{BlendMode, Canvas, Color4f, Paint, Point, Rect, canvas::SaveLayerRec};

/// First colour stop of the marker brush: `#46414401` (`"FAD"` + alpha 1) in `.slint`.
const MARKER: (u8, u8, u8, u8) = (0x46, 0x41, 0x44, 0x01);

/// Minimum band height (in physical pixels) before allocating a mask layer.
const MIN_BAND: f32 = 0.5;

/// Active bounded fade layer to finish when the item's `restore_state` runs.
#[derive(Clone, Copy)]
pub(crate) struct Active {
    rect: Rect,
    top: f32,
    bottom: f32,
}

/// `(top, bottom)` fade band heights in physical pixels, if `brush` is the fade marker.
pub(crate) fn bands(brush: &Brush, scale_factor: f32) -> Option<(f32, f32)> {
    let Brush::ConicGradient(gradient) = brush else { return None };
    let first = gradient.stops().next()?.color;
    if (first.red(), first.green(), first.blue(), first.alpha()) != MARKER {
        return None;
    }
    let (top, bottom) = gradient.center_or_default_scaled(0., 0., scale_factor);
    let clean = |v: f32| if v.is_finite() { v.max(0.) } else { 0. };
    Some((clean(top), clean(bottom)))
}

/// Starts a bounded offscreen layer for `rect` if either edge has a visible fade band.
/// Returns `None` when both bands are effectively zero so callers pay zero layer cost.
pub(crate) fn begin(canvas: &Canvas, rect: Rect, top: f32, bottom: f32) -> Option<Active> {
    let height = rect.height();
    if rect.is_empty() || height <= 0. || (top < MIN_BAND && bottom < MIN_BAND) {
        return None;
    }
    let (top, bottom) = if top + bottom > height {
        let k = height / (top + bottom);
        (top * k, bottom * k)
    } else {
        (top, bottom)
    };
    let rec = SaveLayerRec::default().bounds(&rect);
    canvas.save_layer(&rec);
    Some(Active { rect, top, bottom })
}

/// Applies the smoothstep vertical alpha mask with `DstIn` and composites the layer.
pub(crate) fn end(canvas: &Canvas, active: Active) {
    let h = active.rect.height();
    // Smoothstep (3t^2 - 2t^3) sampled at quarters matches LyricsColumn's edge curve.
    const CURVE: [(f32, f32); 5] = [
        (0.0, 0.0),
        (0.25, 0.15625),
        (0.5, 0.5),
        (0.75, 0.84375),
        (1.0, 1.0),
    ];

    let mut colors = Vec::with_capacity(10);
    let mut positions = Vec::with_capacity(10);
    let mut push_stop = |pos: f32, alpha: f32| {
        let pos = pos.clamp(0., 1.);
        if positions.last().is_some_and(|&prev: &f32| (prev - pos).abs() < 1e-5) {
            // Keep the last colour at a shared boundary (e.g. top and bottom bands touching).
            if let Some(last) = colors.last_mut() {
                *last = Color4f::new(1., 1., 1., alpha);
            }
        } else {
            positions.push(pos);
            colors.push(Color4f::new(1., 1., 1., alpha));
        }
    };

    if active.top >= MIN_BAND {
        let top_frac = (active.top / h).clamp(0., 1.);
        for (u, a) in CURVE {
            push_stop(top_frac * u, a);
        }
    } else {
        push_stop(0., 1.);
    }

    if active.bottom >= MIN_BAND {
        let bot_frac = (active.bottom / h).clamp(0., 1.);
        let start = (1. - bot_frac).clamp(0., 1.);
        for (u, a) in CURVE {
            push_stop(start + bot_frac * u, 1. - a);
        }
    } else {
        push_stop(1., 1.);
    }

    if let Some(shader) = skia_safe::gradient::shaders::linear_gradient(
        (Point::new(0., active.rect.top), Point::new(0., active.rect.bottom)),
        &skia_safe::gradient::Gradient::new(
            crate::gradient_colors(&colors, &positions),
            skia_safe::gradient::Interpolation {
                in_premul: skia_safe::gradient::interpolation::InPremul::No,
                ..Default::default()
            },
        ),
        None,
    ) {
        let mut paint = Paint::default();
        paint.set_blend_mode(BlendMode::DstIn);
        paint.set_shader(shader);
        canvas.draw_rect(active.rect, &paint);
    }
    canvas.restore();
}

#[cfg(test)]
mod tests {
    use super::*;
    use i_slint_core::Color;
    use i_slint_core::graphics::{ConicGradientBrush, GradientStop};

    fn marker(top: f32, bottom: f32) -> Brush {
        let c = Color::from_argb_u8(MARKER.3, MARKER.0, MARKER.1, MARKER.2);
        Brush::ConicGradient(
            ConicGradientBrush::new(
                0.,
                [GradientStop { color: c, position: 0. }, GradientStop { color: c, position: 1. }],
            )
            .with_center(top, bottom),
        )
    }

    fn pixel(surface: &mut skia_safe::Surface, x: i32, y: i32) -> (u8, u8, u8) {
        let image = surface.image_snapshot();
        let pixmap = image.peek_pixels().unwrap();
        let c = pixmap.get_color((x, y));
        (c.r(), c.g(), c.b())
    }

    #[test]
    fn marker_carries_scaled_band_heights() {
        assert_eq!(bands(&marker(24., 64.), 2.0), Some((48., 128.)));
        assert_eq!(bands(&marker(-10., 0.), 2.0), Some((0., 0.)));
        assert_eq!(bands(&Brush::SolidColor(Color::from_rgb_u8(1, 2, 3)), 2.0), None);
        // Glass and tilt markers are not fade masks.
        let glass = Brush::ConicGradient(ConicGradientBrush::new(
            0.,
            [
                GradientStop { color: Color::from_argb_u8(1, 0x47, 0x4c, 0x53), position: 0. },
                GradientStop { color: Color::from_argb_u8(0x80, 0, 0, 0), position: 1. },
            ],
        ));
        assert_eq!(bands(&glass, 2.0), None);
    }

    #[test]
    fn zero_bands_skip_layer_allocation() {
        let mut surface = skia_safe::surfaces::raster_n32_premul((100, 100)).unwrap();
        let rect = Rect::from_wh(100., 100.);
        assert!(begin(surface.canvas(), rect, 0., 0.).is_none());
    }

    #[test]
    fn fades_top_and_bottom_edges_smoothly_onto_backdrop() {
        let mut surface = skia_safe::surfaces::raster_n32_premul((40, 200)).unwrap();
        let canvas = surface.canvas();
        // Dark teal backdrop so faded white content blends visibly towards it.
        canvas.clear(skia_safe::Color::from_rgb(20, 40, 60));

        let rect = Rect::from_wh(40., 200.);
        let active = begin(canvas, rect, 40., 40.).expect("active fade layer");
        let white = Paint::new(Color4f::new(1., 1., 1., 1.), None);
        canvas.draw_rect(rect, &white);
        end(canvas, active);

        let top_edge = pixel(&mut surface, 20, 0);
        let top_mid = pixel(&mut surface, 20, 20);
        let center = pixel(&mut surface, 20, 100);
        let bot_mid = pixel(&mut surface, 20, 180);
        let bot_edge = pixel(&mut surface, 20, 199);

        // Edges dissolve completely to backdrop; centre stays pure white.
        assert!(top_edge.0 <= 25 && top_edge.2 <= 65, "top edge not faded: {top_edge:?}");
        assert!(bot_edge.0 <= 25 && bot_edge.2 <= 65, "bottom edge not faded: {bot_edge:?}");
        assert_eq!(center, (255, 255, 255));
        assert!(top_edge.0 < top_mid.0 && top_mid.0 < center.0, "top ramp not monotonic: {top_edge:?} {top_mid:?} {center:?}");
        assert!(bot_edge.0 < bot_mid.0 && bot_mid.0 < center.0, "bottom ramp not monotonic: {bot_edge:?} {bot_mid:?} {center:?}");
    }

    #[test]
    fn bottom_only_fade_keeps_top_edge_crisp() {
        let mut surface = skia_safe::surfaces::raster_n32_premul((40, 200)).unwrap();
        let canvas = surface.canvas();
        canvas.clear(skia_safe::Color::from_rgb(20, 40, 60));

        let rect = Rect::from_wh(40., 200.);
        let active = begin(canvas, rect, 0., 50.).expect("bottom fade layer");
        let white = Paint::new(Color4f::new(1., 1., 1., 1.), None);
        canvas.draw_rect(rect, &white);
        end(canvas, active);

        assert_eq!(pixel(&mut surface, 20, 0), (255, 255, 255));
        assert_eq!(pixel(&mut surface, 20, 100), (255, 255, 255));
        let bot_edge = pixel(&mut surface, 20, 199);
        assert!(bot_edge.0 <= 25 && bot_edge.2 <= 65, "bottom edge not faded: {bot_edge:?}");
    }
}
