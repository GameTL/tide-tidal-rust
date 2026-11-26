// TIDE patch: this whole file. See TIDE-PATCH.md, item 8.

//! An edge-fade alpha mask for a rectangle's children along either axis:
//! content smoothly dissolves into transparency at the viewport edges instead
//! of hard-clipping (used by Now Playing's Lyrics/Queue scroll lists vertically
//! and their sliding column container horizontally).
//!
//! Slint 1.18.1 has no alpha-mask layer item, so a rectangle asks for the mask
//! with a marker brush as its `background`:
//!
//! ```slint
//! // Vertical (top, bottom):
//! background: @conic-gradient(at <top> <bottom>, #46414401 0deg, #46414401 360deg);
//! // Horizontal (left, right):
//! background: @conic-gradient(at <left> <right>, #46414801 0deg, #46414801 360deg);
//! ```
//!
//! The gradient's centre carries the two fade band sizes in logical lengths.
//! When both bands are zero, nothing is allocated and children draw directly
//! as usual. Otherwise the children draw into a `save_layer` bounded to the
//! rectangle itself (never window-sized), and on restore a smoothstep linear
//! alpha gradient is applied with `BlendMode::DstIn`.
//!
//! Other renderers draw the marker as an all but transparent gradient and clip
//! children normally.

use i_slint_core::Brush;
use skia_safe::{BlendMode, Canvas, Color4f, Paint, Point, Rect, canvas::SaveLayerRec};

/// First colour stop for vertical edge fade: `#46414401` (`"FAD"` + alpha 1).
const MARKER_VERTICAL: (u8, u8, u8, u8) = (0x46, 0x41, 0x44, 0x01);

/// First colour stop for horizontal edge fade: `#46414801` (`"FAH"` + alpha 1).
const MARKER_HORIZONTAL: (u8, u8, u8, u8) = (0x46, 0x41, 0x48, 0x01);

/// Minimum band size (in physical pixels) before allocating a mask layer.
const MIN_BAND: f32 = 0.5;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Axis {
    Vertical,
    Horizontal,
}

/// Active bounded fade layer to finish when the item's `restore_state` runs.
#[derive(Clone, Copy)]
pub(crate) struct Active {
    rect: Rect,
    axis: Axis,
    start: f32,
    end: f32,
}

/// `(axis, start_band, end_band)` in physical pixels, if `brush` is a fade marker.
pub(crate) fn spec(brush: &Brush, scale_factor: f32) -> Option<(Axis, f32, f32)> {
    let Brush::ConicGradient(gradient) = brush else { return None };
    let first = gradient.stops().next()?.color;
    let rgba = (first.red(), first.green(), first.blue(), first.alpha());
    let axis = if rgba == MARKER_VERTICAL {
        Axis::Vertical
    } else if rgba == MARKER_HORIZONTAL {
        Axis::Horizontal
    } else {
        return None;
    };
    let (start, end) = gradient.center_or_default_scaled(0., 0., scale_factor);
    let clean = |v: f32| if v.is_finite() { v.max(0.) } else { 0. };
    Some((axis, clean(start), clean(end)))
}

/// Starts a bounded offscreen layer for `rect` if either edge has a visible fade band.
/// Returns `None` when both bands are effectively zero so callers pay zero layer cost.
pub(crate) fn begin(
    canvas: &Canvas,
    rect: Rect,
    axis: Axis,
    start: f32,
    end: f32,
) -> Option<Active> {
    let extent = match axis {
        Axis::Vertical => rect.height(),
        Axis::Horizontal => rect.width(),
    };
    if rect.is_empty() || extent <= 0. || (start < MIN_BAND && end < MIN_BAND) {
        return None;
    }
    let (start, end) = if start + end > extent {
        let k = extent / (start + end);
        (start * k, end * k)
    } else {
        (start, end)
    };
    let rec = SaveLayerRec::default().bounds(&rect);
    canvas.save_layer(&rec);
    Some(Active { rect, axis, start, end })
}

/// Applies the smoothstep alpha mask with `DstIn` and composites the layer.
pub(crate) fn end(canvas: &Canvas, active: Active) {
    let (extent, p0, p1) = match active.axis {
        Axis::Vertical => (
            active.rect.height(),
            Point::new(0., active.rect.top),
            Point::new(0., active.rect.bottom),
        ),
        Axis::Horizontal => (
            active.rect.width(),
            Point::new(active.rect.left, 0.),
            Point::new(active.rect.right, 0.),
        ),
    };
    // Smoothstep (3t^2 - 2t^3) sampled at quarters.
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
            if let Some(last) = colors.last_mut() {
                *last = Color4f::new(1., 1., 1., alpha);
            }
        } else {
            positions.push(pos);
            colors.push(Color4f::new(1., 1., 1., alpha));
        }
    };

    if active.start >= MIN_BAND {
        let frac = (active.start / extent).clamp(0., 1.);
        for (u, a) in CURVE {
            push_stop(frac * u, a);
        }
    } else {
        push_stop(0., 1.);
    }

    if active.end >= MIN_BAND {
        let frac = (active.end / extent).clamp(0., 1.);
        let origin = (1. - frac).clamp(0., 1.);
        for (u, a) in CURVE {
            push_stop(origin + frac * u, 1. - a);
        }
    } else {
        push_stop(1., 1.);
    }

    if let Some(shader) = skia_safe::gradient::shaders::linear_gradient(
        (p0, p1),
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

    fn marker(rgba: (u8, u8, u8, u8), a: f32, b: f32) -> Brush {
        let c = Color::from_argb_u8(rgba.3, rgba.0, rgba.1, rgba.2);
        Brush::ConicGradient(
            ConicGradientBrush::new(
                0.,
                [GradientStop { color: c, position: 0. }, GradientStop { color: c, position: 1. }],
            )
            .with_center(a, b),
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
        assert_eq!(spec(&marker(MARKER_VERTICAL, 24., 64.), 2.0), Some((Axis::Vertical, 48., 128.)));
        assert_eq!(spec(&marker(MARKER_HORIZONTAL, 32., 32.), 2.0), Some((Axis::Horizontal, 64., 64.)));
        assert_eq!(spec(&marker(MARKER_VERTICAL, -10., 0.), 2.0), Some((Axis::Vertical, 0., 0.)));
        assert_eq!(spec(&Brush::SolidColor(Color::from_rgb_u8(1, 2, 3)), 2.0), None);
        // Glass marker is not a fade mask.
        let glass = Brush::ConicGradient(ConicGradientBrush::new(
            0.,
            [
                GradientStop { color: Color::from_argb_u8(1, 0x47, 0x4c, 0x53), position: 0. },
                GradientStop { color: Color::from_argb_u8(0x80, 0, 0, 0), position: 1. },
            ],
        ));
        assert_eq!(spec(&glass, 2.0), None);
    }

    #[test]
    fn zero_bands_skip_layer_allocation() {
        let mut surface = skia_safe::surfaces::raster_n32_premul((100, 100)).unwrap();
        let rect = Rect::from_wh(100., 100.);
        assert!(begin(surface.canvas(), rect, Axis::Vertical, 0., 0.).is_none());
        assert!(begin(surface.canvas(), rect, Axis::Horizontal, 0., 0.).is_none());
    }

    #[test]
    fn fades_top_and_bottom_edges_smoothly_onto_backdrop() {
        let mut surface = skia_safe::surfaces::raster_n32_premul((40, 200)).unwrap();
        let canvas = surface.canvas();
        canvas.clear(skia_safe::Color::from_rgb(20, 40, 60));

        let rect = Rect::from_wh(40., 200.);
        let active = begin(canvas, rect, Axis::Vertical, 40., 40.).expect("active fade layer");
        let white = Paint::new(Color4f::new(1., 1., 1., 1.), None);
        canvas.draw_rect(rect, &white);
        end(canvas, active);

        let top_edge = pixel(&mut surface, 20, 0);
        let top_mid = pixel(&mut surface, 20, 20);
        let center = pixel(&mut surface, 20, 100);
        let bot_mid = pixel(&mut surface, 20, 180);
        let bot_edge = pixel(&mut surface, 20, 199);

        assert!(top_edge.0 <= 25 && top_edge.2 <= 65, "top edge not faded: {top_edge:?}");
        assert!(bot_edge.0 <= 25 && bot_edge.2 <= 65, "bottom edge not faded: {bot_edge:?}");
        assert_eq!(center, (255, 255, 255));
        assert!(top_edge.0 < top_mid.0 && top_mid.0 < center.0);
        assert!(bot_edge.0 < bot_mid.0 && bot_mid.0 < center.0);
    }

    #[test]
    fn fades_left_and_right_edges_smoothly_onto_backdrop() {
        let mut surface = skia_safe::surfaces::raster_n32_premul((200, 40)).unwrap();
        let canvas = surface.canvas();
        canvas.clear(skia_safe::Color::from_rgb(20, 40, 60));

        let rect = Rect::from_wh(200., 40.);
        let active = begin(canvas, rect, Axis::Horizontal, 40., 40.).expect("horizontal fade");
        let white = Paint::new(Color4f::new(1., 1., 1., 1.), None);
        canvas.draw_rect(rect, &white);
        end(canvas, active);

        let left_edge = pixel(&mut surface, 0, 20);
        let left_mid = pixel(&mut surface, 20, 20);
        let center = pixel(&mut surface, 100, 20);
        let right_mid = pixel(&mut surface, 180, 20);
        let right_edge = pixel(&mut surface, 199, 20);

        assert!(left_edge.0 <= 25 && left_edge.2 <= 65, "left edge not faded: {left_edge:?}");
        assert!(right_edge.0 <= 25 && right_edge.2 <= 65, "right edge not faded: {right_edge:?}");
        assert_eq!(center, (255, 255, 255));
        assert!(left_edge.0 < left_mid.0 && left_mid.0 < center.0);
        assert!(right_edge.0 < right_mid.0 && right_mid.0 < center.0);
    }

    #[test]
    fn bottom_only_fade_keeps_top_edge_crisp() {
        let mut surface = skia_safe::surfaces::raster_n32_premul((40, 200)).unwrap();
        let canvas = surface.canvas();
        canvas.clear(skia_safe::Color::from_rgb(20, 40, 60));

        let rect = Rect::from_wh(40., 200.);
        let active = begin(canvas, rect, Axis::Vertical, 0., 50.).expect("bottom fade layer");
        let white = Paint::new(Color4f::new(1., 1., 1., 1.), None);
        canvas.draw_rect(rect, &white);
        end(canvas, active);

        assert_eq!(pixel(&mut surface, 20, 0), (255, 255, 255));
        assert_eq!(pixel(&mut surface, 20, 100), (255, 255, 255));
        let bot_edge = pixel(&mut surface, 20, 199);
        assert!(bot_edge.0 <= 25 && bot_edge.2 <= 65, "bottom edge not faded: {bot_edge:?}");
    }
}
