use blitz_traits::node_id::NodeId;
use parley::{AlignmentOptions, BreakReason, IndentOptions, YieldData};
use style::values::specified::box_::{DisplayInside, DisplayOutside};
use style::values::{
    computed::{BaselineSource, CSSPixelLength, Contain},
    generics::text::GenericTextIndent,
};
use taffy::{
    AvailableSpace, AxisStaticPosition, BlockContainerStyle, BlockContext, BlockFormattingContext,
    BoxSizing, CollapsibleMarginSet, CompactLength, CoreStyle as _, Direction, LayoutInput,
    LayoutOutput, LayoutPartialTree as _, MaybeMath as _, MaybeResolve as _, OofCandidate,
    OofCandidates, OofItemStyle, OofPositioningArea, Overflow, Point, RequestedAxis,
    ResolveOrZero as _, RunMode, Size, SizingMode,
};

#[cfg(feature = "floats")]
use taffy::{BlockItemStyle as _, Clear, Float, prelude::TaffyMaxContent};

use stylo_taffy::StyleFlags;

use super::resolve_calc_value;
use crate::layout::LayoutPassState;
#[cfg(feature = "writing-mode")]
use crate::node::{TextOrientation, VerticalTextPaint};
use crate::node::{
    InlineNoteFragment, InlineNoteSource, InterlineAnchor, InterlineLine, InterlineNoteStream,
    InterlinePart,
};
use crate::stylo_to_parley;

/// Make Parley resolve upright clusters' vertical metrics.
///
/// A vertical writing mode advances upright text down the column by the font's vertical
/// metrics, which are not the horizontal advances shaping produces (a CJK font may set a
/// glyph's `hmtx` advance below one em while its `vmtx` advance is one em, and upright Latin
/// is an em tall but half an em wide). Line breaking has to use them, or a full line paints
/// past the end of its column. `text-orientation` decides which clusters are upright:
/// `sideways` rotates all of them, `upright` keeps all of them upright, and the initial
/// `mixed` follows UAX #50.
#[cfg(feature = "writing-mode")]
fn set_vertical_advances(
    layout: &mut parley::layout::Layout<crate::node::TextBrush>,
    text: &str,
    orientation: TextOrientation,
) {
    layout.set_vertical_advances(text, orientation);
}

#[cfg(feature = "writing-mode")]
fn text_orientation(sideways: bool, upright: bool) -> TextOrientation {
    if sideways {
        TextOrientation::Sideways
    } else if upright {
        TextOrientation::Upright
    } else {
        TextOrientation::Mixed
    }
}

fn interline_slot_geometry(
    body_lines: &[parley::Line<'_, crate::node::TextBrush>],
    full_width: f32,
    slot: usize,
) -> (f32, f32) {
    if let Some(line) = body_lines.get(slot) {
        let metrics = line.metrics();
        let start = if slot == 0 {
            metrics.inline_min_coord.max(metrics.indent)
        } else {
            metrics.inline_min_coord
        };
        return (
            start,
            (metrics.inline_max_coord - start).max(0.0),
        );
    }
    (0.0, full_width.max(0.0))
}

fn interline_marker_id(index: usize) -> u64 {
    u64::MAX - index as u64
}

fn interline_line_parts(
    stream: &InterlineNoteStream,
    line: parley::Line<'_, crate::node::TextBrush>,
    scale: f32,
) -> Vec<InterlinePart> {
    let line_range = line.text_range();
    let relative_origin = line.metrics().inline_min_coord + line.metrics().offset;
    let mut parts = Vec::new();
    let first_source = stream
        .sources
        .partition_point(|source| source.text_range.end <= line_range.start);
    for (source_index, source) in stream.sources.iter().enumerate().skip(first_source) {
        if source.text_range.start >= line_range.end {
            break;
        }
        if !source.active {
            continue;
        }
        let start = line_range.start.max(source.text_range.start);
        let end = line_range.end.min(source.text_range.end);
        if start >= end {
            continue;
        }
        let mut inline_offset = None;
        let mut advance = 0.0;
        for item in line.items() {
            match item {
                parley::PositionedLayoutItem::GlyphRun(glyph_run) => {
                    let mut cluster_pen = glyph_run.offset();
                    for cluster in glyph_run.visual_clusters() {
                        let cluster_range = cluster.text_range();
                        let cluster_advance = cluster.advance();
                        if start <= cluster_range.start && cluster_range.end <= end {
                            inline_offset.get_or_insert(cluster_pen - relative_origin);
                            advance += cluster_advance;
                        }
                        cluster_pen += cluster_advance;
                    }
                }
                parley::PositionedLayoutItem::InlineBox(_) => {}
            }
        }
        parts.push(InterlinePart {
            source_index,
            text_range: start..end,
            inline_offset: inline_offset.unwrap_or(0.0) / scale,
            advance: advance / scale,
        });
    }
    parts
}

#[derive(Clone, Copy, Default)]
struct InlineNoteCursor {
    segment: usize,
    unit: usize,
}

struct InlineNotePlan {
    next: InlineNoteCursor,
    segment: usize,
    first_range: std::ops::Range<usize>,
    second_range: std::ops::Range<usize>,
    first_unit_range: std::ops::Range<usize>,
    second_unit_range: std::ops::Range<usize>,
    first_shaped_clusters: std::ops::Range<u32>,
    second_shaped_clusters: std::ops::Range<u32>,
    first_line: bool,
    first_fragment: bool,
    last_fragment: bool,
    outer_line_end: bool,
    edges: NoteFragmentEdges,
    first_hanging_start: bool,
    first_hanging_end: bool,
    second_hanging_end: bool,
    first_advance: f32,
    second_advance: f32,
    first_hanging_advance: f32,
    second_hanging_advance: f32,
    first_justification_opportunities: u32,
    second_justification_opportunities: u32,
    first_line_height: f32,
    second_line_height: f32,
    continues: bool,
}

#[derive(Clone, Copy, Default)]
struct NoteHanging {
    start: bool,
    force: bool,
    allow: bool,
    last: bool,
}

#[derive(Clone, Copy, Default)]
struct NoteFragmentEdges {
    inline_start_margin: f32,
    inline_start_padding_border: f32,
    inline_end_margin: f32,
    inline_end_padding_border: f32,
    block_start_margin: f32,
    block_start_padding_border: f32,
    block_end_margin: f32,
    block_end_padding_border: f32,
}

impl NoteFragmentEdges {
    fn inline_start(self) -> f32 {
        self.inline_start_margin + self.inline_start_padding_border
    }

    fn inline_end(self) -> f32 {
        self.inline_end_margin + self.inline_end_padding_border
    }

    fn block_start(self) -> f32 {
        self.block_start_margin + self.block_start_padding_border
    }

    fn block_end(self) -> f32 {
        self.block_end_margin + self.block_end_padding_border
    }
}

fn note_fragment_edges(
    source: &InlineNoteSource,
    first_fragment: bool,
    last_fragment: bool,
) -> NoteFragmentEdges {
    let edges = source.box_edges;
    let include_start = edges.clone_decoration || first_fragment;
    let include_end = edges.clone_decoration || last_fragment;
    let (margin_start, margin_end, padding_border_start, padding_border_end) =
        if source.layout.is_rtl() {
            (
                edges.margin.right,
                edges.margin.left,
                edges.padding.right + edges.border.right,
                edges.padding.left + edges.border.left,
            )
        } else {
            (
                edges.margin.left,
                edges.margin.right,
                edges.padding.left + edges.border.left,
                edges.padding.right + edges.border.right,
            )
        };
    NoteFragmentEdges {
        inline_start_margin: if include_start { margin_start } else { 0.0 },
        inline_start_padding_border: if include_start { padding_border_start } else { 0.0 },
        inline_end_margin: if include_end { margin_end } else { 0.0 },
        inline_end_padding_border: if include_end { padding_border_end } else { 0.0 },
        block_start_margin: edges.margin.top,
        block_start_padding_border: edges.padding.top + edges.border.top,
        block_end_margin: edges.margin.bottom,
        block_end_padding_border: edges.padding.bottom + edges.border.bottom,
    }
}

fn inline_note_range_advance(
    segment: &crate::node::InlineNoteSegment,
    start: usize,
    end: usize,
    first_line: bool,
    hanging: NoteHanging,
) -> f32 {
    if start >= end {
        return 0.0;
    }
    let mut advance = segment.prefix_advances[end] - segment.prefix_advances[start];
    let first_text = (start..end).find(|index| segment.units[*index].inline_box_index.is_none());
    let last_text = (start..end)
        .rev()
        .find(|index| segment.units[*index].inline_box_index.is_none());
    let (Some(first_text), Some(last_text)) = (first_text, last_text) else {
        return advance;
    };
    let first = &segment.units[first_text];
    let last = &segment.units[last_text];
    if end - start == 1 {
        return note_unit_edge_advance(first, first_line, hanging);
    }
    if first_text == start {
        let start_advance = if hanging.start && first.starts_with_hanging_opening {
            if first_line {
                first.first_line_start_hanging_advance
            } else {
                first.line_start_hanging_advance
            }
        } else if first_line {
            first.first_line_start_advance
        } else {
            first.line_start_advance
        };
        advance += start_advance - first.advance;
    }
    if last_text == end - 1 {
        advance += note_unit_end_advance(last, hanging) - last.advance;
    }
    advance
}

fn inline_note_range_advance_with_allow(
    segment: &crate::node::InlineNoteSegment,
    start: usize,
    end: usize,
    first_line: bool,
    hanging: NoteHanging,
    available: f32,
) -> (f32, bool) {
    let advance = inline_note_range_advance(segment, start, end, first_line, hanging);
    if !hanging.force || advance <= available {
        return (advance, false);
    }

    let allow = inline_note_range_advance(
        segment,
        start,
        end,
        first_line,
        NoteHanging {
            start: hanging.start,
            allow: true,
            last: hanging.last,
            ..NoteHanging::default()
        },
    );
    if allow < advance && allow <= available {
        (allow, true)
    } else {
        (advance, false)
    }
}

fn inline_note_text_range(
    segment: &crate::node::InlineNoteSegment,
    start: usize,
    end: usize,
) -> std::ops::Range<usize> {
    let selected = &segment.units[start..end];
    let first = selected.iter().find(|unit| unit.inline_box_index.is_none());
    let last = selected.iter().rev().find(|unit| unit.inline_box_index.is_none());
    match (first, last) {
        (Some(first), Some(last)) => first.text_range.start..last.text_range.end,
        _ => {
            let anchor = selected
                .first()
                .map(|unit| unit.text_range.start)
                .unwrap_or(segment.text_range.start);
            anchor..anchor
        }
    }
}

fn inline_note_shaped_range(
    segment: &crate::node::InlineNoteSegment,
    start: usize,
    end: usize,
) -> std::ops::Range<u32> {
    let selected = &segment.units[start..end];
    let first = selected.iter().find(|unit| unit.inline_box_index.is_none());
    let last = selected.iter().rev().find(|unit| unit.inline_box_index.is_none());
    match (first, last) {
        (Some(first), Some(last)) => {
            first.shaped_clusters.start..last.shaped_clusters.end
        }
        _ => 0..0,
    }
}

fn note_unit_edge_advance(
    unit: &crate::node::InlineNoteUnit,
    first_line: bool,
    hanging: NoteHanging,
) -> f32 {
    let normal = if first_line {
        unit.first_line_edges_advance
    } else {
        unit.line_edges_advance
    };
    let end_hanging = if hanging.force && unit.force_hanging_at_end {
        if first_line {
            unit.first_line_edges_hanging_advance
        } else {
            unit.line_edges_hanging_advance
        }
    } else if hanging.allow && unit.allow_hanging_at_end {
        if first_line {
            unit.first_line_edges_allow_hanging_advance
        } else {
            unit.line_edges_allow_hanging_advance
        }
    } else if hanging.last && unit.last_hanging_at_end {
        if first_line {
            unit.first_line_edges_last_hanging_advance
        } else {
            unit.line_edges_last_hanging_advance
        }
    } else {
        normal
    };
    if hanging.start && unit.starts_with_hanging_opening {
        let start_hanging = if first_line {
            unit.first_line_edges_start_hanging_advance
        } else {
            unit.line_edges_start_hanging_advance
        };
        end_hanging + start_hanging - normal
    } else {
        end_hanging
    }
}

fn note_unit_end_advance(unit: &crate::node::InlineNoteUnit, hanging: NoteHanging) -> f32 {
    if hanging.force && unit.force_hanging_at_end {
        unit.line_end_hanging_advance
    } else if hanging.allow && unit.allow_hanging_at_end {
        unit.line_end_allow_hanging_advance
    } else if hanging.last && unit.last_hanging_at_end {
        unit.line_end_last_hanging_advance
    } else {
        unit.line_end_advance
    }
}

#[allow(clippy::too_many_arguments)]
fn inline_note_atomic_positions(
    source: &InlineNoteSource,
    segment: &crate::node::InlineNoteSegment,
    note_box: &parley::layout::PositionedInlineBox,
    fragment: &InlineNoteFragment,
    unit_range: &std::ops::Range<usize>,
    row_advance: f32,
    row_justification_opportunities: u32,
    row_fill_amount: f32,
    justification_amount: f32,
    row_block_offset: f32,
    first_line: bool,
    hanging_start: bool,
    hanging_end: bool,
    hanging_advance: f32,
) -> Vec<(u64, f32, f32, f32, f32)> {
    if unit_range.is_empty() {
        return Vec::new();
    }

    let units = &segment.units;
    let first_text = unit_range
        .clone()
        .find(|index| units[*index].inline_box_index.is_none());
    let last_text = unit_range
        .clone()
        .rev()
        .find(|index| units[*index].inline_box_index.is_none());
    let mut advances = units[unit_range.clone()]
        .iter()
        .map(|unit| unit.advance)
        .collect::<Vec<_>>();

    if first_text == Some(unit_range.start) {
        let unit = &units[unit_range.start];
        let start_advance = if hanging_start && unit.starts_with_hanging_opening {
            if first_line {
                unit.first_line_start_hanging_advance
            } else {
                unit.line_start_hanging_advance
            }
        } else if first_line {
            unit.first_line_start_advance
        } else {
            unit.line_start_advance
        };
        advances[0] += start_advance - unit.advance;
    }

    // Match the fragment width selected by the line breaker, including hanging at the end.
    if last_text == unit_range.end.checked_sub(1) {
        let natural_advance = advances.iter().sum::<f32>();
        advances[unit_range.end - unit_range.start - 1] += row_advance - natural_advance;
    }

    let content_width = (note_box.width - fragment.inline_start_edge - fragment.inline_end_edge)
        .max(0.0);
    let effective_advance = row_advance
        + row_justification_opportunities as f32 * (justification_amount + row_fill_amount);
    let inline_origin = if source.layout.is_rtl() {
        note_box.x
            + fragment.inline_end_edge
            + (content_width - effective_advance).max(0.0)
            - hanging_advance
    } else {
        note_box.x + fragment.inline_start_edge
    };
    let block_origin = note_box.y + row_block_offset + fragment.block_start_inset;
    let mut visual_units = unit_range.clone().collect::<Vec<_>>();
    visual_units.sort_unstable_by_key(|index| units[*index].visual_order_index);

    let row_justification_amount = justification_amount + row_fill_amount;
    let mut positions = Vec::new();
    let mut inline_pen = 0.0;
    for unit_index in visual_units {
        let unit = &units[unit_index];
        if let Some(atomic_node_id) = unit.atomic_node_id {
            positions.push((
                atomic_node_id,
                inline_origin + inline_pen,
                block_origin + unit.inline_box_block_offset,
                unit.advance,
                unit.line_height,
            ));
        }

        let is_row_start_hanging = unit_index == unit_range.start
            && hanging_start
            && unit.starts_with_hanging_opening;
        let is_row_end = unit_index + 1 == unit_range.end;
        let is_row_end_hanging = is_row_end
            && hanging_end
            && (unit.force_hanging_at_end
                || unit.allow_hanging_at_end
                || unit.last_hanging_at_end);
        let extra_justification = if justification_amount > 0.0
            && unit.is_justification_opportunity
            && !is_row_start_hanging
            && !is_row_end_hanging
            && !(is_row_end && unit.is_word_separator)
        {
            row_justification_amount
        } else {
            0.0
        };
        inline_pen += advances[unit_index - unit_range.start] + extra_justification;
    }

    positions
}

fn inline_note_range_height(
    source: &InlineNoteSource,
    segment: &crate::node::InlineNoteSegment,
    start: usize,
    end: usize,
) -> f32 {
    note_unit_range_height(
        source.layout.is_vertical(),
        source.strut_line_height,
        source.strut_over,
        source.strut_under,
        &segment.units,
        start,
        end,
    )
}

fn note_unit_range_height(
    vertical: bool,
    strut_line_height: f32,
    strut_over: f32,
    strut_under: f32,
    units: &[crate::node::InlineNoteUnit],
    start: usize,
    end: usize,
) -> f32 {
    if start >= end {
        return strut_line_height;
    }
    let units = &units[start..end];
    // An atomic inline is a line of its own content: its margin box supplies the
    // subline's block extent, and the note's strut must not be added on top. Text
    // units already carry the line height of their own runs.
    let has_atomic = units.iter().any(|unit| unit.atomic_node_id.is_some());
    if vertical {
        units.iter().fold(
            if has_atomic {
                0.0
            } else {
                strut_line_height
            },
            |height, unit| height.max(unit.line_height),
        )
    } else {
        let (over, under) = units.iter().fold(
            if has_atomic {
                (0.0, 0.0)
            } else {
                (strut_over, strut_under)
            },
            |(over, under), unit| (over.max(unit.line_over), under.max(unit.line_under)),
        );
        over + under
    }
}

fn inline_note_justification_opportunities(
    segment: &crate::node::InlineNoteSegment,
    start: usize,
    end: usize,
    hanging_start: bool,
    hanging_end: bool,
) -> u32 {
    if start >= end {
        return 0;
    }
    let count = segment.units[start..end]
        .iter()
        .filter(|unit| unit.is_justification_opportunity)
        .count() as u32;
    let start_hang = hanging_start
        && segment.units[start].starts_with_hanging_opening
        && segment.units[start].is_justification_opportunity;
    let last = &segment.units[end - 1];
    let end_hang = hanging_end
        && (last.force_hanging_at_end || last.allow_hanging_at_end || last.last_hanging_at_end)
        && last.is_justification_opportunity;
    let end_excluded = end_hang || segment.units[end - 1].is_word_separator;
    count
        .saturating_sub(u32::from(start_hang))
        .saturating_sub(u32::from(end_excluded))
}

fn inline_note_row_prefix_advance(
    segment: &crate::node::InlineNoteSegment,
    start: usize,
    end: usize,
    first_line: bool,
) -> f32 {
    if start >= end {
        return 0.0;
    }
    let first = &segment.units[start];
    let mut advance = segment.prefix_advances[end] - segment.prefix_advances[start];
    let start_advance = if first_line {
        first.first_line_start_advance
    } else {
        first.line_start_advance
    };
    advance += start_advance - first.advance;
    advance
}

fn inline_note_balanced_split(
    segment: &crate::node::InlineNoteSegment,
    start: usize,
    end: usize,
    first_line: bool,
    first_hanging: NoteHanging,
    second_hanging: NoteHanging,
    available: f32,
) -> Option<usize> {
    if end == start + 1 {
        return Some(end);
    }

    let first_legal = segment
        .legal_split_boundaries
        .partition_point(|boundary| *boundary <= start);
    let last_legal = segment
        .legal_split_boundaries
        .partition_point(|boundary| *boundary < end);
    let legal = &segment.legal_split_boundaries[first_legal..last_legal];
    if legal.is_empty() {
        // Keep an unbreakable punctuation group together in the first row. There
        // is no legal two-row split for this prefix, but the empty second row is
        // still a valid composition and lets outer-line breaking make progress.
        return Some(end);
    }

    let first_hanging_for_split = |split: usize| NoteHanging {
        last: second_hanging.last && split == end,
        ..first_hanging
    };
    let second_hanging_for_split = |split: usize| NoteHanging {
        last: second_hanging.last && split < end,
        ..second_hanging
    };
    let first_width = |split: usize| {
        inline_note_range_advance_with_allow(
            segment,
            start,
            split,
            first_line,
            first_hanging_for_split(split),
            available,
        )
        .0
    };
    let second_width = |split: usize| {
        inline_note_range_advance_with_allow(
            segment,
            split,
            end,
            false,
            second_hanging_for_split(split),
            available,
        )
        .0
    };
    let score = |split: usize| first_width(split).max(second_width(split));
    let mut best = None;
    let mut consider = |split| {
        if first_width(split) < second_width(split) {
            return;
        }
        let replace = best.is_none_or(|old| {
            score(split).total_cmp(&score(old)).is_lt()
                || (score(split) == score(old) && split > old)
        });
        if replace {
            best = Some(split);
        }
    };

    if segment.nonnegative_advances {
        let balance = legal.partition_point(|split| first_width(*split) < second_width(*split));
        if balance == legal.len() {
            return Some(end);
        }
        for split in legal.iter().copied().skip(balance).take(3) {
            consider(split);
        }
    } else {
        for split in legal.iter().copied() {
            consider(split);
        }
    }
    best.or(Some(end))
}

fn plan_inline_note_fragment(
    source: &InlineNoteSource,
    cursor: InlineNoteCursor,
    available: f32,
    allow_overflow: bool,
    allow_hanging_start: bool,
    force_final_line_end: bool,
) -> Option<InlineNotePlan> {
    let mut segment_index = cursor.segment;
    let mut start_unit = cursor.unit;
    while segment_index < source.segments.len()
        && start_unit >= source.segments[segment_index].units.len()
        && !source.segments[segment_index].units.is_empty()
    {
        segment_index += 1;
        start_unit = 0;
    }
    let segment = source.segments.get(segment_index)?;
    if segment.units.is_empty() {
        let next = InlineNoteCursor {
            segment: segment_index + 1,
            unit: 0,
        };
        let first_fragment = segment_index == 0 && start_unit == 0;
        let last_fragment = next.segment >= source.segments.len();
        let outer_line_end = next.segment < source.segments.len() || !source.has_following_content;
        return Some(InlineNotePlan {
            next,
            segment: segment_index,
            first_range: segment.text_range.start..segment.text_range.start,
            second_range: segment.text_range.end..segment.text_range.end,
            first_unit_range: 0..0,
            second_unit_range: 0..0,
            first_shaped_clusters: 0..0,
            second_shaped_clusters: 0..0,
            first_line: false,
            first_fragment,
            last_fragment,
            outer_line_end,
            edges: note_fragment_edges(source, first_fragment, last_fragment),
            first_hanging_start: false,
            first_hanging_end: false,
            second_hanging_end: false,
            first_advance: 0.0,
            second_advance: 0.0,
            first_hanging_advance: 0.0,
            second_hanging_advance: 0.0,
            first_justification_opportunities: 0,
            second_justification_opportunities: 0,
            first_line_height: source.strut_line_height.max(segment.line_height),
            second_line_height: source.strut_line_height.max(segment.line_height),
            continues: next.segment < source.segments.len(),
        });
    }

    if start_unit >= segment.units.len() {
        return None;
    }

    let first_line = segment_index == 0 && start_unit == 0;
    let first_hanging_start = allow_hanging_start
        && first_line
        && segment.units[start_unit].inline_box_index.is_none()
        && segment.units[start_unit].starts_with_hanging_opening;
    let mut best = None;
    for end_unit in start_unit + 1..=segment.units.len() {
        if end_unit != segment.units.len() && !segment.units[end_unit - 1].can_split_after {
            continue;
        }
        let candidate_continues = end_unit < segment.units.len()
            || segment_index + 1 < source.segments.len();
        let first_fragment = segment_index == 0 && start_unit == 0;
        let last_fragment = !candidate_continues;
        let edges = note_fragment_edges(source, first_fragment, last_fragment);
        let final_outer_line = !candidate_continues
            && (!source.has_following_content || force_final_line_end);
        let outer_line_end = candidate_continues || final_outer_line;
        let first_split_hanging = NoteHanging {
            start: first_hanging_start,
            force: outer_line_end,
            ..NoteHanging::default()
        };
        let second_split_hanging = NoteHanging {
            force: outer_line_end,
            last: final_outer_line,
            ..NoteHanging::default()
        };
        let Some(split) = inline_note_balanced_split(
            segment,
            start_unit,
            end_unit,
            first_line,
            first_split_hanging,
            second_split_hanging,
            available,
        ) else {
            continue;
        };

        let first_hanging = NoteHanging {
            start: first_hanging_start,
            force: outer_line_end,
            last: final_outer_line && split == end_unit,
            ..NoteHanging::default()
        };
        let second_hanging = NoteHanging {
            force: outer_line_end,
            last: final_outer_line && split < end_unit,
            ..NoteHanging::default()
        };

        let first_force_advance =
            inline_note_range_advance(segment, start_unit, split, first_line, first_hanging);
        let second_force_advance =
            inline_note_range_advance(segment, split, end_unit, false, second_hanging);
        let first_normal_advance =
            inline_note_range_advance(
                segment,
                start_unit,
                split,
                first_line,
                NoteHanging {
                    start: first_hanging_start,
                    ..NoteHanging::default()
                },
            );
        let second_normal_advance =
            inline_note_range_advance(
                segment,
                split,
                end_unit,
                false,
                NoteHanging::default(),
            );
        let (first_advance, first_allow_hanging) = inline_note_range_advance_with_allow(
            segment,
            start_unit,
            split,
            first_line,
            first_hanging,
            available,
        );
        let (second_advance, second_allow_hanging) = inline_note_range_advance_with_allow(
            segment,
            split,
            end_unit,
            false,
            second_hanging,
            available,
        );
        let first_hanging_end = outer_line_end
            && (first_force_advance < first_normal_advance || first_allow_hanging);
        let second_hanging_end = outer_line_end
            && (second_force_advance < second_normal_advance || second_allow_hanging);
        if first_advance < second_advance {
            continue;
        }
        let first_hanging_advance = (first_normal_advance - first_advance).max(0.0);
        let second_hanging_advance = (second_normal_advance - second_advance).max(0.0);
        let width = first_advance.max(second_advance) + edges.inline_start() + edges.inline_end();
        if width <= available || (allow_overflow && split == end_unit) {
            best = Some((
                end_unit,
                split,
                first_advance,
                second_advance,
                first_hanging_advance,
                second_hanging_advance,
                first_fragment,
                last_fragment,
                outer_line_end,
                edges,
                first_hanging_end,
                second_hanging_end,
            ));
        } else if segment.nonnegative_advances {
            break;
        }
    }

    let (
        end_unit,
        split,
        first_advance,
        second_advance,
        first_hanging_advance,
        second_hanging_advance,
        first_fragment,
        last_fragment,
        outer_line_end,
        edges,
        first_hanging_end,
        second_hanging_end,
    ) = best?;
    let first_range = inline_note_text_range(segment, start_unit, split);
    let second_range = inline_note_text_range(segment, split, end_unit);
    let first_unit_range = start_unit..split;
    let second_unit_range = split..end_unit;
    let first_shaped_clusters = inline_note_shaped_range(segment, start_unit, split);
    let second_shaped_clusters = inline_note_shaped_range(segment, split, end_unit);
    let next = if end_unit == segment.units.len() {
        InlineNoteCursor {
            segment: segment_index + 1,
            unit: 0,
        }
    } else {
        InlineNoteCursor {
            segment: segment_index,
            unit: end_unit,
        }
    };

    Some(InlineNotePlan {
        next,
        segment: segment_index,
        first_range,
        second_range,
        first_unit_range,
        second_unit_range,
        first_shaped_clusters,
        second_shaped_clusters,
        first_line,
        first_fragment,
        last_fragment,
        outer_line_end,
        edges,
        first_hanging_start,
        first_hanging_end,
        second_hanging_end,
        first_justification_opportunities: inline_note_justification_opportunities(
            segment,
            start_unit,
            split,
            first_hanging_start,
            first_hanging_end,
        ),
        second_justification_opportunities: inline_note_justification_opportunities(
            segment,
            split,
            end_unit,
            false,
            second_hanging_end,
        ),
        first_advance,
        second_advance,
        first_hanging_advance,
        second_hanging_advance,
        first_line_height: inline_note_range_height(source, segment, start_unit, split),
        second_line_height: if split < end_unit {
            inline_note_range_height(source, segment, split, end_unit)
        } else {
            source.strut_line_height
        },
        continues: next.segment < source.segments.len()
            || next.unit < source.segments.get(next.segment).map_or(0, |next| next.units.len()),
    })
}

fn resolve_inline_note_break(
    breaker: &mut parley::layout::BreakLines<'_, crate::node::TextBrush>,
    box_break: &parley::layout::BoxBreakData,
    sources: &[InlineNoteSource],
    source_indices: &std::collections::HashMap<u64, usize>,
    cursors: &mut [InlineNoteCursor],
    fragments: &mut [Option<InlineNoteFragment>],
    parent_strut: Option<(f32, f32)>,
    inline_wraps: bool,
) -> Option<bool> {
    let source_index = *source_indices.get(&box_break.inline_box_id)?;
    let source = &sources[source_index];
    let cursor = &mut cursors[source_index];
    let inline_box_index = box_break.inline_box_index;

    if cursor.segment >= source.segments.len() {
        breaker.resolve_custom_inline_box(
            inline_box_index,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            parley::InlineBoxKind::OutOfFlow,
        );
        return Some(false);
    }

    let available = if inline_wraps {
        breaker.state().line_max_advance()
            - breaker.current_line_indent()
            - box_break.advance
    } else {
        f32::MAX
    };
    let allow_overflow = box_break.advance <= 0.0;
    let mut plan = plan_inline_note_fragment(
        source,
        *cursor,
        available,
        allow_overflow,
        box_break.advance <= 0.0,
        false,
    );
    let force_final_line_end = plan.as_ref().is_some_and(|plan| {
        !plan.continues
            && source.has_following_content
            && !breaker.following_content_fits_after_custom_box(
                inline_box_index,
                plan.first_advance.max(plan.second_advance)
                    + plan.edges.inline_start()
                    + plan.edges.inline_end(),
                inline_wraps,
            )
    });
    if force_final_line_end {
        plan = plan_inline_note_fragment(
            source,
            *cursor,
            available,
            allow_overflow,
            box_break.advance <= 0.0,
            true,
        );
    }
    let Some(plan) = plan else {
        if box_break.advance > 0.0 {
            breaker.break_current_line();
            return Some(true);
        }
        breaker.resolve_custom_inline_box(
            inline_box_index,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            parley::InlineBoxKind::OutOfFlow,
        );
        return Some(false);
    };

    let content_height = plan.first_line_height + plan.second_line_height;
    let box_height = content_height + plan.edges.block_start() + plan.edges.block_end();
    let (body_font_size, body_line_height) =
        parent_strut.unwrap_or((box_height.max(1.0), box_height));
    let body_gap = ((body_line_height - body_font_size) * 0.5).max(0.0);
    let extent = if box_height > body_line_height {
        box_height + 2.0 * body_gap
    } else {
        box_height
    };
    let (ascent, descent) = if box_height > body_line_height {
        (extent * 0.5, extent * 0.5)
    } else {
        (box_height * 0.5, box_height * 0.5)
    };

    fragments[inline_box_index] = Some(InlineNoteFragment {
        source_index,
        segment_index: plan.segment,
        first_range: plan.first_range,
        second_range: plan.second_range,
        first_unit_range: plan.first_unit_range,
        second_unit_range: plan.second_unit_range,
        first_shaped_clusters: plan.first_shaped_clusters,
        second_shaped_clusters: plan.second_shaped_clusters,
        first_line: plan.first_line,
        first_fragment: plan.first_fragment,
        last_fragment: plan.last_fragment,
        outer_line_end: plan.outer_line_end,
        first_hanging_start: plan.first_hanging_start,
        first_hanging_end: plan.first_hanging_end,
        second_hanging_end: plan.second_hanging_end,
        first_advance: plan.first_advance,
        second_advance: plan.second_advance,
        first_hanging_advance: plan.first_hanging_advance,
        second_hanging_advance: plan.second_hanging_advance,
        first_justification_opportunities: plan.first_justification_opportunities,
        second_justification_opportunities: plan.second_justification_opportunities,
        first_line_height: plan.first_line_height,
        second_line_height: plan.second_line_height,
        inline_start_edge: plan.edges.inline_start(),
        inline_end_edge: plan.edges.inline_end(),
        block_start_inset: plan.edges.block_start(),
        block_end_inset: plan.edges.block_end(),
    });
    *cursor = plan.next;
    breaker.resolve_custom_inline_box(
        inline_box_index,
        plan.first_advance.max(plan.second_advance) + plan.edges.inline_start() + plan.edges.inline_end(),
        box_height,
        box_height * 0.5,
        ascent,
        descent,
        parley::InlineBoxKind::CustomInFlowCentered,
    );
    breaker.set_custom_inline_box_justification_profile(
        inline_box_index,
        plan.first_advance,
        plan.first_justification_opportunities,
        plan.second_advance,
        plan.second_justification_opportunities,
    );

    if plan.continues {
        breaker.break_current_line();
        Some(true)
    } else {
        Some(false)
    }
}

/// Subtract a child's margins from the definite axes of the available space it is laid out in.
/// Taffy's convention is that the parent subtracts a child's margins from the available space
/// before passing it to the child, and that the child does not subtract them again.
fn subtract_margins(mut child_inputs: LayoutInput, margin: taffy::Rect<f32>) -> LayoutInput {
    child_inputs.available_space = child_inputs
        .available_space
        .maybe_sub(margin.sum_axes())
        .maybe_max(Size::ZERO);
    child_inputs
}

/// The line breaker compares accumulated advances with a max-content limit directly;
/// a small cushion keeps exact float equality from producing a spurious soft wrap.
const INLINE_BOX_MAX_CONTENT_CUSHION: f32 = 0.01;

/// Layout inputs for an atomic inline box, with any sizing keyword on its `width` style
/// (`min-content`, `max-content`, `fit-content`, `fit-content(...)`, `stretch`) resolved
/// into the available space or known width the box is measured with.
fn inline_box_inputs(
    width_style: taffy::Dimension,
    margin: taffy::Rect<f32>,
    child_inputs: LayoutInput,
) -> LayoutInput {
    let mut inputs = subtract_margins(child_inputs, margin);
    let stretch_width = inputs.available_space.width.into_option();
    let percent_basis = child_inputs.parent_size.width;

    match width_style.tag() {
        CompactLength::MIN_CONTENT_TAG => inputs.available_space.width = AvailableSpace::MinContent,
        CompactLength::MAX_CONTENT_TAG => inputs.available_space.width = AvailableSpace::MaxContent,
        CompactLength::FIT_CONTENT_PX_TAG => {
            inputs.available_space.width = AvailableSpace::Definite(width_style.value())
        }
        CompactLength::FIT_CONTENT_PERCENT_TAG => {
            if let Some(basis) = percent_basis {
                inputs.available_space.width =
                    AvailableSpace::Definite(basis * width_style.value());
            }
        }
        CompactLength::FIT_CONTENT_KEYWORD_TAG => {
            if let Some(width) = stretch_width {
                inputs.available_space.width = AvailableSpace::Definite(width);
            }
        }
        _ if width_style.is_fit_content_calc() => {
            if let Some(basis) = percent_basis {
                inputs.available_space.width =
                    AvailableSpace::Definite(resolve_calc_value(width_style.calc_value(), basis));
            }
        }
        CompactLength::STRETCH_TAG => {
            if let Some(width) = stretch_width {
                inputs.known_dimensions.width = Some(width);
                inputs.available_space.width = AvailableSpace::Definite(width);
            }
        }
        _ => {}
    }
    inputs
}

impl LayoutPassState<'_> {
    fn inline_box_inputs_for_child(
        &self,
        child_node_id: NodeId,
        scale: f32,
        width_style: taffy::Dimension,
        margin: taffy::Rect<f32>,
        padding: taffy::Rect<f32>,
        border: taffy::Rect<f32>,
        child_inputs: LayoutInput,
    ) -> LayoutInput {
        let mut inputs = inline_box_inputs(width_style, margin, child_inputs);
        if !width_style.is_auto() {
            return inputs;
        }
        let Some(max_content_inline_size) = self.nodes[child_node_id]
            .element_data()
            .and_then(|element| element.inline_layout_data.as_ref())
            .map(|layout| layout.layout.calculate_content_widths().max / scale)
        else {
            return inputs;
        };
        let natural = max_content_inline_size
            + padding.horizontal_axis_sum()
            + border.horizontal_axis_sum();
        let fits = match inputs.available_space.width {
            taffy::AvailableSpace::MinContent => false,
            taffy::AvailableSpace::MaxContent => true,
            taffy::AvailableSpace::Definite(limit) => natural <= limit,
        };
        if fits {
            inputs.known_dimensions.width = Some(natural + INLINE_BOX_MAX_CONTENT_CUSHION);
        }
        inputs
    }

    pub(crate) fn compute_inline_layout(
        &mut self,
        node_id: NodeId,
        inputs: taffy::tree::LayoutInput,
        block_ctx: Option<&mut BlockContext<'_>>,
    ) -> taffy::LayoutOutput {
        let LayoutInput {
            known_dimensions,
            parent_size,
            run_mode,
            ..
        } = inputs;
        let style = self.nodes[node_id].layout_style();

        // Pull these out earlier to avoid borrowing issues
        let is_scroll_container =
            style.overflow().x.is_scroll_container() || style.overflow().y.is_scroll_container();
        let padding = style
            .padding()
            .resolve_or_zero(parent_size.width, resolve_calc_value);
        let border = style
            .border()
            .resolve_or_zero(parent_size.width, resolve_calc_value);
        let padding_border_size = (padding + border).sum_axes();
        let box_sizing_adjustment = if style.box_sizing() == BoxSizing::ContentBox {
            padding_border_size
        } else {
            Size::ZERO
        };

        // Resolve node's preferred/min/max sizes (width/heights) against the available space (percentages resolve to pixel values)
        // For ContentSize mode, we pretend that the node has no size styles as these should be ignored.
        let (clamped_style_size, min_size, max_size, _aspect_ratio) = match inputs.sizing_mode {
            SizingMode::ContentSize => {
                let node_size = known_dimensions;
                let node_min_size = Size::NONE;
                let node_max_size = Size::NONE;
                (node_size, node_min_size, node_max_size, None)
            }
            SizingMode::InherentSize => {
                let aspect_ratio = style.aspect_ratio();
                let style_size = style
                    .size()
                    .maybe_resolve(parent_size, resolve_calc_value)
                    .maybe_apply_aspect_ratio(aspect_ratio)
                    .maybe_add(box_sizing_adjustment);
                let style_min_size = style
                    .min_size()
                    .maybe_resolve(parent_size, resolve_calc_value)
                    .maybe_apply_aspect_ratio(aspect_ratio)
                    .maybe_add(box_sizing_adjustment);
                let style_max_size = style
                    .max_size()
                    .maybe_resolve(parent_size, resolve_calc_value)
                    .maybe_add(box_sizing_adjustment);

                let node_size =
                    known_dimensions.or(style_size.maybe_clamp(style_min_size, style_max_size));
                (node_size, style_min_size, style_max_size, aspect_ratio)
            }
        };

        // If both min and max in a given axis are set and max <= min then this determines the size in that axis
        let min_max_definite_size = min_size.zip_map(max_size, |min, max| match (min, max) {
            (Some(min), Some(max)) if max <= min => Some(min),
            _ => None,
        });

        let styled_based_known_dimensions = known_dimensions
            .or(min_max_definite_size)
            .or(clamped_style_size)
            .maybe_max(padding_border_size);

        // Short-circuit layout if the container's size is fully determined by the container's size and the run mode
        // is ComputeSize (and thus the container's size is all that we're interested in)
        if run_mode == RunMode::ComputeSize {
            if let Size {
                width: Some(width),
                height: Some(height),
            } = styled_based_known_dimensions
            {
                return LayoutOutput::from_outer_size(Size { width, height });
            }
        }

        drop(style);

        // Unwrap the block formatting context if one was passed, or else create a new one
        match block_ctx {
            Some(inherited_bfc) if !is_scroll_container => self.compute_inline_layout_inner(
                node_id,
                LayoutInput {
                    known_dimensions: styled_based_known_dimensions,
                    ..inputs
                },
                inherited_bfc,
            ),
            _ => {
                let mut root_bfc = BlockFormattingContext::new();
                let mut root_ctx = root_bfc.root_block_context();
                self.compute_inline_layout_inner(
                    node_id,
                    LayoutInput {
                        known_dimensions: styled_based_known_dimensions,
                        ..inputs
                    },
                    &mut root_ctx,
                )
            }
        }
    }

    /// The out-of-flow positions for which an inline span between the inline box `box_id` and its
    /// inline root `root_id` is a containing block (as `SPAN_*_CB` flags)
    fn inline_span_cb_flags(&self, root_id: NodeId, box_id: NodeId) -> StyleFlags {
        let root = &self.nodes[root_id];
        // The spans of an anonymous inline root are descendants of its parent
        let root_parent = root.is_anonymous().then_some(root.parent).flatten();

        let mut flags = StyleFlags::empty();
        let mut current = self.nodes[box_id].parent;
        while let Some(id) = current {
            if id == root_id || Some(id) == root_parent {
                return flags;
            }
            let ancestor = &self.nodes[id];
            if let Some(style) = ancestor.primary_styles() {
                let display = style.clone_display();
                if display.outside() == DisplayOutside::Inline
                    && display.inside() == DisplayInside::Flow
                {
                    let claims = stylo_taffy::convert::inline_containing_block_claims(&style);
                    if claims.absolute {
                        flags |= StyleFlags::SPAN_ABSOLUTE_CB;
                    }
                    if claims.fixed {
                        flags |= StyleFlags::SPAN_FIXED_CB;
                    }
                }
            }
            current = ancestor.parent;
        }
        StyleFlags::empty()
    }

    fn compute_inline_layout_inner(
        &mut self,
        node_id: NodeId,
        inputs: taffy::tree::LayoutInput,
        block_ctx: &mut BlockContext<'_>,
    ) -> taffy::LayoutOutput {
        let scale = self.viewport.scale();
        let LayoutInput {
            known_dimensions,
            parent_size,
            available_space,
            sizing_mode,
            ..
        } = inputs;

        // Take inline layout to satisfy borrow checker
        let mut inline_layout = self.nodes[node_id]
            .data
            .downcast_element_mut()
            .unwrap()
            .take_inline_layout()
            .unwrap();

        let note_ids: std::collections::HashSet<_> = inline_layout
            .inline_notes
            .iter()
            .map(|note| note.node_id.as_u64())
            .collect();
        let note_box_indices: Vec<_> = inline_layout
            .layout
            .inline_boxes()
            .enumerate()
            .filter_map(|(index, inline_box)| {
                note_ids.contains(&inline_box.id).then_some(index)
            })
            .collect();
        for index in note_box_indices {
            inline_layout.layout.reset_custom_inline_box(index);
        }
        inline_layout.inline_note_fragments.fill(None);

        use style::properties::generated::longhands::box_decoration_break::computed_value::T as BoxDecorationBreak;
        for note in &mut inline_layout.inline_notes {
            let style = self.child_layout_style(&self.nodes[note.node_id]);
            let margin = style
                .margin()
                .resolve_or_zero(parent_size, resolve_calc_value);
            let padding = style
                .padding()
                .resolve_or_zero(parent_size, resolve_calc_value);
            let border = style
                .border()
                .resolve_or_zero(parent_size, resolve_calc_value);
            let clone_decoration = self.nodes[note.node_id]
                .primary_styles()
                .is_some_and(|styles| {
                    matches!(
                        styles.get_border().box_decoration_break,
                        BoxDecorationBreak::Clone
                    )
                });
            note.box_edges = crate::node::InlineNoteBoxEdges {
                margin: taffy::Rect {
                    left: margin.left * scale,
                    right: margin.right * scale,
                    top: margin.top * scale,
                    bottom: margin.bottom * scale,
                },
                padding: taffy::Rect {
                    left: padding.left * scale,
                    right: padding.right * scale,
                    top: padding.top * scale,
                    bottom: padding.bottom * scale,
                },
                border: taffy::Rect {
                    left: border.left * scale,
                    right: border.right * scale,
                    top: border.top * scale,
                    bottom: border.bottom * scale,
                },
                clone_decoration,
            };
        }
        let style = self.nodes[node_id].layout_style();

        // Note: both horizontal and vertical percentage padding/borders are resolved against the container's inline size (i.e. width).
        // This is not a bug, but is how CSS is specified (see: https://developer.mozilla.org/en-US/docs/Web/CSS/padding#values)
        let padding = style
            .padding()
            .resolve_or_zero(parent_size.width, resolve_calc_value);
        let border = style
            .border()
            .resolve_or_zero(parent_size.width, resolve_calc_value);
        let container_pb = padding + border;
        let pb_sum = container_pb.sum_axes();
        let box_sizing_adjustment = if style.box_sizing() == BoxSizing::ContentBox {
            pb_sum
        } else {
            Size::ZERO
        };

        // Scrollbar gutters are reserved when the `overflow` property is set to `Overflow::Scroll`.
        // However, the axis are switched (transposed) because a node that scrolls vertically needs
        // *horizontal* space to be reserved for a scrollbar
        let scrollbar_gutter = style.overflow().transpose().map(|overflow| match overflow {
            Overflow::Scroll => style.scrollbar_width(),
            _ => 0.0,
        });
        // TODO: make side configurable based on the `direction` property
        let mut content_box_inset = container_pb;
        content_box_inset.right += scrollbar_gutter.x;
        content_box_inset.bottom += scrollbar_gutter.y;

        let has_styles_preventing_being_collapsed_through = !style.is_block()
            || style.overflow().x.is_scroll_container()
            || style.overflow().y.is_scroll_container()
            || style.position().is_out_of_flow()
            || padding.top > 0.0
            || padding.bottom > 0.0
            || border.top > 0.0
            || border.bottom > 0.0;
        // || matches!(node_size.height, Some(h) if h > 0.0)
        // || matches!(node_min_size.height, Some(h) if h > 0.0)
        // || !inline_layout.text.is_empty();
        // || inline_layout.layout.inline_boxes().len() > 0;

        // Resolve node's preferred/min/max sizes (width/heights) against the available space (percentages resolve to pixel values)
        // For ContentSize mode, we pretend that the node has no size styles as these should be ignored.
        let (node_size, node_min_size, node_max_size, aspect_ratio) = match sizing_mode {
            SizingMode::ContentSize => {
                let node_size = known_dimensions;
                let node_min_size = Size::NONE;
                let node_max_size = Size::NONE;
                (node_size, node_min_size, node_max_size, None)
            }
            SizingMode::InherentSize => {
                let aspect_ratio = style.aspect_ratio();
                let style_size = style
                    .size()
                    .maybe_resolve(parent_size, resolve_calc_value)
                    .maybe_apply_aspect_ratio(aspect_ratio)
                    .maybe_add(box_sizing_adjustment);
                let style_min_size = style
                    .min_size()
                    .maybe_resolve(parent_size, resolve_calc_value)
                    .maybe_apply_aspect_ratio(aspect_ratio)
                    .maybe_add(box_sizing_adjustment);
                let style_max_size = style
                    .max_size()
                    .maybe_resolve(parent_size, resolve_calc_value)
                    .maybe_add(box_sizing_adjustment);

                let node_size =
                    known_dimensions.or(style_size.maybe_clamp(style_min_size, style_max_size));
                (node_size, style_min_size, style_max_size, aspect_ratio)
            }
        };

        let suppresses_baseline = style.contain().suppresses_baseline();
        drop(style);

        // Short circuit if inline context contains no text or inline boxes
        if !has_styles_preventing_being_collapsed_through
            && inline_layout.text.is_empty()
            && inline_layout.layout.inline_boxes().len() == 0
        {
            // Put layout back
            self.nodes[node_id]
                .data
                .downcast_element_mut()
                .unwrap()
                .inline_layout_data = Some(inline_layout);
            return LayoutOutput::from_outer_size(
                Size::ZERO.maybe_max(container_pb.sum_axes().map(Some)),
            );
        }

        // Compute available space
        let available_space = Size {
            width: known_dimensions
                .width
                .map(AvailableSpace::from)
                .unwrap_or(available_space.width)
                .maybe_set(known_dimensions.width)
                .maybe_set(node_size.width)
                .map_definite_value(|size| {
                    (size.maybe_clamp(node_min_size.width, node_max_size.width)
                        - content_box_inset.horizontal_axis_sum())
                    .max(0.0)
                }),
            height: known_dimensions
                .height
                .map(AvailableSpace::from)
                .unwrap_or(available_space.height)
                .maybe_set(known_dimensions.height)
                .maybe_set(node_size.height)
                .map_definite_value(|size| {
                    (size.maybe_clamp(node_min_size.height, node_max_size.height)
                        - content_box_inset.vertical_axis_sum())
                    .max(0.0)
                }),
        };

        // Compute size of inline boxes
        let child_inputs = taffy::tree::LayoutInput {
            known_dimensions: Size::NONE,
            available_space,
            sizing_mode: SizingMode::InherentSize,
            parent_size: Size {
                width: available_space.width.into_option(),
                // Anonymous blocks do not establish the containing block for percentages.
                height: if self.nodes[node_id].is_anonymous() {
                    parent_size.height
                } else {
                    available_space.height.into_option()
                },
            },
            // Atomic inlines (e.g. inline-block) establish independent formatting
            // contexts: their margins never collapse with their children's margins.
            vertical_margins_are_collapsible: taffy::Line::FALSE,
            ..inputs
        };
        #[cfg(feature = "floats")]
        let float_child_inputs = taffy::tree::LayoutInput {
            available_space: Size::MAX_CONTENT,
            ..child_inputs
        };

        // Atomic inline descendants of an inline note are measured through the
        // same Taffy child-layout path as ordinary inline boxes. Their final
        // advances then feed the note's grapheme/box-unit prefix profile.
        for note in &mut inline_layout.inline_notes {
            for inline_box in note.layout.inline_boxes_mut() {
                if inline_box.kind != parley::InlineBoxKind::InFlow {
                    continue;
                }
                let child_node_id = NodeId::from_u64(inline_box.id);
                let style = self.child_layout_style(&self.nodes[child_node_id]);
                let margin = style
                    .margin()
                    .resolve_or_zero(inputs.parent_size, resolve_calc_value);
                let padding = style
                    .padding()
                    .resolve_or_zero(inputs.parent_size, resolve_calc_value);
                let border = style
                    .border()
                    .resolve_or_zero(inputs.parent_size, resolve_calc_value);
                let is_flow = matches!(
                    style.effective_display().inside(),
                    DisplayInside::Flow | DisplayInside::FlowRoot
                );
                let baseline_source = style.style.get_box().clone_baseline_source();
                let uses_last_baseline = match baseline_source {
                    BaselineSource::Auto => is_flow,
                    BaselineSource::First => false,
                    BaselineSource::Last => true,
                };
                let overflow = style.overflow();
                let is_scroll_container = !matches!(overflow.y, Overflow::Visible | Overflow::Clip);
                let is_block_axis_scroll_container = is_flow
                    && is_scroll_container
                    && !style
                        .flags
                        .intersects(StyleFlags::IS_BUTTON | StyleFlags::IS_BUTTON_INPUT);
                let contain_layout = style.style.get_box().clone_contain().contains(Contain::LAYOUT);
                let exports_baseline = !(baseline_source == BaselineSource::Auto
                    && is_block_axis_scroll_container)
                    && !contain_layout;
                let auto_inline_size = style.size().width.is_auto();
                let inline_insets = padding.horizontal_axis_sum() + border.horizontal_axis_sum();
                let cached_max_content_inline_size = auto_inline_size
                    .then(|| {
                        self.nodes[child_node_id]
                            .element_data()
                            .and_then(|element| element.inline_layout_data.as_ref())
                            .filter(|layout| layout.layout.lines().count() == 1)
                            .map(|layout| layout.layout.calculate_content_widths().max / scale)
                    })
                    .flatten();
                // A note subline treats an inline-block as one atomic item. Its auto inline
                // size must therefore see its max-content line, so the note's outer two-row
                // split cannot introduce extra rows inside the atom. Explicit width sizing
                // keywords are still resolved by `inline_box_inputs` below.
                let mut note_child_inputs = child_inputs;
                note_child_inputs.available_space = Size::MAX_CONTENT;
                let mut child_inputs =
                    inline_box_inputs(style.size().width, margin, note_child_inputs);
                if let Some(max_content_inline_size) = cached_max_content_inline_size {
                    child_inputs.known_dimensions.width =
                        Some(max_content_inline_size + inline_insets);
                }
                drop(style);

                let mut output = self.compute_child_layout(
                    crate::taffy_node_id(child_node_id),
                    child_inputs,
                );
                if auto_inline_size {
                    let max_content_inline_size = self.nodes[child_node_id]
                        .element_data()
                        .and_then(|element| element.inline_layout_data.as_ref())
                        .map(|layout| layout.layout.calculate_content_widths().max / scale);
                    if let Some(max_content_inline_size) = max_content_inline_size {
                        let current_content_size =
                            (output.size.width - inline_insets).max(0.0);
                        let max_content_border_box = output.size.width
                            + (max_content_inline_size - current_content_size).max(0.0);
                        if max_content_border_box > output.size.width {
                            // The first measurement may have used advances from the
                            // previous scale. Give the refreshed max-content width a
                            // subpixel cushion: the line breaker compares accumulated
                            // advances with this limit, and exact equality can otherwise
                            // split the atom into two lines at some DPI/zoom products.
                            child_inputs.known_dimensions.width = Some(
                                max_content_border_box + INLINE_BOX_MAX_CONTENT_CUSHION,
                            );
                            output = self.compute_child_layout(
                                crate::taffy_node_id(child_node_id),
                                child_inputs,
                            );
                        }
                    }
                }
                inline_box.width = (margin.left + margin.right + output.size.width) * scale;
                inline_box.baseline = if exports_baseline {
                    let baseline = if uses_last_baseline {
                        output.baselines.last.or(output.baselines.first)
                    } else {
                        output.baselines.first.or(output.baselines.last)
                    };
                    baseline.map(|baseline| {
                        let baseline = if is_scroll_container {
                            baseline.clamp(0.0, output.size.height)
                        } else {
                            baseline
                        };
                        (margin.top + baseline) * scale
                    })
                } else {
                    None
                };
                let margin_box_height = margin.top + margin.bottom + output.size.height;
                inline_box.height = if inline_box.baseline.is_some() {
                    (margin_box_height * scale).min(f32::MAX)
                } else {
                    (margin_box_height.max(0.0) * scale).min(f32::MAX)
                };

                let child_layout = self.nodes[child_node_id].unrounded_layout_mut();
                child_layout.size = output.size;
                child_layout.scrollable_overflow_rect = output.scrollable_overflow_rect;
                child_layout.padding = padding;
                child_layout.border = border;
                child_layout.margin = margin;
            }

            note.layout.break_all_lines(None);
            note.segments = super::construct::inline_note_segments(
                &note.layout,
                &note.text,
                &note.ruby_ranges,
            );
            note.min_content_advance = note
                .segments
                .iter()
                .map(|segment| segment.min_content_advance)
                .fold(0.0, f32::max);
            note.max_content_advance = note
                .segments
                .iter()
                .map(|segment| segment.max_content_advance)
                .fold(0.0, f32::max)
                .max(note.min_content_advance);
        }
        let note_intrinsic_by_id: std::collections::HashMap<_, _> = inline_layout
            .inline_notes
            .iter()
            .map(|note| {
                (
                    note.node_id.as_u64(),
                    (
                        note.min_content_advance,
                        note.max_content_advance,
                        note.box_edges,
                        note.layout.is_rtl(),
                    ),
                )
            })
            .collect();
        let mut assigned_note_ids = std::collections::HashSet::new();
        let intrinsic_note_widths: Vec<_> = inline_layout
            .layout
            .inline_boxes()
            .enumerate()
            .filter_map(|(index, inline_box)| {
                if inline_box.kind != parley::InlineBoxKind::CustomInFlow
                    || !assigned_note_ids.insert(inline_box.id)
                {
                    return None;
                }
                let (min_content, max_content, edges, is_rtl) =
                    *note_intrinsic_by_id.get(&inline_box.id)?;
                let (start, end) = if is_rtl {
                    (
                        edges.margin.right + edges.padding.right + edges.border.right,
                        edges.margin.left + edges.padding.left + edges.border.left,
                    )
                } else {
                    (
                        edges.margin.left + edges.padding.left + edges.border.left,
                        edges.margin.right + edges.padding.right + edges.border.right,
                    )
                };
                let min_extra = if edges.clone_decoration {
                    start + end
                } else {
                    start.max(end)
                };
                Some((index, min_content + min_extra, max_content + start + end))
            })
            .collect();
        for (index, min, max) in intrinsic_note_widths {
            inline_layout
                .layout
                .set_inline_box_intrinsic_widths(index, min, max);
        }

        // Update inline boxes
        #[cfg(feature = "writing-mode")]
        let parent_vertical = self.layout_wm_of(node_id).is_vertical();
        #[cfg(not(feature = "writing-mode"))]
        let parent_vertical = false;
        for ibox in inline_layout.layout.inline_boxes_mut() {
            if matches!(
                ibox.kind,
                parley::InlineBoxKind::HangBoundary
                    | parley::InlineBoxKind::InlineEdge
                    | parley::InlineBoxKind::Anchor
                    | parley::InlineBoxKind::OutOfFlow
                    | parley::InlineBoxKind::CustomOutOfFlow
                    | parley::InlineBoxKind::CustomInFlow
            ) {
                continue;
            }
            let style = self.child_layout_style(&self.nodes[NodeId::from_u64(ibox.id)]);
            let margin = style
                .margin()
                .resolve_or_zero(inputs.parent_size, resolve_calc_value);
            let padding = style
                .padding()
                .resolve_or_zero(inputs.parent_size, resolve_calc_value);
            let border = style
                .border()
                .resolve_or_zero(inputs.parent_size, resolve_calc_value);

            #[cfg(feature = "floats")]
            let is_floated = style.float().is_floated();
            #[cfg(not(feature = "floats"))]
            let is_floated = false;

            let is_out_of_flow = style.position().is_out_of_flow();
            // `baseline-source: auto` selects the last baseline for inline-blocks and the first
            // for other atomic inlines. A block-axis scroll-container inline-block (except buttons)
            // instead uses its bottom margin edge, but only with `auto` (css-align-3 §9.1).
            // `overflow: clip` is not a scroll container. Exported scroll-container baselines
            // are clamped to the border box.
            // A layout-contained box is treated as having no baseline (css-contain-1 §3.3).
            let overflow = style.overflow();
            let box_style = style.style.get_box();
            let is_flow = matches!(
                style.effective_display().inside(),
                DisplayInside::Flow | DisplayInside::FlowRoot
            );
            let baseline_source = box_style.clone_baseline_source();
            let uses_last_baseline = match baseline_source {
                BaselineSource::Auto => is_flow,
                BaselineSource::First => false,
                BaselineSource::Last => true,
            };
            let is_scroll_container = !matches!(overflow.y, Overflow::Visible | Overflow::Clip);
            let is_block_axis_scroll_container = is_flow
                && is_scroll_container
                && !style
                    .flags
                    .intersects(StyleFlags::IS_BUTTON | StyleFlags::IS_BUTTON_INPUT);
            let contain_layout = box_style.clone_contain().contains(Contain::LAYOUT);
            let exports_baseline = !(baseline_source == BaselineSource::Auto
                && is_block_axis_scroll_container)
                && !contain_layout;
            let auto_inline_size = style.size().width.is_auto();
            let inline_insets = padding.horizontal_axis_sum() + border.horizontal_axis_sum();
            let box_inputs = self.inline_box_inputs_for_child(
                NodeId::from_u64(ibox.id),
                scale,
                style.size().width,
                margin,
                padding,
                border,
                child_inputs,
            );
            drop(style);

            if is_out_of_flow || is_floated {
                ibox.width = 0.0;
                ibox.height = 0.0;
                ibox.baseline = None;
            } else {
                let mut box_inputs = box_inputs;
                let mut output =
                    self.compute_child_layout(taffy::NodeId::from(ibox.id), box_inputs);
                if auto_inline_size {
                    let refreshed = self.nodes[NodeId::from_u64(ibox.id)]
                        .element_data()
                        .and_then(|element| element.inline_layout_data.as_ref())
                        .map(|layout| layout.layout.calculate_content_widths().max / scale);
                    if let Some(max_content_inline_size) = refreshed {
                        let natural = max_content_inline_size + inline_insets;
                        let fits = match box_inputs.available_space.width {
                            taffy::AvailableSpace::MinContent => false,
                            taffy::AvailableSpace::MaxContent => true,
                            taffy::AvailableSpace::Definite(limit) => natural <= limit,
                        };
                        if fits && natural > output.size.width {
                            box_inputs.known_dimensions.width =
                                Some(natural + INLINE_BOX_MAX_CONTENT_CUSHION);
                            output =
                                self.compute_child_layout(taffy::NodeId::from(ibox.id), box_inputs);
                        }
                    }
                }
                ibox.baseline = if exports_baseline {
                    let baseline = if uses_last_baseline {
                        output.baselines.last.or(output.baselines.first)
                    } else {
                        output.baselines.first.or(output.baselines.last)
                    };
                    baseline.map(|baseline| {
                        let baseline = if is_scroll_container {
                            baseline.clamp(0.0, output.size.height)
                        } else {
                            baseline
                        };
                        (margin.top + baseline) * scale
                    })
                } else {
                    None
                };
                let advance = (margin.left + margin.right + output.size.width) * scale;
                ibox.width = if parent_vertical
                    && ibox.kind == parley::InlineBoxKind::InFlow
                    && ibox.baseline.is_some()
                {
                    // The child's border box is rounded independently when it is
                    // finalized. Keep one whole physical pixel beyond the rounded
                    // inline extent so the following glyph cannot sit under it.
                    advance.ceil() + 1.0
                } else {
                    advance
                };
                // Vertical margins adjust the space the box reserves in the line. A box with a
                // baseline splits that space into ascent (`margin.top + baseline`) and descent
                // (`margin.bottom + height - baseline`), either of which may be negative. A box
                // without a baseline sits on the baseline and cannot reserve negative space.
                // Kept finite: huge author lengths can sum to infinity, which is taller than
                // the line breaker's `f32::MAX` height limit, and it then yields
                // `MaxHeightExceeded` for this box forever without advancing.
                let margin_box_height = margin.top + margin.bottom + output.size.height;
                ibox.height = if ibox.baseline.is_some() {
                    (margin_box_height * scale).min(f32::MAX)
                } else {
                    (margin_box_height.max(0.0) * scale).min(f32::MAX)
                };
            }
        }

        let inline_boxes: Vec<_> = inline_layout.layout.inline_boxes().collect();
        let mut visible_box_suffix = vec![false; inline_boxes.len() + 1];
        for index in (0..inline_boxes.len()).rev() {
            let inline_box = inline_boxes[index];
            let visible = match inline_box.kind {
                parley::InlineBoxKind::InFlow => inline_box.width > 0.0 || inline_box.height > 0.0,
                parley::InlineBoxKind::CustomInFlow
                | parley::InlineBoxKind::CustomInFlowCentered => true,
                parley::InlineBoxKind::InlineEdge => inline_box.width != 0.0,
                parley::InlineBoxKind::OutOfFlow
                | parley::InlineBoxKind::CustomOutOfFlow
                | parley::InlineBoxKind::HangBoundary
                | parley::InlineBoxKind::Anchor => false,
            };
            visible_box_suffix[index] = visible || visible_box_suffix[index + 1];
        }
        let mut visible_text_suffix = vec![false; inline_layout.text.len() + 1];
        let mut text_has_visible = false;
        for (index, character) in inline_layout.text.char_indices().rev() {
            text_has_visible |= !character.is_whitespace();
            visible_text_suffix[index] = text_has_visible;
        }
        let following_content: Vec<_> = inline_layout
            .inline_notes
            .iter()
            .map(|note| {
                let marker = inline_boxes.iter().enumerate().find(|(_, inline_box)| {
                    inline_box.kind == parley::InlineBoxKind::CustomInFlow
                        && inline_box.id == note.node_id.as_u64()
                });
                let Some((first_index, marker)) = marker else {
                    return false;
                };
                let has_text = visible_text_suffix
                    .get(marker.index)
                    .copied()
                    .unwrap_or(false);
                let next_box = first_index.saturating_add(note.slot_count).min(inline_boxes.len());
                has_text || visible_box_suffix[next_box]
            })
            .collect();
        for (note, has_following_content) in
            inline_layout.inline_notes.iter_mut().zip(following_content)
        {
            note.has_following_content = has_following_content;
        }

        let text_indent = self.nodes[node_id]
            .primary_styles()
            .map(|s| s.clone_text_indent())
            .unwrap_or_else(GenericTextIndent::zero);
        let indent_options = IndentOptions {
            each_line: text_indent.each_line,
            hanging: text_indent.hanging,
        };
        // Percentage indents do not contribute to intrinsic widths.
        inline_layout.layout.set_text_indent(
            text_indent.length.resolve(CSSPixelLength::new(0.0)).px() * scale,
            indent_options,
        );

        let pbw = container_pb.horizontal_components().sum() * scale;
        let width = known_dimensions
            .width
            .map(|w| (w * scale) - pbw)
            .unwrap_or_else(|| {
                // TODO: Cache content widths.
                //
                // This is a little tricky as the size of the inline boxes may depend on whether we are sizing under
                // and a min-content or max-content constraint. So if we want to compute both widths in one pass then
                // we need to store both a min-content and max-content size on each box.
                let content_sizes = inline_layout.layout.calculate_content_widths();
                let min_content_width = content_sizes.min;
                let max_content_width = content_sizes.max;

                #[cfg(feature = "floats")]
                let float_width = match available_space.width {
                    AvailableSpace::Definite(_) => 0.0,
                    AvailableSpace::MinContent => {
                        let mut width: f32 = 0.0;
                        for ibox in inline_layout.layout.inline_boxes_mut() {
                            if matches!(
                                ibox.kind,
                                parley::InlineBoxKind::HangBoundary
                                    | parley::InlineBoxKind::InlineEdge
                                    | parley::InlineBoxKind::Anchor
                            ) {
                                continue;
                            }
                            let (is_floated, margin) = {
                                let style =
                                    self.child_layout_style(&self.nodes[NodeId::from_u64(ibox.id)]);
                                (
                                    style.float().is_floated(),
                                    style
                                        .margin()
                                        .resolve_or_zero(inputs.parent_size, resolve_calc_value),
                                )
                            };

                            if is_floated {
                                let output = self.compute_child_layout(
                                    taffy::NodeId::from(ibox.id),
                                    subtract_margins(child_inputs, margin),
                                );
                                width = width.max(output.size.width + margin.left + margin.right);
                            }
                        }

                        width * scale
                    }
                    AvailableSpace::MaxContent => {
                        // When computing a max-content size the available width is effectively
                        // infinite, so floats never wrap onto a new "band" due to a lack of
                        // horizontal space. They only move below preceding floats when the `clear`
                        // property forces them to.
                        //
                        // Floats that share a band sit side-by-side and so their widths sum, whereas
                        // floats pushed onto a new band (via `clear`) stack vertically and so we
                        // take the maximum extent across bands rather than summing.
                        let mut left_band: f32 = 0.0;
                        let mut right_band: f32 = 0.0;
                        let mut width: f32 = 0.0;
                        for ibox in inline_layout.layout.inline_boxes_mut() {
                            if matches!(
                                ibox.kind,
                                parley::InlineBoxKind::HangBoundary
                                    | parley::InlineBoxKind::InlineEdge
                                    | parley::InlineBoxKind::Anchor
                            ) {
                                continue;
                            }
                            let (float, clear, margin) = {
                                let style =
                                    self.child_layout_style(&self.nodes[NodeId::from_u64(ibox.id)]);
                                (
                                    style.float(),
                                    style.clear(),
                                    style
                                        .margin()
                                        .resolve_or_zero(inputs.parent_size, resolve_calc_value),
                                )
                            };

                            if float.is_floated() {
                                if matches!(clear, Clear::Left | Clear::Both) {
                                    left_band = 0.0;
                                }
                                if matches!(clear, Clear::Right | Clear::Both) {
                                    right_band = 0.0;
                                }

                                let output = self.compute_child_layout(
                                    taffy::NodeId::from(ibox.id),
                                    subtract_margins(child_inputs, margin),
                                );
                                let box_width = output.size.width + margin.left + margin.right;

                                match float {
                                    Float::Left => left_band += box_width,
                                    Float::Right => right_band += box_width,
                                    Float::None => {}
                                }
                                width = width.max(left_band + right_band);
                            }
                        }

                        width * scale
                    }
                };

                #[cfg(not(feature = "floats"))]
                let float_width = 0.0;

                let computed_width = match available_space.width {
                    AvailableSpace::MinContent => min_content_width.max(float_width),
                    AvailableSpace::MaxContent => max_content_width + float_width,
                    AvailableSpace::Definite(limit) => (limit * scale)
                        .min(max_content_width + float_width)
                        .max(min_content_width),
                }
                .ceil();

                let style_width = node_size.width.map(|w| w * scale);
                let min_width = node_min_size.width.map(|w| w * scale);
                let max_width = node_max_size.width.map(|w| w * scale);

                (style_width)
                    .unwrap_or(computed_width + pbw)
                    .max(computed_width)
                    .maybe_clamp(min_width, max_width)
                    - pbw
            });

        #[cfg(not(feature = "floats"))]
        let _ = block_ctx; // Suppress unused variable warning

        // Set block context width if this is a block context root
        #[cfg(feature = "floats")]
        let is_bfc_root = block_ctx.is_bfc_root();
        #[cfg(feature = "floats")]
        if is_bfc_root {
            block_ctx.set_width((width + pbw) / scale);
        }

        // Create sub-context to account for the inline layout's padding/border
        #[cfg(feature = "floats")]
        let outer_block_ctx = block_ctx;
        #[cfg(feature = "floats")]
        let mut block_ctx =
            outer_block_ctx.sub_context(container_pb.top, [container_pb.left, container_pb.right]);
        // block_ctx.apply_content_box_inset([container_pb.left, container_pb.right]);

        if inputs.run_mode == taffy::RunMode::ComputeSize
            && inputs.axis == RequestedAxis::Horizontal
        {
            // Put layout back
            self.nodes[node_id]
                .data
                .downcast_element_mut()
                .unwrap()
                .inline_layout_data = Some(inline_layout);

            let measured_size = inputs.known_dimensions.unwrap_or(taffy::Size {
                width: width.ceil() / scale,
                // Height is ignored if RequestedAxis if Horizontal
                height: 0.0,
            });

            let clamped_size = inputs
                .known_dimensions
                .or(node_size)
                .unwrap_or(measured_size + content_box_inset.sum_axes())
                .maybe_clamp(node_min_size, node_max_size)
                .maybe_max(container_pb.sum_axes().map(Some));

            return LayoutOutput::from_outer_size(clamped_size);
        }

        let resolved_text_indent = text_indent
            .length
            .resolve(CSSPixelLength::new((width / scale).max(0.0)))
            .px();
        inline_layout
            .layout
            .set_text_indent(resolved_text_indent * scale, indent_options);

        let inline_wraps = self.nodes[node_id]
            .primary_styles()
            .or_else(|| {
                self.nodes[node_id]
                    .parent
                    .and_then(|parent| self.nodes[parent].primary_styles())
            })
            .is_none_or(|styles| {
                stylo_to_parley::text_wrap_mode(styles.clone_text_wrap_mode())
                    == parley::TextWrapMode::Wrap
            });

        // A ruby annotation is drawn but takes no room in the line (CSS Ruby 1: the annotation
        // is laid out over its base), so its advances are cleared before the lines are broken.
        inline_layout
            .layout
            .clear_advances(&inline_layout.text, &inline_layout.ruby_ranges);

        // In vertical writing modes Taffy's block axis is physical x, so a line box must be as
        // tall as its tallest box rather than `max(ascent) + max(descent)` (CSS Writing Modes 3
        // §7.1: the font's ascent and descent belong to the inline axis).
        #[cfg(feature = "writing-mode")]
        {
            let wm = self.layout_wm_of(node_id);
            inline_layout.layout.set_vertical(wm.is_vertical());
            if wm.is_vertical() {
                set_vertical_advances(
                    &mut inline_layout.layout,
                    &inline_layout.text,
                    text_orientation(wm.is_sideways(), wm.is_upright()),
                );
            }
        }

        #[cfg(not(feature = "floats"))]
        {
            if inline_layout.inline_notes.is_empty() {
                inline_layout.layout.break_all_lines(Some(width));
            } else {
                let parent_strut = inline_layout.layout.root_span_size_and_line_height();
                let notes = &inline_layout.inline_notes;
                let source_indices: std::collections::HashMap<_, _> = notes
                    .iter()
                    .enumerate()
                    .map(|(index, note)| (note.node_id.as_u64(), index))
                    .collect();
                let mut cursors = vec![InlineNoteCursor::default(); notes.len()];
                let fragments = &mut inline_layout.inline_note_fragments;
                fragments.fill(None);
                let mut breaker = inline_layout.layout.break_lines();
                {
                    let state = breaker.state_mut();
                    state.set_layout_max_advance(width);
                    state.set_line_max_advance(width);
                }

                while let Some(yield_data) = breaker.break_next() {
                    match yield_data {
                        YieldData::InlineBoxBreak(box_break) => {
                            if resolve_inline_note_break(
                                &mut breaker,
                                &box_break,
                                notes,
                                &source_indices,
                                &mut cursors,
                                fragments,
                                parent_strut,
                                inline_wraps,
                            )
                            .is_none()
                            {
                                breaker.resolve_custom_inline_box(
                                    box_break.inline_box_index,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    parley::InlineBoxKind::OutOfFlow,
                                );
                            }
                        }
                        YieldData::LineBreak(_) | YieldData::MaxHeightExceeded(_) => {}
                    }
                }
                breaker.finish();
            }
        }

        // Out-of-flow candidates bubbled up from this container and its in-flow subtree.
        // These are laid out by the out-of-flow positioning pass (`compute_oof_layout`).
        let mut oof_candidates = OofCandidates::new();

        // Perform inline layout
        #[cfg(feature = "floats")]
        {
            // `perform_layout` breaks lines independently of `compute_size`, so the cleared
            // annotation advances and the writing mode have to be re-applied here.
            inline_layout
                .layout
                .clear_advances(&inline_layout.text, &inline_layout.ruby_ranges);
            #[cfg(feature = "writing-mode")]
            {
                let wm = self.layout_wm_of(node_id);
                inline_layout.layout.set_vertical(wm.is_vertical());
                if wm.is_vertical() {
                    set_vertical_advances(
                        &mut inline_layout.layout,
                        &inline_layout.text,
                        text_orientation(wm.is_sideways(), wm.is_upright()),
                    );
                }
            }
            let parent_strut = inline_layout.layout.root_span_size_and_line_height();
            let notes = &inline_layout.inline_notes;
            let source_indices: std::collections::HashMap<_, _> = notes
                .iter()
                .enumerate()
                .map(|(index, note)| (note.node_id.as_u64(), index))
                .collect();
            let mut note_cursors = vec![InlineNoteCursor::default(); notes.len()];
            let note_fragments = &mut inline_layout.inline_note_fragments;
            note_fragments.fill(None);
            let mut breaker = inline_layout.layout.break_lines();
            let initial_slot = block_ctx.find_content_slot(0.0, Clear::None, None);
            let mut has_active_floats = initial_slot.segment_id.is_some();
            let state = breaker.state_mut();
            state.set_layout_max_advance(width);
            state.set_line_max_advance(initial_slot.width * scale);
            state.set_line_x(initial_slot.x * scale);
            state.set_line_y((initial_slot.y * scale) as f64);

            // TODO: revert state and retry layout if a line doesn't fit
            //
            // Save initial state. Saved state is used to revert the layout to a previous state if needed
            // (e.g. to revert a line that doesn't fit in the space it was laid out into)
            //
            // let mut saved_state = breaker.state().clone();

            while let Some(yield_data) = breaker.break_next() {
                match yield_data {
                    YieldData::LineBreak(_line_break_data) => {
                        let state = breaker.state_mut();

                        if has_active_floats {
                            // TODO: revert state and retry layout if a line doesn't fit
                            // saved_state = state.clone();

                            let min_y = state.line_y() / scale as f64;
                            let next_slot =
                                block_ctx.find_content_slot(min_y as f32, Clear::None, None);
                            has_active_floats = next_slot.segment_id.is_some();

                            state.set_line_max_advance(next_slot.width * scale);
                            state.set_line_x(next_slot.x * scale);
                            state.set_line_y((next_slot.y * scale) as f64);
                        } else {
                            state.set_line_x(0.0);
                            state.set_line_max_advance(width);
                        }

                        continue;
                    }
                    YieldData::MaxHeightExceeded(_data) => {
                        // TODO
                        continue;
                    }
                    YieldData::InlineBoxBreak(box_break_data) => {
                        if let Some(line_broken) = resolve_inline_note_break(
                            &mut breaker,
                            &box_break_data,
                            notes,
                            &source_indices,
                            &mut note_cursors,
                            note_fragments,
                            parent_strut,
                            inline_wraps,
                        ) {
                            if line_broken {
                                let state = breaker.state_mut();
                                let min_y = (state.line_y() / scale as f64) as f32;
                                let next_slot =
                                    block_ctx.find_content_slot(min_y, Clear::None, None);
                                has_active_floats = next_slot.segment_id.is_some();
                                state.set_line_max_advance(next_slot.width * scale);
                                state.set_line_x(next_slot.x * scale);
                                state.set_line_y((next_slot.y * scale) as f64);
                            }
                            continue;
                        }

                        let state = breaker.state_mut();
                        let node_id = NodeId::from_u64(box_break_data.inline_box_id);

                        let (direction, clear, margin) = {
                            let style = self.child_layout_style(&self.nodes[node_id]);
                            // We can assume that the box is a float because we only set `break_on_box: true` for floats
                            let direction = match style.float() {
                                Float::Left => taffy::FloatDirection::Left,
                                Float::Right => taffy::FloatDirection::Right,
                                Float::None => unreachable!(),
                            };
                            (
                                direction,
                                style.clear(),
                                style
                                    .margin()
                                    .resolve_or_zero(inputs.parent_size, resolve_calc_value),
                            )
                        };

                        let margin_sum = margin.sum_axes();

                        let mut output = self.compute_child_layout(
                            crate::taffy_node_id(node_id),
                            float_child_inputs,
                        );
                        let min_y = state.line_y() as f32 / scale;

                        // Note: `pos` is content-box relative
                        let pos = block_ctx.place_floated_box(
                            crate::taffy_node_id(node_id).into(),
                            output.size + margin_sum,
                            min_y,
                            direction,
                            clear,
                            false,
                        );

                        let min_y = state.line_y() / scale as f64; //.max(pos.y as f64);
                        let next_slot =
                            block_ctx.find_content_slot(min_y as f32, Clear::None, None);
                        has_active_floats = next_slot.segment_id.is_some();

                        state.set_line_max_advance(next_slot.width * scale);
                        state.set_line_x(next_slot.x * scale);
                        state.set_line_y((next_slot.y * scale) as f64);

                        let location = taffy::Point {
                            x: pos.x + margin.left + container_pb.left,
                            y: pos.y + margin.top + container_pb.top,
                        };
                        let layout = self.nodes[node_id].unrounded_layout_mut();
                        layout.size = output.size;
                        layout.location = location;
                        layout.margin = margin;

                        // Translate anchors from item-relative to container-relative
                        // coordinates and collect candidates bubbled from the float's subtree
                        if !output.oof_candidates.is_empty() {
                            output.oof_candidates.translate(location);
                            oof_candidates.append(&mut output.oof_candidates);
                        }

                        // dbg!(&layout.size);
                        // dbg!(&layout.location);

                        // Floats are out-of-flow and must not contribute to the line's height.
                        state.append_inline_box_to_line(
                            box_break_data.advance,
                            f32::NEG_INFINITY,
                            f32::NEG_INFINITY,
                        );

                        // if float.is_floated() {
                        //     println!("INLINE FLOATED BOX ({}) {:?}", ibox.id, float);
                        //     println!(
                        //         "w:{} h:{} x:{}, y:{}",
                        //         layout.size.width, layout.size.height, 0, 0
                        //     );
                        // }
                    }
                }
            }
            breaker.finish();
        }

        // Propagate the height consumed by floats placed within this container to the
        // enclosing block context (so that the BFC root can contain them)
        #[cfg(feature = "floats")]
        let float_height_contribution = block_ctx.floated_content_height_contribution();
        #[cfg(feature = "floats")]
        outer_block_ctx.add_child_floated_content_height_contribution(
            container_pb.top + float_height_contribution,
        );

        let (alignment, last_line_alignment) = self.nodes[node_id]
            .primary_styles()
            .map(|s| {
                (
                    stylo_to_parley::text_align(s.clone_text_align()),
                    stylo_to_parley::text_align_last(s.clone_text_align_last()),
                )
            })
            .unwrap_or((parley::layout::Alignment::Start, None));

        inline_layout.layout.align(
            alignment,
            AlignmentOptions {
                align_when_overflowing: false,
                last_line_alignment,
            },
        );

        // Parley lays out empty text as a single strut-height line (text-editor semantics),
        // but a line box containing no text, inline boxes or other in-flow content is a
        // zero-height line box in CSS (CSS2 §9.4.2).
        let has_inline_content =
            !inline_layout.text.is_empty() || inline_layout.layout.inline_boxes().len() > 0;

        let mut height = if has_inline_content {
            inline_layout.layout.height()
        } else {
            0.0
        };

        // A forced line break (e.g. `<br>` or a preserved newline) at the end of the inline
        // content ends the final line box without starting a new one. Parley still emits an
        // empty line after it (so that editors have a line to place the cursor on), so that
        // line is excluded from the measured height (and from the last baseline).
        let line_count = inline_layout.layout.len();
        let mut trailing_empty_line = None;
        if line_count >= 2
            && let (Some(prev_line), Some(last_line)) = (
                inline_layout.layout.get(line_count - 2),
                inline_layout.layout.get(line_count - 1),
            )
            && prev_line.break_reason() == BreakReason::Explicit
            && last_line.text_range().is_empty()
            && last_line.items().next().is_none()
        {
            height -= last_line.metrics().line_height;
            trailing_empty_line = Some(line_count - 1);
        }

        #[cfg(feature = "floats")]
        {
            if is_bfc_root {
                height = height.max(float_height_contribution * scale)
            };
        }

        if let Some(mut stream) = inline_layout.interline.take() {
            let body_lines = inline_layout.layout.lines().collect::<Vec<_>>();
            let parent_strut = inline_layout.layout.root_span_size_and_line_height();
            let body_font_size = parent_strut.map(|(font_size, _)| font_size).unwrap_or(0.0);
            let body_strut_line_height = parent_strut
                .map(|(_, line_height)| line_height)
                .filter(|line_height| *line_height > 0.0)
                .unwrap_or_else(|| {
                    body_lines
                        .last()
                        .map(|line| line.metrics().line_height)
                        .unwrap_or(0.0)
                });
            let body_gap = ((body_strut_line_height - body_font_size) * 0.5).max(0.0);

            let source_indices: std::collections::HashMap<_, _> = stream
                .sources
                .iter()
                .enumerate()
                .map(|(index, source)| (source.node_id.as_u64(), index))
                .collect();
            let marker_sources: std::collections::HashMap<_, _> = (0..stream.sources.len())
                .map(|index| (interline_marker_id(index), index))
                .collect();

            let mut anchors = Vec::new();
            let mut exact_anchor_sources = std::collections::HashSet::new();
            let mut inline_boundaries = vec![Vec::new(); body_lines.len()];
            for (line_index, line) in body_lines.iter().enumerate() {
                for item in line.items() {
                    let parley::PositionedLayoutItem::InlineBox(inline_box) = item else {
                        continue;
                    };
                    if inline_box.kind == parley::InlineBoxKind::Anchor {
                        let Some(&source_index) = source_indices.get(&inline_box.id) else {
                            continue;
                        };
                        if !stream.sources[source_index].active {
                            continue;
                        }
                        if exact_anchor_sources.contains(&source_index) {
                            continue;
                        }
                        anchors.push(InterlineAnchor {
                            source_index,
                            slot: line_index,
                            position: inline_box.x / scale,
                        });
                        continue;
                    }
                    if inline_box.kind != parley::InlineBoxKind::CustomInFlowCentered {
                        continue;
                    }
                    let Some(Some(fragment)) =
                        inline_layout.inline_note_fragments.get(inline_box.index)
                    else {
                        continue;
                    };
                    let Some(source) = inline_layout.inline_notes.get(fragment.source_index) else {
                        continue;
                    };
                    let Some(segment) = source.segments.get(fragment.segment_index) else {
                        continue;
                    };
                    let first_font_size = source
                        .layout
                        .root_span_size_and_line_height()
                        .map(|(font_size, _)| font_size)
                        .unwrap_or(fragment.first_line_height);
                    let first_gap =
                        ((fragment.first_line_height - first_font_size) * 0.5).max(0.0);
                    let first_boundary =
                        (line.metrics().line_height - inline_box.height) * 0.5
                            + fragment.block_start_inset;
                    inline_boundaries[line_index].push((first_boundary, first_gap));

                    if !source.layout.is_rtl() {
                        let box_ids = source
                            .layout
                            .inline_boxes()
                            .map(|inline_box| inline_box.id)
                            .collect::<Vec<_>>();
                        for (unit_range, first_line) in [
                            (&fragment.first_unit_range, true),
                            (&fragment.second_unit_range, false),
                        ] {
                            for unit_index in unit_range.clone() {
                                let Some(box_index) = segment.units[unit_index].inline_box_index
                                else {
                                    continue;
                                };
                                let Some(box_id) = box_ids.get(box_index) else {
                                    continue;
                                };
                                let Some(&nested_source) = source_indices.get(box_id) else {
                                    continue;
                                };
                                if !stream.sources[nested_source].active
                                    || !exact_anchor_sources.insert(nested_source)
                                {
                                    continue;
                                }
                                let prefix = inline_note_row_prefix_advance(
                                    segment,
                                    unit_range.start,
                                    unit_index,
                                    first_line,
                                );
                                anchors.push(InterlineAnchor {
                                    source_index: nested_source,
                                    slot: line_index,
                                    position: (inline_box.x
                                        + fragment.inline_start_edge
                                        + prefix)
                                        / scale,
                                });
                            }
                        }
                    }
                }
            }

            if anchors.is_empty() {
                inline_layout.interline = None;
            } else {
                let body_line_heights = body_lines
                    .iter()
                    .map(|line| line.metrics().line_height)
                    .collect::<Vec<_>>();
                let body_line_tops = body_lines
                    .iter()
                    .map(|line| line.metrics().block_min_coord)
                    .collect::<Vec<_>>();
                let body_original_height = height;
                let first_slot = anchors[0].slot;
                let mut anchor_by_source = vec![None; stream.sources.len()];
                for anchor in &anchors {
                    anchor_by_source[anchor.source_index] = Some(*anchor);
                }

                let mut current_slot = first_slot;
                let (mut slot_start, mut slot_width) =
                    interline_slot_geometry(&body_lines, width, current_slot);
                let mut slot_of_line = Vec::new();
                let mut no_justify_lines = Vec::new();
                let mut breaker = stream.layout.break_lines();
                {
                    let state = breaker.state_mut();
                    state.set_layout_max_advance(width);
                    state.set_line_max_advance(slot_width);
                    state.set_line_x(slot_start);
                    state.set_line_y(0.0);
                }

                while let Some(yield_data) = breaker.break_next() {
                    match yield_data {
                        YieldData::LineBreak(_) => {
                            slot_of_line.push(current_slot);
                            current_slot += 1;
                            (slot_start, slot_width) =
                                interline_slot_geometry(&body_lines, width, current_slot);
                            let state = breaker.state_mut();
                            state.set_line_max_advance(slot_width);
                            state.set_line_x(slot_start);
                        }
                        YieldData::InlineBoxBreak(box_break) => {
                            let Some(&source_index) =
                                marker_sources.get(&box_break.inline_box_id)
                            else {
                                breaker.resolve_custom_inline_box(
                                    box_break.inline_box_index,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    parley::InlineBoxKind::OutOfFlow,
                                );
                                continue;
                            };
                            let source = &stream.sources[source_index];
                            if !source.active {
                                breaker.resolve_custom_inline_box(
                                    box_break.inline_box_index,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    parley::InlineBoxKind::CustomInFlow,
                                );
                                continue;
                            }
                            let Some(anchor) = anchor_by_source[source_index] else {
                                breaker.resolve_custom_inline_box(
                                    box_break.inline_box_index,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    parley::InlineBoxKind::CustomInFlow,
                                );
                                continue;
                            };

                            let mut current_advance = box_break.advance;
                            if anchor.slot > current_slot {
                                if current_advance > 0.0
                                    && breaker.break_current_line().is_some()
                                {
                                    no_justify_lines.push(slot_of_line.len());
                                    slot_of_line.push(current_slot);
                                }
                                current_slot = anchor.slot;
                                (slot_start, slot_width) =
                                    interline_slot_geometry(&body_lines, width, current_slot);
                                let state = breaker.state_mut();
                                state.set_line_max_advance(slot_width);
                                state.set_line_x(slot_start);
                                current_advance = 0.0;
                            }

                            let mut gap = 0.0;
                            if anchor.slot == current_slot {
                                let target = anchor.position * scale - slot_start;
                                let remaining = (slot_width
                                    - current_advance
                                    - source.font_size)
                                    .max(0.0);
                                gap = (target - current_advance).clamp(0.0, remaining);
                            }
                            breaker.resolve_custom_inline_box(
                                box_break.inline_box_index,
                                gap,
                                0.0,
                                0.0,
                                0.0,
                                0.0,
                                parley::InlineBoxKind::CustomInFlow,
                            );
                        }
                        YieldData::MaxHeightExceeded(_) => {}
                    }
                }
                breaker.finish();
                for line_index in no_justify_lines {
                    stream.layout.suppress_line_justification(line_index);
                }

                while slot_of_line.len() < stream.layout.len() {
                    slot_of_line.push(current_slot);
                    current_slot += 1;
                }
                slot_of_line.truncate(stream.layout.len());

                let note_alignment = if alignment == parley::layout::Alignment::Justify {
                    parley::layout::Alignment::Justify
                } else {
                    parley::layout::Alignment::Start
                };
                stream.layout.align(
                    note_alignment,
                    AlignmentOptions {
                        align_when_overflowing: false,
                        last_line_alignment: None,
                    },
                );

                let mut line_by_slot = vec![None; stream.layout.len()];
                let mut line_slots = vec![0usize; stream.layout.len()];
                let mut line_heights = vec![0.0; stream.layout.len()];
                let mut line_advances = vec![0.0; stream.layout.len()];
                let mut line_half_leadings = vec![0.0; stream.layout.len()];
                let mut line_parts = vec![Vec::new(); stream.layout.len()];
                for (line_index, line) in stream.layout.lines().enumerate() {
                    let slot = slot_of_line[line_index];
                    if slot >= line_by_slot.len() {
                        line_by_slot.resize(slot + 1, None);
                    }
                    line_by_slot[slot] = Some(line_index);
                    line_slots[line_index] = slot;
                    line_heights[line_index] = line.metrics().line_height;
                    line_advances[line_index] = line.metrics().advance;
                    let parts = interline_line_parts(&stream, line, scale);
                    if let Some(part) = parts.first() {
                        let source = &stream.sources[part.source_index];
                        line_half_leadings[line_index] =
                            ((line.metrics().line_height - source.font_size) * 0.5).max(0.0);
                    }
                    line_parts[line_index] = parts;
                }
                while line_by_slot.len() < body_lines.len() {
                    line_by_slot.push(None);
                }

                let virtual_line_count = line_by_slot.len().max(body_lines.len());
                let mut body_offsets = vec![0.0; body_lines.len()];
                let mut note_deltas = vec![0.0; stream.layout.len()];
                let mut note_tops = vec![0.0; stream.layout.len()];
                let mut cumulative = 0.0;
                for slot in 0..virtual_line_count {
                    let original_top = if slot < body_lines.len() {
                        body_line_tops[slot]
                    } else {
                        body_original_height
                            + (slot - body_lines.len()) as f32 * body_strut_line_height
                    };
                    let note_line = line_by_slot.get(slot).and_then(|line| *line);
                    let note_height = note_line
                        .map(|line_index| line_heights[line_index])
                        .unwrap_or(0.0);
                    let note_gap = note_line
                        .map(|line_index| line_half_leadings[line_index])
                        .unwrap_or(0.0);
                    let body_gap_for_line = if slot < body_lines.len() {
                        ((body_line_heights[slot] - body_font_size) * 0.5).max(0.0)
                    } else {
                        body_gap
                    };
                    let adjustment = if note_line.is_none() {
                        0.0
                    } else if slot < inline_boundaries.len()
                        && !inline_boundaries[slot].is_empty()
                    {
                        inline_boundaries[slot]
                            .iter()
                            .map(|(boundary, inline_gap)| {
                                boundary + note_gap.min(*inline_gap)
                            })
                            .fold(f32::INFINITY, f32::min)
                    } else {
                        note_gap.min(body_gap_for_line)
                    };
                    let extra = note_height - adjustment;
                    if slot < body_lines.len() {
                        body_offsets[slot] = cumulative + extra;
                    }
                    if let Some(line_index) = note_line {
                        let original_note_top = stream
                            .layout
                            .get(line_index)
                            .map(|line| line.metrics().block_min_coord)
                            .unwrap_or(0.0);
                        let target_top = original_top + cumulative;
                        note_deltas[line_index] = target_top - original_note_top;
                        note_tops[line_index] = target_top;
                    }
                    cumulative += extra;
                }

                let synthetic_lines =
                    virtual_line_count.saturating_sub(body_lines.len());
                let new_height = body_original_height
                    + synthetic_lines as f32 * body_strut_line_height
                    + cumulative;
                inline_layout
                    .layout
                    .shift_line_block_offsets(&body_offsets);
                inline_layout.layout.set_block_size(new_height);
                stream.layout.shift_line_block_offsets(&note_deltas);
                inline_layout.line_block_offsets =
                    body_offsets.iter().map(|offset| offset / scale).collect();
                inline_layout.interline_anchors = anchors;
                inline_layout.interline_lines = (0..stream.layout.len())
                    .map(|line_index| InterlineLine {
                        slot_index: line_slots[line_index],
                        block_offset: note_tops[line_index] / scale,
                        height: line_heights[line_index] / scale,
                        advance: line_advances[line_index] / scale,
                        half_leading: line_half_leadings[line_index] / scale,
                        parts: std::mem::take(&mut line_parts[line_index]),
                    })
                    .collect();
                inline_layout.interline = Some(stream);
                height = new_height;
            }
        }

        // Note: `width` and `height` are content-box measurements of the inline content.
        // `known_dimensions` must not be substituted in here: those are border-box sizes, and
        // using them for the scrollable overflow rect (which adds padding below) would
        // double-count padding, incorrectly making the container's content overflow it.
        let measured_size = taffy::Size {
            width: width / scale,
            height: height / scale,
        };

        // Unbreakable content (e.g. `white-space: pre` text) may overflow the max advance the
        // lines were broken at. `width` is the advance the lines were broken at, so the actual
        // extent of the line boxes must be taken from the laid-out lines for `content_size`.
        let content_width = f32_max(width, inline_layout.layout.width()) / scale;

        let clamped_size = inputs
            .known_dimensions
            .or(node_size)
            .unwrap_or(measured_size + content_box_inset.sum_axes())
            .maybe_clamp(node_min_size, node_max_size);
        let final_size = Size {
            width: clamped_size.width,
            height: f32_max(
                clamped_size.height,
                aspect_ratio
                    .map(|ratio| clamped_size.width / ratio)
                    .unwrap_or(0.0),
            ),
        }
        .maybe_max(container_pb.sum_axes().map(Some));

        let container_direction = self.nodes[node_id].layout_style().direction();

        // `align-content` aligns the line boxes (as a single unit) within the content box in
        // the block axis. Floats are positioned relative to the formatting context and are not
        // moved. The offset is stored on the text layout for painting and hit-testing.
        let align_content = BlockContainerStyle::align_content(&self.nodes[node_id].layout_style());
        let block_offset = if align_content.keyword() == taffy::AlignContentKeyword::Normal {
            0.0
        } else {
            let free_space =
                final_size.height - content_box_inset.vertical_axis_sum() - measured_size.height;
            taffy::compute_block_align_content_offset(align_content, free_space)
        };
        inline_layout.block_offset = block_offset;
        let line_box_top = container_pb.top + block_offset;

        // Store sizes and positions of inline boxes
        let mut ibox_order: u32 = 0;
        let mut span_cb_flags = StyleFlags::empty();
        for line in inline_layout.layout.lines() {
            for item in line.items() {
                let parley::layout::PositionedLayoutItem::InlineBox(ibox) = item else {
                    continue;
                };
                if ibox.kind == parley::InlineBoxKind::CustomInFlowCentered {
                    if inputs.run_mode == RunMode::PerformLayout {
                        let Some(Some(fragment)) =
                            inline_layout.inline_note_fragments.get(ibox.index)
                        else {
                            continue;
                        };
                        let Some(source) = inline_layout.inline_notes.get(fragment.source_index)
                        else {
                            continue;
                        };
                        let Some(segment) = source.segments.get(fragment.segment_index) else {
                            continue;
                        };
                        let justification_amount = line.justification_amount_per_opportunity();
                        let (first_row_fill, second_row_fill) =
                            fragment.justification_fill_amounts(justification_amount);
                        let rows = [
                            (
                                &fragment.first_unit_range,
                                fragment.first_advance,
                                fragment.first_justification_opportunities,
                                first_row_fill,
                                0.0,
                                fragment.first_line,
                                fragment.first_hanging_start,
                                fragment.first_hanging_end,
                                fragment.first_hanging_advance,
                            ),
                            (
                                &fragment.second_unit_range,
                                fragment.second_advance,
                                fragment.second_justification_opportunities,
                                second_row_fill,
                                fragment.first_line_height,
                                false,
                                false,
                                fragment.second_hanging_end,
                                fragment.second_hanging_advance,
                            ),
                        ];
                        for (
                            unit_range,
                            row_advance,
                            row_justification_opportunities,
                            row_fill,
                            row_block_offset,
                            first_line,
                            hanging_start,
                            hanging_end,
                            hanging_advance,
                        ) in rows
                        {
                            if !segment.units[unit_range.clone()]
                                .iter()
                                .any(|unit| unit.atomic_node_id.is_some())
                            {
                                continue;
                            }
                            for (
                                atomic_node_id,
                                inline_position,
                                block_position,
                                margin_box_inline_size,
                                margin_box_block_size,
                            ) in
                                inline_note_atomic_positions(
                                    source,
                                    segment,
                                    &ibox,
                                    fragment,
                                    unit_range,
                                    row_advance,
                                    row_justification_opportunities,
                                    row_fill,
                                    justification_amount,
                                    row_block_offset,
                                    first_line,
                                    hanging_start,
                                    hanging_end,
                                    hanging_advance,
                                )
                            {
                                let child = NodeId::from_u64(atomic_node_id);
                                let (margin, inset_offset) = {
                                    let style = self.child_layout_style(&self.nodes[child]);
                                    let margin = style
                                        .margin()
                                        .resolve_or_zero(inputs.parent_size, resolve_calc_value);
                                    let inset_offset = if style.position() == taffy::Position::Relative
                                    {
                                        let inset_style = style.inset();
                                        let content_width = final_size.width
                                            - content_box_inset.horizontal_axis_sum();
                                        let content_height = final_size.height
                                            - content_box_inset.vertical_axis_sum();
                                        let inset = taffy::Rect {
                                            left: inset_style
                                                .left
                                                .maybe_resolve(content_width, resolve_calc_value),
                                            right: inset_style
                                                .right
                                                .maybe_resolve(content_width, resolve_calc_value),
                                            top: inset_style
                                                .top
                                                .maybe_resolve(content_height, resolve_calc_value),
                                            bottom: inset_style
                                                .bottom
                                                .maybe_resolve(content_height, resolve_calc_value),
                                        };
                                        let direction = style.direction();
                                        Point {
                                            x: if direction == Direction::Rtl {
                                                inset.right
                                                    .map(|value| -value)
                                                    .or(inset.left)
                                                    .unwrap_or(0.0)
                                            } else {
                                                inset.left
                                                    .or(inset.right.map(|value| -value))
                                                    .unwrap_or(0.0)
                                            },
                                            y: inset
                                                .top
                                                .or(inset.bottom.map(|value| -value))
                                                .unwrap_or(0.0),
                                        }
                                    } else {
                                        Point::ZERO
                                    };
                                    (margin, inset_offset)
                                };
                                let child_layout = self.nodes[child].unrounded_layout_mut();
                                child_layout.size = Size {
                                    width: (margin_box_inline_size / scale
                                        - margin.left
                                        - margin.right)
                                        .max(0.0),
                                    height: (margin_box_block_size / scale
                                        - margin.top
                                        - margin.bottom)
                                        .max(0.0),
                                };
                                child_layout.location = Point {
                                    x: inline_position / scale
                                        + margin.left
                                        + container_pb.left
                                        + inset_offset.x,
                                    y: block_position / scale
                                        + margin.top
                                        + line_box_top
                                        + inset_offset.y,
                                };
                            }
                        }
                    }
                    continue;
                }
                if matches!(
                    ibox.kind,
                    parley::InlineBoxKind::HangBoundary
                        | parley::InlineBoxKind::InlineEdge
                        | parley::InlineBoxKind::Anchor
                ) {
                    continue;
                }
                    let order = ibox_order;
                    ibox_order += 1;
                    if inputs.run_mode == RunMode::PerformLayout {
                        span_cb_flags |=
                            self.inline_span_cb_flags(node_id, NodeId::from_u64(ibox.id));
                    }
                    let node = &self.nodes[NodeId::from_u64(ibox.id)];
                    let style = self.child_layout_style(node);
                    let padding = style
                        .padding()
                        .resolve_or_zero(child_inputs.parent_size, resolve_calc_value);
                    let border = style
                        .border()
                        .resolve_or_zero(child_inputs.parent_size, resolve_calc_value);
                    let margin = style
                        .margin()
                        .resolve_or_zero(child_inputs.parent_size, resolve_calc_value);

                    #[cfg(feature = "floats")]
                    let is_floated = style.float() != Float::None;
                    #[cfg(not(feature = "floats"))]
                    let is_floated = false;

                    let position = style.position();
                    let is_absolute = position.is_out_of_flow();
                    let item_direction = style.direction();
                    // Inline formatting contexts have no `justify-items`/`align-items` for an
                    // `auto` self-alignment to defer to, so it behaves as `normal`.
                    let justify_self =
                        OofItemStyle::justify_self(&style).unwrap_or(taffy::AlignItems::NORMAL);
                    let align_self =
                        OofItemStyle::align_self(&style).unwrap_or(taffy::AlignItems::NORMAL);

                    // The static position of an absolutely positioned box depends on the
                    // display its hypothetical box would have had (the display specified
                    // before position:absolute blockified it): inline-level boxes sit at
                    // their position within the line, while block-level boxes start at the
                    // content-box left edge of their containing block.
                    let is_inline_level =
                        style.style.get_box().original_display.outside() == DisplayOutside::Inline;

                    // Resolve relative inset offsets against the containing block
                    // (the content box of the inline container).
                    let container_content_size = final_size - content_box_inset.sum_axes();
                    let inset_style = style.inset();
                    let inset = taffy::Rect {
                        left: inset_style
                            .left
                            .maybe_resolve(container_content_size.width, resolve_calc_value),
                        right: inset_style
                            .right
                            .maybe_resolve(container_content_size.width, resolve_calc_value),
                        top: inset_style
                            .top
                            .maybe_resolve(container_content_size.height, resolve_calc_value),
                        bottom: inset_style
                            .bottom
                            .maybe_resolve(container_content_size.height, resolve_calc_value),
                    };
                    let box_inputs = self.inline_box_inputs_for_child(
                        NodeId::from_u64(ibox.id),
                        scale,
                        style.size().width,
                        margin,
                        padding,
                        border,
                        child_inputs,
                    );
                    drop(style);

                    if is_absolute {
                        // The static-position rectangle
                        // (https://www.w3.org/TR/css-position-3/#staticpos-rect):
                        // - An inline-level box's rectangle is zero-width at its position
                        //   within the line (`ibox.y` is the baseline as out-of-flow boxes are
                        //   zero-sized) and spans the line box in the block axis.
                        // - A block-level box's rectangle spans the containing block's content
                        //   box in the inline axis and is zero-height below the line box.
                        let line_metrics = line.metrics();
                        let line_top = (line_metrics.block_min_coord / scale) + line_box_top;
                        let line_bottom = (line_metrics.block_max_coord / scale) + line_box_top;
                        let (inline_area, block_area) = if is_inline_level {
                            let x = (ibox.x / scale) + container_pb.left;
                            (
                                taffy::Line { start: x, end: x },
                                taffy::Line {
                                    start: line_top,
                                    end: line_bottom,
                                },
                            )
                        } else {
                            (
                                taffy::Line {
                                    start: container_pb.left,
                                    end: final_size.width - container_pb.right,
                                },
                                taffy::Line {
                                    start: line_bottom,
                                    end: line_bottom,
                                },
                            )
                        };

                        oof_candidates.push(OofCandidate {
                            node: taffy::NodeId::from(ibox.id),
                            order,
                            position,
                            static_position: taffy::Point {
                                x: AxisStaticPosition::from_alignment(
                                    justify_self.resolve_self_relative(
                                        item_direction,
                                        container_direction,
                                        true,
                                    ),
                                    inline_area,
                                    container_direction.is_rtl(),
                                ),
                                y: AxisStaticPosition::from_alignment(
                                    align_self.resolve_self_relative(
                                        item_direction,
                                        container_direction,
                                        false,
                                    ),
                                    block_area,
                                    false,
                                ),
                            },
                        });
                    } else if is_floated {
                        let layout = self.nodes[NodeId::from_u64(ibox.id)].unrounded_layout_mut();
                        layout.padding = padding; //.map(|p| p / scale);
                        layout.border = border; //.map(|p| p / scale);
                        layout.margin = margin;
                    } else {
                        // Re-measure the box to get its border-box size (this hits the layout
                        // cache). The size cannot be recovered from `ibox` dimensions as the
                        // space reserved in the line is clamped to be non-negative.
                        let mut output =
                            self.compute_child_layout(taffy::NodeId::from(ibox.id), box_inputs);
                        let size = output.size;
                        #[cfg(feature = "writing-mode")]
                        let vertical = self.layout_wm_of(node_id).is_vertical();
                        let node = &mut self.nodes[NodeId::from_u64(ibox.id)];

                        let is_relative = position == taffy::Position::Relative;
                        let inset_offset = if is_relative {
                            taffy::Point {
                                x: if container_direction == Direction::Rtl {
                                    inset.right.map(|x| -x).or(inset.left).unwrap_or(0.0)
                                } else {
                                    inset.left.or(inset.right.map(|x| -x)).unwrap_or(0.0)
                                },
                                y: inset.top.or(inset.bottom.map(|x| -x)).unwrap_or(0.0),
                            }
                        } else {
                            taffy::Point::ZERO
                        };

                        let layout = node.unrounded_layout_mut();
                        layout.size = size;
                        layout.scrollable_overflow_rect = output.scrollable_overflow_rect;
                        layout.location.x =
                            (ibox.x / scale) + margin.left + container_pb.left + inset_offset.x;
                        // Parley positions the margin box; offset to the border box even
                        // when a negative top margin makes it extend above the margin box.
                        #[cfg(feature = "writing-mode")]
                        let margin_box_block_offset =
                            // A vertical line's block axis is its column axis. Atomic
                            // inline baselines are horizontal metrics, so center the
                            // margin box in that line instead of using the baseline as
                            // a block-axis offset.
                            if vertical
                                && ibox.kind == parley::InlineBoxKind::InFlow
                                && ibox.baseline.is_some()
                            {
                                let line_start = line.metrics().block_min_coord / scale;
                                let line_box_height = (line.metrics().block_max_coord
                                    - line.metrics().block_min_coord)
                                    / scale;
                                let margin_box_height =
                                    margin.top + layout.size.height + margin.bottom;
                                line_start + (line_box_height - margin_box_height) * 0.5
                            } else {
                                ibox.y / scale
                            };
                        #[cfg(not(feature = "writing-mode"))]
                        let margin_box_block_offset = ibox.y / scale;
                        layout.location.y =
                            margin_box_block_offset + margin.top + line_box_top + inset_offset.y;
                        layout.padding = padding; //.map(|p| p / scale);
                        layout.border = border; //.map(|p| p / scale);
                        layout.margin = margin;

                        // Translate anchors from item-relative to container-relative
                        // coordinates and collect candidates bubbled from the box's subtree
                        if !output.oof_candidates.is_empty() {
                            let location = layout.location;
                            output.oof_candidates.translate(location);
                            oof_candidates.append(&mut output.oof_candidates);
                        }
                    }
            }
        }

        // println!("INLINE LAYOUT FOR {:?}. max_advance: {:?}", node_id, max_advance);
        // dbg!(&inline_layout.text);
        // println!("Computed: w: {} h: {}", inline_layout.layout.width(), inline_layout.layout.height());
        // println!("known_dimensions: w: {:?} h: {:?}", inputs.known_dimensions.width, inputs.known_dimensions.height);
        // println!("\n");

        let line_baseline =
            |line: parley::Line<'_, _>| (line.metrics().baseline / scale) + line_box_top;
        let exports_baseline = has_inline_content && !suppresses_baseline;
        let first_baseline = exports_baseline
            .then(|| inline_layout.layout.lines().next().map(line_baseline))
            .flatten();
        let last_line_index = trailing_empty_line.unwrap_or(line_count).checked_sub(1);
        let last_baseline = exports_baseline
            .then(|| last_line_index.and_then(|i| inline_layout.layout.get(i)))
            .flatten()
            .map(line_baseline);

        if inputs.run_mode == RunMode::PerformLayout {
            inline_layout.span_cb_flags = span_cb_flags;

            // Record the physical geometry needed to paint a vertical inline root.
            // Parley works in the axes of the container's own writing mode, so the
            // painter needs the physical block extent to map (and, for
            // `vertical-rl`, mirror) line box positions onto physical x.
            #[cfg(feature = "writing-mode")]
            {
                let wm = self.layout_wm_of(node_id);
                inline_layout.vertical_text = if wm.is_vertical() {
                    Some(VerticalTextPaint {
                        // In a vertical writing mode, Taffy's block axis — which
                        // `final_size.height` measures — is physical x.
                        block_extent: final_size.height - content_box_inset.vertical_axis_sum(),
                        block_axis_starts_at_right: !wm.is_vertical_lr(),
                        // Taffy's inline axis — which `final_size.width` measures
                        // — is physical y.
                        inline_extent: final_size.width - content_box_inset.horizontal_axis_sum(),
                        // Mirror only when Parley's idea of the line's start edge (the top,
                        // or the bottom for a right-to-left paragraph) differs from the
                        // writing mode's inline start.
                        inline_axis_needs_mirror: wm.is_inline_tb() != wm.is_bidi_ltr(),
                        // `TEXT_SIDEWAYS` comes from `text-orientation: sideways`;
                        // `VERTICAL_SIDEWAYS` from `writing-mode: sideways-*`, which
                        // rotates everything too. `UPRIGHT` only exists for
                        // `text-orientation: upright`, so its absence means `mixed`.
                        text_orientation: text_orientation(wm.is_sideways(), wm.is_upright()),
                    })
                } else {
                    None
                };
            }
        }

        // Put layout back
        self.nodes[node_id]
            .data
            .downcast_element_mut()
            .unwrap()
            .inline_layout_data = Some(inline_layout);

        let oof_position_inset = taffy::Rect {
            left: border.left,
            right: border.right + scrollbar_gutter.x,
            top: border.top,
            bottom: border.bottom + scrollbar_gutter.y,
        };

        LayoutOutput {
            size: final_size,
            scrollable_overflow_rect: {
                let content_extent = taffy::Size {
                    width: content_width,
                    height: measured_size.height,
                } + padding.sum_axes();
                taffy::Rect {
                    left: 0.0,
                    right: content_extent.width,
                    top: 0.0,
                    bottom: content_extent.height + block_offset.max(0.0),
                }
            },
            baselines: taffy::Baselines {
                first: first_baseline,
                last: last_baseline,
            },
            top_margin: CollapsibleMarginSet::ZERO,
            bottom_margin: CollapsibleMarginSet::ZERO,
            margins_can_collapse_through: !has_styles_preventing_being_collapsed_through
                && final_size.height == 0.0
                && measured_size.height == 0.0,
            oof_candidates,
            oof_positioning_area: Some(OofPositioningArea {
                size: final_size - oof_position_inset.sum_axes(),
                offset: Point {
                    x: oof_position_inset.left,
                    y: oof_position_inset.top,
                },
            }),
        }
    }
}

#[inline(always)]
fn f32_max(a: f32, b: f32) -> f32 {
    a.max(b)
}
