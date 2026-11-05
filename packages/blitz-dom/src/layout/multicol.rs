//! Multi-column layout: a container that fragments its content into columns.
//!
//! The columns are one fragmentainer sequence, so the container's flow is laid out twice over.
//! First it is laid out as a *single* column one column-width wide, which is the only width its
//! text and blocks can be measured at — a paragraph wrapped at the container's full width would
//! not fit the column it is later cut into. Then the flow is cut at legal break points, and
//! [`fragmentation`](super::fragmentation) places each slice beside the last.
//!
//! How tall a column has to be is what the container's box reports to its parent, so the
//! balancing happens here, during layout: `column-fill: balance` asks for the shortest column
//! that still holds the content in the number of columns asked for, and that height is the
//! container's used height.

use taffy::{
    AvailableSpace, BlockContext, CoreStyle as _, LayoutInput, LayoutOutput, NodeId, Rect,
    ResolveOrZero as _, Size, compute_block_layout,
};
use thin_vec::ThinVec;

use style::values::computed::length::{
    NonNegativeLengthOrAuto, NonNegativeLengthPercentageOrNormal,
};
use style::values::computed::{ColumnCount, Float, LengthPercentage, Overflow};
use style::properties::ComputedValues;
use style::values::specified::box_::{DisplayInside, DisplayOutside};

use super::LayoutPassState;
use super::fragmentation::{
    Band, BreakPlan, CloneSpan, Edge, FlowAxis, Rank, Unit, cut_bands, edges_of, is_multicol,
};
use super::resolve_calc_value;
use crate::{dom_node_id, taffy_node_id};

/// The column parameters of a multi-column container, in CSS pixels.
struct Columns {
    /// How many columns the container asks for; `0` when the content is to decide.
    count: usize,
    /// The width of one column's content box.
    width: f32,
    /// The space between two columns.
    gap: f32,
    /// Whether the columns are filled in order rather than balanced.
    sequential: bool,
}

impl Columns {
    /// `column-count`, `column-width`, `column-gap` and `column-fill` as the multicol sizing
    /// algorithm needs them (CSS Multi-column 3 §5.1).
    fn measure(styles: &ComputedValues, content_width: f32, basis: f32) -> Self {
        let count = match styles.clone_column_count() {
            ColumnCount::Integer(number) => number.0.max(1) as usize,
            ColumnCount::Auto => 0,
        };
        let wanted = match styles.clone_column_width() {
            NonNegativeLengthOrAuto::LengthPercentage(length) => Some(length.0.px()),
            NonNegativeLengthOrAuto::Auto => None,
        };
        let gap = match styles.clone_column_gap() {
            NonNegativeLengthPercentageOrNormal::LengthPercentage(value) => {
                resolve(&value.0, basis).unwrap_or(0.0)
            }
            // `normal` is one em of the container's own font.
            NonNegativeLengthPercentageOrNormal::Normal => styles.clone_font_size().computed_size.px(),
        };
        use style::properties::generated::longhands::column_fill::computed_value::T as ColumnFill;
        let sequential = matches!(styles.clone_column_fill(), ColumnFill::Auto);

        let (count, width) = match (count, wanted) {
            // Both given: the number of columns is at least the requested one, and each column
            // is no narrower than the requested width.
            (count, Some(wanted)) => {
                let mut columns = ((content_width + gap) / (wanted + gap)).floor() as usize;
                columns = columns.max(count).max(1);
                let mut width = (content_width - gap * (columns as f32 - 1.0)) / columns as f32;
                if width < wanted && columns > 1 {
                    columns -= 1;
                    width = (content_width - gap * (columns as f32 - 1.0)) / columns as f32;
                }
                (columns, width.max(1.0))
            }
            (count, None) => {
                let columns = count.max(1);
                (
                    columns,
                    ((content_width - gap * (columns as f32 - 1.0)) / columns as f32).max(1.0),
                )
            }
        };

        Self {
            count,
            width,
            gap,
            sequential,
        }
    }
}

/// A computed length in CSS pixels; a percentage resolves against `basis`.
fn resolve(value: &LengthPercentage, basis: f32) -> Option<f32> {
    use style::values::computed::Au;
    Some(value.to_used_value(Au::from_f32_px(basis)).to_f32_px())
}

impl LayoutPassState<'_> {
    /// Lay out a multi-column container; see the module docs.
    ///
    /// `count_hint` is the number of columns a previous pass found its content filled. It is given
    /// only when the container is being measured again at the width those columns get; see the
    /// correction at the end of this function.
    pub(crate) fn compute_multicol_layout(
        &mut self,
        node_id: NodeId,
        inputs: LayoutInput,
        block_ctx: Option<&mut BlockContext<'_>>,
        count_hint: Option<usize>,
    ) -> LayoutOutput {
        let dom_id = dom_node_id(node_id);
        let parent_width = inputs.parent_size.width.unwrap_or(0.0).max(0.0);
        // Whether the container was measured under a width that is known. An intrinsic query — the
        // max-content or min-content measure a flex row, a float or a shrink-to-fit box asks with —
        // is not, and reading it as the parent's width collapses the box: a flex item with
        // `columns: 2` measured under a parent of nothing at all would ask its two columns to hold
        // the text in one pixel each and draw none of it (`baseline-000`).
        let definite_width = inputs.known_dimensions.width.or(match inputs.available_space.width {
            AvailableSpace::Definite(width) => Some(width),
            _ => None,
        });
        let available_width = definite_width.unwrap_or(parent_width);

        // What the container's own styles say, read before its content is laid out: a borrow of
        // the styles cannot outlive a pass that writes into the same tree. The insets are the
        // container's own box, which the single-column pass adds back on so that its children sit
        // the same distance from its border as they would in one column.
        let (columns, insets, content_width, definite, inline_root) = {
            let node = &self.nodes[dom_id];
            let Some(styles) = node.primary_styles() else {
                return LayoutOutput::HIDDEN;
            };
            if !is_multicol(&styles) {
                return LayoutOutput::HIDDEN;
            }
            let style = node.layout_style();
            let padding = style
                .padding()
                .resolve_or_zero(Some(parent_width), resolve_calc_value);
            let border = style
                .border()
                .resolve_or_zero(Some(parent_width), resolve_calc_value);
            let insets = Rect {
                left: padding.left + border.left,
                right: padding.right + border.right,
                top: padding.top + border.top,
                bottom: padding.bottom + border.bottom,
            };
            let content_width = (available_width - insets.left - insets.right).max(0.0);
            let columns = Columns::measure(&styles, content_width, parent_width);
            let definite = style_height(&styles, inputs);
            (columns, insets, content_width, definite, node.flags.is_inline_root())
        };

        // Pass one: this container's flow as a single column.
        let mut columns = columns;
        let mut content_width = content_width;
        if let Some(used) = count_hint.filter(|used| *used >= 1 && *used < columns.count) {
            // A definite width is shared out among the columns the content fills, not among the
            // columns the style names (`widows-orphans-002`: `columns: 4` across 792px whose two
            // paragraphs, kept two lines each by `widows: 2`, fill two columns of 396px, not four
            // of 198). CSS Multi-column 1 §3 balances the content into as few as it needs.
            columns.width = (content_width - columns.gap * (used as f32 - 1.0)) / used as f32;
            columns.count = used;
        }
        if definite_width.is_none() {
            // Measured intrinsically, the container is one column of that measure (CSS Multicol 1
            // §5): ask the flow what it needs at the incoming constraint, give every column that
            // width, and let the box be as wide as the columns it will ask for together with the
            // gaps between them.
            let probe = LayoutInput {
                known_dimensions: Size {
                    width: None,
                    height: definite,
                },
                available_space: Size {
                    width: inputs.available_space.width,
                    height: inputs.available_space.height,
                },
                ..inputs
            };
            let measured = if inline_root {
                self.compute_inline_layout(dom_id, probe, None)
            } else {
                compute_block_layout(self, node_id, probe, None)
            };
            let one = (measured.size.width - insets.left - insets.right).max(1.0);
            columns.width = one;
            content_width = one;
        }
        let narrow = LayoutInput {
            known_dimensions: Size {
                width: Some(columns.width + insets.left + insets.right),
                height: definite,
            },
            available_space: Size {
                width: AvailableSpace::Definite(columns.width + insets.left + insets.right),
                height: match definite {
                    Some(height) => AvailableSpace::Definite(height),
                    None => AvailableSpace::MaxContent,
                },
            },
            ..inputs
        };
        // A multi-column container starts a new block formatting context, so the parent's
        // collapsing margins do not reach into it: `None` is how the block algorithm is told so.
        let _ = block_ctx;
        let flow = if inline_root {
            self.compute_inline_layout(dom_id, narrow, None)
        } else {
            compute_block_layout(self, node_id, narrow, None)
        };

        // A box that spans every column is as wide as the container, not as wide as a column, so
        // the narrow pass has the wrong measure for it. It is measured again at the container's
        // own width, box by box: the flow keeps the positions the narrow pass gave it, which is
        // what the bands below cut at, and the spanner gets the width — and so the height — it is
        // painted with.
        let spanned = !inline_root && self.holds_spanner(dom_id);
        if spanned {
            // The container's own content height, which is the basis a spanner's percentage height
            // asks for. A height given as a percentage of an auto-height container stays indefinite.
            let container_height =
                definite.map(|height| (height - insets.top - insets.bottom).max(0.0));
            self.remeasure_spanners(dom_id, content_width, columns.width, container_height);
        }

        let flow_height = (flow.size.height - insets.top - insets.bottom).max(0.0);
        let (edges, units, spans, extent) = edges_of(&*self, dom_id, columns.width);
        // The pass knows where the flow ends: its own height. `extent` only sees the blocks of the
        // flow, and a column of text has none — its lines are the container's own.
        let flow_end = flow_height.max(extent);

        // How tall a column is: the container's own height when it has one, otherwise the
        // shortest height that still holds the flow in the columns asked for.
        //
        // Balancing measures the *flow*, which is `flow_end` and not the box the pass reported:
        // with a height on the container the box stops at that height while the flow runs on
        // below it.
        //
        // A definite height is a ceiling, not a floor: `column-fill: balance` still asks for the
        // shortest columns that hold the flow, and Chrome measures a 300px flow in a 200px-tall
        // two-column box as two columns of 150px — the container keeps its own 200px box. Filling
        // sequentially (`column-fill: auto`) instead fills the height the author gave.
        let (bands, height) = if spanned {
            let segments = self.segments(dom_id, content_width);
            self.spanner_bands(
                &segments,
                &edges,
                &units,
                &spans,
                &columns,
                insets,
                content_width,
                definite,
                flow_end,
            )
        } else {
            let height = match definite {
                Some(limit) if columns.sequential => limit,
                Some(limit) => limit.min(balance(&edges, &units, &spans, flow_end, columns.count)),
                None if columns.sequential => flow_height,
                None => balance(&edges, &units, &spans, flow_height, columns.count),
            };
            let bands: ThinVec<_> =
                cut_bands(&edges, &units, &spans, 0.0, height, columns.width, flow_end)
                    .into_iter()
                    .enumerate()
                    .map(|(index, mut band)| {
                        band.origin = taffy::Point {
                            x: insets.left + index as f32 * (columns.width + columns.gap),
                            y: insets.top,
                        };
                        // Chrome measures a column rule by the fragmentainer's own depth: a container
                        // that was given a height rules to that height even when balancing made the
                        // columns shorter (multicol-rule-nested-balancing-001).
                        band.depth = definite.unwrap_or(height);
                        band
                    })
                    .collect();
            (bands, height)
        };

        #[cfg(debug_assertions)]
        if std::env::var_os("CUOTUO_DEBUG_FRAGMENTATION").is_some() {
            eprintln!(
                "multicol: count {} width {:.1} gap {:.1} definite {:?} flow {flow_height:.1}..{flow_end:.1} edges {} sequential {} spanner {} height {height:.1}",
                columns.count,
                columns.width,
                columns.gap,
                definite,
                edges.len(),
                columns.sequential,
                spanned,
            );
            for (index, band) in bands.iter().enumerate() {
                eprintln!(
                    "  band {index}: {:.1}..{:.1} at ({:.1},{:.1}) cross {:.1}",
                    band.start, band.end, band.origin.x, band.origin.y, band.cross
                );
            }
            // Where the flow may be cut, and what cannot be cut at all: these two decide every
            // band above, so a column that holds the wrong box is explained here.
            let ranks = edges
                .iter()
                .map(|edge| format!("{:.1}:{}", edge.at, edge.rank_name()))
                .collect::<Vec<_>>()
                .join(" ");
            let whole = units
                .iter()
                .map(|unit| {
                    format!(
                        "{:.1}..{:.1}{}",
                        unit.start,
                        unit.end,
                        if unit.flex_item { " item" } else { "" }
                    )
                })
                .collect::<Vec<_>>()
                .join(" ");
            eprintln!("  edges: {ranks}");
            eprintln!("  units: {whole}");
        }

        // How many columns the flow actually filled. For a box measured for its own size that is the
        // number it reports as its width: `columns: 4` whose content cannot be cut into four without
        // breaking `widows` ends up two columns wide — `widows-orphans-002`, whose assert asks for the
        // 4 X's "arranged in a 2 by 2 square" and whose reference is half as wide as we were. A flow
        // cut by spanners has runs, not columns, in its bands, and a sequential fill can spill past
        // the count into rows below; neither is a column count, so both keep the author's.
        let used = if spanned {
            columns.count
        } else {
            bands.len().max(1).min(columns.count)
        };

        // A box of definite width that balanced into fewer columns than it named has to be measured
        // again, because the columns it now uses are wider and its content reflows in them: cutting
        // the first pass's flow into wider bands would leave it laid out for the narrow ones, which
        // is how `widows-orphans-002` came to draw two columns of 198px where its reference draws two
        // of 396. One correction pass is enough — a second asking for fewer still is a flow whose
        // height does not depend on the column width, and the hinted measure reports its own count.
        //
        // Only a horizontal flow is corrected: this module cuts bands along the flow's block axis
        // today, so in a vertical container the band count says nothing about how many columns the
        // content fills, and the correction collapsed `break-inside-avoid-min-block-size-2`'s three
        // columns into one. That is the axis work the module docs record as missing, not this rule.
        let vertical_flow = self.nodes[dom_id].primary_styles().is_some_and(|styles| {
            use style::values::computed::WritingModeProperty as Wm;
            matches!(
                styles.get_inherited_box().clone_writing_mode(),
                Wm::VerticalRl | Wm::VerticalLr | Wm::SidewaysRl | Wm::SidewaysLr
            )
        });
        // At least two columns must be involved: a flow cut into a single band is not evidence that
        // the content needs fewer columns than it names, since the single band is usually the floor
        // that keeps a column no shorter than the tallest thing that cannot be cut at all. What that
        // floor should be when `break-inside: avoid` merely asks is a separate question, and
        // `break-inside-avoid-min-block-size-1` (failing before this rule too) is on its far side.
        if count_hint.is_none()
            && definite_width.is_some()
            && !spanned
            && !columns.sequential
            && !vertical_flow
            && used >= 2
            && used < columns.count
        {
            return self.compute_multicol_layout(node_id, inputs, block_ctx, Some(used));
        }

        self.nodes[dom_id].break_plan = Some(Box::new(BreakPlan {
            axis: FlowAxis::Y,
            stride: columns.width + columns.gap,
            bands,
        }));

        // The container is as tall as one column, not as tall as the flow it was cut from —
        // unless the author gave it a height, which the pass already honoured. A flow cut by a
        // spanner is as tall as all of its runs together.
        LayoutOutput {
            size: Size {
                width: if definite_width.is_some() {
                    available_width
                } else {
                    // As wide as the columns its content filled, with the room between them.
                    let used = used as f32;
                    columns.width * used
                        + columns.gap * (used - 1.0).max(0.0)
                        + insets.left
                        + insets.right
                },
                height: definite.unwrap_or(height) + insets.top + insets.bottom,
            },
            ..flow
        }
    }

    /// The container's flow cut at the boxes that span every column.
    ///
    /// A spanner ends the columns before it and the content after it starts a new set below it
    /// (CSS Multi-column 3 §1.3), so the flow is a sequence of runs: a run of column content,
    /// balanced into the columns asked for, and a spanner, which is one fragmentainer as wide as
    /// the container. The runs stack, and the container is as tall as all of them.
    fn spanner_bands(
        &self,
        segments: &[Segment],
        edges: &[Edge],
        units: &[Unit],
        spans: &[CloneSpan],
        columns: &Columns,
        insets: Rect<f32>,
        content_width: f32,
        definite: Option<f32>,
        flow_end: f32,
    ) -> (ThinVec<Band>, f32) {
        let stride = columns.width + columns.gap;
        let mut runs: Vec<Segment> = segments.to_vec();
        let last = runs.last().map(|run| run.end).unwrap_or(0.0);
        // Content the children do not account for — the container's own lines, a box that
        // overflows its parent — is a run of column content of its own.
        if flow_end > last {
            runs.push(Segment {
                start: last,
                end: flow_end,
                spanned: false,
            });
        }

        let mut bands = ThinVec::new();
        let mut offset = insets.top;
        // Where the previous run left the flow. A spanner's own margins collapse with the boxes
        // around it, and the space they take belongs to neither run — it is the gap between them,
        // and the runs below have to be pushed down by it.
        let mut cursor = 0.0;
        for run in runs {
            if run.end <= run.start {
                continue;
            }
            offset += (run.start - cursor).max(0.0);
            cursor = run.end;
            if run.spanned {
                bands.push(Band {
                    start: run.start,
                    end: run.end,
                    origin: taffy::Point {
                        x: insets.left,
                        y: offset,
                    },
                    cross: content_width,
                    depth: run.end - run.start,
                });
                offset += run.end - run.start;
                continue;
            }
            let run_height = run.end - run.start;
            let inside: Vec<Edge> = edges
                .iter()
                .filter(|edge| edge.at > run.start && edge.at < run.end)
                .copied()
                .collect();
            let whole: Vec<Unit> = units
                .iter()
                .filter(|unit| unit.end > run.start && unit.start < run.end)
                .copied()
                .collect();
            let clones: Vec<CloneSpan> = spans
                .iter()
                .filter(|span| span.end > run.start && span.start < run.end)
                .copied()
                .collect();
            let height = match definite {
                Some(limit) if columns.sequential => limit,
                Some(limit) => {
                    limit.min(balance(&inside, &whole, &clones, run_height, columns.count))
                }
                None if columns.sequential => run_height,
                None => balance(&inside, &whole, &clones, run_height, columns.count),
            };
            for (index, mut band) in cut_bands(
                &inside,
                &whole,
                &clones,
                run.start,
                height,
                columns.width,
                run.end,
            )
            .into_iter()
            .enumerate()
            {
                band.origin = taffy::Point {
                    x: insets.left + index as f32 * stride,
                    y: offset,
                };
                band.depth = definite.unwrap_or(height);
                bands.push(band);
            }
            offset += height;
        }
        (bands, (offset - insets.top).max(0.0))
    }

    /// Whether one of the container's own children spans every column.
    fn holds_spanner(&self, container: crate::NodeId) -> bool {
        self.nodes[container]
            .children
            .iter()
            .any(|child| self.spans_flow(*child))
    }

    /// Does this box span every column — itself, or because the only thing inside it does?
    ///
    /// A spanner cannot be lifted out of a wrapper that holds content of its own, but a wrapper
    /// that holds nothing else is the box the columns break around, and it is as wide as the
    /// container because the box inside it is. A wrapper with text or another box beside the
    /// spanner wants the wrapper itself broken at the spanner, which is not done yet.
    fn spans_flow(&self, id: crate::NodeId) -> bool {
        let node = &self.nodes[id];
        if node.spans_columns() {
            return true;
        }
        // Only a box that takes part in its parent's block flow can be carried along: a float, an
        // atomic inline, a box that clips or scrolls its overflow, and a flex, grid, table or
        // multi-column container all start a formatting context of their own, and a spanner
        // inside one of those stays inside it (`multicol-span-all-004`).
        let in_flow = node.is_anonymous()
            || node.primary_styles().is_some_and(|styles| {
                let display = styles.clone_display();
                let overflow = &styles.get_box();
                display.outside() == DisplayOutside::Block
                    && matches!(
                        display.inside(),
                        DisplayInside::Flow | DisplayInside::FlowRoot
                    )
                    && overflow.overflow_x == Overflow::Visible
                    && overflow.overflow_y == Overflow::Visible
                    && styles.clone_float() == Float::None
                    // A transform or a filter makes the box a containing block and a stacking
                    // context of its own, and `contain: paint|layout|content|strict` gives it an
                    // independent formatting context; each of those is a boundary for a spanner
                    // (`multicol-span-all-010`).
                    && styles.get_box().transform.0.is_empty()
                    && styles.get_effects().filter.0.is_empty()
                    && !node
                        .layout_style()
                        .contain()
                        .establishes_independent_formatting_context()
                    && !is_multicol(&styles)
            });
        if !in_flow {
            return false;
        }
        let mut only = None;
        for child in node.children.iter() {
            let child = &self.nodes[*child];
            match &child.data {
                crate::node::NodeData::Text(_) => {
                    if child.text_content().trim().is_empty() {
                        continue;
                    }
                    return false;
                }
                crate::node::NodeData::Element(_) | crate::node::NodeData::AnonymousBlock(_) => {}
                // A comment is not content: it does not stop the box beside it from spanning.
                _ => continue,
            }
            if only.is_some() {
                return false;
            }
            only = Some(child.id);
        }
        only.is_some_and(|child| self.spans_flow(child))
    }

    /// Measure the boxes that span every column at the container's own width.
    ///
    /// The narrow pass gave a spanner a column's width, and a spanner is as wide as the container.
    /// Measuring one box on its own leaves the flow the narrow pass built untouched — the
    /// positions everything else is cut at are still its own — while the spanner gets the width,
    /// and so the height, it is painted with.
    ///
    /// The width has to be asked for in the spanner's own terms, because stretching is what a
    /// *parent* does to a child: a box whose width is `auto` is given the container's content
    /// width less its own margins, while a table chooses its own width from the room it is given
    /// and is only told how much room that is.
    /// `container_height` is the content height of the multi-column container, which is what a
    /// spanner resolves a percentage height against — its containing block is the container, not a
    /// column, and the spanner fills every column.
    fn remeasure_spanners(
        &mut self,
        container: crate::NodeId,
        content_width: f32,
        column_width: f32,
        container_height: Option<f32>,
    ) {
        let ids: Vec<crate::NodeId> = {
            let children = self.nodes[container].layout_children.borrow();
            children
                .as_ref()
                .map(|children| children.to_vec())
                .unwrap_or_default()
        };
        for id in ids {
            if !self.spans_flow(id) {
                continue;
            }
            let location = self.nodes[id].unrounded_layout().location;
            let narrow_width = self.nodes[id].unrounded_layout().size.width;
            // Everything the measure needs is read before it runs: a borrow of the tree cannot
            // outlive a pass that writes into it.
            let width = {
                let style = self.nodes[id].layout_style();
                let margin = style.margin().resolve_or_zero(Some(content_width), resolve_calc_value);
                let padding =
                    style.padding().resolve_or_zero(Some(content_width), resolve_calc_value);
                let border = style.border().resolve_or_zero(Some(content_width), resolve_calc_value);
                let insets = margin.left
                    + margin.right
                    + padding.left
                    + padding.right
                    + border.left
                    + border.right;
                // A box that filled the column filled it because its parent stretched it, and the
                // parent is not here to do it again. A table was never stretched: it chooses its
                // own width from the room it is given.
                let stretched = (narrow_width + insets - column_width).abs() < 0.5
                    && !self.nodes[id].flags.is_table_root();
                let authored = match self.nodes[id].primary_styles().map(|styles| styles.clone_width())
                {
                    Some(style::values::computed::Size::LengthPercentage(value)) => {
                        resolve(&value.0, content_width)
                    }
                    _ => None,
                };
                authored.or(stretched.then_some((content_width - insets).max(0.0)))
            };
            let inputs = LayoutInput {
                run_mode: taffy::RunMode::PerformLayout,
                sizing_mode: taffy::tree::SizingMode::InherentSize,
                axis: taffy::tree::RequestedAxis::Both,
                known_dimensions: Size { width, height: None },
                known_dimensions_are_definite: Size { width: true, height: true },
                parent_size: Size {
                    width: Some(content_width),
                    // The spanner's containing block is the multi-column container, so a height it
                    // gives as a percentage asks for the container's content height — which the
                    // narrow pass had already resolved. Without it the measure comes back as content
                    // height and the re-measure silently shortens the spanner
                    // (`multicol-span-all-children-height-001`, whose 50% of 200px came out 18px).
                    height: container_height,
                },
                available_space: Size {
                    width: AvailableSpace::Definite(content_width),
                    height: AvailableSpace::MaxContent,
                },
                vertical_margins_are_collapsible: taffy::Line::TRUE,
            };
            let out = self.compute_child_layout_in_current_wm(taffy_node_id(id), inputs, None);
            // The measure is the spanner's own, but the box keeps the place the flow gave it:
            // everything after the spanner is cut at the positions the narrow pass produced.
            let layout = self.nodes[id].unrounded_layout_mut();
            layout.size = out.size;
            layout.location = location;
        }
    }

    /// The container's flow cut into runs at its spanning children.
    ///
    /// Positions are the children's outer boxes, so a margin belongs to the run it sits in, and
    /// two adjacent children that neither spans one run: they share the columns they are balanced
    /// into. `basis` is the width a percentage margin of a child resolves against.
    fn segments(&self, container: crate::NodeId, basis: f32) -> Vec<Segment> {
        let ids: Vec<crate::NodeId> = {
            let children = self.nodes[container].layout_children.borrow();
            children.as_ref().map(|children| children.to_vec()).unwrap_or_default()
        };
        let mut runs: Vec<Segment> = Vec::new();
        for id in ids {
            let node = &self.nodes[id];
            if node.is_out_of_flow() || node.taffy_display() == taffy::Display::None {
                continue;
            }
            let layout = node.unrounded_layout();
            let margin = node
                .layout_style()
                .margin()
                .resolve_or_zero(Some(basis), resolve_calc_value);
            // A run reaches from this child's outer top to its outer bottom. Where two children's
            // margins collapse, the second one's outer top is where the first one's outer bottom
            // was, so the runs do not overlap.
            let start = (layout.location.y - margin.top)
                .max(runs.last().map(|run| run.end).unwrap_or(0.0));
            let end = start.max(layout.location.y + layout.size.height + margin.bottom);
            let spanned = self.spans_flow(id);
            match runs.last_mut() {
                Some(run) if !spanned && !run.spanned => run.end = run.end.max(end),
                _ => runs.push(Segment {
                    start,
                    end: end.max(start),
                    spanned,
                }),
            }
        }
        runs
    }
}

/// One run of a flow that holds a spanning box: either content that fills the columns, or one box
/// that spans all of them.
#[derive(Clone, Copy)]
struct Segment {
    start: f32,
    end: f32,
    spanned: bool,
}

/// The height the author gave the container, in CSS pixels.
fn style_height(styles: &ComputedValues, inputs: LayoutInput) -> Option<f32> {
    let basis = inputs.parent_size.height.unwrap_or(0.0);
    let height = match styles.clone_height() {
        style::values::computed::Size::LengthPercentage(value) => resolve(&value.0, basis)?,
        _ => return None,
    };
    (height > 0.0).then_some(height)
}

/// The shortest column height, measured at a legal break point, that holds the flow in `count`
/// columns.
///
/// Walking the heights upwards is what makes the columns as even as the boxes allow: the first
/// one that fits is the balanced height. The arithmetic share is a candidate like any other, and
/// it wins when nothing else can fit — a box with no legal break inside it is cut rather than
/// left to overflow. Chrome measures a single 300px block in `columns: 3` as three columns of
/// 100px, and three blocks of 100px as two columns of 200px.
///
/// The share is a floor as well as a starting point: no column is shorter than the tallest thing
/// the flow cannot cut, which is what keeps a single line box from being balanced into slivers.
fn balance(edges: &[Edge], units: &[Unit], spans: &[CloneSpan], flow_height: f32, count: usize) -> f32 {
    let count = count.max(1);
    let tallest = units
        .iter()
        .map(Unit::height)
        .fold(0.0, f32::max)
        .min(flow_height);
    let shortest = (flow_height / count as f32).max(tallest);
    let mut candidates: Vec<f32> = edges.iter().map(|edge| edge.at).collect();
    candidates.push(shortest);
    candidates.sort_by(|a, b| a.total_cmp(b));
    for candidate in candidates {
        if candidate < shortest {
            continue;
        }
        let bands = cut_bands(edges, units, spans, 0.0, candidate, 0.0, flow_height);
        if bands.len() > count {
            continue;
        }
        // A height is only balanced if the flow could actually have been cut where it lands. A
        // fragment that ends on the column's edge at a point a `widows` or `orphans` count bars means
        // the height forced the very break the counts exist to prevent, so it is not a balanced
        // height and the next one up is (CSS Break 3 §3.3/§3.4 read with CSS Multi-column 1 §3).
        // Only the *count-barred* points are refused: a fragment ending where an unbreakable box
        // bars the cut has nowhere else to go, and round 24's blunter version, which refused both,
        // cost nine `css-multicol` pairs for that. The last band is the end of the flow, not a cut.
        let forced_over_a_count = bands[..bands.len() - 1].iter().any(|band| {
            edges.iter().any(|edge| {
                edge.by_counts
                    && edge.rank == Rank::Forbidden
                    && (edge.at - band.end).abs() < 0.5
            })
        });
        if forced_over_a_count {
            continue;
        }
        return candidate;
    }
    flow_height.max(shortest)
}
