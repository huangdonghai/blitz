//! CSS Fragmentation: where the laid-out flow is cut, and where the pieces land.
//!
//! Blitz lays a document out once, as one continuous flow, and paints the part of it the
//! viewport shows. Paged media wants that flow cut at legal break points with each slice placed
//! in its own page box. The cutting happens here, after layout, and without a second layout
//! pass: a [`Band`] names one slice of the flow — in the flow's own coordinates — and where the
//! slice's starting line sits on the canvas. Paint translates the slice and clips it to its
//! fragmentainer, and that clip is what `box-decoration-break: slice` already asks of a box
//! broken across two fragments.
//!
//! Only a horizontal flow fragments today. A `vertical-rl` document paginates along x, and the
//! bands of such a flow are not computed yet, so it keeps its single continuous page; the axis
//! is a field of [`BreakPlan`] so that adding it changes no caller.
//!
//! A band's [`Band::end`] records where the break decision placed the cut; a fragmentainer is a
//! whole page box whatever it holds, so paint clips to [`BreakPlan::fragment_rect`].

use blitz_traits::node_id::NodeId;
use style::media_queries::MediaType;
use style::properties::ComputedValues;
use style::values::computed::length::NonNegativeLengthOrAuto;
use style::properties::PropertyDeclaration;
use style::values::computed::{BreakBetween, BreakWithin, ColumnCount, Float, Overflow, PageName};
use style::values::specified::box_::{DisplayInside, DisplayOutside};
use taffy::{CoreStyle as _, Point, Rect, ResolveOrZero as _, Size};
use thin_vec::ThinVec;

use super::resolve_calc_value;
use crate::layout::is_replaced_element;
use crate::{BaseDocument, Node};

/// The physical direction a fragmented flow advances along.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlowAxis {
    /// Down the page: the block axis of `horizontal-tb`.
    Y,
    /// Along x: the block axis of a vertical writing mode. Not produced yet.
    X,
}

/// What a position in the flow is worth as a place to cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rank {
    /// `break-before`/`break-after` demands a cut here. Nothing outvotes it, not an ancestor's
    /// `break-inside: avoid` (CSS Break 3 §3.2: a forced break cannot be prevented).
    Forced,
    /// A legal unforced break point (CSS Break 3 §4.1): between two blocks of the flow.
    Legal,
    /// `break-before`/`break-after: avoid` — the flow must not be cut here.
    Forbidden,
}

/// One box edge, measured along the flow from the fragmentainer's content-box origin.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Edge {
    pub(crate) at: f32,
    rank: Rank,
}

/// A stretch of the flow that no break may fall inside: a line box, an atomic inline box, a
/// replaced element. A column has to be tall enough to hold the tallest of them, which is what
/// stops balancing from cutting a single line into slivers.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Unit {
    pub(crate) start: f32,
    pub(crate) end: f32,
}

impl Unit {
    /// How tall the unit is along the flow.
    pub(crate) fn height(&self) -> f32 {
        (self.end - self.start).max(0.0)
    }
}

/// One fragmentainer: which slice of the flow it shows, and where that slice is placed.
#[derive(Clone, Copy, Debug)]
pub struct Band {
    /// Flow line the slice starts at.
    pub start: f32,
    /// Flow line the cut was placed at: the next break point, the end of the content, or the
    /// far edge of the fragmentainer when content overflowed it with no legal point to use.
    pub end: f32,
    /// Where [`Self::start`] sits on the canvas, relative to the fragmentainer node's own
    /// border box.
    pub origin: Point<f32>,
    /// How wide the fragmentainer is across the flow. A page is as wide as its box; a column is
    /// narrower than the container that holds it, and a box that spans every column is as wide
    /// as that container again — so this is not a property of the plan.
    pub cross: f32,
}

/// The cut plan of one fragmented flow.
#[derive(Clone, Debug)]
pub struct BreakPlan {
    pub axis: FlowAxis,
    /// Distance from one fragmentainer's origin to the next. A page box is more than the area it
    /// holds, because a page has a margin, so this measures the paper and not the area.
    pub stride: f32,
    pub bands: ThinVec<Band>,
}

impl BreakPlan {
    /// The canvas rectangle one band's content is clipped to: the fragmentainer's content box,
    /// cut short at the flow line where the next break was placed. Painting the whole page box
    /// would show the content that belongs to the following fragmentainer as well.
    pub fn fragment_rect(&self, band: &Band) -> Rect<f32> {
        let span = (band.end - band.start).max(0.0);
        match self.axis {
            FlowAxis::Y => Rect {
                left: band.origin.x,
                top: band.origin.y,
                right: band.origin.x + band.cross,
                bottom: band.origin.y + span,
            },
            FlowAxis::X => Rect {
                left: band.origin.x,
                top: band.origin.y,
                right: band.origin.x + span,
                bottom: band.origin.y + band.cross,
            },
        }
    }

    /// How far the fragmentainer's subtree shifts to draw one band.
    pub fn translate(&self, band: &Band) -> Point<f32> {
        match self.axis {
            FlowAxis::Y => Point {
                x: band.origin.x,
                y: band.origin.y - band.start,
            },
            FlowAxis::X => Point {
                x: band.origin.x - band.start,
                y: band.origin.y,
            },
        }
    }

    /// The far canvas line the plan paints to — the last fragmentainer's far edge — which is
    /// how a caller sizes the picture a paged document is drawn into.
    pub fn canvas_extent(&self) -> f32 {
        match self.bands.last() {
            Some(last) => match self.axis {
                FlowAxis::Y => last.origin.y + self.stride,
                FlowAxis::X => last.origin.x + self.stride,
            },
            None => 0.0,
        }
    }
}

impl BaseDocument {
    /// Read `@page` and remember the paper the document is printed on.
    ///
    /// This runs before layout: the page area is the root box's containing block in paged media,
    /// so a document that asks for 5in paper has to be laid out for 5in and not for the window.
    pub(crate) fn resolve_page_box(&mut self) {
        self.page_box = (*self.media_type() == MediaType::print()).then(|| page_box(self));
    }

    /// Compute the page bands of the root flow and hang them on the root element.
    ///
    /// Runs on every layout pass, and only under print: a screen document has no page sequence,
    /// and walking the whole flow costs a traversal nobody asked for while scrolling a page.
    pub(crate) fn resolve_fragmentation(&mut self) {
        let root_id = self.root_element().id;
        let plan = if self.page_box.is_some() {
            page_plan(self, root_id)
        } else {
            None
        };
        self.nodes[root_id].break_plan = plan.map(Box::new);
    }

    /// The size of the picture a paged document is drawn into: every page box it was cut into,
    /// stacked. `None` for a document that is not paginated.
    pub fn page_canvas(&self) -> Option<Size<f32>> {
        let page = self.page_box?;
        let plan = self.root_element().break_plan.as_deref()?;
        Some(Size {
            width: page.size.width,
            height: match plan.axis {
                FlowAxis::Y => plan.canvas_extent(),
                FlowAxis::X => page.size.height,
            },
        })
    }
}

/// The paper one page is printed on, and the margin that keeps content off its edge, in the CSS
/// pixels layout works in.
#[derive(Clone, Copy, Debug)]
pub struct PageBox {
    pub size: Size<f32>,
    /// The margins, as insets from the page box's edge.
    pub margin: Rect<f32>,
}

impl PageBox {
    /// The page area: what the root box is laid out inside (CSS Page 3 §5.2).
    pub fn area(&self) -> Size<f32> {
        Size {
            width: (self.size.width - self.margin.left - self.margin.right).max(1.0),
            height: (self.size.height - self.margin.top - self.margin.bottom).max(1.0),
        }
    }

    /// Where the page area of page `index` begins on the canvas.
    pub fn area_origin(&self, index: usize) -> Point<f32> {
        Point {
            x: self.margin.left,
            y: self.margin.top + index as f32 * self.size.height,
        }
    }
}

/// The window the document was given, in CSS pixels.
fn window(doc: &BaseDocument) -> Size<f32> {
    let size = doc.stylist.device().au_viewport_size();
    Size {
        width: size.width.to_f32_px(),
        height: size.height.to_f32_px(),
    }
}

/// `@page { size; margin }` as read from the document's stylesheets.
///
/// A document with no page rules prints on the window it was given, with no margin. `:first`,
/// `:left` and `:right` are not matched yet, so every rule that asks for the starting page's name
/// is applied; a named page's own `size` is not sought either, because the page a document starts
/// on is the only one whose box is resolved here.
fn page_box(doc: &BaseDocument) -> PageBox {
    let mut page = PageBox {
        size: window(doc),
        margin: Rect {
            left: 0.0,
            right: 0.0,
            top: 0.0,
            bottom: 0.0,
        },
    };
    let guard = doc.guard().read();
    // The unnamed `@page`, which is what a page with no name is printed on.
    let unnamed = style::Atom::default();
    for (data, _) in doc.stylist.iter_extra_data_origins() {
        let Some(rules) = data.pages.rules.get(&unnamed) else {
            continue;
        };
        for entry in rules.iter() {
            let rule = entry.rule.read_with(&guard);
            for declaration in rule.block.read_with(&guard).declarations() {
                match declaration {
                    PropertyDeclaration::Size(size) => apply_size(size, &mut page, doc),
                    // A shorthand reaches a declaration block as its four longhands.
                    PropertyDeclaration::MarginTop(value) => {
                        page.margin.top = margin_px(value, page.size.height);
                    }
                    PropertyDeclaration::MarginRight(value) => {
                        page.margin.right = margin_px(value, page.size.width);
                    }
                    PropertyDeclaration::MarginBottom(value) => {
                        page.margin.bottom = margin_px(value, page.size.height);
                    }
                    PropertyDeclaration::MarginLeft(value) => {
                        page.margin.left = margin_px(value, page.size.width);
                    }
                    _ => {}
                }
            }
        }
    }
    page
}

/// `@page { size: … }`: two edges, one edge with the other left to the paper the window gives,
/// an orientation, or a named sheet of paper.
fn apply_size(size: &style::values::specified::PageSize, page: &mut PageBox, doc: &BaseDocument) {
    use style::values::specified::page::PageSize as S;
    let window = window(doc);
    match size {
        S::Auto => {}
        S::Size(physical) => {
            // `size: 500px` gives one edge and leaves the other `auto`, which is the measure the
            // window supplies. An `auto` edge arrives as nothing, so a zero is read as `auto`.
            let width = positive(absolute(&physical.width.0)).unwrap_or(window.width);
            let height = positive(absolute(&physical.height.0)).unwrap_or(window.height);
            page.size = Size { width, height };
        }
        S::Orientation(orientation) => {
            use style::values::specified::page::PageSizeOrientation as O;
            let (short, long) = (
                page.size.width.min(page.size.height),
                page.size.width.max(page.size.height),
            );
            page.size = match orientation {
                O::Portrait => Size {
                    width: short,
                    height: long,
                },
                O::Landscape => Size {
                    width: long,
                    height: short,
                },
            };
        }
        S::PaperSize(paper, orientation) => {
            let short = positive(absolute(&paper.short_edge().0.into())).unwrap_or(window.width);
            let long = positive(absolute(&paper.long_edge().0.into())).unwrap_or(window.height);
            let (width, height) = match orientation {
                style::values::specified::page::PageSizeOrientation::Portrait => (short, long),
                _ => (long, short),
            };
            page.size = Size { width, height };
        }
    }
}

/// A page-rule length in CSS pixels. A percentage of a page resolves against the edge it
/// measures; a font- or viewport-relative length has no context to resolve against here, and
/// asks for nothing.
fn length(value: &style::values::specified::LengthPercentage, basis: f32) -> Option<f32> {
    use style::values::computed::Au;
    let computed = value.compute_without_context()?;
    Some(computed.to_used_value(Au::from_f32_px(basis)).to_f32_px())
}

/// A page-rule length that the spec allows to be an absolute length only: a paper size name or
/// an edge of `size`. A font- or viewport-relative length asks for nothing here.
fn absolute(value: &style::values::specified::Length) -> Option<f32> {
    value.to_computed_pixel_length_without_context().ok()
}

/// A measure that was actually given: `size` leaves an edge at nothing when it is `auto`.
fn positive(value: Option<f32>) -> Option<f32> {
    value.filter(|value| *value > 0.0)
}

/// A page-rule `margin`: `auto` on a page asks for nothing.
fn margin_px(value: &style::values::specified::Margin, basis: f32) -> f32 {
    match value {
        style::values::specified::Margin::LengthPercentage(value) => {
            length(value, basis).unwrap_or(0.0)
        }
        _ => 0.0,
    }
}

/// Does this box fragment its own content into columns? Not yet, so such a box is treated as
/// one unbreakable block.
pub(crate) fn is_multicol(style: &ComputedValues) -> bool {
    let column = style.get_column();
    !matches!(column.column_count, ColumnCount::Auto)
        || column.column_width != NonNegativeLengthOrAuto::auto()
}

/// The rank a `break-before`/`break-after` value gives the edge it applies to.
fn rank_between(break_value: BreakBetween) -> Option<Rank> {
    match break_value {
        BreakBetween::Always | BreakBetween::Page | BreakBetween::Left | BreakBetween::Right => {
            Some(Rank::Forced)
        }
        BreakBetween::Avoid => Some(Rank::Forbidden),
        BreakBetween::Auto => None,
    }
}

/// Does `break-inside` forbid cutting this box? `avoid-column` is ignored while columns are
/// unimplemented.
fn avoids_inside(break_value: BreakWithin) -> bool {
    matches!(
        break_value,
        BreakWithin::Avoid | BreakWithin::AvoidPage
    )
}

/// Walks a laid-out flow and records every position where it may, must, or must not be cut.
struct Planner<'a> {
    doc: &'a BaseDocument,
    /// Read the unrounded layout rather than the rounded one: a column asks for its flow's break
    /// points while layout is still running, before anything has been rounded.
    unrounded: bool,
    edges: Vec<Edge>,
    /// The stretches of this flow that cannot be cut (see [`Unit`]).
    units: Vec<Unit>,
    /// Farthest any box reaches along the flow.
    extent: f32,
    /// The page name of the content walked so far; a change forces a break (CSS Page 3 §6.5).
    page: PageName,
    /// Whether any content has been walked. The name a flow starts with forces no break: a
    /// forced break at the start of a fragmentainer asks for an empty one, and those are dropped.
    seen_content: bool,
}

impl<'a> Planner<'a> {
    /// Visit the layout children of `parent`.
    ///
    /// `offset` is `parent`'s border box in flow coordinates and `inline_size` is its content
    /// inline size, which is what a percentage margin of a child resolves against. `breakable`
    /// says whether a legal cut may be recorded here at all: `false` once the walk is inside a
    /// box that must not be cut or that fragments its own content. `inherited` is the page name
    /// the parent's content is printed on, which a child with `page: auto` keeps.
    fn walk(
        &mut self,
        parent: NodeId,
        offset: Point<f32>,
        inline_size: f32,
        breakable: bool,
        inherited: &PageName,
    ) {
        let doc = self.doc;
        let children = doc.nodes[parent].layout_children.borrow();
        let Some(children) = children.as_ref() else {
            return;
        };

        for &child_id in children.iter() {
            let child = &doc.nodes[child_id];
            // A text node carries no layout of its own: its lines live in the inline root that
            // holds it, which is why a cut cannot yet be placed between two of them.
            let Some(data) = child.element_data() else {
                continue;
            };
            let Some(styles) = child.primary_styles() else {
                continue;
            };
            if child.taffy_display() == taffy::Display::None || child.is_out_of_flow() {
                continue;
            }

            let layout = if self.unrounded {
                child.unrounded_layout()
            } else {
                child.final_layout()
            };
            let top = offset.y + layout.location.y;

            // A percentage margin resolves against the containing block's inline size; `auto`
            // resolves to nothing, which is what it means in a block flow.
            let margin = child
                .layout_style()
                .margin()
                .resolve_or_zero(Some(inline_size), resolve_calc_value);
            let outer_top = top - margin.top;
            let bottom = top + layout.size.height;
            let outer_bottom = bottom + margin.bottom;
            // How far the flow reaches is where its content ends, not where the last box's
            // margin does: a margin left at the bottom of a fragmentainer asks for no page.
            self.extent = self.extent.max(bottom);

            // Only a block-level box of the flow makes a break point. A float, an atomic inline
            // or a replaced element is cut wherever the page needs it, and no break point is
            // offered at its edges.
            let display = styles.clone_display();
            // An anonymous block is block-level by construction (it exists because a block
            // parent and an inline child needed one), whatever display value it was styled with.
            let in_flow_block = (display.outside() == DisplayOutside::Block || child.is_anonymous())
                && styles.clone_float() == Float::None
                && child.taffy_position() == taffy::Position::Static;

            // A named page breaks the flow where the *content* changes name (CSS Page 3 §6.5).
            // The name of the box that holds the text is what counts, so a chain of wrappers each
            // asking for a name breaks nothing when the content inside returns to the name the
            // flow already had — and a wrapper with no content of its own never breaks at all.
            let used = match styles.clone_page() {
                PageName::Auto => inherited.clone(),
                named => named,
            };
            // An atomic inline-level box (inline-block, inline-table, inline-flex, a replaced
            // element) is one unbreakable thing on a line: no break of any kind is placed at its
            // edges, and its own content takes no part in the page-name sequence. What it does
            // carry is the name of the last content inside it, which is what the flow continues
            // with after it.
            if !child.is_anonymous() && display.outside() == DisplayOutside::Inline {
                // The break is never *at* the box: it sits on a line, and lines are what the
                // flow breaks between. It only leaves its name behind for the next box to
                // compare with (page-name-inline-block-002 breaks after it, -003 not between two).
                self.units.push(Unit {
                    start: outer_top,
                    end: outer_bottom,
                });
                self.page = self.last_page_name(child_id, &used);
                self.seen_content = true;
                continue;
            }
            let replaced = is_replaced_element(&data.name.local);
            if replaced {
                self.units.push(Unit {
                    start: outer_top,
                    end: outer_bottom,
                });
            }
            // A block-level replaced element is content too, and it has none of its own.
            let holds_content =
                in_flow_block && (child.flags.is_inline_root() || replaced);
            let page_change = holds_content && self.seen_content && used != self.page;
            if holds_content {
                self.page = used.clone();
                self.seen_content = true;
            }

            let before = rank_between(styles.clone_break_before())
                .or_else(|| page_change.then_some(Rank::Forced))
                .or_else(|| in_flow_block.then_some(Rank::Legal));
            if let Some(rank) = before {
                self.push(outer_top, rank, breakable);
            }
            if let Some(rank) = rank_between(styles.clone_break_after()) {
                self.push(outer_bottom, rank, breakable);
            }

            // Descend only into a box that shares its parent's fragmentainer: a scroll container
            // scrolls instead of breaking, a flex, grid or table container and a multi-column
            // container fragment their content by their own rules, and an atomic box does not
            // break at all.
            let own_fragmentainer = !matches!(
                display.inside(),
                DisplayInside::Flow | DisplayInside::FlowRoot
            ) || styles.get_box().overflow_x != Overflow::Visible
                || styles.get_box().overflow_y != Overflow::Visible
                || is_multicol(&styles);
            let breakable_inside = breakable
                && !avoids_inside(styles.clone_break_inside())
                && !own_fragmentainer;
            // A line box ends where the flow may be cut (CSS Break 3 §4.1): a text block taller
            // than the fragmentainer is otherwise cut rather than carried on.
            self.line_edges(child, top, breakable_inside);

            if breakable_inside {
                let border = layout.border;
                let padding = layout.padding;
                self.walk(
                    child_id,
                    Point {
                        x: offset.x + layout.location.x,
                        y: top,
                    },
                    (layout.size.width - border.left - border.right - padding.left - padding.right)
                        .max(0.0),
                    breakable_inside,
                    &used,
                );
            }
        }
    }

    /// The page name the last content of this subtree asks for, descending without allowing any
    /// break inside it. A box that declares none keeps the name it inherits.
    fn last_page_name(&self, node_id: NodeId, inherited: &PageName) -> PageName {
        let doc = self.doc;
        let node = &doc.nodes[node_id];
        let own = node
            .primary_styles()
            .map(|styles| styles.clone_page())
            .unwrap_or(PageName::Auto);
        let used = match own {
            PageName::Auto => inherited.clone(),
            named => named,
        };
        let children = node.layout_children.borrow();
        let Some(children) = children.as_ref().and_then(|children| children.last()) else {
            return used;
        };
        // A box that holds text keeps its own name; only a box whose content is further boxes
        // carries the name of the last of them.
        if node.flags.is_inline_root() {
            return used;
        }
        self.last_page_name(*children, &used)
    }

    /// The break points at the ends of one block's own line boxes, filtered by widows and
    /// orphans. `top` is where the block's border box sits in the flow.
    fn line_edges(&mut self, node: &Node, top: f32, breakable: bool) {
        if !breakable {
            return;
        }
        let Some(text) = node
            .element_data()
            .and_then(|data| data.inline_layout_data.as_deref())
        else {
            return;
        };
        let Some(styles) = node.primary_styles() else {
            return;
        };
        let layout = if self.unrounded {
            node.unrounded_layout()
        } else {
            node.final_layout()
        };
        let origin = top + (layout.padding.top + layout.border.top) + text.block_offset;
        let scale = text.layout.scale();
        let widows = styles.clone_widows().max(1) as usize;
        let orphans = styles.clone_orphans().max(1) as usize;
        let lines: Vec<f32> = text
            .layout
            .lines()
            .map(|line| origin + line.metrics().block_max_coord / scale)
            .collect();
        let mut top = origin;
        for (index, y) in lines.iter().enumerate() {
            // A line box is the smallest thing a flow can be cut between: no break falls inside
            // one, so a fragmentainer has to be tall enough to hold the tallest of them.
            self.units.push(Unit {
                start: top,
                end: *y,
            });
            top = *y;
            let before = index + 1;
            // The last line's bottom is where the block ends, which is the break between two
            // blocks and not one inside this one.
            let after = lines.len() - before;
            if after == 0 {
                continue;
            }
            let rank = if after < widows || before < orphans {
                Rank::Forbidden
            } else {
                Rank::Legal
            };
            self.push(*y, rank, true);
        }
    }

    fn push(&mut self, at: f32, rank: Rank, breakable: bool) {
        if breakable || rank == Rank::Forced {
            self.edges.push(Edge { at, rank });
        }
    }

    /// Cut the recorded flow into fragmentainers `extent` deep.
    fn bands(mut self, start: f32, extent: f32, cross: f32, content_end: f32) -> ThinVec<Band> {
        self.edges.sort_by(|a, b| a.at.total_cmp(&b.at));
        cut_bands(&self.edges, start, extent, cross, content_end)
    }
}

/// Place the cuts: fill one fragmentainer, then start the next at the cut.
///
/// A forced break takes the band wherever it falls inside it. Otherwise the band ends at the
/// last legal point it can hold, and only when there is no legal point at all does the flow get
/// cut mid-box — which clips rather than loses the content, and is what an unbreakable box
/// taller than the page asks for.
pub(crate) fn cut_bands(
    edges: &[Edge],
    start: f32,
    extent: f32,
    cross: f32,
    content_end: f32,
) -> ThinVec<Band> {
    let mut bands = ThinVec::new();
    // The most fragmentainers content could need if it broke at its own edges and nowhere else.
    let limit_pages = (content_end / extent.max(1.0)).ceil() as usize + 2;
    let mut cursor = start;
    loop {
        let limit = cursor + extent;
        let mut forced = None;
        let mut legal = None;
        for edge in edges.iter() {
            if edge.at <= cursor {
                continue;
            }
            if edge.at > limit {
                break;
            }
            match edge.rank {
                Rank::Forced => {
                    forced = Some(edge.at);
                    break;
                }
                Rank::Legal => legal = Some(edge.at),
                Rank::Forbidden => {
                    // A box's start edge and the previous box's end edge are the same position,
                    // so `avoid` on the first cancels the offer of the second.
                    if legal == Some(edge.at) {
                        legal = None;
                    }
                }
            }
        }

        let cut = match forced {
            Some(at) => at,
            None if content_end <= limit => content_end,
            None => legal.unwrap_or(limit),
        };
        // The last fragmentainer is a whole page box: content may hang below the flow's own end
        // line (an out-of-flow box does), and the page it belongs to is still printed.
        let last = cut >= content_end || bands.len() + 1 >= limit_pages;
        bands.push(Band {
            start: cursor,
            end: if last { cursor + extent } else { cut },
            origin: Point::ZERO,
            cross,
        });
        if last {
            break;
        }
        cursor = cut;
    }
    bands
}

/// The break points of one container's flow, and where that flow ends, read during layout.
///
/// A multi-column container needs this inside the layout pass: how tall a column has to be is
/// decided by where its flow may be cut, and that is not known until the flow has been laid out
/// at the column's width.
pub(crate) fn edges_of(
    doc: &BaseDocument,
    container_id: crate::NodeId,
    basis: f32,
) -> (Vec<Edge>, Vec<Unit>, f32) {
    let mut planner = Planner {
        doc,
        unrounded: true,
        edges: Vec::new(),
        units: Vec::new(),
        extent: 0.0,
        page: PageName::Auto,
        seen_content: false,
    };
    planner.line_edges(&doc.nodes[container_id], 0.0, true);
    planner.walk(container_id, Point::ZERO, basis, true, &PageName::Auto);
    planner.edges.sort_by(|a, b| a.at.total_cmp(&b.at));
    let end = planner.extent;
    (planner.edges, planner.units, end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(at: f32, rank: Rank) -> Edge {
        Edge { at, rank }
    }

    /// A flow that fits one fragmentainer is one page, however short it is: the page box is the
    /// height of the paper, not of the content.
    #[test]
    fn one_page_when_it_fits() {
        let edges = [edge(0.0, Rank::Legal), edge(26.0, Rank::Legal)];
        let bands = cut_bands(&edges, 0.0, 600.0, 800.0, 120.0);
        assert_eq!(bands.len(), 1);
        assert_eq!((bands[0].start, bands[0].end), (0.0, 600.0));
    }

    /// A forced break ends its band wherever it falls, even when the band could hold more.
    #[test]
    fn forced_break_ends_the_band() {
        let edges = [
            edge(26.0, Rank::Legal),
            edge(44.0, Rank::Forced),
            edge(62.0, Rank::Legal),
        ];
        let bands = cut_bands(&edges, 0.0, 600.0, 800.0, 200.0);
        assert_eq!(bands.len(), 1 + 1);
        assert_eq!((bands[0].start, bands[0].end), (0.0, 44.0));
        assert_eq!((bands[1].start, bands[1].end), (44.0, 644.0));
    }

    /// Content that overflows is cut at the last legal point, so no box is split.
    #[test]
    fn unforced_break_uses_the_last_legal_point() {
        let edges = [edge(26.0, Rank::Legal), edge(50.0, Rank::Legal)];
        let bands = cut_bands(&edges, 0.0, 60.0, 800.0, 120.0);
        assert_eq!((bands[0].start, bands[0].end), (0.0, 50.0));
        assert_eq!((bands[1].start, bands[1].end), (50.0, 110.0));
    }

    /// `break-after: avoid` on the box that follows cancels the point its predecessor offered.
    #[test]
    fn avoid_cancels_the_point_at_the_same_position() {
        let edges = [edge(26.0, Rank::Legal), edge(26.0, Rank::Forbidden)];
        let bands = cut_bands(&edges, 0.0, 60.0, 800.0, 120.0);
        // With no legal point left, the flow is cut at the page edge rather than at 26.
        assert_eq!((bands[0].start, bands[0].end), (0.0, 60.0));
    }

    /// A box taller than the page has no legal point inside it: the page is filled and the rest
    /// continues on the next one, clipped rather than dropped.
    #[test]
    fn unbreakable_box_longer_than_the_page_still_advances() {
        let edges = [edge(0.0, Rank::Legal)];
        let bands = cut_bands(&edges, 0.0, 60.0, 800.0, 200.0);
        assert_eq!(bands.len(), 4);
        assert_eq!((bands[0].start, bands[0].end), (0.0, 60.0));
        assert_eq!((bands[3].start, bands[3].end), (180.0, 240.0));
    }
}

/// Compute the page plan for the flow rooted at `root_id`, or `None` when the flow cannot be
/// fragmented yet.
fn page_plan(doc: &BaseDocument, root_id: NodeId) -> Option<BreakPlan> {
    #[cfg(feature = "writing-mode")]
    if doc.nodes[root_id].writing_mode().is_vertical() {
        return None;
    }

    let page = doc.page_box?;
    let area = page.area();
    #[cfg(debug_assertions)]
    if std::env::var_os("CUOTUO_DEBUG_FRAGMENTATION").is_some() {
        eprintln!("page box: {:?} area {:?}", page, area);
    }
    if area.height <= 1.0 {
        return None;
    }

    let root_page = doc.nodes[root_id]
        .primary_styles()
        .map(|styles| styles.clone_page())
        .unwrap_or(PageName::Auto);
    let mut planner = Planner {
        doc,
        unrounded: false,
        edges: Vec::new(),
        units: Vec::new(),
        extent: 0.0,
        page: root_page.clone(),
        seen_content: false,
    };
    planner.walk(root_id, Point::ZERO, area.width, true, &root_page);
    // Where the content ends, and not the bottom of the first page: a flow shorter than a page
    // that asks for a break after its last box still gets one page, not two.
    let content_end = planner.extent;

    #[cfg(debug_assertions)]
    if std::env::var_os("CUOTUO_DEBUG_FRAGMENTATION").is_some() {
        let dump = planner
            .edges
            .iter()
            .map(|edge| {
                let rank = match edge.rank {
                    Rank::Forced => "forced",
                    Rank::Legal => "legal",
                    Rank::Forbidden => "avoid",
                };
                format!("{:.1}:{rank}", edge.at)
            })
            .collect::<Vec<_>>()
            .join(" ");
        eprintln!("fragmentation edges ({}): {dump}", doc.base_url());
    }

    let mut bands = planner.bands(0.0, area.height, area.width, content_end);
    for (index, band) in bands.iter_mut().enumerate() {
        band.origin = page.area_origin(index);
    }

    #[cfg(debug_assertions)]
    if std::env::var_os("CUOTUO_DEBUG_FRAGMENTATION").is_some() {
        let dump = bands
            .iter()
            .map(|b| format!("[{:.1}..{:.1}]@{:.1}", b.start, b.end, b.origin.y))
            .collect::<Vec<_>>()
            .join(" ");
        eprintln!("fragmentation bands: {dump} (content end {content_end:.1})");
    }

    Some(BreakPlan {
        axis: FlowAxis::Y,
        stride: page.size.height,
        bands,
    })
}
