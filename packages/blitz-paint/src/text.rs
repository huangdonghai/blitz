use anyrender::PaintScene;
use blitz_dom::{
    BaseDocument, NodeId, node::TextBrush, node::TextOrientation, node::VerticalTextPaint,
    util::ToColorColor,
};
use icu_properties::{CodePointMapData, props::VerticalOrientation};
use kurbo::{Affine, BezPath, Cap, Circle, Rect, Stroke};
use parley::{Affinity, Cursor, Layout, Line, LineMetrics, PositionedLayoutItem, Selection};
use peniko::Fill;
use std::collections::HashMap;
use style::properties::generated::longhands::text_decoration_style::computed_value::T as TextDecorationStyle;
use style::values::computed::{TextEmphasisPosition, TextEmphasisStyle};
use style::values::specified::text::{TextEmphasisFillMode, TextEmphasisShapeKeyword};
use style::values::computed::Display;
use style::values::computed::{
    Length, LengthPercentage, TextDecorationLength, TextDecorationLine, TextUnderlinePosition,
};
use style::values::generics::text::{GenericTextDecorationInset, GenericTextDecorationLength};

use crate::color::{Color, ToColorColor as _};
use crate::{FONT_EMBOLDEN_ENABLED, SELECTION_COLOR};

/// Draw the backgrounds of inline elements (e.g. `<span style="background: ...">`).
///
/// Each glyph run carries the node id of the innermost inline element it belongs to
/// (via its brush). We look up that node's `background-color` and, if non-transparent,
/// fill a rectangle covering the run's advance and its font's ascent/descent so that the
/// background sits behind the text.
///
/// The inline root's own background is painted separately (as a normal block box), so
/// runs belonging to the root are skipped to avoid drawing it twice.
pub(crate) fn draw_inline_backgrounds<'a>(
    scene: &mut impl PaintScene,
    lines: impl Iterator<Item = Line<'a, TextBrush>>,
    doc: &BaseDocument,
    transform: Affine,
    inline_root_id: NodeId,
    vertical: Option<&VerticalTextPaint>,
) {
    // The rectangles are built in Parley's axes, like the glyphs, so a vertical inline root
    // has to map them onto physical space the same way. Without that the box of an inline
    // `background` lands next to the column instead of under its own text - on the sample
    // book page a highlighted character (`<span class="kong">`) had no background under it.
    #[cfg(feature = "writing-mode")]
    let transform = match vertical {
        Some(vertical) => transform * vertical_transform(vertical),
        None => transform,
    };
    #[cfg(not(feature = "writing-mode"))]
    let _ = vertical;

    for line in lines {
        for item in line.items() {
            let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                continue;
            };

            let node_id = glyph_run.style().brush.id;
            if node_id == inline_root_id {
                continue;
            }

            let Some(styles) = doc.get_node(node_id).and_then(|node| node.primary_styles()) else {
                continue;
            };

            let current_color = styles.clone_color();
            let bg_color = styles
                .get_background()
                .background_color
                .resolve_to_absolute(&current_color)
                .as_srgb_color();
            if bg_color == Color::TRANSPARENT {
                continue;
            }

            let metrics = glyph_run.run().font_metrics();
            let x = glyph_run.offset() as f64;
            let w = glyph_run.advance() as f64;
            let baseline = glyph_run.baseline() as f64;
            // Upright text is set in a square cell centred on the line's central baseline, so
            // the font's horizontal ascent and descent say nothing about the block axis: the
            // box spans the line box instead, as it does in Chrome.
            let (y0, y1) = if vertical.is_some() {
                (
                    f64::from(line.metrics().block_min_coord),
                    f64::from(line.metrics().block_max_coord),
                )
            } else {
                (
                    baseline - f64::from(metrics.ascent),
                    baseline + f64::from(metrics.descent),
                )
            };
            let rect = Rect::new(x, y0, x + w, y1);

            scene.fill(Fill::NonZero, transform, bg_color, None, &rect);
        }
    }
}

/// Per-font-face cache of the OS/2 `usWinAscent / unitsPerEm` ratio, keyed by the font
/// blob's unique id and face index. `None` is cached for fonts without a usable OS/2 or
/// head table so they aren't re-parsed either.
type WinAscentCache = HashMap<(u64, u32), Option<f32>>;

/// Metrics for an emphasis mark's glyph: `(advance, x_min, y_min, x_max, y_max)` as
/// `unitsPerEm` ratios.
type EmphasisMetricsCache = HashMap<(u64, u32, u32), Option<(f32, f32, f32, f32, f32)>>;

/// The glyph an emphasis mark is drawn with, with the metrics needed to centre it.
fn emphasis_glyph(
    cache: &mut EmphasisMetricsCache,
    font: &parley::FontData,
    character: char,
) -> Option<(u32, f32, f32, f32, f32, f32)> {
    use skrifa::raw::{FontRef, TableProvider as _};
    use skrifa::{
        MetadataProvider as _,
        instance::{LocationRef, Size},
    };

    let font_ref = FontRef::from_index(font.data.as_ref(), font.index).ok()?;
    let glyph_id = font_ref.charmap().map(character)?;
    let metrics = font_ref.glyph_metrics(Size::unscaled(), LocationRef::default());
    let cached = cache
        .entry((font.data.id(), font.index, glyph_id.to_u32()))
        .or_insert_with(|| {
            let units_per_em = font_ref.head().ok()?.units_per_em() as f32;
            let advance = metrics.advance_width(glyph_id)?;
            let bounds = metrics.bounds(glyph_id)?;
            Some((
                advance / units_per_em,
                bounds.x_min / units_per_em,
                bounds.y_min / units_per_em,
                bounds.x_max / units_per_em,
                bounds.y_max / units_per_em,
            ))
        });
    let cached = (*cached)?;
    Some((
        glyph_id.to_u32(),
        cached.0,
        cached.1,
        cached.2,
        cached.3,
        cached.4,
    ))
}

/// Cached [`vertical_glyph_metrics`], keyed by font face and glyph id.
type VerticalMetricsCache = HashMap<(u64, u32, u32), Option<(f32, f32)>>;

/// The font's vertical origin and vertical advance for a glyph, as `unitsPerEm`
/// ratios.
///
/// Vertical typesetting places each glyph with its vertical origin on the pen
/// and advances by its vertical advance (CSS Writing Modes 3 5.1.1). OpenType
/// derives the origin from `VORG` when the face has one, and otherwise from the
/// glyph's bounding box and its `vmtx` top side bearing (`vertOriginY = yMax +
/// tsb`).
///
/// Both matter for upright text. Approximating the origin with the top of the
/// em box leaves punctuation, whose ink sits in a corner, visibly high; and
/// advancing by the horizontal advance packs Latin letters, which are about an
/// em tall when upright but only half an em wide, on top of each other.
fn vertical_glyph_metrics(font: &parley::FontData, glyph_id: u32) -> Option<(f32, f32)> {
    use skrifa::raw::{FontRef, TableProvider as _};
    use skrifa::{
        GlyphId, MetadataProvider as _,
        instance::{LocationRef, Size},
    };
    let font_ref = FontRef::from_index(font.data.as_ref(), font.index).ok()?;
    let units_per_em = font_ref.head().ok()?.units_per_em() as f32;
    let glyph_id = GlyphId::new(glyph_id);
    let vmtx = font_ref.vmtx().ok()?;
    let advance = vmtx.advance(glyph_id)? as f32 / units_per_em;
    let origin = if let Ok(vorg) = font_ref.vorg() {
        vorg.vertical_origin_y(glyph_id) as f32 / units_per_em
    } else {
        let bounds = font_ref
            .glyph_metrics(Size::unscaled(), LocationRef::default())
            .bounds(glyph_id)?;
        let top_side_bearing = vmtx.side_bearing(glyph_id)?;
        (bounds.y_max + top_side_bearing as f32) / units_per_em
    };
    Some((origin, advance))
}

/// The font's OS/2 `usWinAscent`, in the same (device) pixels as `font_size`.
///
/// This is the ascent browsers use as the top of the "em box" when positioning overlines. It
/// is typically taller than the hhea ascent Parley exposes via [`parley::layout::run::RunMetrics`].
/// Returns `None` when the font has no OS/2 table.
fn win_ascent(cache: &mut WinAscentCache, font: &parley::FontData, font_size: f32) -> Option<f32> {
    let ratio = *cache
        .entry((font.data.id(), font.index))
        .or_insert_with(|| win_ascent_ratio(font));
    Some(ratio? * font_size)
}

/// The unitless `usWinAscent / unitsPerEm` ratio for a font face, or `None` when the font
/// has no usable head/OS/2 table.
fn win_ascent_ratio(font: &parley::FontData) -> Option<f32> {
    use skrifa::raw::{FontRef, TableProvider as _};
    let font_ref = FontRef::from_index(font.data.as_ref(), font.index).ok()?;
    let units_per_em = font_ref.head().ok()?.units_per_em();
    let win_ascent = font_ref.os2().ok()?.us_win_ascent();
    Some(win_ascent as f32 / units_per_em as f32)
}

/// Mirrors Blink's `SelectBestDashGap`: choose the gap length (as close as possible to
/// `gap_length`) that fits a whole number of `dash_length` dashes across `stroke_length`,
/// so dashes are evenly distributed and a dash lands at each end of the line.
fn select_best_dash_gap(stroke_length: f64, dash_length: f64, gap_length: f64) -> f64 {
    let available_length = stroke_length + gap_length;
    let min_num_dashes = (available_length / (dash_length + gap_length)).floor();
    let max_num_dashes = min_num_dashes + 1.0;
    let min_num_gaps = min_num_dashes - 1.0;
    let max_num_gaps = max_num_dashes - 1.0;
    if min_num_gaps < 1.0 {
        return gap_length;
    }
    let min_gap = (stroke_length - min_num_dashes * dash_length) / min_num_gaps;
    let max_gap = (stroke_length - max_num_dashes * dash_length) / max_num_gaps;
    if max_gap <= 0.0 || (min_gap - gap_length).abs() < (max_gap - gap_length).abs() {
        min_gap
    } else {
        max_gap
    }
}

/// A text decoration resolved from a single "decorating box" (an element that
/// sets `text-decoration-line`), ready to be drawn over a glyph run. Because the
/// `text-decoration-*` properties do not inherit, decorations are propagated from
/// ancestors rather than inherited, so a run may be covered by several of these.
///
/// Only the run-independent (style-derived) values are stored here so an instance
/// can be cached on the ancestor stack and reused across every run within the same
/// decorating box. The `text-decoration-inset` and `text-underline-offset` values
/// depend on the run's advance/font-size and are resolved lazily when drawing.
#[derive(Clone)]
struct ResolvedDecoration {
    line: TextDecorationLine,
    style: TextDecorationStyle,
    color: Color,
    thickness: TextDecorationLength,
    /// `text-underline-offset`; `None` for `auto`. Resolved per run.
    underline_offset: Option<LengthPercentage>,
    /// Whether `text-underline-position: under` is in effect.
    underline_under: bool,
    /// Whether `text-underline-position: from-font` is in effect.
    underline_from_font: bool,
    /// `text-decoration-inset`. Resolved per run.
    inset: GenericTextDecorationInset<LengthPercentage>,
}

/// An emphasis mark: the glyph drawn over (or under) every character of a box.
#[derive(Clone, Copy)]
struct EmphasisMark {
    glyph: char,
    color: Color,
    /// `true` for `text-emphasis-position: over`, the initial value in a horizontal mode.
    over: bool,
}

/// An element on the current ancestor path (inline root -> run node), caching the
/// values resolved from its computed style so descending into the same node across
/// consecutive runs doesn't re-resolve them.
/// One `text-shadow`: the text drawn again behind itself, offset and tinted.
#[derive(Clone, Copy)]
pub(crate) struct TextShadow {
    dx: f64,
    dy: f64,
    blur: f64,
    color: Color,
}

struct DecorationStackEntry {
    node_id: NodeId,
    /// This node's (inherited) text colour, used for the glyphs of runs whose
    /// innermost node is this one.
    text_color: Color,
    /// The decoration this node introduces as a decorating box, if any.
    decoration: Option<ResolvedDecoration>,
    /// The emphasis mark this node draws over its own text (`text-emphasis` inherits, so the
    /// innermost node of a run carries the mark that applies to it).
    emphasis: Option<EmphasisMark>,
    /// The text shadows this node asks for (`text-shadow` inherits).
    shadows: Vec<TextShadow>,
}

/// Resolve the cached style values for a single node into a [`DecorationStackEntry`].
fn resolve_decoration_entry(doc: &BaseDocument, node_id: NodeId) -> DecorationStackEntry {
    let Some(styles) = doc.get_node(node_id).and_then(|node| node.primary_styles()) else {
        return DecorationStackEntry {
            node_id,
            text_color: Color::BLACK,
            decoration: None,
            emphasis: None,
            shadows: Vec::new(),
        };
    };

    let itext = styles.get_inherited_text();
    let text = styles.get_text();
    let text_color = itext.color.as_color_color();

    let drawn_lines = TextDecorationLine::UNDERLINE
        | TextDecorationLine::OVERLINE
        | TextDecorationLine::LINE_THROUGH;
    let line = text.text_decoration_line;
    // Decorations propagate through the box tree, and a `display: contents` element
    // generates no box, so its decorations have no effect on descendants.
    let is_contents = styles.clone_display().is_contents();
    let decoration = (!is_contents && line.intersects(drawn_lines)).then(|| {
        // `text-decoration-color: currentColor` (the initial value) resolves against
        // the decorating box's own colour, not the descendant run's.
        let color = text
            .text_decoration_color
            .as_absolute()
            .map(ToColorColor::as_color_color)
            .unwrap_or(text_color);

        ResolvedDecoration {
            line,
            style: text.text_decoration_style,
            color,
            thickness: text.text_decoration_thickness.clone(),
            underline_offset: itext.text_underline_offset.non_auto(),
            underline_under: itext
                .text_underline_position
                .contains(TextUnderlinePosition::UNDER),
            underline_from_font: itext
                .text_underline_position
                .contains(TextUnderlinePosition::FROM_FONT),
            inset: text.text_decoration_inset.clone(),
        }
    });

    DecorationStackEntry {
        node_id,
        text_color,
        emphasis: emphasis_mark(itext, text_color),
        shadows: resolve_shadows(itext),
        decoration,
    }
}

/// The run-dependent geometry used to position and size a decoration, captured from a
/// single glyph run. Firefox draws a decoration once for the whole decorating box using
/// the box's own font, so we prefer the values from a run whose innermost node *is* the
/// decorating box (its own text), falling back to the first run it covers.
#[derive(Clone)]
struct DecorationRunGeometry {
    /// The baseline the decoration is positioned from (the run's own, `vertical-align`
    /// shifted, baseline).
    baseline: f32,
    ascent: f32,
    descent: f32,
    underline_offset: f32,
    underline_size: f32,
    strikethrough_size: f32,
    font: parley::FontData,
    font_size: f32,
    css_font_size: f64,
}

/// A decoration accumulated across the runs of a single line for one decorating box.
///
/// The `text-decoration-*` properties describe the whole box, so we gather the horizontal
/// extent it covers on the line (`min_x`/`max_x`) and the font geometry to draw with, then
/// paint a single line spanning the box rather than one segment per run.
struct LineDecoration {
    node_id: NodeId,
    deco: ResolvedDecoration,
    min_x: f64,
    max_x: f64,
    /// Inline position of the first glyph this box covers, and the end of the last one. The
    /// decoration runs from glyph to glyph (CSS Text Decoration 3 2), so spaces that only pad
    /// the box's edges are outside it.
    first_ink_x: Option<f64>,
    last_ink_end: Option<f64>,
    /// Geometry from a run whose innermost node is the decorating box itself.
    own: Option<DecorationRunGeometry>,
    /// Geometry from the first run the box covers (fallback when it has no own text on
    /// this line, e.g. it only wraps differently-sized descendants).
    first: Option<DecorationRunGeometry>,
}

/// Reusable scratch storage for drawing text across all inline formatting contexts in a
/// document. The vectors are cleared between uses without releasing their allocations.
#[derive(Default)]
pub(crate) struct DrawTextContext {
    stack: Vec<DecorationStackEntry>,
    path_scratch: Vec<NodeId>,
    deco_boxes: Vec<LineDecoration>,
    glyph_batches: Vec<GlyphBatch>,
    win_ascent_ratios: WinAscentCache,
    vertical_metrics: VerticalMetricsCache,
    emphasis_metrics: EmphasisMetricsCache,
}

/// Resolve the CSS `text-decoration-thickness` to a device-pixel size.
///
/// - Percentages are resolved against the font size,
/// - `from-font` uses the metrics from Parley,
/// - `auto` uses a thickness of `font-size / 10` (minimum: 1px).
///
/// The result is floored to a whole number of device pixels (minimum 1), matching
/// browsers, which snap decoration thickness to whole pixels.
fn decoration_size(
    thickness: &TextDecorationLength,
    metric_size: f32,
    css_font_size: f64,
    scale: f64,
) -> f32 {
    (match thickness {
        GenericTextDecorationLength::LengthPercentage(lp) => {
            lp.resolve(Length::new(css_font_size as f32)).px() as f64 * scale
        }
        GenericTextDecorationLength::FromFont => metric_size as f64,
        GenericTextDecorationLength::Auto => (css_font_size / 10.0).max(1.0) * scale,
    })
    .floor()
    .max(1.0) as f32
}

/// Draws a single decoration line of the given `text-decoration-style` spanning
/// `[x0, x0 + width]` along Parley's inline axis. `baseline` is the line's baseline and
/// `offset` positions the line along the block axis (`y = baseline - offset + size / 2`,
/// growing away from the text). Geometry is built explicitly (rather than relying on
/// backend dash support) so it renders consistently across renderers.
///
/// The geometry is built in Parley's layout coordinates, so `transform` has to map those
/// onto physical space 閳?see [`vertical_transform`] for the vertical writing modes.
///
/// `block_sign` orients the block axis: `+1` when a larger block coordinate means the
/// same as it does horizontally (below the baseline), `-1` when it points the other way
/// (see the note on the decoration block axis in [`flush_line_decorations`]).
///
/// `double_dir` points away from the text (`+1` = down, `-1` = up) and is used to place the
/// second line of a `double` decoration.
#[allow(clippy::too_many_arguments)]
fn draw_decoration_line(
    scene: &mut impl PaintScene,
    transform: Affine,
    scale: f64,
    x0: f64,
    width: f64,
    baseline: f32,
    offset: f32,
    size: f32,
    brush: &anyrender::Paint,
    block_sign: f64,
    double_dir: f64,
    deco_style: TextDecorationStyle,
    inset_start: f64,
    inset_end: f64,
) {
    // Inset the line from the box's start/end edges (inline-start is the left edge in
    // Parley's horizontal axes). Negative insets extend the line past the text.
    //
    // The ends are snapped to whole device pixels, which is what Chrome draws: a decoration is
    // a rule, not a glyph, and leaving it on a fractional advance paints a half-covered column
    // of pixels that the page that builds the same rule out of boxes does not have.
    let start = (x0 + inset_start).round();
    let end = (x0 + width - inset_end).ceil();
    let x = start;
    let w = end - start;
    if w <= 0.0 {
        return;
    }
    // `offset` is the signed distance from the baseline to the near edge of the line,
    // measured the same way a horizontal underline's is (negative = below the baseline).
    // When the block axis points the other way (see `block_sign`), the line has to be
    // mirrored about the baseline so it still sits on the same physical side.
    let size = size as f64;
    let y = f64::from(baseline) - block_sign * (f64::from(offset) - size / 2.0);
    // The thickness is whole pixels (`decoration_size` floors it), so snapping the near edge
    // makes both of them whole: a rule left on a fractional baseline paints two half-covered
    // rows, which is lighter than the integer-height box a reference page would draw for it.
    let y = (y - size / 2.0).round() + size / 2.0;
    if std::env::var_os("CUOTUO_DEBUG_DECO").is_some() {
        let p0 = transform * kurbo::Point::new(x, y);
        let p1 = transform * kurbo::Point::new(x + w, y);
        eprintln!("  line x0={x} w={w} baseline={baseline} offset={offset} size={size} block_sign={block_sign} y={y} -> p0={p0:?} p1={p1:?} xform={transform:?}");
    }
    let butt_stroke = Stroke::new(size).with_caps(Cap::Butt);

    match deco_style {
        TextDecorationStyle::MozNone => {
            // no line. Equivalent to `text-decoration-line: none`
        }
        // `solid`
        TextDecorationStyle::Solid => {
            let line = kurbo::Line::new((x, y), (x + w, y));
            scene.stroke(&butt_stroke, transform, brush, None, &line);
        }
        // Two lines, each `size` thick, separated by a 1px (CSS) gap. This
        // matches Chrome, where the second line is offset by `thickness + 1px`
        // and sits further from the text (below for underline, above for
        // overline). `double_dir` is in block-axis terms, so it follows the
        // block axis rather than the physical y axis.
        TextDecorationStyle::Double => {
            let one_css_px = scale;
            for cy in [y, y + block_sign * double_dir * (size + one_css_px)] {
                let line = kurbo::Line::new((x, cy), (x + w, cy));
                scene.stroke(&butt_stroke, transform, brush, None, &line);
            }
        }
        // Round dots (diameter = line thickness) spaced one dot apart.
        TextDecorationStyle::Dotted => {
            let radius = size / 2.0;
            let step = 2.0 * size;
            let mut cx = x + radius;
            while cx <= x + w - radius {
                let dot = Circle::new((cx, y), radius);
                scene.fill(Fill::NonZero, transform, brush, None, &dot);
                cx += step;
            }
        }
        // Dashes as filled rectangles. Dash/gap lengths are relative to the
        // thickness (matching Blink): thinner lines use proportionally longer
        // dashes and gaps so they don't read as dotted or solid.
        TextDecorationStyle::Dashed => {
            let (dash_ratio, gap_ratio) = if size >= 3.0 { (2.0, 1.0) } else { (3.0, 2.0) };
            let dash = size * dash_ratio;
            let nominal_gap = size * gap_ratio;
            // Nudge the gap so a whole number of dashes fits the run evenly.
            let gap = if w > 2.0 * dash {
                select_best_dash_gap(w, dash, nominal_gap)
            } else {
                nominal_gap
            };
            let mut dx = x;
            while dx < x + w {
                let end = (dx + dash).min(x + w);
                let rect = Rect::new(dx, y - size / 2.0, end, y + size / 2.0);
                scene.fill(Fill::NonZero, transform, brush, None, &rect);
                dx += dash + gap;
            }
        }
        // A squiggle built from alternating quadratic B鑼厇iers.
        TextDecorationStyle::Wavy => {
            let amplitude = size;
            let half_wave = (2.0 * size).max(1.0);
            let mut path = BezPath::new();
            path.move_to((x, y));
            let mut px = x;
            let mut up = true;
            while px < x + w {
                let nx = (px + half_wave).min(x + w);
                // A quad B鑼厇ier reaches half the control-point offset at its
                // midpoint, so use `2 * amplitude` to hit the target peak.
                let ctrl_y = if up {
                    y - 2.0 * amplitude
                } else {
                    y + 2.0 * amplitude
                };
                path.quad_to(((px + nx) / 2.0, ctrl_y), (nx, y));
                px = nx;
                up = !up;
            }
            let stroke = Stroke::new(size).with_caps(Cap::Round);
            scene.stroke(&stroke, transform, brush, None, &path);
        }
    }
}


/// The mark a box's `text-emphasis-style` asks for, with its colour.
///
/// The shapes are the characters the spec names (CSS Text Decoration 3 5.5), so the mark is
/// drawn from the font like any other glyph rather than as a shape of our own.
fn emphasis_mark(
    itext: &style::properties::style_structs::InheritedText,
    text_color: Color,
) -> Option<EmphasisMark> {
    let glyph = match &itext.text_emphasis_style {
        TextEmphasisStyle::None => return None,
        TextEmphasisStyle::String(string) => string.chars().next()?,
        TextEmphasisStyle::Keyword { fill, shape } => {
            use TextEmphasisFillMode::{Filled, Open};
            use TextEmphasisShapeKeyword::{Circle, Dot, DoubleCircle, Sesame, Triangle};
            match (fill, shape) {
                (Filled, Dot) => '\u{2022}',
                (Open, Dot) => '\u{25E6}',
                (Filled, Circle) => '\u{25CF}',
                (Open, Circle) => '\u{25CB}',
                (Filled, DoubleCircle) => '\u{25C9}',
                (Open, DoubleCircle) => '\u{25CE}',
                (Filled, Triangle) => '\u{25B2}',
                (Open, Triangle) => '\u{25B3}',
                (Filled, Sesame) => '\u{FE45}',
                (Open, Sesame) => '\u{FE46}',
            }
        }
    };
    let color = itext
        .text_emphasis_color
        .as_absolute()
        .map(ToColorColor::as_color_color)
        .unwrap_or(text_color);
    Some(EmphasisMark {
        glyph,
        color,
        over: !itext.text_emphasis_position.contains(TextEmphasisPosition::UNDER),
    })
}

/// The `text-shadow` list of a box, resolved to offsets and colours.
fn resolve_shadows(itext: &style::properties::style_structs::InheritedText) -> Vec<TextShadow> {
    let current = itext.color;
    itext
        .text_shadow
        .0
        .iter()
        .map(|shadow| TextShadow {
            dx: f64::from(shadow.horizontal.px()),
            dy: f64::from(shadow.vertical.px()),
            blur: f64::from(shadow.blur.px()),
            color: shadow.color.resolve_to_absolute(&current).as_color_color(),
        })
        .collect()
}

/// Glyphs waiting for their line's decorations to be painted.
///
/// A decoration is painted *before* the text it belongs to: a 20 px underline sitting on the
/// baseline is covered by the glyphs, not painted over them, which is what Chrome draws and
/// what a reference page that builds the same picture out of boxes has. The decoration
/// geometry is only complete once every run of the line has been walked, so the drawing calls
/// are queued with what they need and issued after `flush_line_decorations`.
struct GlyphBatch {
    font: parley::FontData,
    font_size: f32,
    normalized_coords: Vec<i16>,
    embolden: kurbo::Vec2,
    paint: anyrender::Paint,
    transform: Affine,
    glyph_xform: Option<Affine>,
    glyphs: Vec<anyrender::Glyph>,
}

/// A glyph's advance in `unitsPerEm` ratios, read from its font.
fn glyph_advance(font: &parley::FontData, glyph_id: u32) -> f64 {
    use skrifa::raw::{FontRef, TableProvider as _};
    use skrifa::{
        MetadataProvider as _,
        instance::{LocationRef, Size},
    };

    let Ok(font_ref) = FontRef::from_index(font.data.as_ref(), font.index) else {
        return 0.0;
    };
    let units_per_em = font_ref
        .head()
        .map(|head| head.units_per_em() as f32)
        .unwrap_or(1000.0);
    font_ref
        .glyph_metrics(Size::unscaled(), LocationRef::default())
        .advance_width(skrifa::GlyphId::new(glyph_id))
        .map(|advance| f64::from(advance / units_per_em))
        .unwrap_or(0.0)
}

/// Collect a run's glyphs in layout coordinates.
///
/// A ruby annotation is drawn over the character it belongs to rather than after it: its
/// advance was cleared before the lines were broken (see the inline layout), so the glyphs it
/// shaped sit where the base character ends. They are moved to the base's centre and lifted by
/// the line's ascent, the same place `text-emphasis` puts its marks.
fn run_glyphs(
    glyph_run: &parley::layout::GlyphRun<'_, TextBrush>,
    annotations: &[std::ops::Range<usize>],
    ascent: f64,
    baseline: f64,
    base_centre: &mut Option<f64>,
) -> Vec<anyrender::Glyph> {
    let plain = || {
        glyph_run
            .positioned_glyphs()
            .map(|glyph| anyrender::Glyph {
                id: glyph.id as _,
                x: glyph.x,
                y: glyph.y,
            })
            .collect::<Vec<_>>()
    };

    let is_annotation = glyph_run
        .visual_clusters()
        .next()
        .map(|cluster| cluster.text_range().start)
        .is_some_and(|start| {
            annotations
                .iter()
                .any(|range| range.start <= start && start < range.end)
        });
    if !is_annotation {
        let mut pen = f64::from(glyph_run.offset());
        let mut last = None;
        for cluster in glyph_run.visual_clusters() {
            let advance = f64::from(cluster.advance());
            last = Some(pen + advance / 2.0);
            pen += advance;
        }
        if let Some(last) = last {
            *base_centre = Some(last);
        }
        return plain();
    }

    let Some(centre) = *base_centre else {
        // Nothing to hang the annotation on: leave it where it is.
        return plain();
    };

    // The shaped advances were cleared, so the annotation's width comes from the font.
    let font = glyph_run.run().font();
    let font_size = f64::from(glyph_run.run().font_size());
    let mut start = f64::INFINITY;
    let mut end = f64::NEG_INFINITY;
    for glyph in glyph_run.positioned_glyphs() {
        let advance = glyph_advance(font, glyph.id) * font_size;
        start = start.min(f64::from(glyph.x));
        end = end.max(f64::from(glyph.x) + advance);
    }
    if !(start.is_finite() && end.is_finite()) {
        return Vec::new();
    }
    let shift = centre - (start + end) / 2.0;
    glyph_run
        .positioned_glyphs()
        .map(|glyph| anyrender::Glyph {
            id: glyph.id as _,
            x: (f64::from(glyph.x) + shift) as f32,
            // The annotation sits in the margin above the base text: its box is one and a half
            // times its size tall and rests on the edge of the ascent, so its baseline is three
            // quarters of the way up. This is where Chrome draws it (measured against a render
            // of the same page).
            y: (baseline - ascent - font_size * 0.75) as f32,
        })
        .collect()
}

/// Where a run's glyphs start and end along the inline axis, in device pixels.
///
/// A decoration runs from the box's first glyph to its last (CSS Text Decoration 3 2), so the
/// whitespace that only pads the box's edges is not covered, while a space between two words is
/// inside the run and stays covered. Whether a cluster is a space is a question for its text: a
/// glyph does not carry one, so the cluster's range is looked up in the line's text.
fn ink_extent(
    glyph_run: &parley::layout::GlyphRun<'_, TextBrush>,
    text: &str,
) -> (Option<f64>, Option<f64>) {
    let mut pen = f64::from(glyph_run.offset());
    let mut first = None;
    let mut end = None;
    for cluster in glyph_run.visual_clusters() {
        let width = f64::from(cluster.advance());
        let is_space = text
            .get(cluster.text_range())
            .is_some_and(|slice| slice.chars().all(char::is_whitespace));
        if !is_space {
            first.get_or_insert(pen);
            end = Some(pen + width);
        }
        pen += width;
    }
    (first, end)
}

/// How far past its line box the ink of a decorated line can reach, in device pixels.
///
/// The thickest decoration in the inline root's subtree, plus the largest underline offset and
/// a line's worth for the lines on the other side of the baseline. `text-decoration-*` do not
/// inherit, so the subtree is walked rather than the root's own style consulted.
fn decoration_reach(doc: &BaseDocument, inline_root_id: NodeId, scale: f64) -> f64 {
    let drawn = TextDecorationLine::UNDERLINE
        | TextDecorationLine::OVERLINE
        | TextDecorationLine::LINE_THROUGH;
    let mut reach: f64 = 0.0;
    let mut pending = vec![inline_root_id];
    while let Some(node_id) = pending.pop() {
        let Some(node) = doc.get_node(node_id) else { continue };
        pending.extend(node.children.iter().copied());

        let Some(styles) = node.primary_styles() else { continue };
        let text = styles.get_text();
        if !text.text_decoration_line.intersects(drawn) || styles.clone_display().is_contents() {
            continue;
        }
        let css_font_size = styles.get_font().font_size.computed_size().px() as f64;
        let thickness =
            f64::from(decoration_size(
                &text.text_decoration_thickness,
                (css_font_size / 10.0) as f32,
                css_font_size,
                scale,
            ));
        let offset = styles
            .get_inherited_text()
            .text_underline_offset
            .non_auto()
            .map(|lp| f64::from(lp.resolve(Length::new(css_font_size as f32)).px()) * scale)
            .unwrap_or(0.0);
        reach = reach.max(thickness + offset.abs() + css_font_size * scale);
    }
    reach
}

/// Paint the decorations accumulated for one line, one line per decorating box.
#[allow(clippy::too_many_arguments)]
fn flush_line_decorations(
    scene: &mut impl PaintScene,
    transform: Affine,
    scale: f64,
    deco_boxes: &[LineDecoration],
    win_ascent_ratios: &mut WinAscentCache,
    inline_root_id: NodeId,
    metrics: &LineMetrics,
    vertical: Option<&VerticalTextPaint>,
) {
    let line_baseline = metrics.baseline;
    // Decorations are positioned in Parley's layout axes, like the glyphs, so a
    // vertical inline root has to map them onto physical space the same way.
    //
    // Parley's block axis runs from the writing mode's block-start, but "under" is
    // always the physical left in a vertical writing mode (CSS Writing Modes 3,
    // Abstract-to-Physical Mappings), which is the *end* of a `vertical-lr` block
    // axis. Decorating offsets are therefore measured backwards along a block axis
    // that does not start at the right edge.
    #[cfg(feature = "writing-mode")]
    let (transform, block_sign) = match vertical {
        Some(vertical) => (
            transform * vertical_transform(vertical),
            if vertical.block_axis_starts_at_right { 1.0 } else { -1.0 },
        ),
        None => (transform, 1.0),
    };
    #[cfg(not(feature = "writing-mode"))]
    let (transform, block_sign) = {
        let _ = vertical;
        (transform, 1.0)
    };

    // In a vertical writing mode the glyphs of a line are centred on the line's block
    // centre, not on the run's alphabetic baseline (see the vertical branch of
    // `stroke_text`): every glyph is placed at `baseline + (block_centre - line
    // baseline)`. A decoration measured from the alphabetic baseline alone would sit
    // that far off its characters, which on the sample book page put every underline
    // a couple of pixels through the glyphs.
    #[cfg(feature = "writing-mode")]
    let baseline_shift = if vertical.is_some() {
        (metrics.block_min_coord + metrics.block_max_coord) / 2.0 - metrics.baseline
    } else {
        0.0
    };
    #[cfg(not(feature = "writing-mode"))]
    let baseline_shift = 0.0f32;

    // CSS Writing Modes 3 5.1.1 sets an upright glyph in a square cell centred on the
    // line's central baseline, so along the block axis that cell reaches half an em either
    // side of the baseline. The font's horizontal ascent/descent/underline metrics say
    // nothing about that, and using them drops the line inside the glyphs: Chrome puts
    // `auto` and `under` alike just outside the cell (compared against Chrome 141).
    let upright_cell = vertical.is_some_and(|v| v.text_orientation != TextOrientation::Sideways);

    // Draw innermost boxes first so ancestors' decorations paint on top, matching the
    // per-run drawing order this replaced (`stack.iter().rev()`).
    for acc in deco_boxes.iter().rev() {
        let deco = &acc.deco;
        // Prefer the decorating box's own font; fall back to the first run it covers.
        let geom = match (&acc.own, &acc.first) {
            (Some(own), _) => own.clone(),
            (None, Some(first)) => {
                // The box's decoration is positioned from the baseline of its own text. Taking
                // the line box's baseline instead put a block's underline a pixel off the same
                // text inside a span (the strut and the run round their ascent differently),
                // and two pages that put the decoration on different boxes have to agree.
                first.clone()
            }
            (None, None) => continue,
        };
        let geom = &geom;
        // Half the square cell an upright glyph is set in (see `upright_cell`).
        let half_cell = geom.font_size / 2.0;
        if std::env::var_os("CUOTUO_DEBUG_DECO").is_some() {
            eprintln!(
                "deco node={:?} vert={:?} line_baseline={line_baseline} run_baseline={} under_off={} min_x={} max_x={}",
                acc.node_id,
                vertical.map(|v| (
                    v.block_extent,
                    v.block_axis_starts_at_right,
                    v.inline_extent,
                    v.inline_axis_needs_mirror
                )),
                geom.baseline,
                geom.underline_offset,
                acc.min_x,
                acc.max_x
            );
        }
        let min_x = acc.first_ink_x.unwrap_or(acc.min_x);
        let max_x = acc.last_ink_end.unwrap_or(acc.max_x);
        let width = max_x - min_x;
        if width <= 0.0 {
            continue;
        }
        let brush = anyrender::Paint::from(deco.color);

        // `text-decoration-inset` shortens (or, when negative, extends) the line from the
        // inline-start/end edges. Percentages resolve against the decoration line length
        // (the box's total advance on this line); `auto` is no inset.
        let (inset_start, inset_end) = match &deco.inset {
            GenericTextDecorationInset::LengthPercentage { start, end } => {
                // The extent is in device pixels; convert to CSS pixels so the resolved
                // result can be scaled back.
                let line_length = Length::new((width / scale) as f32);
                (
                    start.resolve(line_length).px() as f64 * scale,
                    end.resolve(line_length).px() as f64 * scale,
                )
            }
            GenericTextDecorationInset::Auto => (0.0, 0.0),
        };

        if deco.line.contains(TextDecorationLine::UNDERLINE) {
            let size = decoration_size(
                &deco.thickness,
                geom.underline_size,
                geom.css_font_size,
                scale,
            );

            // `text-underline-offset` moves the underline away from the text. `auto` keeps
            // the font's suggested position; otherwise it resolves against the font size
            // (percentages relative to 1em).
            let extra_underline_offset = deco
                .underline_offset
                .as_ref()
                .map(|lp| lp.resolve(Length::new(geom.css_font_size as f32)).px() as f64 * scale)
                .unwrap_or(0.0);

            // `text-underline-position: under` places the underline below the glyph box
            // (below descenders) rather than at the font's suggested position near the
            // alphabetic baseline. We anchor the top of the line at the descent so it clears
            // descending glyphs like "gqy".
            // An explicit offset under `auto` is taken from the alphabetic
            // baseline, as CSS and Chrome have it, not from the auto gap.
            let base_offset = if upright_cell {
                -half_cell
            } else if deco.underline_under {
                -geom.descent
            } else if deco.underline_offset.is_some() && !deco.underline_from_font {
                0.0
            } else {
                geom.underline_offset
            };
            let offset = base_offset - extra_underline_offset as f32;

            // A `double` underline extends downward, away from the text.
            draw_decoration_line(
                scene,
                transform,
                scale,
                min_x,
                width,
                geom.baseline + baseline_shift,
                offset,
                size,
                &brush,
                block_sign,
                1.0,
                deco.style,
                inset_start,
                inset_end,
            );
        }
        if deco.line.contains(TextDecorationLine::OVERLINE) {
            // Fonts don't provide a dedicated overline metric, so reuse the underline
            // thickness. The line sits at the top of the "em box": its lower edge rests on
            // the ascent so it clears the glyphs, and it extends upward from there.
            //
            // Browsers use the OS/2 `usWinAscent` for this edge, which is taller than the
            // hhea-based ascent Parley reports; using the smaller value would draw the
            // overline too low. Fall back to Parley's ascent for fonts without a usable
            // OS/2 table.
            let size = decoration_size(
                &deco.thickness,
                geom.underline_size,
                geom.css_font_size,
                scale,
            );
            let offset = if upright_cell {
                half_cell + size
            } else {
                let ascent =
                    win_ascent(win_ascent_ratios, &geom.font, geom.font_size).unwrap_or(geom.ascent);
                ascent + size
            };

            // A `double` overline extends upward, away from the text.
            draw_decoration_line(
                scene,
                transform,
                scale,
                min_x,
                width,
                geom.baseline + baseline_shift,
                offset,
                size,
                &brush,
                block_sign,
                -1.0,
                deco.style,
                inset_start,
                inset_end,
            );
        }
        if deco.line.contains(TextDecorationLine::LINE_THROUGH) {
            let size = decoration_size(
                &deco.thickness,
                geom.strikethrough_size,
                geom.css_font_size,
                scale,
            );

            // Centre the line-through a third of the ascent above the baseline, matching
            // Chrome (which places it `2/3 * ascent` below the text-top). Parley's
            // `strikethrough_offset` (the font's `yStrikeoutPosition`) sits lower and would
            // draw the line too close to the baseline.
            let offset = if upright_cell {
                // Through the middle of the cell, which is where the line box's axis runs.
                size / 2.0
            } else {
                let ascent =
                    win_ascent(win_ascent_ratios, &geom.font, geom.font_size).unwrap_or(geom.ascent);
                ascent / 3.0 + size / 2.0
            };

            draw_decoration_line(
                scene,
                transform,
                scale,
                min_x,
                width,
                geom.baseline + baseline_shift,
                offset,
                size,
                &brush,
                block_sign,
                1.0,
                deco.style,
                inset_start,
                inset_end,
            );
        }
    }
}

/// Block- and inline-axis geometry of an inline root laid out in a vertical
/// writing mode, with extents converted to device px.
///
/// `None` when the inline root is horizontal (or its layout has not been
/// performed yet), in which case Parley's coordinates are already physical.
#[cfg(feature = "writing-mode")]
pub(crate) fn vertical_text(
    doc: &BaseDocument,
    inline_root_id: NodeId,
    scale: f64,
) -> Option<VerticalTextPaint> {
    let vertical = doc
        .get_node(inline_root_id)?
        .element_data()?
        .inline_layout_data
        .as_ref()?
        .vertical_text?;
    Some(VerticalTextPaint {
        block_extent: vertical.block_extent * scale as f32,
        block_axis_starts_at_right: vertical.block_axis_starts_at_right,
        inline_extent: vertical.inline_extent * scale as f32,
        inline_axis_needs_mirror: vertical.inline_axis_needs_mirror,
        text_orientation: vertical.text_orientation,
    })
}

/// Maps Parley's layout coordinates (x = inline axis, y = block axis) onto the
/// physical space a vertical inline root paints its glyphs in.
///
/// Decorations are positioned from a run's baseline along the block axis, which is
/// Parley's `y` 閳?so for a vertical root a "horizontal" decoration line is really a
/// line running down the column, and the block axis it is offset along is physical
/// `x`. Both axes can start at the far edge (see [`vertical_text`]), hence the
/// mirroring.
#[cfg(feature = "writing-mode")]
fn vertical_transform(vertical: &VerticalTextPaint) -> Affine {
    let (x_from_block, x_offset) = if vertical.block_axis_starts_at_right {
        (-1.0, f64::from(vertical.block_extent))
    } else {
        (1.0, 0.0)
    };
    let (y_from_inline, y_offset) = if vertical.inline_axis_needs_mirror {
        (-1.0, f64::from(vertical.inline_extent))
    } else {
        (1.0, 0.0)
    };
    Affine::new([
        // Physical x = block axis (mirrored for `vertical-rl`),
        // physical y = inline axis (mirrored when it does not start at the top).
        0.0,
        y_from_inline,
        x_from_block,
        0.0,
        x_offset,
        y_offset,
    ])
}

/// The extent of a line in the coordinates the painter draws its glyphs in, so
/// it can be culled against the visible region.
///
/// Parley reports the line in its own axes, so a vertical inline root needs the
/// same block/inline transposition 閳?and the same mirroring 閳?that the glyphs
/// get in [`stroke_text`].
fn line_extent(line: &Line<'_, TextBrush>, vertical: Option<&VerticalTextPaint>) -> Rect {
    let metrics = line.metrics();
    let inline = (metrics.offset, metrics.offset + metrics.advance);
    let block = (metrics.block_min_coord, metrics.block_max_coord);
    let Some(vertical) = vertical else {
        return Rect::new(
            f64::from(inline.0),
            f64::from(block.0),
            f64::from(inline.1),
            f64::from(block.1),
        );
    };
    let x = |block: f32| {
        if vertical.block_axis_starts_at_right {
            vertical.block_extent - block
        } else {
            block
        }
    };
    let y = |inline: f32| {
        if vertical.inline_axis_needs_mirror {
            vertical.inline_extent - inline
        } else {
            inline
        }
    };
    let (x0, x1) = (x(block.0), x(block.1));
    let (y0, y1) = (y(inline.0), y(inline.1));
    Rect::new(
        f64::from(x0.min(x1)),
        f64::from(y0.min(y1)),
        f64::from(x0.max(x1)),
        f64::from(y0.max(y1)),
    )
}

pub(crate) fn stroke_text<'a>(
    scene: &mut impl PaintScene,
    lines: impl Iterator<Item = Line<'a, TextBrush>>,
    doc: &BaseDocument,
    transform: Affine,
    scale: f64,
    inline_root_id: NodeId,
    cull_rect: Rect,
    context: &mut DrawTextContext,
) {
    let DrawTextContext {
        stack,
        path_scratch,
        deco_boxes,
        glyph_batches,
        win_ascent_ratios,
        vertical_metrics,
        emphasis_metrics,
    } = context;
    stack.clear();
    path_scratch.clear();
    deco_boxes.clear();

    // Vertical writing modes: Parley positions glyphs in the inline root's own
    // axes (x = inline advance, y = block axis / baseline), which coincide with
    // physical space only for `horizontal-tb`. Map them in `draw_glyphs` below.
    #[cfg(feature = "writing-mode")]
    let vertical = vertical_text(doc, inline_root_id, scale);
    // Without the feature every inline root is horizontal, which `line_extent`
    // takes as its `None`.
    #[cfg(not(feature = "writing-mode"))]
    let vertical: Option<VerticalTextPaint> = None;
    // The laid-out text, so `text-orientation: mixed` can be decided per
    // character: glyphs carry only their glyph id, not their source character.
    #[cfg(feature = "writing-mode")]
    let layout_text: &str = doc
        .get_node(inline_root_id)
        .and_then(|node| node.element_data())
        .and_then(|data| data.inline_layout_data.as_ref())
        .map(|layout| layout.text.as_str())
        .unwrap_or_default();
    #[cfg(feature = "writing-mode")]
    let vo_map = CodePointMapData::<VerticalOrientation>::new();

    // Persistent stack mirroring the ancestor path (document root -> current run's
    // node) as we walk the runs. The `text-decoration-*` properties are *not*
    // inherited; instead a decoration set on an ancestor is propagated to the
    // descendant text it wraps. Rather than re-resolving styles for the whole
    // ancestor chain on every run, we cache each node's resolved values here and,
    // for each run, only resolve styles for the nodes newly descended into (popping
    // as we ascend). `path_scratch` is a reusable buffer for the run's node path.
    for line in lines {
        // A line is a row of text in a horizontal writing mode and a whole
        // column of it in a vertical one, so a paragraph that is only partly on
        // screen otherwise hands the rasterizer every line it has 閳?and a long
        // vertical document, which is mostly off screen, is nearly all of them.
        // Glyphs can overhang their line box (italics, accents, a vertical
        // origin outside the em box), so cull with a line's worth of slack.
        let extent = line_extent(&line, vertical.as_ref());
        let slack = f64::from(line.metrics().line_height).max(1.0);
        // A decoration is as thick as `text-decoration-thickness` says and can be pushed away
        // from the baseline, so the ink of a line reaches past the line box: a 4em underline in
        // a 1em `overflow: hidden` box paints inside the box although the line itself sits
        // above it. Culling on the line box alone dropped those pixels entirely.
        let reach = slack + decoration_reach(doc, inline_root_id, scale);
        let extent = extent.inflate(reach, reach);
        if extent.x1 < cull_rect.x0
            || extent.x0 > cull_rect.x1
            || extent.y1 < cull_rect.y0
            || extent.y0 > cull_rect.y1
        {
            continue;
        }

        // Decorations accumulated for this line, keyed by decorating box, so each box is
        // painted once (spanning all its runs) using its own font 閳?matching Firefox, which
        // draws one decoration per box rather than one stepped segment per differently-sized
        // run. Clearing preserves the allocation for the next line and inline context.
        deco_boxes.clear();

        // Difference between the painted and the laid-out inline position on
        // this line, carried from run to run (see the upright branch below).
        #[cfg(feature = "writing-mode")]
        let mut pen_delta = 0.0f32;

        // Glyphs of this line, drawn once its decorations are in the scene.
        glyph_batches.clear();

        // Where the last ordinary character of this line sits along the inline axis: a ruby
        // annotation is drawn over it.
        let mut base_centre: Option<f64> = None;

        // An annotation is text the layout recorded as taking no room in the line: a `<rt>` element
    // or an emphasis mark, both of which are drawn over the character they belong to.
    let annotations = doc
        .get_node(inline_root_id)
        .and_then(|node| node.element_data())
        .and_then(|element| element.inline_layout_data.as_ref())
        .map(|data| data.ruby_ranges.clone())
        .unwrap_or_default();

    for item in line.items() {
            if let PositionedLayoutItem::GlyphRun(glyph_run) = item {
                let run = glyph_run.run();
                let font = run.font();
                let font_size = run.font_size();
                let metrics = run.font_metrics();
                let style = glyph_run.style();
                let synthesis = run.synthesis();
                let glyph_xform = synthesis
                    .skew()
                    .map(|angle| Affine::skew(angle.to_radians().tan() as f64, 0.0));

                let css_font_size = font_size as f64 / scale;

                // Reconcile the stack with this run's ancestor path. Build the path
                // (inline root -> run node), keep the shared prefix already on the stack,
                // pop the rest, and resolve styles only for the newly-descended nodes.
                path_scratch.clear();
                let mut walk_id = Some(style.brush.id);
                while let Some(node_id) = walk_id {
                    path_scratch.push(node_id);
                    // Past the inline root as well: `text-decoration` propagates from every
                    // ancestor box to the text inside it, and a block nested in a decorated
                    // block is a line of its own (the corpus's propagation cases are exactly
                    // that). An inline root only says where the line's layout data lives.
                    walk_id = doc.get_node(node_id).and_then(|node| node.parent);
                }
                path_scratch.reverse();

                // A decoration does not reach into an atomic inline: `inline-block`,
                // `inline-flex` and their kin are laid out as boxes of their own, so only the
                // decorations from that box downwards apply to the text inside it.
                let boundary = path_scratch
                    .iter()
                    .rposition(|&node_id| {
                        doc.get_node(node_id)
                            .and_then(|node| node.primary_styles())
                            .is_some_and(|styles| {
                                let display = styles.clone_display();
                                display == Display::InlineBlock
                                    || display == Display::InlineFlex
                                    || display == Display::InlineGrid
                                    || display == Display::InlineTable
                            })
                    })
                    .unwrap_or(0);
                path_scratch.drain(..boundary);

                let shared = stack
                    .iter()
                    .zip(path_scratch.iter())
                    .take_while(|(entry, node_id)| entry.node_id == **node_id)
                    .count();
                stack.truncate(shared);
                for &node_id in &path_scratch[shared..] {
                    stack.push(resolve_decoration_entry(doc, node_id));
                }

                // The glyph colour comes from the run's own node (the stack top): `color`
                // inherits, so the innermost inline element already carries the right value.
                let text_color = stack.last().map(|e| e.text_color).unwrap_or(Color::BLACK);

                let embolden = if FONT_EMBOLDEN_ENABLED {
                    let fs = font_size as f64 / scale;
                    kurbo::Vec2::new((0.015125 * fs).min(0.3), (0.0121 * fs).min(0.3))
                } else {
                    kurbo::Vec2::default()
                };

                // The inline extent this run covers, in Parley's layout coordinates.
                // A vertical root overrides it with the extent it actually paints
                // (see the `run_x0`/`run_x1` assignments in that branch): upright
                // glyphs advance by their vertical metrics, which Parley's layout
                // does not know about, so the two drift apart along the line.
                let mut run_x0 = glyph_run.offset() as f64;
                let mut run_x1 = run_x0 + glyph_run.advance() as f64;

                // Map glyph positions from the inline root's axes onto physical
                // space for vertical writing modes (see the note at the top).
                #[cfg(feature = "writing-mode")]
                if let Some(vertical) = vertical {
                    // Upright glyphs are set on the line's central baseline (鎼?.2);
                    // sideways ones are rotated 90鎺?clockwise and keep the same
                    // axis. See the note on `vertical_text`.
                    let line_metrics = *line.metrics();
                    let line_centre =
                        (line_metrics.block_min_coord + line_metrics.block_max_coord) / 2.0;
                    let baseline = glyph_run.baseline();
                    // Upright glyphs advance by their vertical metrics, which
                    // differ from the horizontal advances Parley laid the run
                    // out with. `pen_delta` carries the accumulated difference
                    // into the following runs, so their start positions follow
                    // what was actually painted instead of overlapping it.
                    let mut pen = glyph_run.offset() + pen_delta;
                    let parley_end = glyph_run.offset() + glyph_run.advance();
                    run_x0 = f64::from(pen);
                    let mut upright = Vec::new();
                    let mut rotated = Vec::new();

                    // Walk clusters rather than `positioned_glyphs`, because the
                    // orientation is decided per character and glyphs do not carry
                    // their source character. The clusters must be the *glyph
                    // run's* own: a line item is split into one glyph run per
                    // style, and `run` spans the whole item. Iterating the item's
                    // clusters here paints every character once per glyph run, and the
                    // pen advances for each of them, so the whole line's text repeats
                    // down the column (a page with nested inline spans and
                    // punctuation trimming produces several runs per item).
                    for cluster in glyph_run.visual_clusters() {
                        let sideways = match vertical.text_orientation {
                            TextOrientation::Upright => false,
                            TextOrientation::Sideways => true,
                            TextOrientation::Mixed => layout_text
                                .get(cluster.text_range())
                                .and_then(|text| text.chars().next())
                                .is_some_and(|c| {
                                    vo_map.get(c) == VerticalOrientation::Rotated
                                }),
                        };
                        for glyph in cluster.glyphs() {
                            let advance = glyph.advance;
                            // The glyph's vertical origin and vertical advance.
                            // The em box and the horizontal advance are the
                            // fallback for faces with no `vmtx`.
                            let cached = *vertical_metrics
                                .entry((font.data.id(), font.index, glyph.id))
                                .or_insert_with(|| vertical_glyph_metrics(font, glyph.id));
                            let (origin, step) = match cached {
                                Some((origin_ratio, advance_ratio)) => {
                                    (origin_ratio * font_size, advance_ratio * font_size)
                                }
                                // CSS Writing Modes 3 5.1.1 requires the UA to
                                // synthesize vertical metrics for faces that
                                // have none: one em of advance, with the em box
                                // filling the cell. Falling back to the
                                // horizontal advance would overlap upright
                                // Latin, which is half an em wide but an em tall.
                                None => (
                                    font_size * metrics.ascent
                                        / (metrics.ascent + metrics.descent).max(f32::EPSILON),
                                    font_size,
                                ),
                            };

                            let inline_pos = pen;
                            // Upright glyphs advance by their vertical advance
                            // (CSS Writing Modes 3 5.1.1) 閳?a Latin letter is
                            // about an em tall but only half an em wide, so
                            // advancing by its width would overlap the next one.
                            // A rotated glyph advances by its width, which the
                            // rotation turns into the step down the column.
                            pen += if sideways { advance } else { step };

                            // The axis is the line box's centre, not the run's own
                            // metrics: 鎼?.4 aligns every inline box in the line to
                            // the dominant (central) baseline. `glyph.y` keeps the
                            // column (block-axis) position and any `vertical-align`
                            // shift.
                            let centre =
                                baseline + glyph.y + (line_centre - line_metrics.baseline);
                            let block_pos = if vertical.block_axis_starts_at_right {
                                vertical.block_extent - centre
                            } else {
                                centre
                            };

                            let (x, y) = if sideways {
                                // Rotated 90鎺?clockwise, so the glyph's ascent points
                                // right and its descent left, and its baseline runs
                                // down the column at `x`.
                                //
                                // Which baseline the line aligns on is what 鎼?.2
                                // decides from `text-orientation`: the central
                                // baseline for `mixed` 閳?which rotates the
                                // horizontal-only scripts too 閳?and `upright`, and
                                // the alphabetic baseline for `sideways`, where the
                                // whole line is one rotated horizontal line and every
                                // glyph therefore shares one baseline whatever its
                                // size. `line_centre` and `line_metrics.baseline` are
                                // those two baselines of the line.
                                let x = match vertical.text_orientation {
                                    TextOrientation::Sideways => {
                                        block_pos - (line_metrics.baseline - line_centre)
                                    }
                                    // The glyph's central baseline sits half its
                                    // ascent-minus-descent to the right of its origin.
                                    _ => block_pos - (metrics.ascent - metrics.descent) / 2.0,
                                };
                                let y = if vertical.inline_axis_needs_mirror {
                                    vertical.inline_extent - inline_pos - advance
                                } else {
                                    inline_pos
                                };
                                (x, y)
                            } else {
                                let x = block_pos - advance / 2.0;
                                let y = if vertical.inline_axis_needs_mirror {
                                    vertical.inline_extent - inline_pos - step + origin
                                } else {
                                    inline_pos + origin
                                };
                                (x, y)
                            };

                            let target = if sideways { &mut rotated } else { &mut upright };
                            target.push(anyrender::Glyph {
                                id: glyph.id as _,
                                x,
                                y,
                            });
                        }
                    }

                    let normalized_coords: &[i16] = bytemuck::cast_slice(run.normalized_coords());
                    glyph_batches.push(GlyphBatch {
                        font: font.clone(),
                        font_size,
                        normalized_coords: normalized_coords.to_vec(),
                        embolden,
                        paint: anyrender::Paint::from(text_color),
                        transform,
                        glyph_xform,
                        glyphs: upright,
                    });
                    if !rotated.is_empty() {
                        // `Affine * Affine` applies the right operand first, so the
                        // synthesis skew still acts in glyph space.
                        let turned = Affine::rotate(std::f64::consts::FRAC_PI_2)
                            * glyph_xform.unwrap_or(Affine::IDENTITY);
                        glyph_batches.push(GlyphBatch {
                            font: font.clone(),
                            font_size,
                            normalized_coords: normalized_coords.to_vec(),
                            embolden,
                            paint: anyrender::Paint::from(text_color),
                            transform,
                            glyph_xform: Some(turned),
                            glyphs: rotated,
                        });
                    }
                    // The decorations of this run have to land on the glyphs that were
                    // painted, not on the position Parley reported.
                    run_x1 = f64::from(pen);
                    pen_delta = pen - parley_end;
                } else {
                    let glyphs = run_glyphs(
                        &glyph_run,
                        &annotations,
                        f64::from(metrics.ascent),
                        f64::from(glyph_run.baseline()),
                        &mut base_centre,
                    );
                    let normalized_coords: &[i16] = bytemuck::cast_slice(run.normalized_coords());

                    // `text-shadow`: the same glyphs drawn behind the text, offset and tinted.
                    // The first shadow of a list is painted last so that it ends up on top.
                    let shadows = stack
                        .last()
                        .map(|entry| entry.shadows.clone())
                        .unwrap_or_default();
                    for shadow in shadows.iter().rev() {
                        let offset = Affine::translate((shadow.dx * scale, shadow.dy * scale));
                        glyph_batches.push(GlyphBatch {
                            font: font.clone(),
                            font_size,
                            normalized_coords: normalized_coords.to_vec(),
                            embolden,
                            paint: anyrender::Paint::from(shadow.color),
                            transform: transform * offset,
                            glyph_xform,
                            glyphs: glyphs.clone(),
                        });
                    }

                    glyph_batches.push(GlyphBatch {
                        font: font.clone(),
                        font_size,
                        normalized_coords: normalized_coords.to_vec(),
                        embolden,
                        paint: anyrender::Paint::from(text_color),
                        transform,
                        glyph_xform,
                        glyphs,
                    });
                }
                #[cfg(not(feature = "writing-mode"))]
                {
                    let glyphs = run_glyphs(
                        &glyph_run,
                        &annotations,
                        f64::from(metrics.ascent),
                        f64::from(glyph_run.baseline()),
                        &mut base_centre,
                    );

                    let normalized_coords: &[i16] = bytemuck::cast_slice(run.normalized_coords());
                    glyph_batches.push(GlyphBatch {
                        font: font.clone(),
                        font_size,
                        normalized_coords: normalized_coords.to_vec(),
                        embolden,
                        paint: anyrender::Paint::from(text_color),
                        transform,
                        glyph_xform,
                        glyphs: glyphs.collect(),
                    });
                }

                // Accumulate this run's contribution to each decorating box on its ancestor
                // path. The decoration is drawn once per box after the whole line has been
                // walked (see `flush_line_decorations`), so mixed font sizes within a box
                // produce a single straight line rather than one stepped segment per run.
                let geometry = DecorationRunGeometry {
                    baseline: glyph_run.baseline(),
                    ascent: metrics.ascent,
                    descent: metrics.descent,
                    underline_offset: metrics.underline_offset,
                    underline_size: metrics.underline_size,
                    strikethrough_size: metrics.strikethrough_size,
                    font: font.clone(),
                    font_size,
                    css_font_size,
                };
                let run_node_id = style.brush.id;

                // A decoration runs from the box's first glyph to its last (CSS Text
                // Decoration 3 2), so spaces that only pad the box's edges are outside it while
                // the spaces between two words stay covered. L4 spells the trimmed set out as
                // `text-decoration-skip-spaces`, whose L3 default is this.
                let (ink_start, ink_end) = ink_extent(&glyph_run, layout_text);



                for entry in stack.iter() {
                    if entry.decoration.is_none() {
                        continue;
                    }
                    let idx = match deco_boxes.iter().position(|d| d.node_id == entry.node_id) {
                        Some(idx) => idx,
                        None => {
                            deco_boxes.push(LineDecoration {
                                node_id: entry.node_id,
                                deco: entry.decoration.clone().unwrap(),
                                min_x: f64::INFINITY,
                                max_x: f64::NEG_INFINITY,
                                first_ink_x: None,
                                last_ink_end: None,
                                own: None,
                                first: None,
                            });
                            deco_boxes.len() - 1
                        }
                    };
                    let acc = &mut deco_boxes[idx];
                    acc.min_x = acc.min_x.min(run_x0);
                    acc.max_x = acc.max_x.max(run_x1);
                    if acc.first_ink_x.is_none()
                        && let Some(start) = ink_start
                    {
                        acc.first_ink_x = Some(start);
                    }
                    if let Some(end) = ink_end {
                        acc.last_ink_end = Some(end);
                    }
                    if acc.first.is_none() {
                        acc.first = Some(geometry.clone());
                    }
                    // A run whose innermost node is the box itself is the box's own text, so
                    // its font is the one Firefox positions and sizes the decoration with.
                    if entry.node_id == run_node_id {
                        acc.own = Some(geometry.clone());
                    }
                }
            }
        }

        flush_line_decorations(
            scene,
            transform,
            scale,
            deco_boxes,
            win_ascent_ratios,
            inline_root_id,
            line.metrics(),
            vertical.as_ref(),
        );

        // The line's text, over its decorations.
        for batch in glyph_batches.drain(..) {
            scene.draw_glyphs(
                &batch.font,
                batch.font_size,
                !FONT_EMBOLDEN_ENABLED, // hint
                &batch.normalized_coords,
                batch.embolden,
                Fill::NonZero,
                &batch.paint,
                1.0, // alpha
                batch.transform,
                batch.glyph_xform,
                batch.glyphs.into_iter(),
            );
        }
    }
}

/// Draw selection highlight rectangles for the given byte range in a layout.
/// Uses Parley's Selection type for accurate geometry calculation.
///
/// `vertical` carries the inline root's vertical writing mode geometry (see
/// [`vertical_text`]); Parley reports selection geometry in the writing mode's
/// own axes, so the rectangles have to be transposed the same way the glyphs
/// are, otherwise the highlight is drawn in the wrong place.
pub(crate) fn draw_text_selection(
    scene: &mut impl PaintScene,
    layout: &Layout<TextBrush>,
    transform: Affine,
    selection_start: usize,
    selection_end: usize,
    vertical: Option<VerticalTextPaint>,
) {
    let anchor = Cursor::from_byte_index(layout, selection_start, Affinity::Downstream);
    let focus = Cursor::from_byte_index(layout, selection_end, Affinity::Downstream);
    let selection = Selection::new(anchor, focus);

    selection.geometry_with(layout, |rect, _line_idx| {
        let rect = match vertical {
            // Parley's inline axis (x) becomes physical y, its block axis (y)
            // becomes physical x 閳?mirrored for `vertical-rl` and for an inline
            // axis that does not start at the measured edge.
            Some(vertical) => {
                let (inline0, inline1) = if vertical.inline_axis_needs_mirror {
                    (
                        vertical.inline_extent as f64 - rect.x1,
                        vertical.inline_extent as f64 - rect.x0,
                    )
                } else {
                    (rect.x0, rect.x1)
                };
                let (block0, block1) = if vertical.block_axis_starts_at_right {
                    (
                        vertical.block_extent as f64 - rect.y1,
                        vertical.block_extent as f64 - rect.y0,
                    )
                } else {
                    (rect.y0, rect.y1)
                };
                kurbo::Rect::new(block0, inline0, block1, inline1)
            }
            None => kurbo::Rect::new(rect.x0, rect.y0, rect.x1, rect.y1),
        };
        scene.fill(Fill::NonZero, transform, SELECTION_COLOR, None, &rect);
    });
}
