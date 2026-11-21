// TIDE patch: this whole file. See TIDE-PATCH.md, item 6.

//! A glass material for a rectangle, after Apple's Liquid Glass (the
//! "regular" variant): what is already drawn behind the rectangle shows
//! through blurred, with its luminosity pulled towards the tint so text on the
//! glass stays legible, a little lensing along the free edges and a light rim.
//! It is drawn by hand rather than with a `save_layer` backdrop filter: that
//! route kept about 14 MB of GPU textures for a 1440x96 bar at 2x (Skia
//! rounds each of its layers up to a power of two); this one keeps 4 MB.
//!
//! Slint has no backdrop filter, so a rectangle asks for it with a marker
//! brush: a conic gradient whose first stop is [`MARKER`] and whose second
//! stop is the tint. Renderers that do not know the marker would draw that
//! gradient, so the `.slint` side only uses it where the app has checked that
//! this renderer is in use (`glass::set_available`).

use std::sync::atomic::{AtomicBool, Ordering};

use i_slint_core::{Brush, Color};
use skia_safe::{Canvas, Color4f, Paint, Point, RRect, TileMode};

/// First stop of the marker brush: `#474c5301` in `.slint`.
const MARKER: (u8, u8, u8, u8) = (0x47, 0x4c, 0x53, 0x01);

/// Gaussian sigma of the blur, in logical pixels.
const BLUR_SIGMA: f32 = 16.;
/// Width of the band along a free edge where the backdrop is bent, and how
/// far it is bent at the edge itself. Logical pixels.
const LENS_BAND: f32 = 18.;
const LENS_BEND: f32 = 22.;
/// Colour of the backdrop: saturation is raised so artwork still reads as
/// colour after the blur, and luminance `l` is compressed to
/// `GAIN * l / (1 + KNEE * l)`: dark and mid tones keep their differences,
/// while a white page under the glass cannot wash out the text on it.
const SATURATION: f32 = 1.45;
const GAIN: f32 = 0.55;
const KNEE: f32 = 1.1;

static AVAILABLE: AtomicBool = AtomicBool::new(false);

/// The app will draw glass. Call before the window is created: the window's
/// buffers must then be readable (see `wgpu_30_surface.rs`), which is decided
/// when the surface is configured.
pub fn set_available(on: bool) {
    AVAILABLE.store(on, Ordering::Relaxed);
}

pub(crate) fn available() -> bool {
    AVAILABLE.load(Ordering::Relaxed)
}

/// The tint, if `brush` is the glass marker.
pub(crate) fn tint(brush: &Brush) -> Option<Color> {
    let Brush::ConicGradient(gradient) = brush else { return None };
    let mut stops = gradient.stops();
    let first = stops.next()?.color;
    if (first.red(), first.green(), first.blue(), first.alpha()) != MARKER {
        return None;
    }
    Some(stops.next()?.color)
}

const LENS_SKSL: &str = r#"
uniform shader content;
uniform float2 size;
uniform float4 edges; // left, top, right, bottom: 1 where the edge is free
uniform float band;
uniform float bend;
uniform float saturation;
uniform float gain;
uniform float knee;

half4 main(float2 p) {
    // Distance to, and inward normal of, the nearest free edge.
    float d = band;
    float2 n = float2(0.0);
    if (edges.x > 0.5 && p.x < d) { d = p.x; n = float2(1.0, 0.0); }
    if (edges.y > 0.5 && p.y < d) { d = p.y; n = float2(0.0, 1.0); }
    if (edges.z > 0.5 && size.x - p.x < d) { d = size.x - p.x; n = float2(-1.0, 0.0); }
    if (edges.w > 0.5 && size.y - p.y < d) { d = size.y - p.y; n = float2(0.0, -1.0); }
    // A thick pane's rounded edge shows what lies further in.
    float t = 1.0 - clamp(d / band, 0.0, 1.0);
    half4 c = content.eval(p + n * (bend * t * t));
    half l = dot(c.rgb, half3(0.2126, 0.7152, 0.0722));
    half3 rgb = mix(half3(l), c.rgb, half(saturation)) * half(gain / (1.0 + knee * l));
    // The band catches a little light.
    rgb += half3(0.06 * t * t) * c.a;
    return half4(clamp(rgb, half3(0.0), half3(c.a)), c.a);
}
"#;

thread_local! {
    /// Compiled once per thread; `None` if this Skia cannot build it, in which
    /// case the glass is the blur alone under a stronger tint.
    static LENS: Option<skia_safe::RuntimeEffect> =
        skia_safe::RuntimeEffect::make_for_shader(LENS_SKSL, None).ok();
}

/// The blur is done at this fraction of the size: a blur this wide has no
/// detail a quarter-size image cannot hold, and the textures it needs are a
/// sixteenth of the pane instead of several times the pane.
const SHRINK: i32 = 4;

/// An offscreen surface like the one `canvas` draws to. On the GPU it is
/// budgeted, so the next frame reuses its texture.
fn scratch_surface(canvas: &Canvas, size: skia_safe::ISize) -> Option<skia_safe::Surface> {
    let info = skia_safe::ImageInfo::new_n32_premul(size, canvas.image_info().color_space());
    match canvas.recording_context() {
        Some(mut context) => skia_safe::gpu::surfaces::render_target(
            &mut context,
            skia_safe::gpu::Budgeted::Yes,
            &info,
            None,
            skia_safe::gpu::SurfaceOrigin::TopLeft,
            None,
            false,
            None,
        ),
        None => skia_safe::surfaces::raster(&info, None, None),
    }
}

/// What is drawn so far in `region` (device pixels), blurred, at 1/SHRINK
/// of the size. The only full-size texture is the copy of `region` itself.
fn blurred_backdrop(
    canvas: &Canvas,
    region: skia_safe::IRect,
    sigma: f32,
) -> Option<skia_safe::Image> {
    // SAFETY: the surface is only used here, while the canvas it owns is alive.
    let mut surface = unsafe { canvas.surface() }?;
    let copy = surface.image_snapshot_with_bounds(region)?;
    let linear = skia_safe::SamplingOptions::from(skia_safe::FilterMode::Linear);
    let mut source = Paint::default();
    source.set_blend_mode(skia_safe::BlendMode::Src);

    // Halve twice: each step averages 2x2 pixels exactly, which one 4x step
    // with a linear filter would not.
    let half_size = skia_safe::ISize::new((region.width() + 1) / 2, (region.height() + 1) / 2);
    let mut half = scratch_surface(canvas, half_size)?;
    half.canvas().draw_image_rect_with_sampling_options(
        &copy,
        None,
        skia_safe::Rect::from_isize(half_size),
        linear,
        &source,
    );
    let half_image = half.image_snapshot();

    let size = skia_safe::ISize::new(
        (region.width() + SHRINK - 1) / SHRINK,
        (region.height() + SHRINK - 1) / SHRINK,
    );
    let mut small = scratch_surface(canvas, size)?;
    let sigma = sigma / SHRINK as f32;
    source.set_image_filter(skia_safe::image_filters::blur(
        (sigma, sigma),
        TileMode::Clamp,
        None,
        None,
    ));
    small.canvas().draw_image_rect_with_sampling_options(
        &half_image,
        None,
        skia_safe::Rect::from_isize(size),
        linear,
        &source,
    );
    Some(small.image_snapshot())
}

/// The shader that draws `backdrop` (see `blurred_backdrop`) bent and toned.
/// `to_pane` maps the backdrop image's pixels to the pane's coordinates.
fn lens_shader(
    backdrop: &skia_safe::Image,
    to_pane: &skia_safe::Matrix,
    size: (f32, f32),
    free_edges: [f32; 4],
    scale: f32,
) -> Option<skia_safe::Shader> {
    let linear = skia_safe::SamplingOptions::from(skia_safe::FilterMode::Linear);
    let content = backdrop.to_shader((TileMode::Clamp, TileMode::Clamp), linear, to_pane)?;
    LENS.with(|lens| {
        let lens = lens.as_ref()?;
        // In the order the SkSL declares them.
        let uniforms: [f32; 11] = [
            size.0,
            size.1,
            free_edges[0],
            free_edges[1],
            free_edges[2],
            free_edges[3],
            LENS_BAND * scale,
            LENS_BEND * scale,
            SATURATION,
            GAIN,
            KNEE,
        ];
        if lens.uniform_size() != std::mem::size_of_val(&uniforms) {
            return None;
        }
        let bytes: Vec<u8> = uniforms.iter().flat_map(|f| f.to_ne_bytes()).collect();
        lens.make_shader(skia_safe::Data::new_copy(&bytes), &[content.into()], None)
    })
}

/// Draws the glass into `rrect` (physical pixels, in the canvas's current
/// coordinates). Nothing here is the size of the window: the backdrop is
/// copied for the pane only (plus the blur's reach) and blurred at a quarter
/// of that size.
pub(crate) fn draw(canvas: &Canvas, rrect: &RRect, tint: Color, scale: f32, alpha: f32) {
    let bounds = *rrect.rect();
    if bounds.is_empty() {
        return;
    }
    let to_device = canvas.local_to_device_as_3x3();
    let device = to_device.map_rect(bounds).0;
    let base = canvas.base_layer_size();
    // An edge that lies on the window's edge is not a free edge of the pane:
    // nothing is behind it to bend, and it gets no rim.
    let free = [
        device.left > 0.5,
        device.top > 0.5,
        device.right < base.width as f32 - 0.5,
        device.bottom < base.height as f32 - 0.5,
    ];

    canvas.save();
    canvas.clip_rrect(rrect, None, !rrect.is_rect());

    // The backdrop: the pane and as far around it as the blur reaches.
    let sigma = BLUR_SIGMA * scale;
    let around: skia_safe::IRect =
        skia_safe::RoundOut::round_out(&device.with_outset((2. * sigma, 2. * sigma)));
    let mut drawn = None;
    if to_device.is_translate()
        && let Some(region) = skia_safe::IRect::intersect(&around, &skia_safe::IRect::from_size(base))
        && let Some(backdrop) = blurred_backdrop(canvas, region, sigma)
    {
        // Backdrop pixels -> device -> the pane's coordinates.
        let mut to_pane = skia_safe::Matrix::scale((
            region.width() as f32 / backdrop.width() as f32,
            region.height() as f32 / backdrop.height() as f32,
        ));
        to_pane.post_translate((
            region.left as f32 - device.left + bounds.left,
            region.top as f32 - device.top + bounds.top,
        ));
        let mut paint = Paint::default();
        paint.set_alpha_f(alpha);
        let lens = lens_shader(
            &backdrop,
            &to_pane,
            (bounds.width(), bounds.height()),
            free.map(|f| if f { 1. } else { 0. }),
            scale,
        );
        // The lens shader works in the pane's own coordinates.
        canvas.save();
        canvas.translate((bounds.left, bounds.top));
        let local = skia_safe::Rect::from_wh(bounds.width(), bounds.height());
        if let Some(lens) = lens {
            paint.set_shader(lens);
            canvas.draw_rect(local, &paint);
            drawn = Some(true);
        } else {
            let mut unshift = to_pane;
            unshift.post_translate((-bounds.left, -bounds.top));
            paint.set_shader(backdrop.to_shader(
                (TileMode::Clamp, TileMode::Clamp),
                skia_safe::SamplingOptions::from(skia_safe::FilterMode::Linear),
                &unshift,
            ));
            canvas.draw_rect(local, &paint);
            drawn = Some(false);
        }
        canvas.restore();
    }

    // Tint. Without the lens shader nothing has dimmed the backdrop, and
    // without a backdrop the pane is simply the tint, opaque.
    let mut tint = Color4f::new(
        tint.red() as f32 / 255.,
        tint.green() as f32 / 255.,
        tint.blue() as f32 / 255.,
        tint.alpha() as f32 / 255.,
    );
    match drawn {
        Some(true) => {}
        Some(false) => tint.a = tint.a.max(0.82),
        None => tint.a = 1.,
    }
    tint.a *= alpha;
    let mut paint = Paint::new(tint, None);
    canvas.draw_rect(bounds, &paint);

    // Rim: a hairline that is brightest along the top and dies away down the
    // sides, as light from above would catch the pane's edge.
    let line = scale.max(1.);
    let mut rim = bounds;
    // Push the edges that are not free out of the clip.
    if !free[0] { rim.left -= 4. * line; }
    if !free[1] { rim.top -= 4. * line; }
    if !free[2] { rim.right += 4. * line; }
    if !free[3] { rim.bottom += 4. * line; }
    rim = rim.with_inset((line / 2., line / 2.));
    let rim_shape = if rrect.is_rect() {
        RRect::new_rect(rim)
    } else {
        let inner = rrect
            .radii_ref()
            .map(|r| Point::new((r.x - line / 2.).max(0.), (r.y - line / 2.).max(0.)));
        RRect::new_rect_radii(rim, &inner)
    };
    let colors = [
        Color4f::new(1., 1., 1., 0.20 * alpha),
        Color4f::new(1., 1., 1., 0.05 * alpha),
    ];
    paint = Paint::default();
    paint.set_style(skia_safe::PaintStyle::Stroke);
    paint.set_stroke_width(line);
    paint.set_anti_alias(!rrect.is_rect());
    paint.set_shader(skia_safe::gradient::shaders::linear_gradient(
        (Point::new(0., bounds.top), Point::new(0., bounds.top + (bounds.height()).min(48. * scale))),
        &skia_safe::gradient::Gradient::new(
            crate::gradient_colors(&colors, &[0., 1.]),
            skia_safe::gradient::Interpolation::default(),
        ),
        None,
    ));
    canvas.draw_rrect(rim_shape, &paint);

    canvas.restore();
}

#[cfg(test)]
mod tests {
    use super::*;
    use skia_safe::Rect;

    fn stripes(width: i32, height: i32) -> skia_safe::Surface {
        let mut surface = skia_safe::surfaces::raster_n32_premul((width, height)).unwrap();
        let canvas = surface.canvas();
        canvas.clear(skia_safe::Color::BLACK);
        let white = Paint::new(Color4f::new(1., 1., 1., 1.), None);
        for x in (0..width).step_by(16) {
            canvas.draw_rect(Rect::from_xywh(x as f32, 0., 8., height as f32), &white);
        }
        surface
    }

    fn pixel(surface: &mut skia_safe::Surface, x: i32, y: i32) -> (u8, u8, u8) {
        let image = surface.image_snapshot();
        let pixmap = image.peek_pixels().unwrap();
        let c = pixmap.get_color((x, y));
        (c.r(), c.g(), c.b())
    }

    #[test]
    fn lens_shader_compiles() {
        let lens = skia_safe::RuntimeEffect::make_for_shader(LENS_SKSL, None).expect("lens SkSL");
        // `lens_shader` hands the uniforms over as eleven floats.
        assert_eq!(lens.uniform_size(), 11 * 4);
        let mut surface = stripes(64, 64);
        let image = surface.image_snapshot();
        let shader = lens_shader(&image, &skia_safe::Matrix::new_identity(), (64., 64.), [0., 1., 0., 0.], 2.);
        assert!(shader.is_some());
    }

    #[test]
    fn marker_brush_carries_the_tint() {
        use i_slint_core::graphics::{ConicGradientBrush, GradientStop};
        let tint = Color::from_argb_u8(0xb8, 0x1b, 0x1b, 0x1c);
        let marker = Color::from_argb_u8(MARKER.3, MARKER.0, MARKER.1, MARKER.2);
        let brush = Brush::ConicGradient(ConicGradientBrush::new(
            0.,
            [
                GradientStop { color: marker, position: 0. },
                GradientStop { color: tint, position: 1. },
            ],
        ));
        assert_eq!(super::tint(&brush), Some(tint));
        assert_eq!(super::tint(&Brush::SolidColor(marker)), None);
        let plain = Brush::ConicGradient(ConicGradientBrush::new(
            0.,
            [
                GradientStop { color: tint, position: 0. },
                GradientStop { color: tint, position: 1. },
            ],
        ));
        assert_eq!(super::tint(&plain), None);
    }

    #[test]
    fn glass_blurs_and_dims_what_is_behind_it_and_nothing_else() {
        let mut surface = stripes(320, 200);
        let canvas = surface.canvas();
        canvas.save();
        canvas.translate((0., 120.));
        let bar = Rect::from_xywh(0., 0., 320., 80.);
        draw(canvas, &RRect::new_rect(bar), Color::from_argb_u8(0x99, 0x1b, 0x1b, 0x1c), 1., 1.);
        canvas.restore();

        // Above the bar the stripes are untouched.
        assert_eq!(pixel(&mut surface, 4, 60), (255, 255, 255));
        assert_eq!(pixel(&mut surface, 12, 60), (0, 0, 0));
        // Inside it, a white and a black stripe have become nearly the same
        // dark grey: blurred together, then dimmed.
        let (a, _, _) = pixel(&mut surface, 164, 170);
        let (b, _, _) = pixel(&mut surface, 172, 170);
        assert!(a.abs_diff(b) < 12, "stripes still visible through the glass: {a} vs {b}");
        assert!(a > 20 && a < 110, "glass over half-white content should be a dark grey, got {a}");
        // The top edge carries the rim highlight.
        let (rim, _, _) = pixel(&mut surface, 164, 120);
        assert!(rim > a, "no rim highlight: {rim} vs {a}");
    }

    /// The window's buffer on macOS is a `CAMetalLayer` drawable. Glass reads
    /// it back, which Metal only promises when the layer is not
    /// `framebufferOnly` (hence `COPY_SRC` in wgpu_30_surface.rs). Apple
    /// silicon happens to allow it either way, so the `framebufferOnly` case
    /// is only reported. No window: the layer is never attached to one.
    #[cfg(target_os = "macos")]
    #[test]
    fn glass_reads_back_a_metal_drawable() {
        use objc2::rc::Retained;
        use objc2_metal::{MTLCreateSystemDefaultDevice, MTLDevice};
        use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};
        use skia_safe::gpu::{self, mtl};

        let Some(device) = MTLCreateSystemDefaultDevice() else {
            eprintln!("no Metal device: skipped");
            return;
        };
        let queue = device.newCommandQueue().unwrap();
        let backend = unsafe {
            mtl::BackendContext::new(
                Retained::as_ptr(&device) as mtl::Handle,
                Retained::as_ptr(&queue) as mtl::Handle,
            )
        };
        let mut context = gpu::direct_contexts::make_metal(&backend, None).unwrap();
        for framebuffer_only in [false, true] {
            let layer = CAMetalLayer::new();
            layer.setDevice(Some(&device));
            layer.setFramebufferOnly(framebuffer_only);
            layer.setDrawableSize(objc2_core_foundation::CGSize::new(320., 200.));
            let drawable = layer.nextDrawable().expect("drawable");
            let texture = drawable.texture();
            let info = unsafe { mtl::TextureInfo::new(Retained::as_ptr(&texture) as mtl::Handle) };
            let target = gpu::backend_render_targets::make_mtl((320, 200), &info);
            let mut surface = gpu::surfaces::wrap_backend_render_target(
                &mut context,
                &target,
                gpu::SurfaceOrigin::TopLeft,
                skia_safe::ColorType::BGRA8888,
                None,
                None,
            )
            .unwrap();
            let canvas = surface.canvas();
            canvas.clear(skia_safe::Color::BLACK);
            let white = Paint::new(Color4f::new(1., 1., 1., 1.), None);
            for x in (0..320).step_by(16) {
                canvas.draw_rect(Rect::from_xywh(x as f32, 0., 8., 200.), &white);
            }
            canvas.save();
            canvas.translate((0., 120.));
            let bar = Rect::from_xywh(0., 0., 320., 80.);
            draw(canvas, &RRect::new_rect(bar), Color::from_argb_u8(0x80, 0x34, 0x34, 0x36), 1., 1.);
            canvas.restore();
            context.flush_and_submit();

            let info = skia_safe::ImageInfo::new_n32_premul((320, 200), None);
            let mut pixels = vec![0u8; 320 * 200 * 4];
            let read = surface.read_pixels(&info, &mut pixels, 320 * 4, (0, 0));
            let luma = |x: usize, y: usize| pixels[(y * 320 + x) * 4 + 1];
            let (a, b) = (luma(164, 170), luma(172, 170));
            eprintln!(
                "framebufferOnly={framebuffer_only}: readable={read}, in the bar a white stripe is {a} and a black one {b}"
            );
            if !framebuffer_only {
                assert!(read, "a readable drawable could not be read");
                assert_eq!((luma(4, 60), luma(12, 60)), (255, 0), "page above the bar changed");
                assert!(a.abs_diff(b) < 12, "no backdrop blur on Metal: {a} vs {b}");
                assert!(a > 20 && a < 110, "glass on Metal is not a dark grey: {a}");
            }
        }
    }

    /// What the glass costs on this machine's GPU: a 1440x900 window at 2x
    /// with a 96 px bar. `cargo test -p i-slint-renderer-skia glass_cost -- --ignored --nocapture`
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn glass_cost_on_metal() {
        use objc2::rc::Retained;
        use objc2_metal::{MTLCreateSystemDefaultDevice, MTLDevice};
        use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};
        use skia_safe::gpu::{self, mtl};

        let device = MTLCreateSystemDefaultDevice().expect("Metal device");
        let queue = device.newCommandQueue().unwrap();
        let backend = unsafe {
            mtl::BackendContext::new(
                Retained::as_ptr(&device) as mtl::Handle,
                Retained::as_ptr(&queue) as mtl::Handle,
            )
        };
        let mut context = gpu::direct_contexts::make_metal(&backend, None).unwrap();
        let (w, h) = (2880, 1800);
        let layer = CAMetalLayer::new();
        layer.setDevice(Some(&device));
        layer.setFramebufferOnly(false);
        layer.setDrawableSize(objc2_core_foundation::CGSize::new(w as f64, h as f64));
        let drawable = layer.nextDrawable().expect("drawable");
        let texture = drawable.texture();
        let info = unsafe { mtl::TextureInfo::new(Retained::as_ptr(&texture) as mtl::Handle) };
        let target = gpu::backend_render_targets::make_mtl((w, h), &info);
        let mut surface = gpu::surfaces::wrap_backend_render_target(
            &mut context,
            &target,
            gpu::SurfaceOrigin::TopLeft,
            skia_safe::ColorType::BGRA8888,
            None,
            None,
        )
        .unwrap();
        let white = Paint::new(Color4f::new(1., 1., 1., 1.), None);
        let mut frame = |context: &mut gpu::DirectContext, glass: bool| {
            let canvas = surface.canvas();
            canvas.clear(skia_safe::Color::BLACK);
            for x in (0..w).step_by(64) {
                canvas.draw_rect(Rect::from_xywh(x as f32, 0., 32., h as f32), &white);
            }
            if glass {
                canvas.save();
                canvas.translate((0., (h - 192) as f32));
                let bar = Rect::from_xywh(0., 0., w as f32, 192.);
                draw(canvas, &RRect::new_rect(bar), Color::from_argb_u8(0x80, 0x34, 0x34, 0x36), 2., 1.);
                canvas.restore();
            }
            context.flush_submit_and_sync_cpu();
        };
        for glass in [false, true, false, true] {
            context.free_gpu_resources();
            for _ in 0..10 {
                frame(&mut context, glass);
            }
            let started = std::time::Instant::now();
            for _ in 0..200 {
                frame(&mut context, glass);
            }
            let bytes = context.resource_cache_usage().resource_bytes;
            eprintln!(
                "glass={glass}: {:.3} ms per frame (GPU finished), GPU cache {:.1} MB",
                started.elapsed().as_secs_f64() * 1000. / 200.,
                bytes as f64 / (1024. * 1024.)
            );
        }
    }

    #[test]
    fn white_content_cannot_wash_out_the_bar() {
        let mut surface = skia_safe::surfaces::raster_n32_premul((320, 200)).unwrap();
        surface.canvas().clear(skia_safe::Color::WHITE);
        let canvas = surface.canvas();
        canvas.save();
        canvas.translate((0., 120.));
        let bar = Rect::from_xywh(0., 0., 320., 80.);
        draw(canvas, &RRect::new_rect(bar), Color::from_argb_u8(0x99, 0x1b, 0x1b, 0x1c), 1., 1.);
        canvas.restore();
        let (v, _, _) = pixel(&mut surface, 160, 170);
        // White text (#fff) on this must clear WCAG AA (4.5:1): the background
        // may be at most about sRGB 117.
        assert!(v < 100, "bar over a white page is too light for white text: {v}");
    }
}
