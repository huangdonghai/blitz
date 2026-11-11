use blitz_traits::{
    events::{BlitzImeEvent, BlitzKeyEvent},
    node_id::NodeId,
    shell::ShellProvider,
};
use keyboard_types::{Key, Modifiers, NamedKey};
use parley::{
    BreakReason, Cluster, ClusterSide, ContentWidths, FontContext, LayoutContext,
    PositionedLayoutItem,
};

use crate::util::ACTION_MOD;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
/// Parley Brush type for Blitz which contains the Blitz node id
pub struct TextBrush {
    /// The node id for the span
    pub id: NodeId,
}

impl TextBrush {
    pub(crate) fn from_id(id: NodeId) -> Self {
        Self { id }
    }
}

#[derive(Clone, Default)]
pub struct TextLayout {
    pub text: String,
    pub content_widths: Option<ContentWidths>,
    pub layout: parley::layout::Layout<TextBrush>,
    /// Pre-shaped content for inline-note compositions, keyed by their defining DOM node.
    /// The paragraph line breaker will choose and position fragments from these layouts;
    /// the note text is not duplicated into the paragraph's line stream.
    pub inline_notes: Vec<InlineNoteSource>,
    /// The fragment ranges selected by the line breaker, indexed by the
    /// corresponding Parley inline-box index.
    pub inline_note_fragments: Vec<Option<InlineNoteFragment>>,
    /// The shared interline-note stream. A block container owns exactly one,
    /// laid out into one interlinear slot per body line.
    pub interline: Option<InterlineNoteStream>,
    /// Zero-width declarations of interline notes on the body lines.
    pub interline_anchors: Vec<InterlineAnchor>,
    /// Note lines selected for the interlinear slots.
    pub interline_lines: Vec<InterlineLine>,
    /// Extra block-axis offset of each body line, in CSS pixels.
    pub line_block_offsets: Vec<f32>,
    /// Margin-note declarations found in this inline formatting context.
    pub margin_anchors: Vec<MarginNoteAnchor>,
    /// Laid-out margin-note groups painted with this inline root.
    pub margin_note_groups: Vec<MarginNoteGroup>,
    /// Block-axis offset (in CSS px) of the line boxes from the top of the container's content
    /// box, as applied by `align-content`.
    pub block_offset: f32,
    /// The out-of-flow positions for which some inline span with an inline box below it is a
    /// containing block (as `SPAN_*_CB` flags). The inline root claims those boxes in its place.
    /// Set by each `RunMode::PerformLayout` run of inline layout.
    pub span_cb_flags: stylo_taffy::StyleFlags,
    /// Byte ranges of the text that belongs to ruby annotations (`<rt>`, `<rtc>`).
    ///
    /// An annotation is drawn but takes no room in the line: its advances are cleared before
    /// breaking (see [`parley::layout::Layout::clear_advances`]), and the painter lifts it above
    /// the character it annotates.
    pub ruby_ranges: Vec<std::ops::Range<usize>>,
    /// Byte ranges of emphasis marks, with the colour `text-emphasis-color` gives them.
    ///
    /// The marks are shaped like text, so the painter needs to know which runs to paint in the
    /// emphasis colour rather than the text's.
    pub emphasis_ranges: Vec<(std::ops::Range<usize>, crate::util::Color)>,
    /// Vertical writing mode geometry, recorded by each `RunMode::PerformLayout`
    /// run of inline layout. `None` for horizontal writing modes.
    ///
    /// Parley lays text out in the axes of the container's own writing mode
    /// (inline axis = "width", block axis = "height"), which only coincides with
    /// physical space for `horizontal-tb`. Painting a vertical inline root
    /// therefore needs the physical block extent in order to map — and, for
    /// `vertical-rl`, mirror — line box positions onto the screen.
    pub vertical_text: Option<VerticalTextPaint>,
}

#[derive(Clone)]
pub struct InlineNoteSource {
    pub node_id: NodeId,
    pub text: String,
    pub layout: parley::layout::Layout<TextBrush>,
    pub ruby_ranges: Vec<std::ops::Range<usize>>,
    pub emphasis_ranges: Vec<(std::ops::Range<usize>, crate::util::Color)>,
    pub segments: Vec<InlineNoteSegment>,
    pub strut_line_height: f32,
    pub strut_over: f32,
    pub strut_under: f32,
    pub vertical_text_orientation: Option<TextOrientation>,
    pub min_content_advance: f32,
    pub max_content_advance: f32,
    pub has_following_content: bool,
    pub box_edges: InlineNoteBoxEdges,
    pub slot_count: usize,
}

/// Resolved box edges for one inline-note element, in Parley's layout units.
#[derive(Clone, Copy, Default)]
pub struct InlineNoteBoxEdges {
    pub margin: taffy::Rect<f32>,
    pub padding: taffy::Rect<f32>,
    pub border: taffy::Rect<f32>,
    pub clone_decoration: bool,
}

#[derive(Clone)]
pub struct InlineNoteSegment {
    pub line_index: usize,
    pub text_range: std::ops::Range<usize>,
    pub line_height: f32,
    pub run_spans: Vec<InlineNoteRunSpan>,
    pub units: Vec<InlineNoteUnit>,
    pub prefix_advances: Vec<f32>,
    pub legal_split_boundaries: Vec<usize>,
    pub nonnegative_advances: bool,
    pub min_content_advance: f32,
    pub max_content_advance: f32,
}

#[derive(Clone)]
pub struct InlineNoteRunSpan {
    pub text_range: std::ops::Range<usize>,
    /// Run item index within the pre-shaped Parley line.
    pub item_index: usize,
}

#[derive(Clone)]
pub struct InlineNoteUnit {
    pub text_range: std::ops::Range<usize>,
    pub shaped_clusters: std::ops::Range<u32>,
    /// Layout box represented by this indivisible unit, when it is atomic inline content.
    pub inline_box_index: Option<usize>,
    /// DOM node represented by this unit when it paints as an atomic inline child.
    pub atomic_node_id: Option<u64>,
    /// Visual line-item index used to place atomic content beside shaped text runs.
    pub line_item_index: usize,
    /// Unit order within its source line after bidi visual reordering.
    pub visual_order_index: usize,
    /// Atomic box block offset from the source line's block minimum.
    pub inline_box_block_offset: f32,
    pub line_height: f32,
    pub line_over: f32,
    pub line_under: f32,
    /// Advance when this unit is inside a note subline.
    pub advance: f32,
    /// Advance when this unit starts a subline (without ending it).
    pub line_start_advance: f32,
    /// Advance when opening punctuation hangs at the start of this subline.
    pub line_start_hanging_advance: f32,
    /// Advance when this unit ends a subline (without starting it).
    pub line_end_advance: f32,
    /// Advance at a subline end when force-end hanging applies.
    pub line_end_hanging_advance: f32,
    /// Advance at a subline end when allow-end hanging applies.
    pub line_end_allow_hanging_advance: f32,
    /// Advance at a subline end when last-line hanging applies.
    pub line_end_last_hanging_advance: f32,
    /// Advance when this unit starts the first formatted line of the note.
    pub first_line_start_advance: f32,
    /// First-line start advance with opening-punctuation hanging.
    pub first_line_start_hanging_advance: f32,
    pub starts_with_hanging_opening: bool,
    /// Advance when this unit is the entire subline.
    pub line_edges_advance: f32,
    /// Advance for a single-unit subline with force-end hanging.
    pub line_edges_hanging_advance: f32,
    /// Single-unit subline advance with opening-punctuation hanging.
    pub line_edges_start_hanging_advance: f32,
    /// Single-unit subline advance with allow-end hanging.
    pub line_edges_allow_hanging_advance: f32,
    /// Single-unit subline advance with last-line hanging.
    pub line_edges_last_hanging_advance: f32,
    /// As above, for the first formatted line of the note.
    pub first_line_edges_advance: f32,
    /// First-line single-unit advance with force-end hanging.
    pub first_line_edges_hanging_advance: f32,
    /// First-line single-unit advance with allow-end hanging.
    pub first_line_edges_allow_hanging_advance: f32,
    /// First-line single-unit advance with last-line hanging.
    pub first_line_edges_last_hanging_advance: f32,
    /// First-line single-unit advance with opening-punctuation hanging.
    pub first_line_edges_start_hanging_advance: f32,
    pub force_hanging_at_end: bool,
    pub allow_hanging_at_end: bool,
    pub last_hanging_at_end: bool,
    pub is_justification_opportunity: bool,
    pub is_word_separator: bool,
    pub can_split_after: bool,
}

#[derive(Clone)]
pub struct InlineNoteFragment {
    pub source_index: usize,
    pub segment_index: usize,
    pub first_range: std::ops::Range<usize>,
    pub second_range: std::ops::Range<usize>,
    pub first_unit_range: std::ops::Range<usize>,
    pub second_unit_range: std::ops::Range<usize>,
    pub first_shaped_clusters: std::ops::Range<u32>,
    pub second_shaped_clusters: std::ops::Range<u32>,
    pub first_line: bool,
    pub first_fragment: bool,
    pub last_fragment: bool,
    pub outer_line_end: bool,
    pub first_hanging_start: bool,
    pub first_hanging_end: bool,
    pub second_hanging_end: bool,
    pub first_advance: f32,
    pub second_advance: f32,
    pub first_hanging_advance: f32,
    pub second_hanging_advance: f32,
    pub first_justification_opportunities: u32,
    pub second_justification_opportunities: u32,
    pub first_line_height: f32,
    pub second_line_height: f32,
    pub inline_start_edge: f32,
    pub inline_end_edge: f32,
    pub block_start_inset: f32,
    pub block_end_inset: f32,
}

/// One pre-shaped, shared interline-note flow.
#[derive(Clone, Default)]
pub struct InterlineNoteStream {
    pub text: String,
    pub layout: parley::layout::Layout<TextBrush>,
    pub ruby_ranges: Vec<std::ops::Range<usize>>,
    pub emphasis_ranges: Vec<(std::ops::Range<usize>, crate::util::Color)>,
    pub sources: Vec<InterlineNoteSource>,
}

#[derive(Clone, Debug)]
pub struct InterlineNoteSource {
    pub node_id: NodeId,
    /// Byte range of this note in [`InterlineNoteStream::text`].
    pub text_range: std::ops::Range<usize>,
    /// Whether the note generated any in-flow content.
    pub active: bool,
    pub strut_line_height: f32,
    pub strut_over: f32,
    pub strut_under: f32,
    pub font_size: f32,
    pub vertical_text_orientation: Option<TextOrientation>,
}

#[derive(Clone, Copy, Debug)]
pub struct InterlineAnchor {
    pub source_index: usize,
    pub slot: usize,
    /// Inline coordinate of the zero-width declaration, in CSS pixels.
    pub position: f32,
}

#[derive(Clone, Debug, Default)]
pub struct InterlineLine {
    pub slot_index: usize,
    /// Slot start in CSS pixels, relative to the container's content box.
    pub block_offset: f32,
    /// The note line's block size in CSS pixels.
    pub height: f32,
    /// The note line's natural inline advance in CSS pixels.
    pub advance: f32,
    /// Half-leading of the note line, in CSS pixels.
    pub half_leading: f32,
    pub parts: Vec<InterlinePart>,
}

#[derive(Clone, Debug)]
pub struct InterlinePart {
    pub source_index: usize,
    pub text_range: std::ops::Range<usize>,
    /// Inline offset of the part in its note line, in CSS pixels.
    pub inline_offset: f32,
    /// Natural inline advance of the part in CSS pixels.
    pub advance: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MarginNoteSide {
    InlineStart,
    InlineEnd,
}

#[derive(Clone, Copy, Debug)]
pub struct MarginNoteAnchor {
    pub node_id: NodeId,
    /// Body line whose block-start the note group aligns to.
    pub line_index: usize,
    /// Scan order within the block, used to break ties on the same line.
    pub order: u32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MarginNoteRect {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

#[derive(Clone)]
pub struct MarginNoteGroup {
    pub node_id: NodeId,
    pub side: MarginNoteSide,
    pub text: String,
    pub layout: parley::layout::Layout<TextBrush>,
    pub ruby_ranges: Vec<std::ops::Range<usize>>,
    pub emphasis_ranges: Vec<(std::ops::Range<usize>, crate::util::Color)>,
    /// Physical translation from the inline root's content origin to the note's
    /// content box, in CSS pixels along the root's inline axis.
    pub inline_dx: f32,
    pub inline_dy: f32,
    /// Content box in page coordinates, in CSS pixels.
    pub content_box: MarginNoteRect,
    pub padding_box: MarginNoteRect,
    pub border_box: MarginNoteRect,
    pub margin_box: MarginNoteRect,
    pub content_inline_extent: f32,
    pub content_block_extent: f32,
    /// The piece of this group that belongs to one page, in page coordinates.
    /// Empty for an unpaged document, where the group itself is the painting
    /// unit. A paged group keeps one shared text layout and slices complete
    /// lines into these fragments; cloning the layout per page would make a
    /// long margin note cost O(pages x note length) to paint.
    pub page_fragments: Vec<MarginNotePageFragment>,
}

#[derive(Clone, Debug)]
pub struct MarginNotePageFragment {
    pub page_index: usize,
    /// Half-open line range in the group's shared layout.
    pub line_start: usize,
    pub line_end: usize,
    /// Physical shift from the group's continuous coordinates to this page's
    /// fragment, in CSS pixels.
    pub shift_x: f32,
    pub shift_y: f32,
    pub content_box: MarginNoteRect,
    pub padding_box: MarginNoteRect,
    pub border_box: MarginNoteRect,
    pub margin_box: MarginNoteRect,
    pub content_block_extent: f32,
}

impl InlineNoteFragment {
    /// Extra justification spacing each row needs to share the note's width on its outer line.
    pub fn justification_fill_amounts(&self, justification_amount: f32) -> (f32, f32) {
        let first_common_width = self.first_advance
            + self.first_justification_opportunities as f32 * justification_amount;
        let second_common_width = self.second_advance
            + self.second_justification_opportunities as f32 * justification_amount;
        let justified_note_width = first_common_width.max(second_common_width);
        let row_fill = |advance: f32, opportunities: u32| {
            if self.outer_line_end && justification_amount > 0.0 && opportunities > 0 {
                (justified_note_width - advance).max(0.0) / opportunities as f32
            } else {
                0.0
            }
        };
        (
            row_fill(first_common_width, self.first_justification_opportunities),
            row_fill(second_common_width, self.second_justification_opportunities),
        )
    }
}

/// Physical geometry needed to paint an inline root laid out in a vertical
/// writing mode. Parley's coordinates are in the writing mode's own axes, so the
/// painter has to map the block axis onto physical x (mirrored for
/// `vertical-rl`, whose block axis starts at the right edge) and the inline axis
/// onto physical y (mirrored when the inline axis runs bottom-to-top).
#[derive(Debug, Clone, Copy)]
pub struct VerticalTextPaint {
    /// Physical block extent of the content box, in CSS px.
    pub block_extent: f32,
    /// Whether the block axis starts at the right edge (`vertical-rl`).
    pub block_axis_starts_at_right: bool,
    /// Physical inline extent of the content box, in CSS px.
    pub inline_extent: f32,
    /// Whether the painter has to mirror the inline axis across the content box.
    ///
    /// Parley positions a line's content against the physical top edge that its
    /// coordinates start from — a left-to-right paragraph is placed at the top for
    /// `text-align: start`, a right-to-left one at the bottom. When that edge is not the
    /// writing mode's inline start — which is the bottom edge for `sideways-lr` with
    /// `direction: ltr`, and for `vertical-*` and `sideways-rl` with `direction: rtl` —
    /// the glyph positions have to be flipped.
    pub inline_axis_needs_mirror: bool,
    /// How `text-orientation` decides which glyphs are rotated.
    pub text_orientation: TextOrientation,
}

/// The `text-orientation` of a vertical inline root.
///
/// CSS Writing Modes §5.1: `mixed` (the initial value) orients each character
/// from UAX #50's `Vertical_Orientation`, `upright` keeps every character
/// upright, and `sideways` rotates every character 90° clockwise.
pub use parley::TextOrientation;

impl TextLayout {
    /// Map a physical point, relative to the inline root's content box, into
    /// Parley's layout coordinates.
    ///
    /// Parley lays text out in the writing mode's own axes (inline axis =
    /// "width", block axis = "height"), which coincide with physical space only
    /// for `horizontal-tb`. Callers that paint a vertical inline root already
    /// transpose the result; hit testing has to transpose the point the other
    /// way, otherwise the along-line position is read off the wrong axis and
    /// selection/caret mapping ignores it entirely.
    pub fn layout_point(&self, x: f32, y: f32) -> (f32, f32) {
        match self.vertical_text {
            Some(vertical) => {
                // Physical inline axis is `y`; physical block axis is `x`
                // (mirrored for `vertical-rl`, whose block axis starts at the
                // right edge). `align-content` shifts the line boxes along the
                // block axis, which here is Parley's own y.
                let inline = y;
                let block = if vertical.block_axis_starts_at_right {
                    vertical.block_extent - x
                } else {
                    x
                };
                let inline = if vertical.inline_axis_needs_mirror {
                    vertical.inline_extent - inline
                } else {
                    inline
                };
                (inline, block - self.block_offset)
            }
            None => (x, y - self.block_offset),
        }
    }

    /// Find a note text position at a physical point relative to the inline
    /// root's content box.
    ///
    /// Inline notes and interline notes are separate Parley layouts rather
    /// than part of the main paragraph's text, so the ordinary inline-root
    /// hit test cannot see them. Their layout coordinates share the inline
    /// root's writing-mode axes.
    pub(crate) fn note_text_position_at_point(&self, x: f32, y: f32) -> Option<(NodeId, usize)> {
        let (layout_x, layout_y) = self.layout_point(x, y);
        let scale = self.layout.scale();
        // The inline boxes and note fragment geometry below are in Parley's
        // layout units, which carry the layout scale; `layout_point` returns
        // CSS pixels. Without the scaling a note's hit box sits at the wrong
        // place whenever the layout is not at 1:1 (every HiDPI window), and a
        // click on the note lands on the body text that happens to be there.
        let (note_x, note_y) = (layout_x * scale, layout_y * scale);

        if let Some(stream) = self.interline.as_ref()
            && let Some((cluster, side)) = Cluster::from_point_exact(
                &stream.layout,
                layout_x * scale,
                layout_y * scale,
            )
        {
            let range = cluster.text_range();
            if let Some(source) = stream.sources.iter().find(|source| {
                source.active
                    && source.text_range.start <= range.start
                    && range.end <= source.text_range.end
            }) {
                let offset = cluster_offset(&cluster, side);
                return Some((
                    source.node_id,
                    offset.saturating_sub(source.text_range.start),
                ));
            }
        }

        for line in self.layout.lines() {
            for item in line.items() {
                let PositionedLayoutItem::InlineBox(inline_box) = item else {
                    continue;
                };
                if inline_box.kind != parley::InlineBoxKind::CustomInFlowCentered {
                    continue;
                }
                let Some(Some(fragment)) = self.inline_note_fragments.get(inline_box.index)
                else {
                    continue;
                };
                if note_x < inline_box.x
                    || note_x > inline_box.x + inline_box.width
                    || note_y < inline_box.y
                    || note_y > inline_box.y + inline_box.height
                {
                    continue;
                }
                let Some(source) = self.inline_notes.get(fragment.source_index) else {
                    continue;
                };
                let Some(segment) = source.segments.get(fragment.segment_index) else {
                    continue;
                };
                let Some(source_line) = source.layout.get(segment.line_index) else {
                    continue;
                };
                if let Some(offset) = inline_note_offset(
                    source,
                    fragment,
                    &source_line,
                    &inline_box,
                    note_x,
                    note_y,
                ) {
                    return Some((source.node_id, offset));
                }
            }
        }

        None
    }

    pub fn new() -> Self {
        Default::default()
    }

    pub fn content_widths(&mut self) -> ContentWidths {
        *self
            .content_widths
            .get_or_insert_with(|| self.layout.calculate_content_widths())
    }
}

fn cluster_offset(cluster: &Cluster<'_, TextBrush>, side: ClusterSide) -> usize {
    let is_leading = side == ClusterSide::Left;
    if cluster.is_rtl() {
        if is_leading {
            cluster.text_range().end
        } else {
            cluster.text_range().start
        }
    } else if is_leading || cluster.is_line_break() == Some(BreakReason::Explicit) {
        cluster.text_range().start
    } else {
        cluster.text_range().end
    }
}

#[allow(clippy::too_many_arguments)]
fn inline_note_offset(
    source: &InlineNoteSource,
    fragment: &InlineNoteFragment,
    source_line: &parley::layout::Line<'_, TextBrush>,
    inline_box: &parley::layout::PositionedInlineBox,
    layout_x: f32,
    layout_y: f32,
) -> Option<usize> {
    let (range, shaped_clusters, row_advance, row_block_offset, row_height, hanging_advance) =
        if layout_y < inline_box.y + fragment.first_line_height {
            (
                &fragment.first_range,
                &fragment.first_shaped_clusters,
                fragment.first_advance,
                0.0,
                fragment.first_line_height,
                fragment.first_hanging_advance,
            )
        } else {
            (
                &fragment.second_range,
                &fragment.second_shaped_clusters,
                fragment.second_advance,
                fragment.first_line_height,
                fragment.second_line_height,
                fragment.second_hanging_advance,
            )
        };
    if layout_y < inline_box.y + row_block_offset
        || layout_y > inline_box.y + row_block_offset + row_height
    {
        return None;
    }

    let content_width = (inline_box.width - fragment.inline_start_edge - fragment.inline_end_edge)
        .max(0.0);
    let inline_origin = if source.layout.is_rtl() {
        inline_box.x + fragment.inline_end_edge + (content_width - row_advance).max(0.0)
            - hanging_advance
    } else {
        inline_box.x + fragment.inline_start_edge
    };
    let target = layout_x - inline_origin;
    let mut pen = 0.0;
    let mut last_end = range.start;

    for item in source_line.items() {
        let PositionedLayoutItem::GlyphRun(run) = item else {
            continue;
        };
        for cluster in run.visual_clusters() {
            let cluster_range = cluster.text_range();
            if cluster_range.start < range.start
                || cluster_range.end > range.end
                || !shaped_clusters.contains(&cluster.shaped_clusters_range().start)
            {
                continue;
            }
            let advance = cluster.advance();
            let cluster_end = pen + advance;
            last_end = cluster_range.end;
            if target <= cluster_end {
                let side = if target < pen + advance * 0.5 {
                    ClusterSide::Left
                } else {
                    ClusterSide::Right
                };
                return Some(cluster_offset(&cluster, side));
            }
            pen = cluster_end;
        }
    }

    Some(if target <= 0.0 { range.start } else { last_end })
}

impl std::fmt::Debug for TextLayout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TextLayout")
    }
}

// TODO: support keypress events
pub enum GeneratedTextInputEvent {
    Input,
    Select,
    PreEditChange,
    Submit,
}

pub struct TextInputData {
    /// A parley TextEditor instance
    pub editor: Box<parley::PlainEditor<TextBrush>>,
    /// Whether the input is a singleline or multiline input
    pub is_multiline: bool,
    /// The scroll offset of the text content within the input, in CSS (unscaled) pixels.
    ///
    /// For single-line inputs this is a horizontal offset; for multi-line inputs it is a
    /// vertical offset. It is kept up to date so that the caret remains visible within the
    /// input's content box.
    pub scroll_offset: f32,
}

// FIXME: Implement Clone for PlainEditor
impl Clone for TextInputData {
    fn clone(&self) -> Self {
        TextInputData::new(self.is_multiline)
    }
}

impl TextInputData {
    pub fn new(is_multiline: bool) -> Self {
        let editor = Box::new(parley::PlainEditor::new(16.0));
        Self {
            editor,
            is_multiline,
            scroll_offset: 0.0,
        }
    }

    pub fn set_text(
        &mut self,
        font_ctx: &mut FontContext,
        layout_ctx: &mut LayoutContext<TextBrush>,
        text: &str,
    ) {
        if self.editor.text() != text {
            self.editor.set_text(text);
            self.editor.driver(font_ctx, layout_ctx).refresh_layout();
        }
    }

    /// Recompute [`Self::scroll_offset`] so that the caret stays visible within the input's
    /// content box.
    ///
    /// `content_box_width` and `content_box_height` are the dimensions of the input's content
    /// box in CSS (unscaled) pixels.
    pub fn clamp_scroll_offset(&mut self, content_box_width: f32, content_box_height: f32) {
        let Some(layout) = self.editor.try_layout() else {
            return;
        };
        // Parley lays out at the editor's scale, so its geometry is in scaled (device) pixels.
        // We convert into CSS (unscaled) pixels to match `scroll_offset` and the content box.
        let scale = layout.scale();

        // The caret geometry relative to the start of the text content.
        let Some(caret) = self.editor.cursor_geometry(1.5) else {
            return;
        };

        // Caret bounds and content/viewport extents along the scrolling axis (CSS pixels).
        let (caret_start, caret_end, content, viewport) = if self.is_multiline {
            (
                caret.y0 as f32 / scale,
                caret.y1 as f32 / scale,
                layout.height() / scale,
                content_box_height,
            )
        } else {
            (
                caret.x0 as f32 / scale,
                caret.x1 as f32 / scale,
                layout.full_width() / scale,
                content_box_width,
            )
        };

        let mut offset = self.scroll_offset;

        // Scroll so that both edges of the caret are within the visible region.
        if caret_end > offset + viewport {
            offset = caret_end - viewport;
        }
        if caret_start < offset {
            offset = caret_start;
        }

        // Never scroll past the content, and never scroll into negative space. The content
        // extent includes the caret so that a caret at the very end remains fully visible
        // (its rendered width extends slightly past the text).
        let max_offset = (content.max(caret_end) - viewport).max(0.0);
        self.scroll_offset = offset.clamp(0.0, max_offset);
    }

    /// The maximum valid value of [`Self::scroll_offset`] (in CSS pixels) given the input's
    /// content box, i.e. the extent by which the text content overflows the content box along
    /// the input's scroll axis.
    ///
    /// `content_box_width` and `content_box_height` are the dimensions of the input's content
    /// box in CSS (unscaled) pixels.
    pub fn max_scroll_offset(&self, content_box_width: f32, content_box_height: f32) -> f32 {
        let Some(layout) = self.editor.try_layout() else {
            return 0.0;
        };
        let scale = layout.scale();
        let (content, viewport) = if self.is_multiline {
            (layout.height() / scale, content_box_height)
        } else {
            (layout.full_width() / scale, content_box_width)
        };
        (content - viewport).max(0.0)
    }

    /// Scroll the input's text content by `delta` CSS pixels along its scroll axis (horizontal
    /// for single-line inputs, vertical for multi-line inputs), clamping to the scrollable
    /// range.
    ///
    /// Returns the portion of `delta` that could not be consumed (because the input was already
    /// scrolled to its limit), so the caller can bubble it up to an ancestor scroller.
    pub fn scroll_by(
        &mut self,
        delta: f32,
        content_box_width: f32,
        content_box_height: f32,
    ) -> f32 {
        let max_offset = self.max_scroll_offset(content_box_width, content_box_height);
        if max_offset <= 0.0 {
            return delta;
        }

        // Match the sign convention used for block scrolling: a positive delta decreases the
        // scroll offset.
        let new_offset = (self.scroll_offset - delta).clamp(0.0, max_offset);
        let consumed = self.scroll_offset - new_offset;
        self.scroll_offset = new_offset;
        delta - consumed
    }

    pub(crate) fn apply_keypress_event(
        &mut self,
        font_ctx: &mut FontContext,
        layout_ctx: &mut LayoutContext<TextBrush>,
        shell_provider: &dyn ShellProvider,
        event: BlitzKeyEvent,
    ) -> Option<GeneratedTextInputEvent> {
        // Do nothing if it is a keyup event
        if !event.state.is_pressed() {
            return None;
        }

        let mods = event.modifiers;
        let shift = mods.contains(Modifiers::SHIFT);
        let action_mod = mods.contains(ACTION_MOD);

        let is_multiline = self.is_multiline;
        let editor = &mut self.editor;
        let mut driver = editor.driver(font_ctx, layout_ctx);
        match event.key {
            Key::Character(c) if action_mod && matches!(c.as_str(), "c" | "x" | "v") => {
                match c.to_lowercase().as_str() {
                    "c" => {
                        if let Some(text) = driver.editor.selected_text() {
                            let _ = shell_provider.set_clipboard_text(text.to_owned());
                        }
                    }
                    "x" => {
                        if let Some(text) = driver.editor.selected_text() {
                            let _ = shell_provider.set_clipboard_text(text.to_owned());
                            driver.delete_selection()
                        }
                    }
                    "v" => {
                        let text = shell_provider.get_clipboard_text().unwrap_or_default();
                        driver.insert_or_replace_selection(&text)
                    }
                    _ => unreachable!(),
                }

                return Some(GeneratedTextInputEvent::Input);
            }
            Key::Character(c) if action_mod && matches!(c.to_lowercase().as_str(), "a") => {
                if shift {
                    driver.collapse_selection()
                } else {
                    driver.select_all()
                }
                return Some(GeneratedTextInputEvent::Select);
            }
            Key::Named(NamedKey::ArrowLeft) => {
                if action_mod {
                    if shift {
                        driver.select_word_left()
                    } else {
                        driver.move_word_left()
                    }
                } else if shift {
                    driver.select_left()
                } else {
                    driver.move_left()
                }
                return Some(GeneratedTextInputEvent::Select);
            }
            Key::Named(NamedKey::ArrowRight) => {
                if action_mod {
                    if shift {
                        driver.select_word_right()
                    } else {
                        driver.move_word_right()
                    }
                } else if shift {
                    driver.select_right()
                } else {
                    driver.move_right()
                }
                return Some(GeneratedTextInputEvent::Select);
            }
            Key::Named(NamedKey::ArrowUp) => {
                if shift {
                    driver.select_up()
                } else {
                    driver.move_up()
                }
                return Some(GeneratedTextInputEvent::Select);
            }
            Key::Named(NamedKey::ArrowDown) => {
                if shift {
                    driver.select_down()
                } else {
                    driver.move_down()
                }
                return Some(GeneratedTextInputEvent::Select);
            }
            Key::Named(NamedKey::Home) => {
                if action_mod {
                    if shift {
                        driver.select_to_text_start()
                    } else {
                        driver.move_to_text_start()
                    }
                } else if shift {
                    driver.select_to_line_start()
                } else {
                    driver.move_to_line_start()
                }
                return Some(GeneratedTextInputEvent::Select);
            }
            Key::Named(NamedKey::End) => {
                if action_mod {
                    if shift {
                        driver.select_to_text_end()
                    } else {
                        driver.move_to_text_end()
                    }
                } else if shift {
                    driver.select_to_line_end()
                } else {
                    driver.move_to_line_end()
                }
                return Some(GeneratedTextInputEvent::Select);
            }
            Key::Named(NamedKey::Delete) => {
                if action_mod {
                    driver.delete_word()
                } else {
                    driver.delete()
                }
                return Some(GeneratedTextInputEvent::Input);
            }

            // On macOS this is handled by the apple standard keybindings
            #[cfg(not(target_os = "macos"))]
            Key::Named(NamedKey::Backspace) => {
                if action_mod {
                    driver.backdelete_word()
                } else {
                    driver.backdelete()
                }
                return Some(GeneratedTextInputEvent::Input);
            }

            Key::Character(c) if c == "\n" => {
                if is_multiline {
                    driver.insert_or_replace_selection("\n");
                    return Some(GeneratedTextInputEvent::Input);
                } else {
                    return Some(GeneratedTextInputEvent::Submit);
                }
            }
            Key::Named(NamedKey::Enter) => {
                if is_multiline {
                    driver.insert_or_replace_selection("\n");
                    return Some(GeneratedTextInputEvent::Input);
                } else {
                    return Some(GeneratedTextInputEvent::Submit);
                }
            }
            Key::Character(s)
                if !mods.contains(Modifiers::CONTROL) && !mods.contains(Modifiers::META) =>
            {
                driver.insert_or_replace_selection(&s);
                return Some(GeneratedTextInputEvent::Input);
            }
            _ => {}
        };

        None
    }

    pub(crate) fn apply_apple_standard_keybinding(
        &mut self,
        font_ctx: &mut FontContext,
        layout_ctx: &mut LayoutContext<TextBrush>,
        shell_provider: &dyn ShellProvider,
        command: &str,
    ) -> Option<GeneratedTextInputEvent> {
        let editor = &mut self.editor;
        let mut driver = editor.driver(font_ctx, layout_ctx);
        let is_multiline = self.is_multiline;

        match command {
            // Inserting Content

            // Inserts a backtab character.
            "insertBacktab:" => {}
            // Inserts a container break, such as a new page break.
            "insertContainerBreak:" => {}
            // Inserts a double quotation mark without substituting a curly quotation mark.
            "insertDoubleQuoteIgnoringSubstitution:" => {
                driver.insert_or_replace_selection("\"");
                return Some(GeneratedTextInputEvent::Input);
            }
            // Inserts a line break character.
            "insertLineBreak:" => {
                driver.insert_or_replace_selection("\n");
                return Some(GeneratedTextInputEvent::Input);
            }
            // Inserts a newline character.
            "insertNewline:" => {
                if is_multiline {
                    driver.insert_or_replace_selection("\n");
                    return Some(GeneratedTextInputEvent::Input);
                } else {
                    return Some(GeneratedTextInputEvent::Submit);
                }
            }
            // Inserts a newline character without invoking the field editor’s normal handling to end editing.
            "insertNewlineIgnoringFieldEditor:" => {
                driver.insert_or_replace_selection("\n");
                return Some(GeneratedTextInputEvent::Input);
            }
            // Inserts a paragraph separator.
            "insertParagraphSeparator:" => {
                driver.insert_or_replace_selection("\n");
                return Some(GeneratedTextInputEvent::Input);
            }
            "insertSingleQuoteIgnoringSubstitution:" => {
                driver.insert_or_replace_selection("'");
                return Some(GeneratedTextInputEvent::Input);
            }
            // Inserts a tab character.
            "insertTab:" | "insertTabIgnoringFieldEditor:" => {
                // Ignore for now seeing as parley has poor support for laying out tabs
            }
            // Inserts the text you specify.
            "insertText:" => {}

            // Deleting Content

            // Deletes content moving backward from the current insertion point.
            // TODO: handle deleteBackwardByDecomposingPreviousCharacter separately
            "deleteBackward:" | "deleteBackwardByDecomposingPreviousCharacter:" => {
                driver.backdelete();
                return Some(GeneratedTextInputEvent::Input);
            }
            "deleteForward:" => {
                driver.delete();
                return Some(GeneratedTextInputEvent::Input);
            }
            // Deletes content from the insertion point to the beginning of the current line.
            "deleteToBeginningOfLine:" => {
                if driver.editor.raw_selection().is_collapsed() {
                    driver.select_to_line_start();
                }
                driver.delete_selection();
                return Some(GeneratedTextInputEvent::Input);
            }
            // Deletes content from the insertion point to the beginning of the current paragraph.
            "deleteToEndOfLine:" => {
                if driver.editor.raw_selection().is_collapsed() {
                    driver.select_to_line_end();
                }
                driver.delete_selection();
                return Some(GeneratedTextInputEvent::Input);
            }
            "deleteToBeginningOfParagraph:" => {
                if driver.editor.raw_selection().is_collapsed() {
                    driver.select_to_hard_line_start();
                }
                driver.delete_selection();
                return Some(GeneratedTextInputEvent::Input);
            }

            // Deletes content from the insertion point to the end of the current line.
            "deleteToEndOfParagraph:" => {
                if driver.editor.raw_selection().is_collapsed() {
                    driver.select_to_hard_line_end();
                }
                driver.delete_selection();
                return Some(GeneratedTextInputEvent::Input);
            }
            // Deletes content from the insertion point to the end of the current paragraph.
            "deleteWordBackward:" => {
                driver.backdelete_word();
                return Some(GeneratedTextInputEvent::Input);
            }
            // Deletes the word preceding the current insertion point.
            "deleteWordForward:" => {
                driver.delete_word();
                return Some(GeneratedTextInputEvent::Input);
            }
            // Deletes the current selection, placing it in a temporary buffer, such as the Clipboard.
            "yank:" => {
                if let Some(text) = driver.editor.selected_text() {
                    let _ = shell_provider.set_clipboard_text(text.to_owned());
                    driver.delete_selection();
                    return Some(GeneratedTextInputEvent::Input);
                }
            }

            // Moving the Insertion Pointer

            // Moves the insertion pointer backward in the current content.
            "moveBackward:" => {
                driver.move_left(); // TODO: Bidi-aware
                return Some(GeneratedTextInputEvent::Select);
            }

            // Moves the insertion pointer down in the current content.
            "moveDown:" => {
                driver.move_down();
                return Some(GeneratedTextInputEvent::Select);
            }
            // Moves the insertion pointer forward in the current content.
            "moveForward:" => {
                driver.move_right();
                return Some(GeneratedTextInputEvent::Select);
            } // TODO: Bidi-aware

            // Moves the insertion pointer left in the current content.
            "moveLeft:" => {
                driver.move_left();
                return Some(GeneratedTextInputEvent::Select);
            }
            // Moves the insertion pointer right in the current content.
            "moveRight:" => {
                driver.move_right();
                return Some(GeneratedTextInputEvent::Select);
            }
            // Moves the insertion pointer up in the current content.
            "moveUp:" => {
                driver.move_up();
                return Some(GeneratedTextInputEvent::Select);
            }

            // Modifying the Selection

            // Extends the selection to include the content before the current selection.
            "moveBackwardAndModifySelection:" => {
                driver.select_left(); // TODO: Bidi-aware
                return Some(GeneratedTextInputEvent::Select);
            }
            // Extends the selection to include the content below the current selection.
            "moveDownAndModifySelection:" => {
                driver.select_down();
                return Some(GeneratedTextInputEvent::Select);
            }
            // Extends the selection to include the content after the current selection.
            "moveForwardAndModifySelection:" => {
                driver.select_right(); // TODO: Bidi-aware
                return Some(GeneratedTextInputEvent::Select);
            }
            // Extends the selection to include the content to the left of the current selection.
            "moveLeftAndModifySelection:" => {
                driver.select_left();
                return Some(GeneratedTextInputEvent::Select);
            }
            // Extends the selection to include the content to the right of the current selection.
            "moveRightAndModifySelection:" => {
                driver.select_right();
                return Some(GeneratedTextInputEvent::Select);
            }
            // Extends the selection to include the content above the current selection.
            "moveUpAndModifySelection:" => {
                driver.select_up();
                return Some(GeneratedTextInputEvent::Select);
            }

            // Changing the Selection
            "selectAll:" => {
                driver.select_all();
                return Some(GeneratedTextInputEvent::Select);
            }
            "selectLine:" => {
                driver.move_to_line_start();
                driver.select_to_line_end();
                return Some(GeneratedTextInputEvent::Select);
            }
            "selectParagraph:" => {
                driver.move_to_hard_line_start();
                driver.select_to_hard_line_end();
                return Some(GeneratedTextInputEvent::Select);
            }
            "selectWord:" => {
                // TODO
            }

            // Moving the Selection in Documents
            "moveToBeginningOfDocument:" => {
                driver.move_to_text_start();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToBeginningOfDocumentAndModifySelection:" => {
                driver.select_to_text_start();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToEndOfDocument:" => {
                driver.move_to_text_end();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToEndOfDocumentAndModifySelection:" => {
                driver.move_to_text_end();
                return Some(GeneratedTextInputEvent::Select);
            }

            // Moving the Selection in Paragraphs
            "moveParagraphBackwardAndModifySelection:" => {}
            "moveParagraphForwardAndModifySelection:" => {}
            "moveToBeginningOfParagraph:" => {
                driver.move_to_hard_line_start();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToBeginningOfParagraphAndModifySelection:" => {
                driver.select_to_hard_line_start();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToEndOfParagraph:" => {
                driver.move_to_hard_line_end();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToEndOfParagraphAndModifySelection:" => {
                driver.select_to_hard_line_end();
                return Some(GeneratedTextInputEvent::Select);
            }

            // Moving the Selection in Lines of Text
            "moveToBeginningOfLine:" => {
                driver.move_to_line_start();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToBeginningOfLineAndModifySelection:" => {
                driver.select_to_line_start();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToEndOfLine:" => {
                driver.move_to_line_end();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToEndOfLineAndModifySelection:" => {
                driver.select_to_line_end();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToLeftEndOfLine:" => {
                driver.move_to_text_start();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToLeftEndOfLineAndModifySelection:" => {
                driver.select_to_line_start();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToRightEndOfLine:" => {
                driver.move_to_line_end();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToRightEndOfLineAndModifySelection:" => {
                driver.select_to_line_end();
                return Some(GeneratedTextInputEvent::Select);
            }

            // Moving the Selection by Word Boundaries
            "moveWordBackward:" => {
                driver.move_word_left();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveWordBackwardAndModifySelection:" => {
                driver.select_word_left();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveWordForward:" => {
                driver.move_word_right();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveWordForwardAndModifySelection:" => {
                driver.select_word_right();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveWordLeft:" => {
                driver.move_word_left();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveWordLeftAndModifySelection:" => {
                driver.select_word_left();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveWordRight:" => {
                driver.move_word_right();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveWordRightAndModifySelection:" => {
                driver.select_word_right();
                return Some(GeneratedTextInputEvent::Select);
            }

            // Scrolling Content

            // Scrolls the content down by a page.
            "scrollPageDown:" => {}
            // Scrolls the content up by a page.
            "scrollPageUp:" => {}
            // Scrolls the content down by a line.
            "scrollLineDown:" => {}
            // Scrolls the content up by a line.
            "scrollLineUp:" => {}
            // Scrolls the content to the beginning of the document.
            "scrollToBeginningOfDocument:" => {}
            // Scrolls the content to the end of the document.
            "scrollToEndOfDocument:" => {}
            // Moves the visible content region down by a page.
            "pageDown:" => {}
            // Moves the visible content region up by a page.
            "pageUp:" => {}
            // Moves the visible content region down by a page, and extends the current selection.
            "pageDownAndModifySelection:" => {}
            // Moves the visible content region up by a page, and extends the current selection.
            "pageUpAndModifySelection:" => {}
            // Moves the visible content region so the current selection is visually centered.
            "centerSelectionInVisibleArea:" => {}

            // Transposing Elements

            // Transposes the content around the current selection.
            "transpose:" => {}
            // Transposes the words around the current selection.
            "transposeWords:" => {}

            // Indenting Content
            // Indents the content at the current selection.
            "indent:" => {}

            // Canceling Operations
            // Cancels the current operation.
            "cancelOperation:" => {}

            // Supporting QuickLook
            // Invokes QuickLook to preview the current selection.
            "quickLookPreviewItems:" => {}

            // Supporting Writing Directions
            "makeBaseWritingDirectionLeftToRight:" => {}
            "makeBaseWritingDirectionNatural:" => {}
            "makeBaseWritingDirectionRightToLeft:" => {}
            "makeTextWritingDirectionLeftToRight:" => {}
            "makeTextWritingDirectionNatural:" => {}
            "makeTextWritingDirectionRightToLeft:" => {}

            // Changing Capitalization
            "capitalizeWord:" => {}
            "changeCaseOfLetter:" => {}
            "lowercaseWord:" => {}
            "uppercaseWord:" => {}

            // Supporting Marked Selections
            "setMark:" => {}
            "selectToMark:" => {}
            "deleteToMark:" => {}
            "swapWithMark:" => {}

            // Supporting Autocomplete
            "complete:" => {}

            // Instance Methods
            "showContextMenuForSelection:" => {}

            // Unknown command
            _ => {}
        };

        None
    }

    pub(crate) fn apply_ime_event(
        &mut self,
        font_ctx: &mut FontContext,
        layout_ctx: &mut LayoutContext<TextBrush>,
        event: BlitzImeEvent,
    ) -> Option<GeneratedTextInputEvent> {
        let editor = &mut self.editor;
        let mut driver = editor.driver(font_ctx, layout_ctx);

        match event {
            BlitzImeEvent::Enabled => {
                // Do nothing
                None
            }
            BlitzImeEvent::Disabled => {
                driver.clear_compose();
                Some(GeneratedTextInputEvent::PreEditChange)
            }
            BlitzImeEvent::Commit(text) => {
                driver.insert_or_replace_selection(&text);
                Some(GeneratedTextInputEvent::Input)
            }
            BlitzImeEvent::Preedit(text, cursor) => {
                if text.is_empty() {
                    driver.clear_compose();
                } else {
                    driver.set_compose(&text, cursor);
                }
                Some(GeneratedTextInputEvent::PreEditChange)
            }
            BlitzImeEvent::DeleteSurrounding {
                before_bytes,
                after_bytes,
            } => {
                let _ = before_bytes;
                let _ = after_bytes;
                // TODO
                None
            }
        }
    }
}
