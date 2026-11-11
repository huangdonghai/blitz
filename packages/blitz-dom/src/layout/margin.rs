use std::collections::{HashMap, HashSet};

use parley::{AlignmentOptions, IndentOptions};
use style::logical_geometry::PhysicalSide;
use style::values::computed::CSSPixelLength;
use taffy::{BoxSizing, CoreStyle as _, MaybeResolve as _, ResolveOrZero as _, Size};

use crate::layout::construct::build_margin_note_content_layout;
use crate::layout::fragmentation::{BreakPlan, FlowAxis};
use crate::layout::resolve_calc_value;
use crate::node::{
    MarginNoteGroup, MarginNotePageFragment, MarginNoteRect, MarginNoteSide, TextBrush, TextLayout,
};
use crate::stylo_to_parley;
use crate::{BaseDocument, NodeId};

#[derive(Clone, Copy)]
struct Rect {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

impl Rect {
    fn new(left: f32, top: f32, right: f32, bottom: f32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    fn page_width(self) -> f32 {
        self.right - self.left
    }

    fn page_height(self) -> f32 {
        self.bottom - self.top
    }
}

impl From<Rect> for MarginNoteRect {
    fn from(rect: Rect) -> Self {
        Self {
            left: rect.left,
            top: rect.top,
            right: rect.right,
            bottom: rect.bottom,
        }
    }
}

#[derive(Clone, Copy)]
struct RootGeometry {
    border_x: f32,
    border_y: f32,
    content_x: f32,
    content_y: f32,
    content_width: f32,
    content_height: f32,
    vertical: bool,
    block_starts_at_right: bool,
    block_start_side: PhysicalSide,
    block_end_side: PhysicalSide,
    inline_start_side: PhysicalSide,
}

struct Plan {
    root_id: NodeId,
    note_id: NodeId,
    side: MarginNoteSide,
    band: Rect,
    declaration_stack: f32,
    order: u32,
    root: RootGeometry,
    wm: stylo_taffy::WritingMode,
    content_inline: f32,
    content_block: f32,
    outer_block: f32,
    margin: taffy::Rect<f32>,
    padding: taffy::Rect<f32>,
    border: taffy::Rect<f32>,
    text: String,
    layout: parley::layout::Layout<TextBrush>,
    ruby_ranges: Vec<std::ops::Range<usize>>,
    emphasis_ranges: Vec<(std::ops::Range<usize>, crate::util::Color)>,
}

struct PendingNote {
    root_id: NodeId,
    note_id: NodeId,
    side: MarginNoteSide,
    band: Rect,
    declaration_stack: f32,
    order: u32,
    root: RootGeometry,
    wm: stylo_taffy::WritingMode,
    content_inline: f32,
    margin: taffy::Rect<f32>,
    padding: taffy::Rect<f32>,
    border: taffy::Rect<f32>,
    block_edges: f32,
    noncontent_block: f32,
    declared_block: Option<f32>,
    min_block: Option<f32>,
    max_block: Option<f32>,
    border_box_sizing: bool,
    content: TextLayout,
}

struct PlacedGroup {
    root_id: NodeId,
    root: RootGeometry,
    wm: stylo_taffy::WritingMode,
    group: MarginNoteGroup,
    block_start_inset: f32,
    block_end_inset: f32,
    margin: taffy::Rect<f32>,
    padding: taffy::Rect<f32>,
    border: taffy::Rect<f32>,
}

fn side_physical(wm: stylo_taffy::WritingMode, side: MarginNoteSide) -> PhysicalSide {
    match side {
        MarginNoteSide::InlineStart => wm.inline_start_physical_side(),
        MarginNoteSide::InlineEnd => wm.inline_end_physical_side(),
    }
}

fn page_rect(doc: &BaseDocument) -> Rect {
    if let Some(page) = doc.page_box {
        let area = page.area();
        Rect::new(0.0, 0.0, area.width, area.height)
    } else {
        let viewport = doc.stylist.device().au_viewport_size();
        Rect::new(
            0.0,
            0.0,
            viewport.width.to_f32_px(),
            viewport.height.to_f32_px(),
        )
    }
}

fn rect_side(rect: taffy::Rect<f32>, side: PhysicalSide) -> f32 {
    match side {
        PhysicalSide::Top => rect.top,
        PhysicalSide::Bottom => rect.bottom,
        PhysicalSide::Left => rect.left,
        PhysicalSide::Right => rect.right,
    }
}

fn page_side(rect: Rect, side: PhysicalSide) -> f32 {
    match side {
        PhysicalSide::Top => rect.top,
        PhysicalSide::Bottom => rect.bottom,
        PhysicalSide::Left => rect.left,
        PhysicalSide::Right => rect.right,
    }
}

/// Read a value from a rect expressed in the logical axes of `wm`.
fn logical_rect_side(
    rect: taffy::Rect<f32>,
    wm: stylo_taffy::WritingMode,
    physical: PhysicalSide,
) -> f32 {
    if physical == wm.inline_start_physical_side() {
        rect.left
    } else if physical == wm.inline_end_physical_side() {
        rect.right
    } else if physical == wm.block_start_physical_side() {
        rect.top
    } else {
        rect.bottom
    }
}

fn physical_side_index(side: PhysicalSide) -> u8 {
    match side {
        PhysicalSide::Top => 0,
        PhysicalSide::Right => 1,
        PhysicalSide::Bottom => 2,
        PhysicalSide::Left => 3,
    }
}

fn stack_position(page: Rect, root: &RootGeometry, progress: f32) -> f32 {
    if root.vertical {
        let physical = if root.block_starts_at_right {
            root.content_x + root.content_width - progress
        } else {
            root.content_x + progress
        };
        if root.block_starts_at_right {
            page.right - physical
        } else {
            physical - page.left
        }
    } else {
        root.content_y + progress - page.top
    }
}

fn root_progress(page: Rect, root: &RootGeometry, stack: f32) -> f32 {
    if root.vertical {
        let physical = if root.block_starts_at_right {
            page.right - stack
        } else {
            page.left + stack
        };
        if root.block_starts_at_right {
            root.content_x + root.content_width - physical
        } else {
            physical - root.content_x
        }
    } else {
        page.top + stack - root.content_y
    }
}

fn root_static_margins(doc: &BaseDocument, wm: stylo_taffy::WritingMode) -> (f32, f32) {
    let Some(root) = doc.try_root_element() else {
        return (0.0, 0.0);
    };
    let (_, layout) = doc.physical_unrounded_geometry(root.id);
    let physical = layout.margin;

    // CSS lets `body` establish the page's writing mode while the root keeps the
    // value it was computed with. Carry the author's logical inline margins
    // across the orthogonal axes instead of reading the now-unused physical side.
    let root_wm = root.writing_mode();
    if root_wm.is_vertical() != wm.is_vertical() {
        let start = rect_side(physical, root_wm.inline_start_physical_side());
        let end = rect_side(physical, root_wm.inline_end_physical_side());
        let fallback_start = rect_side(physical, wm.inline_start_physical_side());
        let fallback_end = rect_side(physical, wm.inline_end_physical_side());
        return (
            if start != 0.0 { start } else { fallback_start }.max(0.0),
            if end != 0.0 { end } else { fallback_end }.max(0.0),
        );
    }

    (
        rect_side(physical, wm.inline_start_physical_side()).max(0.0),
        rect_side(physical, wm.inline_end_physical_side()).max(0.0),
    )
}

fn resolve_side(
    declared: stylo_to_parley::stylo::HutuNoteSide,
    start: f32,
    end: f32,
) -> MarginNoteSide {
    match declared {
        stylo_to_parley::stylo::HutuNoteSide::InlineStart => MarginNoteSide::InlineStart,
        stylo_to_parley::stylo::HutuNoteSide::InlineEnd => MarginNoteSide::InlineEnd,
        stylo_to_parley::stylo::HutuNoteSide::Auto => {
            if end > start {
                MarginNoteSide::InlineEnd
            } else {
                MarginNoteSide::InlineStart
            }
        }
    }
}

fn available_margin_band(
    doc: &BaseDocument,
    root_id: NodeId,
    page: Rect,
    wm: stylo_taffy::WritingMode,
    side: MarginNoteSide,
    start_margin: f32,
    end_margin: f32,
) -> Option<Rect> {
    let physical = side_physical(wm, side);
    let page_outer = page_side(page, physical);
    let floor = if matches!(side, MarginNoteSide::InlineStart) {
        start_margin
    } else {
        end_margin
    };
    let mut edge = if matches!(physical, PhysicalSide::Left | PhysicalSide::Top) {
        page_outer + floor
    } else {
        page_outer - floor
    };
    let mut found = floor > 0.01;
    let root_id_to_skip = doc.try_root_element().map(|root| root.id);

    let mut ancestors = Vec::new();
    let mut current = Some(root_id);
    while let Some(id) = current {
        let node = &doc.nodes[id];
        if node
            .element_data()
            .is_some_and(|element| element.layout_data.is_some())
        {
            ancestors.push(id);
        }
        current = node.parent;
    }
    ancestors.reverse();

    for id in ancestors {
        if Some(id) == root_id_to_skip {
            continue;
        }
        let (pos, layout) = doc.physical_unrounded_geometry(id);
        let border = taffy::Rect {
            left: pos.x,
            top: pos.y,
            right: pos.x + layout.size.width,
            bottom: pos.y + layout.size.height,
        };
        let (outside, inside) = match physical {
            PhysicalSide::Left => (border.left - layout.margin.left, border.left),
            PhysicalSide::Right => (border.right + layout.margin.right, border.right),
            PhysicalSide::Top => (border.top - layout.margin.top, border.top),
            PhysicalSide::Bottom => (border.bottom + layout.margin.bottom, border.bottom),
        };
        if (inside - outside).abs() <= 0.01 || !outside.is_finite() || !inside.is_finite() {
            continue;
        }
        let touches = matches!(physical, PhysicalSide::Left | PhysicalSide::Top)
            .then(|| outside <= edge + 0.01 && inside > edge + 0.01)
            .unwrap_or_else(|| outside >= edge - 0.01 && inside < edge - 0.01);
        if !touches {
            continue;
        }
        edge = inside;
        found = true;
    }

    if !found {
        return None;
    }
    let thickness = (edge - page_outer).abs();
    if thickness <= 0.01 {
        return None;
    }
    if wm.is_vertical() {
        let top = if matches!(physical, PhysicalSide::Top) {
            page.top
        } else {
            page.bottom - thickness
        };
        Some(Rect::new(page.left, top, page.right, top + thickness))
    } else {
        let left = if matches!(physical, PhysicalSide::Left) {
            page.left
        } else {
            page.right - thickness
        };
        Some(Rect::new(left, page.top, left + thickness, page.bottom))
    }
}

fn root_geometry(doc: &BaseDocument, root_id: NodeId) -> Option<RootGeometry> {
    let node = doc.get_node(root_id)?;
    let (pos, layout) = doc.physical_unrounded_geometry(root_id);
    let wm = node.writing_mode();
    let content_x = pos.x + layout.border.left + layout.padding.left;
    let content_y = pos.y + layout.border.top + layout.padding.top;
    let content_width = layout.size.width
        - layout.border.left
        - layout.border.right
        - layout.padding.left
        - layout.padding.right;
    let content_height = layout.size.height
        - layout.border.top
        - layout.border.bottom
        - layout.padding.top
        - layout.padding.bottom;
    Some(RootGeometry {
        border_x: pos.x,
        border_y: pos.y,
        content_x,
        content_y,
        content_width,
        content_height,
        vertical: wm.is_vertical(),
        block_starts_at_right: wm.block_start_physical_side() == PhysicalSide::Right,
        block_start_side: wm.block_start_physical_side(),
        block_end_side: wm.block_end_physical_side(),
        inline_start_side: wm.inline_start_physical_side(),
    })
}

fn padding_border_box(content: Rect, padding: taffy::Rect<f32>, border: taffy::Rect<f32>) -> Rect {
    Rect::new(
        content.left - padding.left - border.left,
        content.top - padding.top - border.top,
        content.right + padding.right + border.right,
        content.bottom + padding.bottom + border.bottom,
    )
}

fn margin_box(border: Rect, margin: taffy::Rect<f32>) -> Rect {
    Rect::new(
        border.left - margin.left,
        border.top - margin.top,
        border.right + margin.right,
        border.bottom + margin.bottom,
    )
}

fn block_flow_start(plan: &BreakPlan, rect: MarginNoteRect) -> f32 {
    match plan.axis {
        FlowAxis::Y => rect.top,
        FlowAxis::X if plan.reversed => plan.measure - rect.right,
        FlowAxis::X => rect.left,
    }
}

fn block_flow_end(plan: &BreakPlan, rect: MarginNoteRect) -> f32 {
    match plan.axis {
        FlowAxis::Y => rect.bottom,
        FlowAxis::X if plan.reversed => plan.measure - rect.left,
        FlowAxis::X => rect.right,
    }
}

fn block_start_at_flow(
    plan: &BreakPlan,
    band: &crate::layout::fragmentation::Band,
    flow: f32,
) -> f32 {
    let local = flow - band.start;
    match plan.axis {
        FlowAxis::Y => band.origin.y + local,
        FlowAxis::X if plan.reversed => band.origin.x + plan.depth - local,
        FlowAxis::X => band.origin.x + local,
    }
}

fn expand_side(rect: &mut Rect, side: PhysicalSide, amount: f32) {
    if amount == 0.0 {
        return;
    }
    match side {
        PhysicalSide::Top => rect.top -= amount,
        PhysicalSide::Right => rect.right += amount,
        PhysicalSide::Bottom => rect.bottom += amount,
        PhysicalSide::Left => rect.left -= amount,
    }
}

fn expand_fragment_box(
    mut rect: Rect,
    insets: taffy::Rect<f32>,
    wm: stylo_taffy::WritingMode,
    block_start: PhysicalSide,
    block_end: PhysicalSide,
    first: bool,
    last: bool,
) -> Rect {
    expand_side(
        &mut rect,
        wm.inline_start_physical_side(),
        rect_side(insets, wm.inline_start_physical_side()),
    );
    expand_side(
        &mut rect,
        wm.inline_end_physical_side(),
        rect_side(insets, wm.inline_end_physical_side()),
    );
    if first {
        expand_side(&mut rect, block_start, rect_side(insets, block_start));
    }
    if last {
        expand_side(&mut rect, block_end, rect_side(insets, block_end));
    }
    rect
}

fn line_block_extents(group: &MarginNoteGroup, scale: f32) -> Vec<(f32, f32)> {
    let Some(first) = group.layout.get(0) else {
        return Vec::new();
    };
    let origin = first.metrics().block_min_coord;
    group
        .layout
        .lines()
        .map(|line| {
            (
                (line.metrics().block_min_coord - origin) / scale,
                (line.metrics().block_max_coord - origin) / scale,
            )
        })
        .collect()
}

/// Slice each placed note into page-local fragments at complete line boundaries.
///
/// The note is shaped and broken once in continuous coordinates. A page break
/// only chooses a range of those lines and translates it to the next page's
/// margin; it never re-breaks the note, so a long margin note costs one text
/// layout regardless of how many pages it spans.
fn paginate_margin_groups(plan: Option<&BreakPlan>, scale: f32, groups: &mut [PlacedGroup]) {
    let Some(plan) = plan else {
        return;
    };
    if plan.bands.is_empty() {
        return;
    }

    let mut occupied = HashMap::<(usize, u8, bool, u8), f32>::new();
    for placed in groups.iter_mut() {
        let lines = line_block_extents(&placed.group, scale);
        if lines.is_empty() {
            continue;
        }
        let group_flow_start = block_flow_start(&plan, placed.group.content_box);
        let group_flow_end = block_flow_end(&plan, placed.group.content_box);
        let Some(start_band) = plan
            .bands
            .iter()
            .position(|band| group_flow_start < band.end && group_flow_end > band.start)
        else {
            continue;
        };

        let side = side_physical(placed.wm, placed.group.side);
        let stack_key = (
            physical_side_index(side),
            placed.root.vertical,
            physical_side_index(placed.root.block_start_side),
        );
        let mut line_cursor = 0usize;
        for (band_index, band) in plan.bands.iter().enumerate().skip(start_band) {
            if line_cursor >= lines.len() {
                break;
            }
            let page_depth = (band.end - band.start).max(1.0);
            let first = line_cursor == 0;
            let desired = if first {
                (group_flow_start - band.start).max(0.0)
            } else {
                placed.block_start_inset
            };
            let key = (band_index, stack_key.0, stack_key.1, stack_key.2);
            let start_local = desired.max(*occupied.get(&key).unwrap_or(&f32::NEG_INFINITY));
            if start_local >= page_depth - 0.01 {
                continue;
            }

            let mut end = line_cursor;
            while end < lines.len() {
                let extent = lines[end].1 - lines[line_cursor].0;
                let end_inset = if end + 1 == lines.len() {
                    placed.block_end_inset
                } else {
                    0.0
                };
                if start_local + extent + end_inset <= page_depth + 0.01 {
                    end += 1;
                } else if end == line_cursor {
                    end += 1;
                    break;
                } else {
                    break;
                }
            }
            if end == line_cursor {
                break;
            }

            let last = end == lines.len();
            let extent = lines[end - 1].1 - lines[line_cursor].0;
            let end_inset = if last { placed.block_end_inset } else { 0.0 };
            let content_start = block_start_at_flow(
                &plan,
                band,
                band.start + start_local,
            );
            let content_end = block_start_at_flow(
                &plan,
                band,
                band.start + start_local + extent,
            );
            let content = if placed.root.vertical {
                Rect::new(
                    content_start.min(content_end),
                    placed.group.content_box.top,
                    content_start.max(content_end),
                    placed.group.content_box.bottom,
                )
            } else {
                Rect::new(
                    placed.group.content_box.left,
                    content_start.min(content_end),
                    placed.group.content_box.right,
                    content_start.max(content_end),
                )
            };
            let padding = expand_fragment_box(
                content,
                placed.padding,
                placed.wm,
                placed.root.block_start_side,
                placed.root.block_end_side,
                first,
                last,
            );
            let border = expand_fragment_box(
                padding,
                placed.border,
                placed.wm,
                placed.root.block_start_side,
                placed.root.block_end_side,
                first,
                last,
            );
            let margin = expand_fragment_box(
                border,
                placed.margin,
                placed.wm,
                placed.root.block_start_side,
                placed.root.block_end_side,
                first,
                last,
            );
            let selected_start = lines[line_cursor].0;
            let original_content_start = match plan.axis {
                FlowAxis::Y => placed.group.content_box.top + selected_start,
                FlowAxis::X if plan.reversed => {
                    placed.group.content_box.right - selected_start
                }
                FlowAxis::X => placed.group.content_box.left + selected_start,
            };
            let (shift_x, shift_y) = match plan.axis {
                FlowAxis::Y => (0.0, content_start - original_content_start),
                FlowAxis::X => (content_start - original_content_start, 0.0),
            };

            placed.group.page_fragments.push(MarginNotePageFragment {
                page_index: band_index,
                line_start: line_cursor,
                line_end: end,
                shift_x,
                shift_y,
                content_box: content.into(),
                padding_box: padding.into(),
                border_box: border.into(),
                margin_box: margin.into(),
                content_block_extent: extent,
            });
            *occupied.entry(key).or_insert(f32::NEG_INFINITY) =
                start_local + extent + end_inset;
            line_cursor = end;
        }
    }
}

fn inline_content_start(
    band: Rect,
    wm: stylo_taffy::WritingMode,
    padding: taffy::Rect<f32>,
    border: taffy::Rect<f32>,
    margin: taffy::Rect<f32>,
    content_inline: f32,
) -> f32 {
    let side = wm.inline_start_physical_side();
    let inset = logical_rect_side(margin, wm, side)
        + logical_rect_side(border, wm, side)
        + logical_rect_side(padding, wm, side);
    if matches!(side, PhysicalSide::Top | PhysicalSide::Left) {
        page_side(band, side) + inset
    } else {
        page_side(band, side) - inset - content_inline
    }
}

fn note_indent(
    style: &style::properties::ComputedValues,
    content_inline: f32,
    scale: f32,
) -> (f32, IndentOptions) {
    let indent = style.clone_text_indent();
    (
        indent
            .length
            .resolve(CSSPixelLength::new(content_inline.max(0.0)))
            .px()
            * scale,
        IndentOptions {
            each_line: indent.each_line,
            hanging: indent.hanging,
        },
    )
}

fn content_extent(value: Option<f32>, natural: f32, border_box: bool, noncontent: f32) -> f32 {
    value
        .map(|value| {
            if border_box {
                (value - noncontent).max(0.0)
            } else {
                value.max(0.0)
            }
        })
        .unwrap_or(natural)
}

pub(crate) fn resolve_margin_notes(doc: &mut BaseDocument) {
    let scale = doc.viewport().scale();
    let page = page_rect(doc);
    let page_plan = doc
        .try_root_element()
        .and_then(|root| root.break_plan.as_deref().cloned());
    let mut anchors = Vec::new();
    for (root_id, node) in doc.nodes.iter_mut() {
        if let Some(text) = node
            .element_data_mut()
            .and_then(|element| element.inline_layout_data.as_mut())
        {
            anchors.extend(
                text.margin_anchors
                    .iter()
                    .copied()
                    .map(|anchor| (root_id, anchor)),
            );
            text.margin_note_groups.clear();
        }
    }
    doc.has_margin_notes = !anchors.is_empty();
    doc.margin_note_roots.clear();
    doc.margin_note_roots
        .extend(anchors.iter().map(|(root_id, _)| *root_id));
    doc.margin_note_roots.sort_unstable();
    doc.margin_note_roots.dedup();
    if anchors.is_empty() {
        return;
    }

    let mut seen = HashSet::new();
    anchors.retain(|(_, anchor)| seen.insert(anchor.node_id));
    anchors.sort_by_key(|(_, anchor)| (anchor.line_index, anchor.order));

    let mut root_cache =
        HashMap::<NodeId, (RootGeometry, stylo_taffy::WritingMode, f32, f32)>::new();
    let mut root_margin_cache = HashMap::<u8, (f32, f32)>::new();
    let mut declarations = HashMap::new();
    for &(root_id, anchor) in &anchors {
        if !root_cache.contains_key(&root_id) {
            let node = &doc.nodes[root_id];
            let Some(geometry) = root_geometry(doc, root_id) else {
                continue;
            };
            let wm = node.writing_mode();
            let (start_margin, end_margin) = *root_margin_cache
                .entry(wm.bits())
                .or_insert_with(|| root_static_margins(doc, wm));
            root_cache.insert(root_id, (geometry, wm, start_margin, end_margin));
        }
        let node = &doc.nodes[root_id];
        let Some(text) = node
            .element_data()
            .and_then(|element| element.inline_layout_data.as_ref())
        else {
            continue;
        };
        let Some(line) = text.layout.get(anchor.line_index) else {
            continue;
        };
        let block = line.metrics().block_min_coord / scale + text.block_offset;
        declarations.insert(anchor.node_id, block.max(0.0));
    }

    let mut placements = HashMap::new();
    let mut bands = HashMap::new();
    for &(root_id, anchor) in &anchors {
        let Some((_, wm, start_margin, end_margin)) = root_cache.get(&root_id).copied() else {
            continue;
        };
        let Some(note_style) = doc.nodes[anchor.node_id].primary_styles() else {
            continue;
        };
        let side = resolve_side(note_style.clone__hutu_note_side(), start_margin, end_margin);
        let band = *bands.entry((root_id, side)).or_insert_with(|| {
            available_margin_band(doc, root_id, page, wm, side, start_margin, end_margin)
                .unwrap_or_else(|| {
                    let physical = side_physical(wm, side);
                    match physical {
                        PhysicalSide::Top => Rect::new(page.left, page.top, page.right, page.top),
                        PhysicalSide::Bottom => {
                            Rect::new(page.left, page.bottom, page.right, page.bottom)
                        }
                        PhysicalSide::Left => {
                            Rect::new(page.left, page.top, page.left, page.bottom)
                        }
                        PhysicalSide::Right => {
                            Rect::new(page.right, page.top, page.right, page.bottom)
                        }
                    }
                })
        });
        placements.insert(anchor.node_id, (side, band));
    }

    let mut pending = Vec::with_capacity(anchors.len());
    let font_ctx = doc.font_ctx.clone();
    let mut font_ctx = font_ctx.lock().unwrap();
    let layout_ctx = &mut doc.layout_ctx;
    let nodes = &doc.nodes;
    for &(root_id, anchor) in &anchors {
        let Some((geometry, wm, _, _)) = root_cache.get(&root_id).copied() else {
            continue;
        };
        let Some(&(side, band)) = placements.get(&anchor.node_id) else {
            continue;
        };
        let note_node = &nodes[anchor.node_id];
        let taffy_style = note_node.layout_style_in(wm);
        let thickness = if geometry.vertical {
            band.page_height()
        } else {
            band.page_width()
        };
        let basis = Size {
            width: Some(thickness),
            height: Some(thickness),
        };
        let margin = taffy_style
            .margin()
            .resolve_or_zero(basis, &resolve_calc_value);
        let padding = taffy_style
            .padding()
            .resolve_or_zero(basis, &resolve_calc_value);
        let border = taffy_style
            .border()
            .resolve_or_zero(basis, &resolve_calc_value);
        let inline_start = wm.inline_start_physical_side();
        let inline_end = wm.inline_end_physical_side();
        let noncontent_inline = logical_rect_side(padding, wm, inline_start)
            + logical_rect_side(border, wm, inline_start)
            + logical_rect_side(padding, wm, inline_end)
            + logical_rect_side(border, wm, inline_end);
        let inline_edges = noncontent_inline
            + logical_rect_side(margin, wm, inline_start)
            + logical_rect_side(margin, wm, inline_end);
        let border_box = taffy_style.box_sizing() == BoxSizing::BorderBox;

        let declared_inline = if geometry.vertical {
            taffy_style.size().height
        } else {
            taffy_style.size().width
        };
        let min_inline = if geometry.vertical {
            taffy_style.min_size().height
        } else {
            taffy_style.min_size().width
        };
        let max_inline = if geometry.vertical {
            taffy_style.max_size().height
        } else {
            taffy_style.max_size().width
        };
        let mut content_inline = content_extent(
            declared_inline.maybe_resolve(thickness, &resolve_calc_value),
            (thickness - inline_edges).max(0.0),
            border_box,
            noncontent_inline,
        );
        if let Some(min) = min_inline
            .maybe_resolve(thickness, &resolve_calc_value)
            .map(|value| content_extent(Some(value), 0.0, border_box, noncontent_inline))
        {
            content_inline = content_inline.max(min);
        }
        if let Some(max) = max_inline
            .maybe_resolve(thickness, &resolve_calc_value)
            .map(|value| content_extent(Some(value), f32::INFINITY, border_box, noncontent_inline))
        {
            content_inline = content_inline.min(max);
        }

        let base_direction = nodes[root_id]
            .primary_styles()
            .map(|style| {
                stylo_to_parley::base_direction(style.clone_direction(), style.clone_unicode_bidi())
            })
            .unwrap_or(parley::BaseDirection::Ltr);
        let content = build_margin_note_content_layout(
            nodes,
            layout_ctx,
            &mut font_ctx,
            anchor.node_id,
            scale,
            wm.is_vertical(),
            base_direction,
        );
        if content.text.is_empty() && content.layout.inline_boxes().len() == 0 {
            continue;
        }
        let block_start = geometry.block_start_side;
        let block_end = geometry.block_end_side;
        let block_edges = logical_rect_side(margin, wm, block_start)
            + logical_rect_side(padding, wm, block_start)
            + logical_rect_side(border, wm, block_start)
            + logical_rect_side(margin, wm, block_end)
            + logical_rect_side(padding, wm, block_end)
            + logical_rect_side(border, wm, block_end);
        let noncontent_block = block_edges
            - logical_rect_side(margin, wm, block_start)
            - logical_rect_side(margin, wm, block_end);
        let declared_block = if geometry.vertical {
            taffy_style.size().width
        } else {
            taffy_style.size().height
        };
        let min_block = if geometry.vertical {
            taffy_style.min_size().width
        } else {
            taffy_style.min_size().height
        };
        let max_block = if geometry.vertical {
            taffy_style.max_size().width
        } else {
            taffy_style.max_size().height
        };
        let block_basis = if geometry.vertical {
            page.page_width()
        } else {
            page.page_height()
        };
        let declaration = *declarations.get(&anchor.node_id).unwrap_or(&0.0);
        let declaration_stack = stack_position(page, &geometry, declaration);
        pending.push(PendingNote {
            root_id,
            note_id: anchor.node_id,
            side,
            band,
            declaration_stack,
            order: anchor.order,
            root: geometry,
            wm,
            content_inline,
            margin,
            padding,
            border,
            block_edges,
            noncontent_block,
            declared_block: declared_block.maybe_resolve(block_basis, &resolve_calc_value),
            min_block: min_block.maybe_resolve(block_basis, &resolve_calc_value),
            max_block: max_block.maybe_resolve(block_basis, &resolve_calc_value),
            border_box_sizing: border_box,
            content,
        });
    }
    drop(font_ctx);

    let mut pass = crate::layout::LayoutPassState::new(doc);
    let mut plans = Vec::with_capacity(pending.len());
    for mut note in pending {
        #[cfg(feature = "writing-mode")]
        {
            pass.layout_wm = note.wm;
        }
        pass.measure_margin_note_layout(
            &mut note.content.layout,
            note.content_inline,
            scale,
            note.wm.is_vertical(),
        );

        #[cfg(feature = "writing-mode")]
        if note.wm.is_vertical() {
            let orientation = pass.nodes[note.root_id]
                .primary_styles()
                .map(|style| stylo_to_parley::text_orientation(style.clone_text_orientation()))
                .unwrap_or(parley::TextOrientation::Mixed);
            note.content
                .layout
                .set_vertical_advances(&note.content.text, orientation);
        }
        #[cfg(feature = "writing-mode")]
        note.content.layout.set_vertical(note.wm.is_vertical());
        note.content
            .layout
            .clear_advances(&note.content.text, &note.content.ruby_ranges);
        let Some(note_style) = pass.nodes[note.note_id].primary_styles() else {
            continue;
        };
        let (indent, indent_options) = note_indent(&note_style, note.content_inline, scale);
        note.content.layout.set_text_indent(indent, indent_options);
        note.content
            .layout
            .break_all_lines(Some((note.content_inline * scale).max(0.0)));
        note.content.layout.align(
            stylo_to_parley::text_align(note_style.clone_text_align()),
            AlignmentOptions {
                align_when_overflowing: false,
                last_line_alignment: stylo_to_parley::text_align_last(
                    note_style.clone_text_align_last(),
                ),
            },
        );
        let mut content_block = content_extent(
            note.declared_block,
            note.content.layout.height() / scale,
            note.border_box_sizing,
            note.noncontent_block,
        );
        if let Some(min) = note.min_block.map(|value| {
            content_extent(
                Some(value),
                0.0,
                note.border_box_sizing,
                note.noncontent_block,
            )
        }) {
            content_block = content_block.max(min);
        }
        if let Some(max) = note.max_block.map(|value| {
            content_extent(
                Some(value),
                f32::INFINITY,
                note.border_box_sizing,
                note.noncontent_block,
            )
        }) {
            content_block = content_block.min(max);
        }
        plans.push(Plan {
            root_id: note.root_id,
            note_id: note.note_id,
            side: note.side,
            band: note.band,
            declaration_stack: note.declaration_stack,
            order: note.order,
            root: note.root,
            wm: note.wm,
            content_inline: note.content_inline,
            content_block,
            outer_block: content_block + note.block_edges,
            margin: note.margin,
            padding: note.padding,
            border: note.border,
            text: note.content.text,
            layout: note.content.layout,
            ruby_ranges: note.content.ruby_ranges,
            emphasis_ranges: note.content.emphasis_ranges,
        });
    }
    let mut ordered = plans.iter_mut().collect::<Vec<_>>();
    ordered.sort_by(|left, right| {
        let left_side = side_physical(left.wm, left.side);
        let right_side = side_physical(right.wm, right.side);
        physical_side_index(left_side)
            .cmp(&physical_side_index(right_side))
            .then_with(|| left.root.vertical.cmp(&right.root.vertical))
            .then_with(|| {
                physical_side_index(left.root.block_start_side)
                    .cmp(&physical_side_index(right.root.block_start_side))
            })
            .then_with(|| {
                left.declaration_stack
                    .total_cmp(&right.declaration_stack)
                    .then_with(|| left.order.cmp(&right.order))
            })
    });
    let mut occupied = f32::NEG_INFINITY;
    let mut stack_key = None;
    let mut placed_groups = Vec::with_capacity(ordered.len());
    for plan in ordered {
        let key = (
            physical_side_index(side_physical(plan.wm, plan.side)),
            plan.root.vertical,
            physical_side_index(plan.root.block_start_side),
        );
        if stack_key != Some(key) {
            stack_key = Some(key);
            occupied = f32::NEG_INFINITY;
        }
        let start = occupied.max(plan.declaration_stack);
        let block_start_progress = root_progress(page, &plan.root, start)
            + logical_rect_side(plan.margin, plan.wm, plan.root.block_start_side);
        let content_block_start = block_start_progress
            + logical_rect_side(plan.border, plan.wm, plan.root.block_start_side)
            + logical_rect_side(plan.padding, plan.wm, plan.root.block_start_side);
        let inline_start = inline_content_start(
            plan.band,
            plan.wm,
            plan.padding,
            plan.border,
            plan.margin,
            plan.content_inline,
        );

        let content = if plan.root.vertical {
            let x = if plan.root.block_starts_at_right {
                plan.root.content_x + plan.root.content_width
                    - content_block_start
                    - plan.content_block
            } else {
                plan.root.content_x + content_block_start
            };
            Rect::new(
                x,
                inline_start,
                x + plan.content_block,
                inline_start + plan.content_inline,
            )
        } else {
            let y = plan.root.content_y + content_block_start;
            Rect::new(
                inline_start,
                y,
                inline_start + plan.content_inline,
                y + plan.content_block,
            )
        };
        let padding_box = Rect::new(
            content.left - plan.padding.left,
            content.top - plan.padding.top,
            content.right + plan.padding.right,
            content.bottom + plan.padding.bottom,
        );
        let border = padding_border_box(content, plan.padding, plan.border);
        let margin_box = margin_box(border, plan.margin);
        let inline_dx = if plan.root.vertical {
            0.0
        } else {
            content.left - plan.root.content_x
        };
        let inline_dy = if plan.root.vertical {
            let root_starts_at_bottom = plan.root.inline_start_side == PhysicalSide::Bottom;
            let content_inline_start = if root_starts_at_bottom {
                content.bottom
            } else {
                content.top
            };
            let root_inline_start = if root_starts_at_bottom {
                plan.root.content_y + plan.root.content_height
            } else {
                plan.root.content_y
            };
            content_inline_start - root_inline_start
        } else {
            0.0
        };
        let first_block = plan
            .layout
            .get(0)
            .map(|line| line.metrics().block_min_coord)
            .unwrap_or(0.0);
        let delta = content_block_start * scale - first_block;
        let offsets = vec![delta; plan.layout.len()];
        plan.layout.shift_line_block_offsets(&offsets);

        let container_pb = if plan.root.vertical {
            taffy::Rect {
                left: content.top - plan.root.border_y,
                top: 0.0,
                right: 0.0,
                bottom: 0.0,
            }
        } else {
            taffy::Rect {
                left: content.left - plan.root.border_x,
                top: 0.0,
                right: 0.0,
                bottom: 0.0,
            }
        };
        let line_box_top = if plan.root.vertical {
            0.0
        } else {
            plan.root.content_y - plan.root.border_y
        };
        pass.place_inline_boxes_in_layout(
            &plan.layout,
            Size {
                width: Some(plan.content_inline),
                height: Some(plan.content_block),
            },
            Size {
                width: plan.content_inline,
                height: plan.content_block,
            },
            container_pb,
            line_box_top,
            if plan.wm.is_bidi_ltr() {
                taffy::Direction::Ltr
            } else {
                taffy::Direction::Rtl
            },
            scale,
            plan.root.vertical,
        );

        let group = MarginNoteGroup {
            node_id: plan.note_id,
            side: plan.side,
            text: std::mem::take(&mut plan.text),
            layout: std::mem::take(&mut plan.layout),
            ruby_ranges: std::mem::take(&mut plan.ruby_ranges),
            emphasis_ranges: std::mem::take(&mut plan.emphasis_ranges),
            inline_dx,
            inline_dy,
            content_box: content.into(),
            padding_box: padding_box.into(),
            border_box: border.into(),
            margin_box: margin_box.into(),
            content_inline_extent: plan.content_inline,
            content_block_extent: plan.content_block,
            page_fragments: Vec::new(),
        };
        let block_start = plan.root.block_start_side;
        let block_end = plan.root.block_end_side;
        let block_start_inset = logical_rect_side(plan.margin, plan.wm, block_start)
            + logical_rect_side(plan.border, plan.wm, block_start)
            + logical_rect_side(plan.padding, plan.wm, block_start);
        let block_end_inset = logical_rect_side(plan.margin, plan.wm, block_end)
            + logical_rect_side(plan.border, plan.wm, block_end)
            + logical_rect_side(plan.padding, plan.wm, block_end);
        placed_groups.push(PlacedGroup {
            root_id: plan.root_id,
            root: plan.root,
            wm: plan.wm,
            group,
            block_start_inset,
            block_end_inset,
            margin: plan.margin,
            padding: plan.padding,
            border: plan.border,
        });
        occupied = start + plan.outer_block;
    }

    paginate_margin_groups(page_plan.as_ref(), scale, &mut placed_groups);
    for placed in placed_groups {
        let node = &mut pass.nodes[placed.root_id];
        if let Some(text) = node
            .element_data_mut()
            .and_then(|element| element.inline_layout_data.as_mut())
        {
            text.margin_note_groups.push(placed.group);
        }
    }
}
