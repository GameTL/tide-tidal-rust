// TIDE patch: this whole file. See TIDE-PATCH.md, item 7.

//! A perspective tilt for a rectangle's children, like a card turned towards
//! the viewer: Now Playing's artwork leans away from the pointer.
//!
//! Slint 1.18.1 transforms are 2D only (`transform-rotation`,
//! `transform-scale-x/y`), so a rectangle asks for the tilt with a marker
//! brush as its `background`:
//!
//! ```slint
//! background: @conic-gradient(at <yaw> * 1px <pitch> * 1px, #3d711701 0deg, #3d711701 360deg);
//! ```
//!
//! The gradient's centre carries the two angles in degrees, as floats: the
//! colour stops are only eight bits, the centre is not. `yaw` turns the
//! rectangle about its vertical axis (positive: the right edge goes away from
//! the viewer), `pitch` about its horizontal axis (positive: the bottom edge
//! goes away). The rectangle itself draws nothing; its children are drawn
//! through the projection, so a drop shadow among them tilts with them.
//! At rest (both angles about zero) the canvas is left as it is.
//!
//! Other renderers draw the marker as what it is: an all but transparent
//! gradient. The children are then drawn flat.

use i_slint_core::Brush;
use skia_safe::{Canvas, Matrix, Rect};

/// First colour stop of the marker brush: `#3d711701` in `.slint`.
const MARKER: (u8, u8, u8, u8) = (0x3d, 0x71, 0x17, 0x01);

/// The largest angle honoured either way, in degrees.
const MAX_DEGREES: f32 = 30.;

/// The eye's distance from the rectangle, in multiples of its larger side.
/// CSS demos of this effect use about `perspective: 1000px` for a 400px card.
const EYE_DISTANCE: f32 = 2.5;

/// Below this (degrees) the tilt is not applied: the art is drawn exactly as
/// without the patch, pixel-aligned.
const REST: f32 = 0.01;

/// `(yaw, pitch)` in degrees, if `brush` is the tilt marker.
pub(crate) fn angles(brush: &Brush) -> Option<(f32, f32)> {
    let Brush::ConicGradient(gradient) = brush else { return None };
    let first = gradient.stops().next()?.color;
    if (first.red(), first.green(), first.blue(), first.alpha()) != MARKER {
        return None;
    }
    // Without `at`, the centre falls back to the middle of a 0x0 box.
    let (yaw, pitch) = gradient.center_or_default(0., 0.);
    let clean = |a: f32| if a.is_finite() { a.clamp(-MAX_DEGREES, MAX_DEGREES) } else { 0. };
    Some((clean(yaw), clean(pitch)))
}

/// The projection of `rect` (device pixels, the canvas's local space) turned
/// by `yaw` and `pitch` degrees about its centre, seen from `EYE_DISTANCE`.
pub(crate) fn matrix(rect: Rect, yaw: f32, pitch: f32) -> Matrix {
    let (sy, cy) = yaw.to_radians().sin_cos();
    let (sp, cp) = pitch.to_radians().sin_cos();
    let d = EYE_DISTANCE * rect.width().max(rect.height()).max(1.);
    // A point (x, y) about the centre turns to
    //   x' = x cos(yaw) - y sin(pitch) sin(yaw)
    //   y' = y cos(pitch)
    //   z' = -x sin(yaw) - y sin(pitch) cos(yaw)        (towards the eye: +)
    // and is seen at (x', y') * d / (d - z'): homogeneous w = 1 - z'/d.
    let about_centre = Matrix::new_all(
        cy,
        -sp * sy,
        0.,
        0.,
        cp,
        0.,
        sy / d,
        sp * cy / d,
        1.,
    );
    let (cx, cyy) = (rect.center_x(), rect.center_y());
    let mut m = Matrix::translate((cx, cyy));
    m.pre_concat(&about_centre);
    m.pre_translate((-cx, -cyy));
    m
}

/// Tilts what is drawn next on `canvas` (the rectangle's children), if the
/// angles are not at rest. The caller's `save`/`restore` around the item
/// scopes it to them.
pub(crate) fn apply(canvas: &Canvas, rect: Rect, (yaw, pitch): (f32, f32)) {
    if yaw.abs() < REST && pitch.abs() < REST {
        return;
    }
    canvas.concat(&matrix(rect, yaw, pitch));
}

#[cfg(test)]
mod tests {
    use super::*;
    use i_slint_core::graphics::{ConicGradientBrush, GradientStop};
    use i_slint_core::Color;
    use skia_safe::Point;

    fn marker(yaw: f32, pitch: f32) -> Brush {
        let c = Color::from_argb_u8(MARKER.3, MARKER.0, MARKER.1, MARKER.2);
        Brush::ConicGradient(
            ConicGradientBrush::new(
                0.,
                [GradientStop { color: c, position: 0. }, GradientStop { color: c, position: 1. }],
            )
            .with_center(yaw, pitch),
        )
    }

    #[test]
    fn the_marker_carries_both_angles() {
        assert_eq!(angles(&marker(6.5, -3.25)), Some((6.5, -3.25)));
        assert_eq!(angles(&marker(90., -90.)), Some((MAX_DEGREES, -MAX_DEGREES)));
        assert_eq!(angles(&Brush::SolidColor(Color::from_rgb_u8(1, 2, 3))), None);
        // The glass marker is not a tilt.
        let glass = Brush::ConicGradient(ConicGradientBrush::new(
            0.,
            [
                GradientStop { color: Color::from_argb_u8(1, 0x47, 0x4c, 0x53), position: 0. },
                GradientStop { color: Color::from_argb_u8(0x80, 0, 0, 0), position: 1. },
            ],
        ));
        assert_eq!(angles(&glass), None);
    }

    #[test]
    fn the_edge_turned_away_looks_shorter() {
        let rect = Rect::from_xywh(100., 50., 400., 400.);
        let m = matrix(rect, 8., 0.);
        let [tl, bl, tr, br] = [(100., 50.), (100., 450.), (500., 50.), (500., 450.)]
            .map(|p| m.map_point(Point::from(p)));
        // Positive yaw: the right edge goes away, the left comes closer.
        let left = bl.y - tl.y;
        let right = br.y - tr.y;
        assert!(right < 400. && left > 400., "left {left}, right {right}");
        // The centre stays put.
        let c = m.map_point(Point::new(300., 250.));
        assert!((c.x - 300.).abs() < 1e-3 && (c.y - 250.).abs() < 1e-3);

        // Positive pitch: the bottom edge goes away.
        let m = matrix(rect, 0., 8.);
        let [tl, tr, bl, br] = [(100., 50.), (500., 50.), (100., 450.), (500., 450.)]
            .map(|p| m.map_point(Point::from(p)));
        assert!(br.x - bl.x < 400. && tr.x - tl.x > 400.);
        assert!(matrix(rect, 0., 0.).is_identity());
    }
}
