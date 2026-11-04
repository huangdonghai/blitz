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
use style::values::computed::{ColumnCount, LengthPercentage};
use style::properties::ComputedValues;

use super::LayoutPassState;
use super::fragmentation::{
    Band, BreakPlan, Edge, FlowAxis, Unit, cut_bands, edges_of, is_multicol,
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
    pub(crate) fn compute_multicol_layout(
        &mut self,
        node_id: NodeId,
        inputs: LayoutInput,
        block_ctx: Option<&mut BlockContext<'_>>,
    ) -> LayoutOutput {
        let dom_id = dom_node_id(node_id);
        let parent_width = inputs.parent_size.width.unwrap_or(0.0).max(0.0);
        let available_width = match inputs.known_dimensions.width {
            Some(width) => width,
            None => match inputs.available_space.width {
                AvailableSpace::Definite(width) => width,
                _ => parent_width,
            },
        };

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
            self.remeasure_spanners(dom_id, content_width, columns.width);
        }

        let flow_height = (flow.size.height - insets.top - insets.bottom).max(0.0);
        let (edges, units, extent) = edges_of(&*self, dom_id, columns.width);
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
                &columns,
                insets,
                content_width,
                definite,
                flow_end,
            )
        } else {
            let height = match definite {
                Some(limit) if columns.sequential => limit,
                Some(limit) => limit.min(balance(&edges, &units, flow_end, columns.count)),
                None if columns.sequential => flow_height,
                None => balance(&edges, &units, flow_height, columns.count),
            };
            let bands: ThinVec<_> = cut_bands(&edges, 0.0, height, columns.width, flow_end)
                .into_iter()
                .enumerate()
                .map(|(index, mut band)| {
                    band.origin = taffy::Point {
                        x: insets.left + index as f32 * (columns.width + columns.gap),
                        y: insets.top,
                    };
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
                width: available_width,
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
        for run in runs {
            if run.end <= run.start {
                continue;
            }
            if run.spanned {
                bands.push(Band {
                    start: run.start,
                    end: run.end,
                    origin: taffy::Point {
                        x: insets.left,
                        y: offset,
                    },
                    cross: content_width,
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
            let height = match definite {
                Some(limit) if columns.sequential => limit,
                Some(limit) => limit.min(balance(&inside, &whole, run_height, columns.count)),
                None if columns.sequential => run_height,
                None => balance(&inside, &whole, run_height, columns.count),
            };
            for (index, mut band) in
                cut_bands(&inside, run.start, height, columns.width, run.end)
                    .into_iter()
                    .enumerate()
            {
                band.origin = taffy::Point {
                    x: insets.left + index as f32 * stride,
                    y: offset,
                };
                bands.push(band);
            }
            offset += height;
        }
        (bands, (offset - insets.top).max(0.0))
    }

    /// Whether one of the container's own children asks to span every column.
    ///
    /// Only a child of the container can span: the flow of a box nested deeper belongs to the box
    /// that holds it, and a `column-span` there does not reach out of it.
    fn holds_spanner(&self, container: crate::NodeId) -> bool {
        self.nodes[container]
            .children
            .iter()
            .any(|child| self.nodes[*child].spans_columns())
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
    fn remeasure_spanners(
        &mut self,
        container: crate::NodeId,
        content_width: f32,
        column_width: f32,
    ) {
        let ids: Vec<crate::NodeId> = {
            let children = self.nodes[container].layout_children.borrow();
            children
                .as_ref()
                .map(|children| children.to_vec())
                .unwrap_or_default()
        };
        for id in ids {
            if !self.nodes[id].spans_columns() {
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
                    height: None,
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
            let end = layout.location.y + layout.size.height + margin.bottom;
            let start = (layout.location.y - margin.top).max(runs.last().map(|run| run.end).unwrap_or(0.0));
            let spanned = node.spans_columns();
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
fn balance(edges: &[Edge], units: &[Unit], flow_height: f32, count: usize) -> f32 {
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
        if cut_bands(edges, 0.0, candidate, 0.0, flow_height).len() <= count {
            return candidate;
        }
    }
    flow_height.max(shortest)
}
