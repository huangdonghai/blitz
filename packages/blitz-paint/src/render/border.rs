use anyrender::PaintScene;
use blitz_dom::node::SpecialElementData;
use blitz_dom::local_name;
use blitz_traits::node_id::NodeId;
use kurbo::{
    Affine, BezPath, Cap, Circle, Insets, Join, PathEl, Point, Rect, Shape as _, Stroke, Vec2,
};
use peniko::{Color, Fill};
use smallvec::SmallVec;
use style::{
    computed_values::border_collapse::T as BorderCollapse,
    values::computed::{BorderStyle, OutlineStyle},
};

use crate::{
    color::{ToColorColor as _, contrast_ratio},
    kurbo_css::Edge,
    render::{ElementCx, PhysicalTracks},
};

/// Place the logical grid tracks in physical paint coordinates.
///
/// Taffy stores grid detail in the writing-mode axes it laid the table out
/// in. A vertical table therefore needs its columns mapped to the physical y
/// axis and its rows mapped to x, with `vertical-rl` mirroring the rows.
fn physical_table_tracks(
    columns: impl IntoIterator<Item = taffy::Line<f32>>,
    rows: impl IntoIterator<Item = taffy::Line<f32>>,
    vertical: bool,
    vertical_rl: bool,
    table_width: f32,
) -> (Vec<taffy::Line<f32>>, Vec<taffy::Line<f32>>) {
    let columns: Vec<_> = columns.into_iter().collect();
    if !vertical {
        return (columns, rows.into_iter().collect());
    }

    let mut x_tracks: Vec<_> = rows
        .into_iter()
        .map(|row| {
            if vertical_rl {
                taffy::Line {
                    start: table_width - row.end,
                    end: table_width - row.start,
                }
            } else {
                row
            }
        })
        .collect();
    x_tracks.sort_by(|a, b| a.start.total_cmp(&b.start));
    (x_tracks, columns)
}

#[derive(Clone, Copy, Debug)]
pub(super) struct TableGridTrack {
    pub line: taffy::Line<f32>,
    /// Index into `TableContext::rows` or `TableContext::columns`.
    pub index: usize,
}

pub(super) struct TableGridGeometry {
    pub x: Vec<TableGridTrack>,
    pub y: Vec<TableGridTrack>,
    /// The table box around the grid, excluding caption rows.
    pub border_box: Rect,
}

/// The physical tracks and border box of the table's grid.
///
/// Captions are rows in the taffy grid but are outside the table box in CSS.
/// Keeping them in the grid is useful for placing and fragmenting them, but
/// every table-level paint operation has to stop at the first and last real
/// row instead of at the wrapper's edge.
pub(super) fn table_grid_geometry<S: taffy::CheapCloneStr>(
    element: &SpecialElementData,
    grid_info: &taffy::DetailedGridInfo<S>,
    vertical: bool,
    vertical_rl: bool,
    table_size: taffy::Size<f32>,
    captions: &[(bool, Rect)],
    border: taffy::Rect<f32>,
) -> TableGridGeometry {
    let SpecialElementData::TableRoot(table) = element else {
        unreachable!("table grid geometry is only for a table root");
    };
    let mut caption_rows = vec![false; table.rows.len()];
    for cell in table.cells.iter().filter(|cell| cell.caption) {
        if let Some(caption) = caption_rows.get_mut(cell.row as usize) {
            *caption = true;
        }
    }

    let columns: Vec<_> = PhysicalTracks::from_tracks(&grid_info.columns)
        .iter_indexed()
        .collect();
    let all_rows: Vec<_> = PhysicalTracks::from_tracks(&grid_info.rows)
        .iter_indexed()
        .map(|(index, row)| {
            let line = if vertical && vertical_rl {
                taffy::Line {
                    start: table_size.width - row.end,
                    end: table_size.width - row.start,
                }
            } else {
                row
            };
            (index, line)
        })
        .collect();
    let rows: Vec<_> = all_rows
        .iter()
        .copied()
        .filter(|(index, _)| !caption_rows.get(*index).copied().unwrap_or(false))
        .collect();

    let (x, mut y) = if vertical {
        let mut x: Vec<_> = rows
            .into_iter()
            .map(|(index, line)| TableGridTrack { line, index })
            .collect();
        x.sort_by(|left, right| left.line.start.total_cmp(&right.line.start));
        let y: Vec<TableGridTrack> = columns
            .into_iter()
            .map(|(index, line)| TableGridTrack { line, index })
            .collect();
        (x, y)
    } else {
        let x: Vec<TableGridTrack> = columns
            .into_iter()
            .map(|(index, line)| TableGridTrack { line, index })
            .collect();
        let y: Vec<TableGridTrack> = rows
            .into_iter()
            .map(|(index, line)| TableGridTrack { line, index })
            .collect();
        (x, y)
    };

    if !vertical && captions.iter().any(|(top, _)| *top) {
        let shift = border.top.max(table.style.gap.height.into_raw().value());
        for track in &mut y {
            track.line.start -= shift;
            track.line.end -= shift;
        }
    }

    let border_box = if vertical {
        let (x0, x1) = if vertical_rl {
            (
                captions
                    .iter()
                    .filter(|(top, _)| !top)
                    .map(|(_, rect)| rect.x1)
                    .reduce(f64::max)
                    .unwrap_or(0.0),
                captions
                    .iter()
                    .filter(|(top, _)| *top)
                    .map(|(_, rect)| rect.x0)
                    .reduce(f64::min)
                    .unwrap_or(table_size.width as f64),
            )
        } else {
            (
                captions
                    .iter()
                    .filter(|(top, _)| *top)
                    .map(|(_, rect)| rect.x1)
                    .reduce(f64::max)
                    .unwrap_or(0.0),
                captions
                    .iter()
                    .filter(|(top, _)| !top)
                    .map(|(_, rect)| rect.x0)
                    .reduce(f64::min)
                    .unwrap_or(table_size.width as f64),
            )
        };
        Rect::new(
            x0.clamp(0.0, table_size.width as f64),
            0.0,
            x1.clamp(0.0, table_size.width as f64),
            table_size.height as f64,
        )
    } else {
        Rect::new(
            0.0,
            captions
                .iter()
                .filter(|(top, _)| *top)
                .map(|(_, rect)| rect.y1)
                .reduce(f64::max)
                .unwrap_or(0.0),
            table_size.width as f64,
            captions
                .iter()
                .filter(|(top, _)| !top)
                .map(|(_, rect)| rect.y0)
                .reduce(f64::min)
                .unwrap_or(table_size.height as f64),
        )
    };
    TableGridGeometry { x, y, border_box }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TableBorderSide {
    Top,
    Right,
    Bottom,
    Left,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TableAxis {
    Row,
    Column,
}

/// The origin classes of CSS 2.1 §17.6.2.1, lowest to highest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum BorderOrigin {
    Table,
    ColumnGroup,
    Column,
    RowGroup,
    Row,
    Cell,
}

#[derive(Clone, Copy)]
struct BorderCandidate {
    width: f64,
    style: BorderStyle,
    color: Color,
    origin: BorderOrigin,
    /// Zero for the physical before/left/top side of an edge, one for after/right/bottom.
    order: u8,
}

impl BorderCandidate {
    fn beats(self, other: Self, prefer_after: bool) -> bool {
        if self.style == BorderStyle::Hidden {
            return other.style != BorderStyle::Hidden;
        }
        if other.style == BorderStyle::Hidden {
            return false;
        }
        if self.width != other.width {
            return self.width > other.width;
        }
        let rank = |style| match style {
            BorderStyle::Double => 8,
            BorderStyle::Solid => 7,
            BorderStyle::Dashed => 6,
            BorderStyle::Dotted => 5,
            BorderStyle::Ridge => 4,
            BorderStyle::Outset => 3,
            BorderStyle::Groove => 2,
            BorderStyle::Inset => 1,
            BorderStyle::None => 0,
            BorderStyle::Hidden => u8::MAX,
        };
        let style = rank(self.style);
        let other_style = rank(other.style);
        if style != other_style {
            return style > other_style;
        }
        if self.origin != other.origin {
            return self.origin > other.origin;
        }
        let order = |candidate: Self| {
            if prefer_after {
                1 - candidate.order
            } else {
                candidate.order
            }
        };
        order(self) < order(other)
    }
}

fn border_candidate(
    node: &blitz_dom::Node,
    origin: BorderOrigin,
    side: TableBorderSide,
    order: u8,
) -> Option<BorderCandidate> {
    let styles = node.primary_styles()?;
    let border = styles.get_border();
    let current = styles.clone_color();
    let (color, width, style) = match side {
        TableBorderSide::Top => (
            &border.border_top_color,
            border.border_top_width.clone(),
            border.border_top_style,
        ),
        TableBorderSide::Right => (
            &border.border_right_color,
            border.border_right_width.clone(),
            border.border_right_style,
        ),
        TableBorderSide::Bottom => (
            &border.border_bottom_color,
            border.border_bottom_width.clone(),
            border.border_bottom_style,
        ),
        TableBorderSide::Left => (
            &border.border_left_color,
            border.border_left_width.clone(),
            border.border_left_style,
        ),
    };
    Some(BorderCandidate {
        width: if style.none_or_hidden() {
            0.0
        } else {
            width.0.to_f32_px() as f64
        },
        style,
        color: color.resolve_to_absolute(&current).as_srgb_color(),
        origin,
        order,
    })
}

fn push_border_candidate(
    candidates: &mut SmallVec<[BorderCandidate; 16]>,
    node: &blitz_dom::Node,
    origin: BorderOrigin,
    side: TableBorderSide,
    order: u8,
) {
    if let Some(candidate) = border_candidate(node, origin, side, order) {
        candidates.push(candidate);
    }
}

fn resolve_border_candidates(
    candidates: &[BorderCandidate],
    prefer_after: bool,
) -> Option<Color> {
    let mut winner: Option<BorderCandidate> = None;
    for candidate in candidates.iter().copied() {
        if candidate.style == BorderStyle::Hidden {
            return None;
        }
        if winner.is_none_or(|current| candidate.beats(current, prefer_after)) {
            winner = Some(candidate);
        }
    }
    winner
        .filter(|candidate| candidate.width > 0.0 && candidate.style != BorderStyle::None)
        .map(|candidate| candidate.color)
}

/// A darker version of a colour (mirrors WebKit/Blink's `Color::Dark`): the
/// brightest channel is reduced by a fixed amount and the others scaled to match,
/// preserving hue.
fn darken(color: Color) -> Color {
    let [r, g, b, a] = color.components;
    let v = r.max(g).max(b);
    if v == 0.0 {
        return color;
    }
    let multiplier = ((v - 0.33) / v).max(0.0);
    Color::new([r * multiplier, g * multiplier, b * multiplier, a])
}

/// A lighter version of a colour (mirrors WebKit/Blink's `Color::Light`): the
/// brightest channel is increased by a fixed amount and the others scaled to
/// match, preserving hue.
fn lighten(color: Color) -> Color {
    let [r, g, b, a] = color.components;
    let v = r.max(g).max(b);
    if v == 0.0 {
        // Pure black: WebKit uses a fixed dark grey (0x545454).
        return Color::new([0.33, 0.33, 0.33, a]);
    }
    let multiplier = (v + 0.33).min(1.0) / v;
    Color::new([r * multiplier, g * multiplier, b * multiplier, a])
}

/// The colour of a single edge of an `inset`/`outset` border.
///
/// An `outset` border is raised: the top/left edges use the "lighter" shade and
/// the bottom/right edges the "darker" shade (`inset` is the reverse). The exact
/// shading matches Chrome's `CalculateInsetOutsetColor`:
///
/// * The darker edge is always darkened.
/// * The lighter edge keeps the base colour unchanged, *except* for colours dark
///   enough that the darkened edge wouldn't have enough contrast against the base
///   colour, which are lightened instead so the bevel stays visible.
pub(crate) fn beveled_edge_color(color: Color, edge: Edge, inset: bool) -> Color {
    let top_or_left = matches!(edge, Edge::Top | Edge::Left);
    let should_darken = top_or_left == inset;

    let dark_color = darken(color);
    if should_darken {
        return dark_color;
    }

    // Lighter edge. Chrome uses the base colour as-is unless the colour is dark
    // enough that the darkened edge lacks contrast against it. The red/green
    // shortcut mirrors Chrome: for those colours the contrast is always
    // sufficient, so the base colour is used without a contrast check.
    let [r, g, _, _] = color.components;
    if r >= 150.0 / 255.0 || g >= 92.0 / 255.0 {
        return color;
    }

    // Lighten only when the darkened edge would be too low-contrast to read.
    const MIN_CONTRAST_RATIO: f32 = 1.75;
    if contrast_ratio(color, dark_color) < MIN_CONTRAST_RATIO {
        lighten(color)
    } else {
        color
    }
}

/// The (outer half, inner half) colours of a single edge of a `groove`/`ridge`
/// border.
///
/// A `groove` looks carved into the page: its outer half is shaded like `inset`
/// and its inner half like `outset`. `ridge` is the reverse (raised).
pub(crate) fn grooved_edge_colors(color: Color, edge: Edge, ridge: bool) -> (Color, Color) {
    let outer = beveled_edge_color(color, edge, !ridge);
    let inner = beveled_edge_color(color, edge, ridge);
    (outer, inner)
}

/// The `(dash, gap)` lengths of a dashed border, as multiples of the border
/// thickness.
///
/// Matches Chrome: borders that are at least 3px use a 2:1 dash-to-gap ratio,
/// while thinner borders use longer dashes and gaps (3:2) so they still read as
/// dashes rather than dots or a solid line. `thickness` and `scale` are in device
/// pixels; the 3px threshold is applied in CSS pixels.
fn dashed_ratios(thickness: f64, scale: f64) -> (f64, f64) {
    if thickness >= 3.0 * scale {
        (2.0, 1.0)
    } else {
        (3.0, 2.0)
    }
}

/// Return `count` points spaced `spacing` apart (by arc length) along `path`,
/// starting at its beginning. Used to place dots along a rounded border.
fn sample_points_along_path(path: &BezPath, spacing: f64, count: usize) -> Vec<Point> {
    // Flatten to a polyline so we can walk it by arc length.
    let mut poly: Vec<Point> = Vec::new();
    kurbo::flatten(path.iter(), 0.1, |el| match el {
        PathEl::MoveTo(p) | PathEl::LineTo(p) => poly.push(p),
        PathEl::ClosePath => {
            if let Some(&first) = poly.first() {
                poly.push(first);
            }
        }
        _ => {}
    });

    let mut points = Vec::with_capacity(count);
    if poly.len() < 2 {
        return points;
    }

    let mut seg = 0;
    let mut seg_start = 0.0;
    let mut seg_len = (poly[1] - poly[0]).hypot();
    for i in 0..count {
        let target = i as f64 * spacing;
        while seg + 2 < poly.len() && target > seg_start + seg_len {
            seg_start += seg_len;
            seg += 1;
            seg_len = (poly[seg + 1] - poly[seg]).hypot();
        }
        let t = if seg_len > 0.0 {
            ((target - seg_start) / seg_len).clamp(0.0, 1.0)
        } else {
            0.0
        };
        points.push(poly[seg].lerp(poly[seg + 1], t));
    }
    points
}

impl ElementCx<'_, '_> {
    /// Draw all borders for a node
    pub(crate) fn draw_border(&self, scene: &mut impl PaintScene) {
        if let Some(grid) = self.table_grid_cx() {
            return grid.draw_border_box(scene);
        }
        self.draw_border_box(scene);
    }

    fn draw_border_box(&self, scene: &mut impl PaintScene) {
        let style = &*self.style;
        let border = style.get_border();
        let current_color = style.clone_color();

        // Fast path: a uniform-width, single-color solid border whose corners are
        // all circular arcs (equal x and y radius — sharp rectangles, ordinary
        // rounded rectangles and circles all qualify) can be drawn as a single
        // stroke along its center-line instead of a fill per edge. Besides being
        // faster, this works around Vello seam artifacts at the quarter-arc joins
        // when a thin circular/elliptical border is drawn as the
        // border-box/padding-box annulus.
        //
        // A full ellipse (`border-radius: 50%` on a non-square box) has the same
        // seam problem but no rounded-rect representation, so it is stroked as an
        // ellipse.
        let all_solid = border.border_top_style == BorderStyle::Solid
            && border.border_right_style == BorderStyle::Solid
            && border.border_bottom_style == BorderStyle::Solid
            && border.border_left_style == BorderStyle::Solid;
        if all_solid {
            let color = border
                .border_top_color
                .resolve_to_absolute(&current_color)
                .as_srgb_color();
            let same_color = [
                &border.border_right_color,
                &border.border_bottom_color,
                &border.border_left_color,
            ]
            .iter()
            .all(|c| c.resolve_to_absolute(&current_color).as_srgb_color() == color);
            let bw = self.frame.border_width;
            let uniform_width = bw.x0 == bw.y0 && bw.x0 == bw.x1 && bw.x0 == bw.y1 && bw.x0 > 0.0;
            if same_color && uniform_width && color.components[3] > 0.0 {
                let bb = self.frame.border_box;
                let pb = self.frame.padding_box;
                let width = (bb.width() - pb.width()) / 2.0;
                // Miter join keeps sharp corners square. It has no effect on rounded
                // corners, whose arcs meet the straight edges tangentially.
                let stroke = Stroke::new(width).with_join(Join::Miter);

                if self.frame.is_uniform_corner_border() {
                    let radii = self.frame.border_radii;
                    // The stroke matches the CSS border shape only when each corner is
                    // sharp or its radius is at least the border width (outer radius
                    // `r`, inner radius `r - width`, concentric). For 0 < r < width CSS
                    // draws a rounded outer edge against a sharp inner corner, which a
                    // stroke can't represent — leave those to the per-edge fill.
                    let is_exact = |r: Vec2| r.x == 0.0 || r.x >= width;
                    let stroke_is_exact = is_exact(radii.top_left)
                        && is_exact(radii.top_right)
                        && is_exact(radii.bottom_right)
                        && is_exact(radii.bottom_left);
                    if stroke_is_exact {
                        let half_width = width / 2.0;
                        // Center-line rect, and corner radii shrunk to match.
                        let centerline_box = bb - Insets::uniform(half_width);
                        let centerline_radius = |r: Vec2| (r.x - half_width).max(0.0);
                        let radii = kurbo::RoundedRectRadii::new(
                            centerline_radius(radii.top_left),
                            centerline_radius(radii.top_right),
                            centerline_radius(radii.bottom_right),
                            centerline_radius(radii.bottom_left),
                        );
                        let rr = kurbo::RoundedRect::from_rect(centerline_box, radii);
                        scene.stroke(&stroke, self.transform, color, None, &rr);
                        return;
                    }
                } else if self.frame.is_elliptical_border() {
                    // Center-line radii, straight from the boxes.
                    let rx = (bb.width() + pb.width()) / 4.0;
                    let ry = (bb.height() + pb.height()) / 4.0;
                    let e = kurbo::Ellipse::new(bb.center(), (rx, ry), 0.0);
                    scene.stroke(&stroke, self.transform, color, None, &e);
                    return;
                }
            }
        }

        // (colour, path) pairs to be filled. Several entries may share a colour;
        // they are grouped before filling so that adjacent same-coloured regions
        // are drawn together, avoiding anti-aliasing seams between them.
        //
        // At most 8 entries: 4 edges, each of which can contribute 2 (the outer
        // and inner halves of a `groove`/`ridge`).
        let mut borders: SmallVec<[(Color, BezPath); 8]> = SmallVec::new();

        for &edge in &[Edge::Top, Edge::Right, Edge::Bottom, Edge::Left] {
            let (color, edge_style) = match edge {
                Edge::Top => (&border.border_top_color, border.border_top_style),
                Edge::Right => (&border.border_right_color, border.border_right_style),
                Edge::Bottom => (&border.border_bottom_color, border.border_bottom_style),
                Edge::Left => (&border.border_left_color, border.border_left_style),
            };
            let color = color.resolve_to_absolute(&current_color).as_srgb_color();

            if color.components[3] <= 0.0 {
                continue;
            }

            match edge_style {
                // `none`/`hidden` produce a zero-width border during layout, but
                // guard against drawing them anyway.
                BorderStyle::None | BorderStyle::Hidden => {}

                // Dashed and dotted edges are drawn immediately as their own
                // (clipped) shapes rather than being batched with the solid edges.
                BorderStyle::Dotted => self.draw_dotted_border_edge(scene, edge, color),
                BorderStyle::Dashed => self.draw_dashed_border_edge(scene, edge, color),

                // A double border is two solid lines separated by a gap, splitting
                // the border width into three equal parts (outer line / gap / inner
                // line). Both lines share a color, so both rings are placed into a
                // single path.
                BorderStyle::Double => {
                    // Needs at least 3px (one device pixel per line and per gap) to
                    // render as two lines; thinner borders fall back to a solid
                    // fill, matching browser behaviour.
                    let path = if self.edge_width(edge) < 3.0 * self.scale {
                        self.frame.border_edge_shape(edge)
                    } else {
                        let mut path = self
                            .frame
                            .border_slice(0.0, 1.0 / 3.0)
                            .border_edge_shape(edge);
                        path.extend(
                            &self
                                .frame
                                .border_slice(2.0 / 3.0, 1.0)
                                .border_edge_shape(edge),
                        );
                        path
                    };
                    borders.push((color, path));
                }

                // `inset`/`outset` are solid, but each edge is shaded lighter or
                // darker to give a bevelled (3D) appearance.
                BorderStyle::Inset | BorderStyle::Outset => {
                    let inset = edge_style == BorderStyle::Inset;
                    let shade = beveled_edge_color(color, edge, inset);
                    borders.push((shade, self.frame.border_edge_shape(edge)));
                }

                // `groove`/`ridge` split each edge into an outer and an inner half,
                // each shaded as if it were `inset`/`outset`, producing a carved or
                // raised ridge.
                BorderStyle::Groove | BorderStyle::Ridge => {
                    let ridge = edge_style == BorderStyle::Ridge;
                    let (outer, inner) = grooved_edge_colors(color, edge, ridge);
                    borders.push((
                        outer,
                        self.frame.border_slice(0.0, 0.5).border_edge_shape(edge),
                    ));
                    borders.push((
                        inner,
                        self.frame.border_slice(0.5, 1.0).border_edge_shape(edge),
                    ));
                }

                // Solid fills the whole edge region with the border colour.
                BorderStyle::Solid => {
                    borders.push((color, self.frame.border_edge_shape(edge)));
                }
            }
        }

        if borders.is_empty() {
            return;
        }

        // Group together identical colors by sorting, then fill each group as a
        // single path.
        borders.sort_unstable_by(|a, b| {
            a.0.components
                .partial_cmp(&b.0.components)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut start = 0;
        while start < borders.len() {
            let color = borders[start].0;
            let mut path = std::mem::take(&mut borders[start].1);
            let mut next = start + 1;
            while next < borders.len() && borders[next].0 == color {
                path.extend(&borders[next].1);
                next += 1;
            }
            scene.fill(Fill::NonZero, self.transform, color, None, &path);
            start = next;
        }
    }

    /// The border width (in device pixels) of a single edge.
    fn edge_width(&self, edge: Edge) -> f64 {
        match edge {
            Edge::Top => self.frame.border_width.y0,
            Edge::Bottom => self.frame.border_width.y1,
            Edge::Left => self.frame.border_width.x0,
            Edge::Right => self.frame.border_width.x1,
        }
    }

    /// Draw a single `dashed` border edge.
    ///
    /// The dashes are produced by stroking the centre line of the border with a
    /// dash pattern (butt caps give them flat, square ends). Everything is clipped
    /// to the edge's region (the same trapezoid used by the solid path) so corners
    /// are mitred, adjacent edges of different colors don't overlap, and any
    /// border-radius is respected.
    fn draw_dashed_border_edge(&self, scene: &mut impl PaintScene, edge: Edge, color: Color) {
        let thickness = self.edge_width(edge);
        if thickness <= 0.0 {
            return;
        }

        let (dash_ratio, gap_ratio) = dashed_ratios(thickness, self.scale);

        // Work out the centre line to stroke and the dash/gap lengths along it.
        let (centerline, dash, gap) = if self.frame.has_border_radius() {
            // Rounded corners: stroke the rounded centre line running through the
            // whole perimeter. Every edge uses the same centre line and pattern, so
            // dashes stay continuous and aligned as they wrap around each corner.
            // Dash and gap keep their ratio, sized so a whole number of periods fit
            // exactly around the perimeter (kurbo merges the dash across the seam).
            let mut centerline = self.frame.border_slice(0.0, 0.5).padding_box_path();
            centerline.close_path();
            let perimeter = centerline.perimeter(0.1);
            if perimeter <= 0.0 {
                return;
            }
            let period0 = (dash_ratio + gap_ratio) * thickness;
            let count = (perimeter / period0).round().max(1.0);
            let period = perimeter / count;
            let dash = period * dash_ratio / (dash_ratio + gap_ratio);
            (centerline, dash, period - dash)
        } else {
            // Square corners: stroke a straight line through the middle of the edge,
            // corner to corner. Dash and gap keep their ratio but are scaled so the
            // edge both starts and ends with a dash (covering the corners).
            let bb = self.frame.border_box;
            let half = thickness / 2.0;
            let (start, end, length) = match edge {
                Edge::Top => (
                    Point::new(bb.x0, bb.y0 + half),
                    Point::new(bb.x1, bb.y0 + half),
                    bb.width(),
                ),
                Edge::Bottom => (
                    Point::new(bb.x0, bb.y1 - half),
                    Point::new(bb.x1, bb.y1 - half),
                    bb.width(),
                ),
                Edge::Left => (
                    Point::new(bb.x0 + half, bb.y0),
                    Point::new(bb.x0 + half, bb.y1),
                    bb.height(),
                ),
                Edge::Right => (
                    Point::new(bb.x1 - half, bb.y0),
                    Point::new(bb.x1 - half, bb.y1),
                    bb.height(),
                ),
            };
            if length <= 0.0 {
                return;
            }
            let dash0 = dash_ratio * thickness;
            let gap0 = gap_ratio * thickness;
            // `count` dashes with `count - 1` gaps between them.
            let count = ((length + gap0) / (dash0 + gap0)).round().max(1.0);
            let r = dash_ratio / gap_ratio;
            let gap = length / (count * r + count - 1.0);

            let mut line = BezPath::new();
            line.move_to(start);
            line.line_to(end);
            (line, r * gap, gap)
        };

        let stroke = Stroke::new(thickness)
            .with_caps(Cap::Butt)
            .with_dashes(0.0, [dash, gap]);
        let clip = self.frame.border_edge_shape(edge);
        scene.push_clip_layer(Fill::NonZero, self.transform, &clip);
        scene.stroke(&stroke, self.transform, color, None, &centerline);
        scene.pop_layer();
    }

    /// Draw a single `dotted` border edge.
    ///
    /// Dots are filled circles (diameter == border thickness). Unlike dashes they
    /// can't be produced by stroking, because kurbo doesn't emit zero-length dashes
    /// (so a round-capped dash pattern would render nothing); drawing them
    /// explicitly also lets us anchor a dot in each square corner. Everything is
    /// clipped to the edge's region, as for the other styles.
    fn draw_dotted_border_edge(&self, scene: &mut impl PaintScene, edge: Edge, color: Color) {
        let thickness = self.edge_width(edge);
        if thickness <= 0.0 {
            return;
        }
        let radius = thickness / 2.0;

        let mut path = BezPath::new();
        if self.frame.has_border_radius() {
            // Rounded corners: dots spaced evenly around the rounded centre line so
            // the ring wraps seamlessly around the corners.
            let mut centerline = self.frame.border_slice(0.0, 0.5).padding_box_path();
            centerline.close_path();
            let perimeter = centerline.perimeter(0.1);
            if perimeter <= 0.0 {
                return;
            }
            let count = (perimeter / (2.0 * thickness)).round().max(1.0);
            let spacing = perimeter / count;
            for center in sample_points_along_path(&centerline, spacing, count as usize) {
                path.extend(Circle::new(center, radius).path_elements(0.1));
            }
        } else {
            // Square corners: a dot is anchored in each corner (both ends of the
            // edge, inset by the radius so it sits snugly in the corner) and the
            // rest are spread evenly between them.
            let bb = self.frame.border_box;
            let length = match edge {
                Edge::Top | Edge::Bottom => bb.width(),
                Edge::Left | Edge::Right => bb.height(),
            };
            if length <= 0.0 {
                return;
            }
            let dot_center = |along: f64| -> Point {
                match edge {
                    Edge::Top => Point::new(bb.x0 + along, bb.y0 + radius),
                    Edge::Bottom => Point::new(bb.x0 + along, bb.y1 - radius),
                    Edge::Left => Point::new(bb.x0 + radius, bb.y0 + along),
                    Edge::Right => Point::new(bb.x1 - radius, bb.y0 + along),
                }
            };

            let span = length - thickness;
            if span <= 0.0 {
                // Edge too short to fit two dots; place a single centred dot.
                let center = dot_center(length / 2.0);
                path.extend(Circle::new(center, radius).path_elements(0.1));
            } else {
                // Aim for a centre-to-centre spacing of about two dot diameters,
                // then adjust it so the first and last dots land in the corners.
                let gaps = (span / (2.0 * thickness)).round().max(1.0) as usize;
                let spacing = span / gaps as f64;
                for i in 0..=gaps {
                    let center = dot_center(radius + i as f64 * spacing);
                    path.extend(Circle::new(center, radius).path_elements(0.1));
                }
            }
        }

        let clip = self.frame.border_edge_shape(edge);
        scene.push_clip_layer(Fill::NonZero, self.transform, &clip);
        scene.fill(Fill::NonZero, self.transform, color, None, &path);
        scene.pop_layer();
    }

    /// Resolve one segment of a collapsed grid edge by CSS 2.1 §17.6.2.1.
    ///
    /// `before` and `after` are the tracks on the two physical sides of the edge, each with the
    /// side of its own border box that meets the edge. A cell grid is built once per table so a
    /// spanned cell contributes the same border along every grid segment it covers.
    #[allow(clippy::too_many_arguments)]
    fn resolve_table_border_color(
        &self,
        element: &SpecialElementData,
        cell_grid: &[Option<NodeId>],
        grid_columns: usize,
        boundary_axis: TableAxis,
        before: Option<(usize, TableBorderSide)>,
        after: Option<(usize, TableBorderSide)>,
        cross_axis: TableAxis,
        cross: usize,
        outer: bool,
    ) -> Option<Color> {
        let SpecialElementData::TableRoot(table) = element else {
            return None;
        };
        let mut candidates: SmallVec<[BorderCandidate; 16]> = SmallVec::new();

        let track = |axis: TableAxis, index: usize| -> Option<(NodeId, Option<NodeId>, BorderOrigin, BorderOrigin)> {
            match axis {
                TableAxis::Row => {
                    let row = table.rows.get(index)?;
                    Some((
                        row.node_id,
                        row.group,
                        BorderOrigin::Row,
                        BorderOrigin::RowGroup,
                    ))
                }
                TableAxis::Column => {
                    let column = table.columns.get(index)?;
                    Some((
                        column.node_id,
                        column.group,
                        BorderOrigin::Column,
                        BorderOrigin::ColumnGroup,
                    ))
                }
            }
        };
        let push_track = |candidates: &mut SmallVec<[BorderCandidate; 16]>,
                          axis: TableAxis,
                          index: usize,
                          side: TableBorderSide,
                          order: u8| {
            let Some((node_id, group, origin, group_origin)) = track(axis, index) else {
                return;
            };
            if let Some(candidate) = self.context.dom.get_node(node_id).and_then(|node| {
                border_candidate(node, origin, side, order)
            }) {
                candidates.push(candidate);
            }
            if let Some(candidate) = group.and_then(|group| {
                self.context
                    .dom
                    .get_node(group)
                    .and_then(|node| border_candidate(node, group_origin, side, order))
            }) {
                candidates.push(candidate);
            }
        };

        for (index, side, order) in before
            .map(|(index, side)| (index, side, 0))
            .into_iter()
            .chain(after.map(|(index, side)| (index, side, 1)))
        {
            push_track(&mut candidates, boundary_axis, index, side, order);

            let (row, column) = match boundary_axis {
                TableAxis::Row => (index, cross),
                TableAxis::Column => (cross, index),
            };
            if let Some(cell_id) = cell_grid
                .get(row * grid_columns + column)
                .copied()
                .flatten()
                && let Some(cell) = self.context.dom.get_node(cell_id)
            {
                push_border_candidate(
                    &mut candidates,
                    cell,
                    BorderOrigin::Cell,
                    side,
                    order,
                );
            }
        }

        let edge_side = after
            .map(|(_, side)| side)
            .or_else(|| before.map(|(_, side)| side))
            .expect("a table border edge has at least one side");
        if outer {
            push_track(
                &mut candidates,
                cross_axis,
                cross,
                edge_side,
                2,
            );
            push_border_candidate(
                &mut candidates,
                self.node,
                BorderOrigin::Table,
                edge_side,
                2,
            );
        }

        let prefer_after = matches!(edge_side, TableBorderSide::Left | TableBorderSide::Right)
            && self.style.clone_direction() == style::computed_values::direction::T::Rtl;
        resolve_border_candidates(&candidates, prefer_after)
    }

    pub(crate) fn draw_table_borders(&self, scene: &mut impl PaintScene) {
        let SpecialElementData::TableRoot(table) = &self.element.special_data else {
            return;
        };
        // Borders are only handled at the table level when BorderCollapse::Collapse
        if table.border_collapse != BorderCollapse::Collapse {
            return;
        }

        let Some(grid_info) = &mut *table.computed_grid_info.borrow_mut() else {
            return;
        };
        let writing_mode = self.node.writing_mode();
        let vertical = writing_mode.is_vertical();
        let layout = *self.node.final_layout();
        let captions: Vec<(bool, Rect)> = table
            .cells
            .iter()
            .filter(|cell| cell.caption)
            .filter_map(|cell| {
                let layout = self.context.dom.get_node(cell.node_id)?.final_layout();
                Some((
                    cell.caption_top,
                    Rect::new(
                        layout.location.x as f64,
                        layout.location.y as f64,
                        (layout.location.x + layout.size.width) as f64,
                        (layout.location.y + layout.size.height) as f64,
                    ),
                ))
            })
            .collect();
        let geometry = table_grid_geometry(
            &self.element.special_data,
            grid_info,
            vertical,
            writing_mode.is_vertical_rl(),
            layout.size,
            &captions,
            layout.border,
        );
        let x_tracks: Vec<_> = geometry.x.iter().map(|track| track.line).collect();
        let y_tracks: Vec<_> = geometry.y.iter().map(|track| track.line).collect();
        if x_tracks.is_empty() || y_tracks.is_empty() {
            return;
        }

        // Taffy's physical x/y tracks swap axes for vertical tables. Keep the logical
        // row/column index beside each physical track so border candidates are read
        // from the right row, row group, column and column group.
        let x_axis = if vertical {
            TableAxis::Row
        } else {
            TableAxis::Column
        };
        let y_axis = if vertical {
            TableAxis::Column
        } else {
            TableAxis::Row
        };
        let x_indices: Vec<usize> = geometry.x.iter().map(|track| track.index).collect();
        let y_indices: Vec<usize> = geometry.y.iter().map(|track| track.index).collect();

        let mut row_cell_start = vec![None; table.rows.len()];
        for cell in table.cells.iter().filter(|cell| !cell.caption) {
            if let Some(layout) = self
                .context
                .dom
                .get_node(cell.node_id)
                .map(|node| node.final_layout())
            {
                row_cell_start[cell.row as usize].get_or_insert(layout.location.y);
            }
        }
        let row_outside = |index: usize| {
            let Some(track) = y_tracks.get(index) else {
                return false;
            };
            let end = (self.transform * Point::new(0.0, track.end as f64 * self.scale)).y;
            if end > self.clip_rect.y1 + 0.5 {
                return false;
            }
            let Some(next_row) = y_indices.get(index + 1).copied() else {
                return false;
            };
            let Some(start) = row_cell_start.get(next_row).copied().flatten() else {
                return false;
            };
            let screen = (self.transform * Point::new(0.0, start as f64 * self.scale)).y;
            screen >= self.clip_rect.y1 - 0.5
        };

        let grid_columns = grid_info.columns.positions.len();
        let grid_rows = grid_info.rows.positions.len();
        let mut cell_grid = vec![None; grid_rows.saturating_mul(grid_columns)];
        for cell in table.cells.iter().filter(|cell| !cell.caption) {
            let Some(node) = self.context.dom.get_node(cell.node_id) else {
                continue;
            };
            let span = |name| {
                node.attr(name)
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .filter(|span| *span > 0)
                    .unwrap_or(1)
            };
            let row_end = (cell.row as usize + span(local_name!("rowspan"))).min(grid_rows);
            let column_end = (cell.column as usize + span(local_name!("colspan"))).min(grid_columns);
            for row in cell.row as usize..row_end {
                for column in cell.column as usize..column_end {
                    cell_grid[row * grid_columns + column] = Some(cell.node_id);
                }
            }
        }

        // The borders are laid out as the gutters between the tracks, and as the border
        // of the table around them. Track positions are relative to the table's border box.
        let border_width = table.style.gap.map(|gap| gap.into_raw().value() as f64);
        let (x_border, y_border) = if vertical {
            (border_width.height, border_width.width)
        } else {
            (border_width.width, border_width.height)
        };
        let x_origin = x_tracks.first().map_or(0.0, |track| track.start) as f64;
        let x_end = x_tracks.last().map_or(0.0, |track| track.end) as f64;
        let y_origin = y_tracks.first().map_or(0.0, |track| track.start) as f64;
        let y_end = y_tracks.last().map_or(0.0, |track| track.end) as f64;
        let left = x_origin - x_border;
        let right = x_end + x_border;
        let top = y_origin - y_border;
        let bottom = y_end + y_border;
        let table_clip = self
            .multicol_clip_rect()
            .map(|clip| clip.intersect(self.clip_rect))
            .filter(|clip| clip.area() > 0.0);
        if let Some(clip) = table_clip {
            scene.push_clip_layer(Fill::NonZero, Affine::IDENTITY, &clip);
        }
        let mut fill = |x0: f64, y0: f64, x1: f64, y1: f64, color: Color| {
            let shape = Rect::new(x0, y0, x1, y1).scale_from_origin(self.scale);
            scene.fill(Fill::NonZero, self.transform, color, None, &shape);
        };

        // Every physical horizontal edge, split by the crossing tracks so a column's
        // border can win over the row's on that segment.
        let horizontal_edges: Vec<_> = std::iter::once((
            top,
            top + y_border,
            None,
            y_indices.first().copied().map(|index| (index, TableBorderSide::Top)),
            true,
            false,
        ))
        .chain(
            y_tracks
                .iter()
                .zip(y_tracks.iter().skip(1))
                .enumerate()
                .map(|(index, (previous, next))| {
                    (
                        previous.end as f64,
                        next.start as f64,
                        Some((y_indices[index], TableBorderSide::Bottom)),
                        Some((y_indices[index + 1], TableBorderSide::Top)),
                        false,
                        row_outside(index),
                    )
                }),
        )
        .chain(std::iter::once((
            bottom - y_border,
            bottom,
            y_indices
                .last()
                .copied()
                .map(|index| (index, TableBorderSide::Bottom)),
            None,
            true,
            false,
        )))
        .collect();
        for (y0, y1, before, after, outer, outside) in horizontal_edges {
            if outside {
                continue;
            }
            for (index, track) in x_tracks.iter().enumerate() {
                let x0 = if !outer {
                    track.start as f64
                } else if index == 0 {
                    left
                } else {
                    x_tracks[index - 1].end as f64
                };
                let x1 = if !outer {
                    track.end as f64
                } else if index + 1 == x_tracks.len() {
                    right
                } else {
                    track.end as f64
                };
                let cross = x_indices[index];
                let Some(color) = self.resolve_table_border_color(
                    &self.element.special_data,
                    &cell_grid,
                    grid_columns,
                    y_axis,
                    before,
                    after,
                    x_axis,
                    cross,
                    outer,
                ) else {
                    continue;
                };
                fill(x0, y0, x1, y1, color);
            }
        }

        // Every physical vertical edge, split by the crossing tracks.
        let vertical_edges: Vec<_> = std::iter::once((
            left,
            left + x_border,
            None,
            x_indices.first().copied().map(|index| (index, TableBorderSide::Left)),
            true,
        ))
        .chain(
            x_tracks
                .iter()
                .zip(x_tracks.iter().skip(1))
                .enumerate()
                .map(|(index, (previous, next))| {
                    (
                        previous.end as f64,
                        next.start as f64,
                        Some((x_indices[index], TableBorderSide::Right)),
                        Some((x_indices[index + 1], TableBorderSide::Left)),
                        false,
                    )
                }),
        )
        .chain(std::iter::once((
            right - x_border,
            right,
            x_indices
                .last()
                .copied()
                .map(|index| (index, TableBorderSide::Right)),
            None,
            true,
        )))
        .collect();
        for (x0, x1, before, after, outer) in vertical_edges {
            for (index, track) in y_tracks.iter().enumerate() {
                let y0 = if !outer {
                    track.start as f64
                } else if index == 0 {
                    top
                } else {
                    y_tracks[index - 1].end as f64
                };
                let y1 = if !outer {
                    if row_outside(index) {
                        track.end as f64 - y_border
                    } else {
                        track.end as f64
                    }
                } else if index + 1 == y_tracks.len() {
                    bottom
                } else {
                    track.end as f64
                };
                let cross = y_indices[index];
                let Some(color) = self.resolve_table_border_color(
                    &self.element.special_data,
                    &cell_grid,
                    grid_columns,
                    x_axis,
                    before,
                    after,
                    y_axis,
                    cross,
                    outer,
                ) else {
                    continue;
                };
                fill(x0, y0, x1, y1, color);
            }
        }
        if table_clip.is_some() {
            scene.pop_layer();
        }
    }

    /// ❌ dotted - Defines a dotted border
    /// ❌ dashed - Defines a dashed border
    /// ✅ solid - Defines a solid border
    /// ❌ double - Defines a double border
    /// ❌ groove - Defines a 3D grooved border. The effect depends on the border-color value
    /// ❌ ridge - Defines a 3D ridged border. The effect depends on the border-color value
    /// ❌ inset - Defines a 3D inset border. The effect depends on the border-color value
    /// ❌ outset - Defines a 3D outset border. The effect depends on the border-color value
    /// ✅ none - Defines no border
    /// ✅ hidden - Defines a hidden border
    pub(crate) fn draw_outline(&self, scene: &mut impl PaintScene) {
        let outline = self.style.get_outline();

        let current_color = self.style.clone_color();
        let color = outline
            .outline_color
            .resolve_to_absolute(&current_color)
            .as_srgb_color();

        let style = match outline.outline_style {
            OutlineStyle::Auto => return,
            OutlineStyle::BorderStyle(style) => style,
        };

        let path = match style {
            BorderStyle::None | BorderStyle::Hidden => return,
            BorderStyle::Solid => self.frame.outline(),

            // TODO: Implement other border styles
            BorderStyle::Inset
            | BorderStyle::Groove
            | BorderStyle::Outset
            | BorderStyle::Ridge
            | BorderStyle::Dotted
            | BorderStyle::Dashed
            | BorderStyle::Double => self.frame.outline(),
        };

        scene.fill(Fill::NonZero, self.transform, color, None, &path);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BorderCandidate, BorderOrigin, physical_table_tracks, resolve_border_candidates,
    };
    use peniko::Color;
    use style::values::computed::BorderStyle;
    use taffy::Line;

    fn line(start: f32, end: f32) -> Line<f32> {
        Line { start, end }
    }

    fn candidate(
        width: f64,
        style: BorderStyle,
        origin: BorderOrigin,
        order: u8,
        color: Color,
    ) -> BorderCandidate {
        BorderCandidate {
            width,
            style,
            color,
            origin,
            order,
        }
    }

    #[test]
    fn horizontal_table_tracks_stay_on_their_axes() {
        let columns = [line(10.0, 30.0), line(30.0, 60.0)];
        let rows = [line(5.0, 15.0), line(15.0, 40.0)];
        let (x, y) = physical_table_tracks(columns, rows, false, false, 60.0);
        assert_eq!(x, columns);
        assert_eq!(y, rows);
    }

    #[test]
    fn vertical_table_tracks_follow_the_physical_axes() {
        let columns = [line(0.0, 40.0), line(40.0, 90.0)];
        let rows = [line(0.0, 20.0), line(20.0, 70.0)];

        let (x, y) = physical_table_tracks(columns, rows, true, false, 70.0);
        assert_eq!(x, rows);
        assert_eq!(y, columns);

        let (x, y) = physical_table_tracks(columns, rows, true, true, 70.0);
        assert_eq!(x, [line(0.0, 50.0), line(50.0, 70.0)]);
        assert_eq!(y, columns);
    }

    #[test]
    fn hidden_suppresses_a_wider_visible_border() {
        let red = Color::from_rgb8(255, 0, 0);
        let blue = Color::from_rgb8(0, 0, 255);
        let candidates = [
            candidate(20.0, BorderStyle::Solid, BorderOrigin::Cell, 0, blue),
            candidate(1.0, BorderStyle::Hidden, BorderOrigin::Table, 1, red),
        ];
        assert_eq!(resolve_border_candidates(&candidates, false), None);
    }

    #[test]
    fn width_beats_style_and_origin() {
        let red = Color::from_rgb8(255, 0, 0);
        let green = Color::from_rgb8(0, 255, 0);
        let candidates = [
            candidate(5.0, BorderStyle::Double, BorderOrigin::Cell, 0, green),
            candidate(9.0, BorderStyle::Inset, BorderOrigin::Table, 1, red),
        ];
        assert_eq!(
            resolve_border_candidates(&candidates, false),
            Some(red)
        );
    }

    #[test]
    fn equal_width_prefers_style_then_cell_origin() {
        let red = Color::from_rgb8(255, 0, 0);
        let green = Color::from_rgb8(0, 255, 0);
        let blue = Color::from_rgb8(0, 0, 255);

        let styles = [
            candidate(5.0, BorderStyle::Dashed, BorderOrigin::Cell, 0, green),
            candidate(5.0, BorderStyle::Solid, BorderOrigin::Table, 1, red),
        ];
        assert_eq!(
            resolve_border_candidates(&styles, false),
            Some(red),
            "solid beats dashed before origin is considered"
        );

        let origins = [
            candidate(5.0, BorderStyle::Solid, BorderOrigin::Column, 0, blue),
            candidate(5.0, BorderStyle::Solid, BorderOrigin::Cell, 1, green),
        ];
        assert_eq!(
            resolve_border_candidates(&origins, false),
            Some(green),
            "the cell's border wins when width and style tie"
        );
    }

    #[test]
    fn equal_borders_follow_the_physical_side_preference() {
        let left = Color::from_rgb8(255, 0, 0);
        let right = Color::from_rgb8(0, 255, 0);
        let candidates = [
            candidate(5.0, BorderStyle::Solid, BorderOrigin::Cell, 0, left),
            candidate(5.0, BorderStyle::Solid, BorderOrigin::Cell, 1, right),
        ];
        assert_eq!(
            resolve_border_candidates(&candidates, false),
            Some(left)
        );
        assert_eq!(
            resolve_border_candidates(&candidates, true),
            Some(right)
        );
    }
}
