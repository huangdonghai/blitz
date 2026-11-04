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
    BreakPlan, Edge, FlowAxis, cut_bands, edges_of, is_multicol,
};
use super::resolve_calc_value;
use crate::dom_node_id;

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
        let (columns, insets, definite, inline_root) = {
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
            (columns, insets, definite, node.flags.is_inline_root())
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

        let flow_height = (flow.size.height - insets.top - insets.bottom).max(0.0);
        let (edges, extent) = edges_of(&*self, dom_id, columns.width);
        // The narrow pass knows where the flow ends: its own height. `extent` only sees the
        // blocks of the flow, and a column of text has none — its lines are the container's own.
        let flow_end = flow_height.max(extent);

        // How tall a column is: the container's own height when it has one, otherwise the
        // shortest height that still holds the flow in the columns asked for.
        let height = match definite {
            Some(height) => height,
            None if columns.sequential => flow_height,
            None => balance(&edges, flow_height, columns.count),
        };

        #[cfg(debug_assertions)]
        if std::env::var_os("CUOTUO_DEBUG_FRAGMENTATION").is_some() {
            eprintln!(
                "multicol: count {} width {:.1} gap {:.1} definite {:?} flow {flow_height:.1}..{flow_end:.1} edges {} sequential {} height {height:.1}",
                columns.count,
                columns.width,
                columns.gap,
                definite,
                edges.len(),
                columns.sequential,
            );
        }

        let bands: ThinVec<_> = cut_bands(&edges, height, flow_end)
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

        self.nodes[dom_id].break_plan = Some(Box::new(BreakPlan {
            axis: FlowAxis::Y,
            fragment: Size {
                width: columns.width,
                height,
            },
            stride: columns.width + columns.gap,
            bands,
        }));

        // The container is as tall as one column, not as tall as the flow it was cut from —
        // unless the author gave it a height, which the narrow pass already honoured.
        LayoutOutput {
            size: Size {
                width: available_width,
                height: match definite {
                    Some(height) => height + insets.top + insets.bottom,
                    None => height + insets.top + insets.bottom,
                },
            },
            ..flow
        }
    }
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
/// Walking the break points upwards is enough: a column never shrinks below the tallest box it
/// has to hold whole, so the first candidate that fits the content is the balanced height.
fn balance(edges: &[Edge], flow_height: f32, count: usize) -> f32 {
    let count = count.max(1);
    let shortest = flow_height / count as f32;
    let mut candidates: Vec<f32> = edges.iter().map(|edge| edge.at).collect();
    candidates.push(flow_height);
    candidates.sort_by(|a, b| a.total_cmp(b));
    for candidate in candidates {
        if candidate < shortest {
            continue;
        }
        if cut_bands(edges, candidate, flow_height).len() <= count {
            return candidate;
        }
    }
    flow_height.max(shortest)
}
