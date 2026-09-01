//! The ambient corner cube: a small wireframe that tumbles in the bottom-right
//! gutter while the picker sits idle.
//!
//! Purely decorative, and deliberately self-contained: nothing in here reads the
//! picker's state, its theme, or its layout. A caller hands over a rectangle, a
//! pose angle and the three palette seeds to shade between, and everything else
//! -- the geometry, the lighting model, the Braille sub-pixel rasterizer -- stays
//! in this file.
//!
//! The picker keeps the four things that are genuinely its own: the `CDH_CORNER_3D`
//! opt-out and the color gate (`App::corner_3d_enabled`), the animation clock
//! (`App::corner_anim_angle`, which only converts an `Instant` into the elapsed
//! time `spin_angle` wants), the gutter carved out of the content area
//! (`reserve_corner_gutter`), and how often the event loop wakes to repaint.

use std::time::Duration;

use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use super::Rgb;

/// Cell footprint the cube asks the layout for. The 2:1 ratio is not cosmetic: a
/// Braille cell is a 2x4 dot matrix, so 14x7 cells is a 28x28 *dot* canvas, and
/// `project_point` scales against the smaller axis -- a square canvas is what
/// stops the cube being shrunk to fit a lopsided one.
pub(super) const WIDTH: u16 = 14;
pub(super) const HEIGHT: u16 = 7;

/// Repaint interval while the cube animates. Every tick rebuilds the whole frame
/// -- ratatui diffs the result so the terminal writes stay tiny, but the widget
/// tree is reconstructed regardless, and this runs precisely while the user sits
/// reading. 20fps is well past smooth for something turning at a fifth of a
/// revolution per second, and costs a third less idle work than 30.
pub(super) const FRAME: Duration = Duration::from_millis(50);

/// Tumble speed of the primary (`ay`) axis, in radians per second. The second
/// axis is derived from the same angle inside `corner_cube_grid`, so this rate
/// alone sets how fast the pose advances per frame.
const SPIN_RATE: f32 = 1.15;

/// The angle at which the cube's tumble returns to its starting pose. The two
/// spin rates are `ay = angle` and `ax = angle * 0.47 + 0.5`; `ay` repeats every
/// TAU and `ax` every TAU/0.47, so the combined pose repeats only when the angle
/// has advanced a common multiple of both -- 100*TAU, since 0.47 = 47/100 clears
/// its denominator at 100 turns.
const SPIN_PERIOD: f32 = std::f32::consts::TAU * 100.0;

/// The pose to draw after `elapsed` of animation.
///
/// Wrapped so a long-lived process keeps f32 precision. Unbounded, the angle
/// reaches ~10^5 rad after a day or so, where an f32 step is ~0.01 rad and the
/// rotation visibly ratchets. We cannot wrap at TAU, though: the two spin rates
/// are `angle` (rate 1) and `angle * 0.47`, so a TAU wrap of the raw angle would
/// snap the second rotation by 0.47*TAU and jump the pose. Both rates realign to
/// their start only after the angle advances a full `SPIN_PERIOD`; wrapping there
/// bounds the magnitude while the pose sequence stays continuous across the seam.
pub(super) fn spin_angle(elapsed: Duration) -> f32 {
    (elapsed.as_secs_f32() * SPIN_RATE).rem_euclid(SPIN_PERIOD)
}

/// The three palette seeds the cube shades between: `accent` is the body of the
/// ramp, `surface` its dim end (see `cube_shadow`) and `highlight` its lit end.
///
/// Raw `Rgb` rather than the picker's `Theme`, because the ramp interpolates in
/// RGB space and so needs the seeds themselves -- and taking nothing but the
/// seeds is what proves the cube cannot reach into the rest of the UI's styling.
pub(super) struct Ink {
    pub(super) accent: Rgb,
    pub(super) surface: Rgb,
    pub(super) highlight: Rgb,
}

/// A palette seed as a terminal color.
fn color_of(rgb: Rgb) -> Color {
    Color::Rgb(rgb.0, rgb.1, rgb.2)
}

/// The 256 Braille cell glyphs (U+2800..U+28FF), mapped from a `CubeCell`'s
/// glyph char to a shared `&'static str`. The renderer draws one per lit cell
/// every frame; without this each would `char::to_string()` into a fresh heap
/// `String` (~800 allocations/sec at the animation rate). The table is built
/// once on first use and every frame after borrows from it.
fn braille_glyph(glyph: char) -> &'static str {
    static TABLE: std::sync::OnceLock<[String; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        std::array::from_fn(|i| {
            char::from_u32(0x2800 + i as u32)
                .expect("U+2800..=U+28FF are all valid scalars")
                .to_string()
        })
    });
    let idx = (glyph as u32).wrapping_sub(0x2800) as usize;
    table.get(idx).map(String::as_str).unwrap_or("\u{2800}")
}

/// Paint the cube into the gutter the caller reserved for it. That area is
/// already excluded from the list and preview, so filling it opaquely cannot
/// clip a path or break the selection bar.
///
/// Always emits a real `Color::Rgb`, never `Color::Reset`. The cube is color-only
/// chrome and the caller gates it on color being enabled, so a colorless frame
/// never reaches here -- `corner_3d_render_is_a_no_op_when_colorless` in
/// `picker.rs` pins that gate from the far side.
pub(super) fn render(frame: &mut Frame, area: Rect, angle: f32, ink: Ink) {
    let grid = corner_cube_grid(angle, area.width as usize, area.height as usize);
    if grid.is_empty() {
        return;
    }
    // Map each cell's depth-derived light onto a shadow -> accent -> highlight
    // ramp, so near edges read hot and far ones sink toward the background.
    // On a light palette `highlight` is the darkest ink and `shadow` sits
    // nearest the surface, which inverts the ramp and still lands on the cue
    // that matters: near edges high-contrast, far edges low.
    let shadow = cube_shadow(ink.accent, ink.surface);
    let accent = ink.accent;
    let highlight = ink.highlight;
    let ramp = |light: f32| -> Color {
        let color = if light < 0.5 {
            lerp_rgb(shadow, accent, light / 0.5)
        } else {
            lerp_rgb(accent, highlight, (light - 0.5) / 0.5)
        };
        color_of(color)
    };
    let surface_bg = color_of(ink.surface);
    let lines: Vec<Line> = grid
        .into_iter()
        .map(|row| {
            let spans: Vec<Span> = row
                .into_iter()
                .map(|cell| match cell {
                    Some(c) => {
                        // One color per cell is all a terminal offers, so the
                        // cell takes the coverage-weighted light of its lit dots.
                        // `braille_glyph` hands back a `&'static str` from a table
                        // built once, sparing the ~800 heap allocations/sec the
                        // old per-cell `glyph.to_string()` cost.
                        Span::styled(
                            braille_glyph(c.glyph),
                            Style::default().fg(ramp(c.light)).bg(surface_bg),
                        )
                    }
                    None => Span::raw(" "),
                })
                .collect();
            Line::from(spans)
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(surface_bg)),
        area,
    );
}

/// The dim end of the cube's shading ramp. Pulls the accent past halfway toward
/// the surface so receding edges stay hue-tinted rather than washing out to
/// gray, while sitting far enough from the lit end to read as depth.
///
/// The pull is 0.55 rather than nearer the surface on purpose. `CUBE_AMBIENT`
/// was lowered to give the lighting its dynamic range back, which drops the
/// darkest edges further down this ramp; pulling all the way to the surface
/// from there would fade the dimmest silhouette edges into the background,
/// breaking the outline. Measured against surface across the dark palettes and
/// daylight, 0.55 keeps even the darkest edge clearly separated from the
/// background while the shadow end still reads as the dim, receding one.
fn cube_shadow(accent: Rgb, surface: Rgb) -> Rgb {
    lerp_rgb(accent, surface, 0.55)
}

/// One rendered terminal cell of the cube: a Braille glyph packing a 2x4 dot
/// matrix, plus the diffuse light (0..1) averaged over its lit dots.
///
/// Braille beats the block-quadrant glyphs it replaced on both axes that
/// matter here. Resolution: 8 dots per cell instead of 4, so a line is a thin
/// stroke rather than a chunky stair-step. Aspect: a terminal cell is about
/// twice as tall as it is wide, so a 2x4 split yields near-square dots, while
/// a 2x2 split yields dots twice as tall as they are wide -- that stretch is
/// what made the old cube look sheared. The whole Braille block is also East
/// Asian Width "Neutral", so nothing here can widen to two columns and tear the
/// drawing apart in a CJK-configured terminal; the full block U+2588 that the
/// quadrant encoding reached for on a solid cell is "Ambiguous" and could.
#[derive(Debug, Clone, Copy)]
struct CubeCell {
    glyph: char,
    light: f32,
}

/// Sub-pixel dot canvas carrying, per dot, its accumulated ink coverage (0..1)
/// plus the nearest depth and the light at that depth. Bundling the buffers keeps
/// the rasterizer signatures small (clippy caps args at 7).
struct DotCanvas {
    width: usize,
    height: usize,
    coverage: Vec<f32>,
    depth: Vec<f32>,
    light: Vec<f32>,
}

impl DotCanvas {
    fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            coverage: vec![0.0; width * height],
            depth: vec![f32::NEG_INFINITY; width * height],
            light: vec![0.0; width * height],
        }
    }

    /// Deposit `coverage` (0..1) of ink at a dot, carrying its `z` and `light`.
    ///
    /// Two things are resolved here, and deliberately differently:
    ///
    /// * **Colour follows depth.** Larger z is nearer the camera; where two edges
    ///   cross, the nearer one owns the dot's light, exactly as the old binary
    ///   plotter did. Only a strictly-nearer z reclaims the light, so the outcome
    ///   is independent of the order edges are drawn in.
    /// * **Coverage accumulates.** A nearer stroke that only *grazes* a dot must
    ///   not blank out the farther ink already sitting under it -- that would
    ///   trade a stair-step for a hole. So coverage saturating-adds rather than
    ///   overwriting: the dot ends up as inked as its busiest pair of strokes make
    ///   it (capped at full), while the *colour* is still the nearest stroke's.
    ///   The residual case -- a near sliver stealing the colour from a far solid
    ///   stroke beneath it -- is left as-is: at the near corner where crossings
    ///   actually happen the two strokes' brightnesses are close, so the visible
    ///   error is negligible and not worth a coverage-weighted colour blend here.
    fn plot_dot(&mut self, x: i32, y: i32, z: f32, light: f32, coverage: f32) {
        if coverage <= 0.0 {
            return;
        }
        if x < 0 || y < 0 || x as usize >= self.width || y as usize >= self.height {
            return;
        }
        let idx = y as usize * self.width + x as usize;
        if z > self.depth[idx] {
            self.depth[idx] = z;
            self.light[idx] = light;
        }
        self.coverage[idx] = (self.coverage[idx] + coverage).min(1.0);
    }

    /// Xiaolin Wu anti-aliased edge draw. Bresenham lit exactly one dot per step
    /// and hard-snapped it to the nearer grid line, which is what stair-stepped
    /// the old edges; Wu instead splits each step's ink across the two dots
    /// straddling the true line, weighted by how close the line runs to each. Fed
    /// the fractional endpoints `project_point` now returns, that turns a vertex
    /// creeping less than a dot per frame into a smooth shift of coverage between
    /// neighbours rather than a periodic one-dot jump.
    ///
    /// Depth and light are interpolated by the parameter `t` along the segment
    /// (0 at `start`, 1 at `end`) so an edge running front-to-back still fades as
    /// it recedes; `plot_dot` then composites the coverage and resolves depth.
    fn draw_edge(&mut self, start: (f32, f32, f32, f32), end: (f32, f32, f32, f32)) {
        let (mut x0, mut y0, mut z0, mut l0) = start;
        let (mut x1, mut y1, mut z1, mut l1) = end;

        // Wu marches the major axis one integer step at a time and splits ink
        // along the minor axis. Transpose steep lines so the major axis is always
        // x, then orient left-to-right so `t` runs forward with the endpoints.
        let steep = (y1 - y0).abs() > (x1 - x0).abs();
        if steep {
            std::mem::swap(&mut x0, &mut y0);
            std::mem::swap(&mut x1, &mut y1);
        }
        if x0 > x1 {
            std::mem::swap(&mut x0, &mut x1);
            std::mem::swap(&mut y0, &mut y1);
            std::mem::swap(&mut z0, &mut z1);
            std::mem::swap(&mut l0, &mut l1);
        }

        let dx = x1 - x0;
        // Coincident (or sub-dot) endpoints: one dot, no gradient to march.
        if dx.abs() < 1e-6 {
            let (px, py) = if steep { (y0, x0) } else { (x0, y0) };
            self.plot_dot(px.round() as i32, py.round() as i32, z0.max(z1), l0, 1.0);
            return;
        }
        let gradient = (y1 - y0) / dx;

        let fpart = |v: f32| v - v.floor();
        let rfpart = |v: f32| 1.0 - fpart(v);

        // Plot the two dots straddling minor coordinate `lower..lower+1` for the
        // step at major coordinate `maj`, honouring the steep transpose and
        // interpolating depth/light at `t`.
        let plot =
            |canvas: &mut Self, maj: i32, lower: i32, lower_cov: f32, upper_cov: f32, t: f32| {
                let z = z0 + (z1 - z0) * t;
                let light = l0 + (l1 - l0) * t;
                if steep {
                    canvas.plot_dot(lower, maj, z, light, lower_cov);
                    canvas.plot_dot(lower + 1, maj, z, light, upper_cov);
                } else {
                    canvas.plot_dot(maj, lower, z, light, lower_cov);
                    canvas.plot_dot(maj, lower + 1, z, light, upper_cov);
                }
            };

        // First endpoint. `xgap` fades the ink where the line starts partway into
        // a dot, so a sub-dot endpoint shift changes coverage smoothly.
        let xend1 = x0.round();
        let yend1 = y0 + gradient * (xend1 - x0);
        let xgap1 = rfpart(x0 + 0.5);
        let ypxl1 = yend1.floor();
        let t1 = ((xend1 - x0) / dx).clamp(0.0, 1.0);
        plot(
            self,
            xend1 as i32,
            ypxl1 as i32,
            rfpart(yend1) * xgap1,
            fpart(yend1) * xgap1,
            t1,
        );

        // Second endpoint.
        let xend2 = x1.round();
        let yend2 = y1 + gradient * (xend2 - x1);
        let xgap2 = fpart(x1 + 0.5);
        let ypxl2 = yend2.floor();
        let t2 = ((xend2 - x0) / dx).clamp(0.0, 1.0);
        plot(
            self,
            xend2 as i32,
            ypxl2 as i32,
            rfpart(yend2) * xgap2,
            fpart(yend2) * xgap2,
            t2,
        );

        // Interior: each step deposits a total of ~1 ink split across the pair, so
        // the busier dot always clears the membership threshold and the stroke
        // never gaps.
        let mut intery = yend1 + gradient;
        for maj in (xend1 as i32 + 1)..(xend2 as i32) {
            let lower = intery.floor();
            let t = ((maj as f32 - x0) / dx).clamp(0.0, 1.0);
            plot(self, maj, lower as i32, rfpart(intery), fpart(intery), t);
            intery += gradient;
        }
    }
}

/// Unit cube centered at the origin.
const CUBE_V: [(f32, f32, f32); 8] = [
    (-1.0, -1.0, -1.0),
    (1.0, -1.0, -1.0),
    (1.0, 1.0, -1.0),
    (-1.0, 1.0, -1.0),
    (-1.0, -1.0, 1.0),
    (1.0, -1.0, 1.0),
    (1.0, 1.0, 1.0),
    (-1.0, 1.0, 1.0),
];

const CUBE_EDGES: [(usize, usize); 12] = [
    (0, 1),
    (1, 2),
    (2, 3),
    (3, 0),
    (4, 5),
    (5, 6),
    (6, 7),
    (7, 4),
    (0, 4),
    (1, 5),
    (2, 6),
    (3, 7),
];

/// The six faces as (corner loop, outward normal). Used to hide the edges that
/// belong only to faces pointing away from the camera.
const CUBE_FACES: [([usize; 4], (f32, f32, f32)); 6] = [
    ([4, 5, 6, 7], (0.0, 0.0, 1.0)),
    ([0, 1, 2, 3], (0.0, 0.0, -1.0)),
    ([1, 2, 6, 5], (1.0, 0.0, 0.0)),
    ([0, 3, 7, 4], (-1.0, 0.0, 0.0)),
    ([3, 2, 6, 7], (0.0, 1.0, 0.0)),
    ([0, 1, 5, 4], (0.0, -1.0, 0.0)),
];

/// Camera distance along +z. Must exceed the cube's corner radius (sqrt(3)) so
/// no vertex passes through the eye.
const CUBE_CAMERA: f32 = 4.6;

/// Largest screen radius the projection can produce, and therefore what the
/// canvas has to accommodate. A corner rides the sphere of radius sqrt(3), so
/// its projected distance from center is
///
/// ```text
///   r(z) = sqrt(3 - z^2) * CUBE_CAMERA / (CUBE_CAMERA - z)
/// ```
///
/// which peaks at about z = 0.65 -- not at either extreme, because swinging a
/// corner toward the eye magnifies it but also foreshortens how far off-axis it
/// sits. Taking the naive bound (full radius at maximum magnification, a pose
/// no corner can actually reach) costs about a third of the drawing's size for
/// nothing. `corner_cube_span_is_a_tight_upper_bound` pins this value.
const CUBE_SPAN: f32 = 1.87;

/// Direction the light arrives from, in *view* space -- fixed relative to the
/// camera rather than to the cube, so the cube tumbles beneath a stationary
/// lamp and each face brightens as it turns into the light. Upper left and
/// tilted toward the viewer: the conventional key-light placement, and the one
/// that keeps at least one visible face well lit at any orientation. The tilt
/// toward the viewer (+z) is deliberately the largest component: a lamp that
/// leans overhead (+y dominant) leaves the side faces -- whose normals are
/// near-horizontal -- all pointing the same amount away from it, so they read
/// identically flat. Leaning the lamp toward the eye instead splays the side
/// faces across a range of angles, so each takes a distinct brightness. Unit
/// length, which `cube_light_direction_is_unit_length` pins.
const CUBE_LIGHT: (f32, f32, f32) = (-0.551380, 0.451129, 0.701757);

/// Floor under the diffuse term. A face turned fully from the lamp lands here
/// rather than at zero: pure Lambert would erase its edges entirely and punch
/// holes in the silhouette, so the cube has to stay whole even where it is
/// unlit. Kept low so the lit faces keep most of the dynamic range and the
/// sides show real gradation rather than sitting bunched near the floor.
const CUBE_AMBIENT: f32 = 0.10;

/// Contrast curve on the diffuse term. Half-Lambert (below) is intrinsically
/// low-contrast -- it compresses the whole sphere of normals into the upper
/// half of the range -- so a gamma > 1 pulls the midtones back down and
/// restores the sense of a shaped, directionally lit object. Tuned against a
/// 64-pose rotation sweep to hold the brightness spread across visible faces
/// even with ambient this low.
const CUBE_GAMMA: f32 = 1.8;

/// Diffuse brightness for a face, given its rotated unit normal. Returns
/// `CUBE_AMBIENT..=1.0`.
///
/// Uses half-Lambert -- `(dot + 1) / 2` with no clamp -- rather than clamped
/// Lambert. Clamped Lambert collapses every face at or past 90 degrees from the
/// lamp onto the exact same `max(0, dot) = 0`, i.e. the ambient floor; with an
/// overhead lamp that is most of the side faces at once, and two visible faces
/// pinned to an identical value read as unlit, not lit. Half-Lambert keeps
/// `dot` flowing through the full -1..1 range, so faces angled away from the
/// lamp still differ from one another and the shading never flat-lines. The
/// gamma curve pays back the contrast half-Lambert costs.
fn face_light(nx: f32, ny: f32, nz: f32) -> f32 {
    let (lx, ly, lz) = CUBE_LIGHT;
    // (dot + 1) / 2 is mathematically in 0..1, but a dot of exactly -1 (a face
    // pointing dead away from the lamp) can land a hair below zero in f32, and
    // a negative base under a fractional gamma is NaN -- clamp so the floor
    // stays the floor rather than blowing up the whole cell.
    let half_lambert = (((nx * lx + ny * ly + nz * lz) + 1.0) * 0.5).max(0.0);
    CUBE_AMBIENT + (1.0 - CUBE_AMBIENT) * half_lambert.powf(CUBE_GAMMA)
}

/// Depth's remaining contribution, now that lighting carries the shading. Kept
/// deliberately narrow: it kicks in along an edge running away from the viewer,
/// where lighting alone is constant and would flatten the recession.
fn depth_falloff(depth: f32) -> f32 {
    0.78 + 0.22 * depth
}

fn cube_edge_index(a: usize, b: usize) -> Option<usize> {
    CUBE_EDGES
        .iter()
        .position(|&(p, q)| (p == a && q == b) || (p == b && q == a))
}

/// Coverage a Braille dot must reach before it counts as part of the glyph. This
/// is the one place coverage must collapse to a binary yes/no -- a terminal cell
/// either sets a dot's bit or it does not. Wu splits each step's ink across the
/// two dots straddling the line, and that split always sums to ~1, so the busier
/// of the pair is always >= 0.5: a threshold at or below 0.5 therefore lights at
/// least one dot per step and a stroke never gaps. 0.35 sits below the 0.5 an
/// even straddle lands on -- so a line running exactly between two dot rows lights
/// both and reads as the ~1.5-dot stroke it is -- yet well above the sliver a line
/// merely grazing a dot deposits, so strokes stay a crisp one-to-two dots wide
/// instead of blooming to double width. (Cell *brightness*, unlike membership, is
/// left continuous and weighted by coverage -- see the cell-assembly loop.)
const CUBE_DOT_ON: f32 = 0.35;

/// Rasterize a rotating cube into Braille sub-pixels. Each terminal cell holds
/// a 2x4 dot matrix, so a `width x height` cell area gives a `2*width x
/// 4*height` dot canvas.
///
/// Two things make this read as a solid object rather than a flat tangle:
///
/// * **Hidden-line removal.** Only edges bordering a camera-facing face are
///   drawn. A full 12-edge wireframe is a Necker cube -- genuinely ambiguous,
///   it visibly flips inside-out as it turns. Culling the back edges leaves the
///   9-edge silhouette the eye already knows as a cube, so the rotation reads
///   in one consistent direction.
/// * **Diffuse lighting.** Each vertex is lit from the averaged normal of the
///   visible faces meeting there, and an edge fades between its two endpoint
///   brightnesses, so a face turning toward the lamp lifts its whole outline as
///   a smooth gradient. Depth then modulates that slightly, so the two cues
///   reinforce rather than compete.
fn corner_cube_grid(angle: f32, width: usize, height: usize) -> Vec<Vec<Option<CubeCell>>> {
    if width == 0 || height == 0 {
        return Vec::new();
    }

    let dot_w = width * 2;
    let dot_h = height * 4;
    let mut canvas = DotCanvas::new(dot_w, dot_h);

    let mut proj = [(0f32, 0f32, 0f32); 8];
    let mut depth_shade = [0f32; 8];
    for (i, &(x, y, z)) in CUBE_V.iter().enumerate() {
        let (x, y, z) = tumble(x, y, z, angle);
        // z spans about -1.73..1.73; map that to 0..1.
        depth_shade[i] = (0.5 + 0.5 * (z / 1.74)).clamp(0.0, 1.0);
        proj[i] = project_point(x, y, z, dot_w, dot_h);
    }

    // Smooth (Gouraud) shading from per-vertex normals. A face is camera-facing
    // when its rotated outward normal still points toward +z (the eye), which is
    // also the culling test -- the same normal decides both whether an edge is
    // drawn and how it is shaded. For each vertex we sum the normals of the
    // *visible* faces meeting there and light the normalized result; an edge then
    // gets a distinct brightness at each end and `draw_edge` interpolates between
    // them, so a face reads as a shaped gradient rather than one flat tone.
    //
    // This replaces the old per-edge flat average of adjacent face lights. Only
    // visible faces contribute a vertex's normal, so a silhouette vertex leans
    // toward its single visible face while the near-corner vertex blends three --
    // and crucially, two faces placed symmetrically about the lamp still hand a
    // shared vertex *different* normals, so the seam between them never flat-lines
    // the way a fill light (rejected: it buys outlier removal with global
    // contrast) would leave it.
    let mut vnormal = [(0f32, 0f32, 0f32); 8];
    let mut edge_drawn = [false; 12];
    for (corners, normal) in CUBE_FACES {
        let (nx, ny, nz) = normal;
        let (nx, ny, nz) = tumble(nx, ny, nz, angle);
        if nz <= 0.0 {
            continue;
        }
        for i in 0..4 {
            let v = &mut vnormal[corners[i]];
            v.0 += nx;
            v.1 += ny;
            v.2 += nz;
            if let Some(edge) = cube_edge_index(corners[i], corners[(i + 1) % 4]) {
                edge_drawn[edge] = true;
            }
        }
    }

    // Light each vertex from its accumulated normal. A drawn edge always borders a
    // visible face, so both its endpoints have a non-zero accumulated normal here.
    let mut vlight = [0.0f32; 8];
    for (i, &(nx, ny, nz)) in vnormal.iter().enumerate() {
        let len = (nx * nx + ny * ny + nz * nz).sqrt();
        if len > 1e-6 {
            vlight[i] = face_light(nx / len, ny / len, nz / len);
        }
    }

    for (i, &(a, b)) in CUBE_EDGES.iter().enumerate() {
        if !edge_drawn[i] {
            continue;
        }
        let (ax0, ay0, az0) = proj[a];
        let (bx0, by0, bz0) = proj[b];
        canvas.draw_edge(
            (ax0, ay0, az0, vlight[a] * depth_falloff(depth_shade[a])),
            (bx0, by0, bz0, vlight[b] * depth_falloff(depth_shade[b])),
        );
    }

    // Braille dot -> bit layout within a 2x4 cell. The low six bits run down
    // the two columns, and the bottom row is the high two bits -- a historical
    // quirk of the 8-dot encoding, hence the table rather than a formula.
    //   (col, row) -> bit
    const BRAILLE_BITS: [[u8; 4]; 2] = [
        [0x01, 0x02, 0x04, 0x40], // left column, top to bottom
        [0x08, 0x10, 0x20, 0x80], // right column, top to bottom
    ];

    (0..height)
        .map(|cy| {
            (0..width)
                .map(|cx| {
                    let mut bits = 0u8;
                    let mut ink_sum = 0.0f32;
                    let mut members = 0u32;
                    for (col, column) in BRAILLE_BITS.iter().enumerate() {
                        for (row, bit) in column.iter().enumerate() {
                            let idx = (cy * 4 + row) * dot_w + cx * 2 + col;
                            let cov = canvas.coverage[idx];
                            // Membership is binary -- the dot's bit is set only
                            // once it holds a real share of a stroke. Brightness,
                            // though, stays continuous: each member dot's light is
                            // weighted by its coverage, so a cell a stroke merely
                            // grazes (few dots, low coverage) reads faint while one
                            // a stroke crosses through the middle (many dots near
                            // full coverage) reads at full brightness. The old mean
                            // of lit-dot light ignored coverage entirely, so a cell
                            // grazed by one dot was as bright as one packed solid.
                            if cov >= CUBE_DOT_ON {
                                bits |= bit;
                                ink_sum += cov * canvas.light[idx];
                                members += 1;
                            }
                        }
                    }
                    if bits == 0 {
                        None
                    } else {
                        Some(CubeCell {
                            glyph: char::from_u32(0x2800 + bits as u32).unwrap_or('\u{2800}'),
                            light: ink_sum / members as f32,
                        })
                    }
                })
                .collect()
        })
        .collect()
}

/// The pose at `angle`: tumble on two axes at unrelated rates so the cube keeps
/// presenting new orientations rather than looping through one. `ay` takes the
/// angle straight, `ax` a slower derived one (the offset just avoids starting
/// square-on to the camera).
///
/// One function rather than the two calls open-coded at each site, because
/// rotations do not commute and the order *is* the pose: it used to be restated
/// at six places -- twice in the rasterizer, four times in mirrors under
/// `mod tests` -- so swapping the rasterizer's two lines changed the rendered
/// cube completely while every mirror went on agreeing with itself and the whole
/// suite stayed green. With one definition, the order is pinned in one place
/// (`tumble_composes_the_two_axes_in_a_pinned_order`) and the mirrors describe
/// the pose the renderer actually draws.
fn tumble(x: f32, y: f32, z: f32, angle: f32) -> (f32, f32, f32) {
    let (x, y, z) = rotate_y(x, y, z, angle);
    rotate_x(x, y, z, angle * 0.47 + 0.5)
}

fn rotate_y(x: f32, y: f32, z: f32, angle: f32) -> (f32, f32, f32) {
    let (s, c) = angle.sin_cos();
    (x * c + z * s, y, -x * s + z * c)
}

fn rotate_x(x: f32, y: f32, z: f32, angle: f32) -> (f32, f32, f32) {
    let (s, c) = angle.sin_cos();
    (x, y * c - z * s, y * s + z * c)
}

/// Perspective-project a rotated point into dot-canvas coordinates.
///
/// Larger z is nearer the eye, so the divisor shrinks as z grows and near
/// corners are drawn *larger*. (The reverse -- near corners drawn smaller while
/// shaded brighter -- is what previously made the cube read as a flat smear:
/// the size and lighting cues pointed opposite ways.)
///
/// Braille dots are near-square, so the same scale applies to both axes.
///
/// The screen coordinates are returned *fractional*, not snapped to the nearest
/// dot. Rounding here is what made the cube stutter: at the running spin rate a
/// vertex advances only ~0.7 dots per frame, so a rounded position sat still for
/// a frame or so and then jumped a whole dot every ~70ms. Handing the fractional
/// position to Wu's anti-aliased rasterizer instead lets that sub-dot motion show
/// as a smooth shift of coverage between neighbouring dots.
fn project_point(x: f32, y: f32, z: f32, width: usize, height: usize) -> (f32, f32, f32) {
    let factor = CUBE_CAMERA / (CUBE_CAMERA - z);
    // Fill the canvas, keeping one dot of margin on every side.
    let scale = (width.min(height) as f32 * 0.5 - 1.0) / CUBE_SPAN;
    let px = width as f32 * 0.5 + x * scale * factor;
    let py = height as f32 * 0.5 - y * scale * factor;
    (px, py, z)
}

/// Linear blend between two RGB seeds; `t` in 0..1 moves `from` -> `to`.
fn lerp_rgb(from: Rgb, to: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    Rgb(mix(from.0, to.0), mix(from.1, to.1), mix(from.2, to.2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corner_cube_grid_draws_a_depth_shaded_braille_wireframe() {
        let grid = corner_cube_grid(0.7, WIDTH as usize, HEIGHT as usize);
        assert_eq!(grid.len(), HEIGHT as usize);
        assert!(grid.iter().all(|row| row.len() == WIDTH as usize));
        let cells: Vec<CubeCell> = grid.iter().flatten().flatten().copied().collect();
        // An edge outline, not a fill: some cells lit, nowhere near all of them.
        //
        // Re-derived for anti-aliasing. Bresenham lit one dot per step, so this
        // pose drew ~30 cells and the old 12..80 bracket fit. Wu now splits each
        // step's ink across the two straddling dots, and the 0.35 membership
        // threshold keeps the busier one (plus, at a true straddle, both), so a
        // stroke reads 1-2 dots wide and this pose lights 46 of the 98 (14x7)
        // cells. The bracket is derived from that, not widened to fit:
        //   * upper 60 (~60% of the grid) rejects a fill -- interior spill would
        //     climb toward 98, and the busiest pose over a full turn still only
        //     reaches 49, so 60 clears the real outline yet bars a fill;
        //   * lower 34 (~a third of the grid) rejects a collapsed outline -- if
        //     the membership threshold were cranked so high it dropped the AA body
        //     (e.g. 0.9), this pose falls to ~20 and trips the floor.
        assert!(
            (34..60).contains(&cells.len()),
            "expected an edge outline, got {} cells",
            cells.len()
        );
        // Braille only -- and never the blank pattern U+2800, which would be an
        // invisible cell masquerading as drawn content.
        assert!(
            cells
                .iter()
                .all(|c| ('\u{2801}'..='\u{28FF}').contains(&c.glyph)),
            "all glyphs should be non-blank braille, got {:?}",
            cells.iter().map(|c| c.glyph).collect::<Vec<_>>()
        );
        assert!(cells.iter().all(|c| (0.0..=1.0).contains(&c.light)));
        // Depth shading must produce real contrast between near and far edges.
        let max_light = cells.iter().map(|c| c.light).fold(0.0f32, f32::max);
        let min_light = cells.iter().map(|c| c.light).fold(1.0f32, f32::min);
        assert!(
            max_light - min_light > 0.2,
            "expected near/far depth contrast, got {min_light}..{max_light}"
        );
    }

    #[test]
    fn corner_cube_hides_edges_facing_away_from_the_camera() {
        // A cube shows at most 9 of its 12 edges from any viewpoint (7 when a
        // face is dead-on). Drawing all 12 is the ambiguous Necker wireframe
        // this renderer deliberately avoids, so sample the spin and prove the
        // back edges really are dropped.
        for step in 0..48 {
            let angle = step as f32 * 0.13;
            let drawn = cube_visible_edges(angle);
            assert!(
                (7..=9).contains(&drawn),
                "angle {angle}: {drawn} edges drawn, expected a culled 7..=9"
            );
        }
    }

    /// Count the edges `corner_cube_grid` would draw at `angle`, mirroring its
    /// culling rule so the test asserts on the rule rather than on pixels. The
    /// pose comes from `tumble`, the same function the renderer poses with, so
    /// the mirror describes the cube actually drawn rather than a restatement.
    fn cube_visible_edges(angle: f32) -> usize {
        let mut visible = [false; 12];
        for (corners, normal) in CUBE_FACES {
            let (nx, ny, nz) = normal;
            let (_, _, nz) = tumble(nx, ny, nz, angle);
            if nz <= 0.0 {
                continue;
            }
            for i in 0..4 {
                visible[cube_edge_index(corners[i], corners[(i + 1) % 4]).unwrap()] = true;
            }
        }
        visible.iter().filter(|v| **v).count()
    }

    #[test]
    fn corner_cube_never_clips_against_its_canvas() {
        // `plot_dot` silently drops out-of-bounds dots, so asserting that lit
        // cells are in bounds proves nothing -- it is true even for a cube
        // scaled ten times too large. Assert on the projection instead: every
        // corner, at every orientation, must land inside the dot canvas. That
        // is what actually fails when the scale is too generous.
        let (dot_w, dot_h) = (WIDTH as usize * 2, HEIGHT as usize * 4);
        for step in 0..360 {
            let angle = step as f32 * 0.0349;
            for &(x, y, z) in CUBE_V.iter() {
                let (x, y, z) = tumble(x, y, z, angle);
                let (px, py, _) = project_point(x, y, z, dot_w, dot_h);
                // Fractional now, so bound the continuous coordinate against the
                // canvas: a corner must land within [0, dim) even before the
                // rasterizer floors it to a dot.
                assert!(
                    px >= 0.0 && px < dot_w as f32 && py >= 0.0 && py < dot_h as f32,
                    "angle {angle}: corner projected to ({px},{py}), outside {dot_w}x{dot_h}"
                );
            }
        }
        // And the cube must actually be there and reasonably large -- a scale
        // that is too small would pass the bounds check above trivially.
        let grid = corner_cube_grid(0.4, WIDTH as usize, HEIGHT as usize);
        let lit = grid.iter().flatten().filter(|c| c.is_some()).count();
        assert!(
            lit >= 20,
            "cube should fill its corner, only {lit} cells lit"
        );
    }

    #[test]
    fn cube_light_direction_is_unit_length() {
        let (lx, ly, lz) = CUBE_LIGHT;
        let len = (lx * lx + ly * ly + lz * lz).sqrt();
        assert!(
            (len - 1.0).abs() < 1e-3,
            "CUBE_LIGHT must be normalized or the diffuse term is scaled, got {len}"
        );
    }

    #[test]
    fn cube_face_light_tracks_the_normal_against_the_lamp() {
        let (lx, ly, lz) = CUBE_LIGHT;
        // Facing straight into the light: fully lit.
        assert!((face_light(lx, ly, lz) - 1.0).abs() < 1e-3);
        // Facing straight away: this is the only orientation that reaches the
        // floor. Under half-Lambert the floor is a single point, not a whole
        // hemisphere -- that is the fix; a face merely angled off the lamp must
        // stay above it.
        assert!((face_light(-lx, -ly, -lz) - CUBE_AMBIENT).abs() < 1e-3);
        // Perpendicular to the lamp: half-Lambert puts dot = 0 at the midpoint
        // of the curve, so this sits strictly between floor and full rather
        // than pinned to the floor the way clamped Lambert left it.
        let (px, py, pz) = (-ly, lx, 0.0);
        let perp = face_light(px, py, pz);
        assert!(
            perp > CUBE_AMBIENT + 0.02 && perp < 1.0,
            "an edge-on face must lift off the floor under half-Lambert, got {perp}"
        );
        // Monotonic in between, so a face turning toward the lamp brightens, and
        // strictly brighter than the perpendicular face it turned from.
        let half = face_light(
            (lx + px) / 2f32.sqrt(),
            (ly + py) / 2f32.sqrt(),
            (lz + pz) / 2f32.sqrt(),
        );
        assert!(
            half > perp && half < 1.0,
            "a half-turned face should sit between the edge-on face and full, got {half}"
        );
    }

    #[test]
    fn cube_shading_depends_on_orientation_not_only_depth() {
        // The distinguishing property of lighting over the depth-only shading
        // this replaced: hold depth constant and brightness must still change.
        // A face's depth cue is symmetric about the z axis, so a pose and its
        // mirror share depths while presenting different normals to the lamp.
        let brightest = |angle: f32| -> f32 {
            corner_cube_grid(angle, WIDTH as usize, HEIGHT as usize)
                .iter()
                .flatten()
                .flatten()
                .map(|c| c.light)
                .fold(0.0f32, f32::max)
        };
        // Sweep a full turn and require the peak brightness itself to vary --
        // under depth-only shading the near corner is always at max depth, so
        // the peak would be pinned and this spread would be ~0.
        let peaks: Vec<f32> = (0..24).map(|i| brightest(i as f32 * 0.26)).collect();
        let spread = peaks.iter().cloned().fold(0.0f32, f32::max)
            - peaks.iter().cloned().fold(1.0f32, f32::min);
        assert!(
            spread > 0.1,
            "peak brightness should swing as faces turn into the lamp, got {spread} over {peaks:?}"
        );
    }

    /// The per-face diffuse brightness for every camera-facing face at `angle`,
    /// mirroring `corner_cube_grid`'s cull-then-light rule so the test asserts
    /// on the lighting model rather than on rasterized pixels.
    fn cube_visible_face_lights(angle: f32) -> Vec<f32> {
        let mut lights = Vec::new();
        for (_corners, normal) in CUBE_FACES {
            let (nx, ny, nz) = normal;
            let (nx, ny, nz) = tumble(nx, ny, nz, angle);
            if nz <= 0.0 {
                continue;
            }
            lights.push(face_light(nx, ny, nz));
        }
        lights
    }

    #[test]
    fn cube_never_pins_two_visible_faces_to_the_ambient_floor() {
        // The defect this shading was rebuilt to kill: clamped Lambert sends
        // every face at or past 90 degrees from the lamp to `max(0, dot) = 0`,
        // i.e. the exact ambient floor. With more than a couple such faces
        // visible at once, two of the three the eye can see sit at an identical
        // dead value and the cube stops reading as lit at all. Half-Lambert
        // makes the floor a single unreachable-in-practice point (only the
        // normal pointing dead away from the lamp), so at most one visible face
        // can ever touch it.
        //
        // Sweep a full turn and assert no pose puts two visible faces on the
        // floor together. This fails against the old clamped model -- verified
        // by mutation: temporarily restoring `max(0.0, dot)` in `face_light`
        // pins two-plus faces to the floor in 9 of these 64 poses.
        for step in 0..64 {
            let angle = step as f32 / 64.0 * std::f32::consts::TAU;
            let lights = cube_visible_face_lights(angle);
            let at_floor = lights
                .iter()
                .filter(|l| (**l - CUBE_AMBIENT).abs() < 1e-3)
                .count();
            assert!(
                at_floor < 2,
                "angle {angle}: {at_floor} visible faces pinned to the ambient \
                 floor together -- that is the flat-shading defect, lights {lights:?}"
            );
        }
    }

    /// The per-vertex diffuse brightness the renderer now shades edges with:
    /// average the normals of the *visible* faces meeting at each vertex,
    /// normalize, light that. Mirrors `corner_cube_grid` so the test asserts on
    /// the shading model rather than on rasterized pixels. Only vertices touched
    /// by a visible face (a non-zero accumulated normal) are returned -- those are
    /// exactly the endpoints of the edges that get drawn.
    fn cube_visible_vertex_lights(angle: f32) -> Vec<f32> {
        let mut vnormal = [(0f32, 0f32, 0f32); 8];
        for (corners, normal) in CUBE_FACES {
            let (nx, ny, nz) = normal;
            let (nx, ny, nz) = tumble(nx, ny, nz, angle);
            if nz <= 0.0 {
                continue;
            }
            for &c in &corners {
                vnormal[c].0 += nx;
                vnormal[c].1 += ny;
                vnormal[c].2 += nz;
            }
        }
        vnormal
            .iter()
            .filter_map(|&(nx, ny, nz)| {
                let len = (nx * nx + ny * ny + nz * nz).sqrt();
                (len > 1e-6).then(|| face_light(nx / len, ny / len, nz / len))
            })
            .collect()
    }

    #[test]
    fn cube_never_pins_two_visible_vertices_to_the_ambient_floor() {
        // The flat-shading floor defect, restated at the level shading now
        // happens. With per-vertex normals the edges are lit from vertex
        // brightnesses, not face brightnesses, so the invariant that keeps the
        // cube from reading as unlit is now vertex-level: no pose may pin two of
        // the drawn vertices to the ambient floor at once.
        //
        // This is a strictly stronger guarantee than the face-level one, and for
        // a good reason: a vertex normal is the average of two or three visible
        // face normals, so it only reaches the floor if that *average* points
        // dead away from the lamp -- rarer than any single face doing so. Over a
        // full turn no vertex touches the floor at all, so any pair-at-floor pose
        // would signal the averaging or culling had broken.
        for step in 0..64 {
            let angle = step as f32 / 64.0 * std::f32::consts::TAU;
            let lights = cube_visible_vertex_lights(angle);
            let at_floor = lights
                .iter()
                .filter(|l| (**l - CUBE_AMBIENT).abs() < 1e-3)
                .count();
            assert!(
                at_floor < 2,
                "angle {angle}: {at_floor} drawn vertices pinned to the ambient \
                 floor together, lights {lights:?}"
            );
        }
    }

    #[test]
    fn corner_cube_vertices_advance_smoothly_between_frames() {
        // Regression cover for the stutter in change (1), which had none. The old
        // `project_point` ended in `.round()`, snapping every vertex to an integer
        // dot. At the spin rate a vertex crosses only ~0.7 dots per frame, so a
        // rounded vertex sat frozen for a frame or two and then jumped a whole dot
        // -- a visible ~72ms ratchet, temporal and independent of resolution.
        // Fractional projection feeding Wu makes that motion continuous.
        //
        // Assert it on the projected vertex path, not on a whole-grid coverage
        // sum: culling legitimately pops whole edges in and out as faces cross the
        // horizon, which would swamp the sub-dot signal. A single vertex's screen
        // position has no such discontinuity, so bound its second difference --
        // discrete acceleration. Smooth sub-dot motion keeps this near zero; a
        // staircase of whole-dot jumps spikes it (a one-dot jump that reverses
        // across three frames contributes a second difference of order sqrt(2)).
        //
        // Load-bearing, proven by mutation: restoring `.round()` in
        // `project_point` lifts the measured maximum from ~0.08 to ~2.24, far past
        // this 0.5 bound, and the test fails. (See the change report.)
        let (dw, dh) = (WIDTH as usize * 2, HEIGHT as usize * 4);
        // One frame's worth of pose advance. Read off the named constants rather
        // than restated as literals: the bound below is only meaningful at the
        // real spin rate and the real frame interval, so a change to either has
        // to move this test with it.
        let dangle = SPIN_RATE * FRAME.as_secs_f32();
        let pos = |vx: f32, vy: f32, vz: f32, a: f32| {
            let (x, y, z) = tumble(vx, vy, vz, a);
            let (px, py, _) = project_point(x, y, z, dw, dh);
            (px, py)
        };
        let mut worst = 0.0f32;
        for &(vx, vy, vz) in CUBE_V.iter() {
            for step in 0..1200 {
                let a = step as f32 * dangle;
                let (x0, y0) = pos(vx, vy, vz, a);
                let (x1, y1) = pos(vx, vy, vz, a + dangle);
                let (x2, y2) = pos(vx, vy, vz, a + 2.0 * dangle);
                let ddx = x2 - 2.0 * x1 + x0;
                let ddy = y2 - 2.0 * y1 + y0;
                worst = worst.max((ddx * ddx + ddy * ddy).sqrt());
            }
        }
        assert!(
            worst < 0.5,
            "vertex screen path should advance smoothly (second difference), got {worst}"
        );
    }

    #[test]
    fn corner_cube_span_is_a_tight_upper_bound() {
        // Sweep the corner sphere and confirm CUBE_SPAN both covers the real
        // maximum (or the cube clips) and does not overshoot it (or the cube is
        // needlessly small). Guards the hand-derived constant against a later
        // change to CUBE_CAMERA silently invalidating it.
        let mut peak = 0.0f32;
        for i in 0..=2000 {
            let z = -3f32.sqrt() + (2.0 * 3f32.sqrt()) * (i as f32 / 2000.0);
            let r = (3.0 - z * z).max(0.0).sqrt() * CUBE_CAMERA / (CUBE_CAMERA - z);
            peak = peak.max(r);
        }
        assert!(
            peak <= CUBE_SPAN,
            "CUBE_SPAN {CUBE_SPAN} must cover the peak {peak} or the cube clips"
        );
        assert!(
            CUBE_SPAN - peak < 0.05,
            "CUBE_SPAN {CUBE_SPAN} wastes {} of canvas over peak {peak}",
            CUBE_SPAN - peak
        );
    }

    #[test]
    fn corner_cube_projects_near_corners_larger_than_far_ones() {
        // Perspective and shading must agree: the corner swung toward the eye
        // is both brighter and further from the center. When these disagree the
        // cube reads inside-out.
        let center = 14.0f32;
        let (near_x, _, near_z) = project_point(1.0, 0.0, 1.5, 28, 28);
        let (far_x, _, far_z) = project_point(1.0, 0.0, -1.5, 28, 28);
        assert!(near_z > far_z, "z ordering: {near_z} should exceed {far_z}");
        assert!(
            (near_x - center).abs() > (far_x - center).abs(),
            "near corner must project wider: near {near_x}, far {far_x}"
        );
    }

    #[test]
    fn spin_angle_advances_at_the_spin_rate_and_wraps_seamlessly() {
        // New cover for the wrap, which had none: it used to be an expression
        // inside `App::corner_anim_angle` and nothing pinned either half of it.
        assert!(
            (spin_angle(Duration::from_secs(1)) - SPIN_RATE).abs() < 1e-3,
            "one second of animation should advance one SPIN_RATE"
        );

        // The wrap must land where *both* tumble axes are back at their starting
        // phase, or the pose jumps once per period. Sample the two axes a hair
        // before and a hair after the seam and require each to be continuous
        // modulo a full turn. Wrapping at TAU instead of SPIN_PERIOD passes the
        // `ay` half and fails the `ax` half by 0.47*TAU, which is what makes this
        // load-bearing rather than a restatement of the formula.
        let at = |angle: f32| spin_angle(Duration::from_secs_f32(angle / SPIN_RATE));
        let gap = |a: f32, b: f32| {
            let tau = std::f32::consts::TAU;
            (a - b).rem_euclid(tau).min((b - a).rem_euclid(tau))
        };
        let step = 0.01;
        let before = at(SPIN_PERIOD - step);
        let after = at(SPIN_PERIOD + step);
        assert!(
            after < before,
            "the angle must actually wrap at SPIN_PERIOD, got {before} then {after}"
        );
        let axis_gaps = [
            gap(before, after),
            gap(before * 0.47 + 0.5, after * 0.47 + 0.5),
        ];
        for (axis, observed) in ["ay", "ax"].iter().zip(axis_gaps) {
            assert!(
                observed < 10.0 * step,
                "{axis} jumps {observed} rad across the wrap: the period is not a whole pose"
            );
        }
    }

    #[test]
    fn tumble_composes_the_two_axes_in_a_pinned_order() {
        // A golden pose, not a restatement of `tumble`: it pins the composition
        // order *and* the two rotation matrices, so a swapped order or a flipped
        // sine sign both show up here. Every other cube assertion is a property
        // (dot counts, visible-edge counts, brightness spreads) that any rigid
        // rotation satisfies, so this is the only place the pose itself is fixed.
        let angle = 0.7;
        let corner = (-1.0, 1.0, -1.0);
        let golden = (-1.409_06, 0.764_545, 0.655_761);
        let observed = tumble(corner.0, corner.1, corner.2, angle);
        let dist = |a: (f32, f32, f32), b: (f32, f32, f32)| {
            ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2) + (a.2 - b.2).powi(2)).sqrt()
        };
        assert!(
            dist(observed, golden) < 1e-4,
            "tumble{corner:?} at {angle} rad is {observed:?}, expected {golden:?}"
        );

        // And the golden discriminates: rotating x first lands somewhere else
        // entirely, so the assertion above cannot pass under a swapped order.
        let reversed = {
            let (x, y, z) = rotate_x(corner.0, corner.1, corner.2, angle * 0.47 + 0.5);
            rotate_y(x, y, z, angle)
        };
        assert!(
            dist(reversed, golden) > 0.1,
            "x-first gives {reversed:?}, too close to the golden to pin the order"
        );
    }
}
