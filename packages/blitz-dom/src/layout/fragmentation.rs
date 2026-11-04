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
use style::computed_values::flex_direction::T as FlexDirection;
use style::computed_values::flex_wrap::T as FlexWrap;
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

impl Rank {
    /// What to call the rank when the bands of a flow are dumped (`CUOTUO_DEBUG_FRAGMENTATION`).
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Rank::Forced => "forced",
            Rank::Legal => "legal",
            Rank::Forbidden => "avoid",
        }
    }
}

impl Edge {
    /// See [`Rank::name`].
    pub(crate) fn rank_name(&self) -> &'static str {
        self.rank.name()
    }
}

/// A stretch of the flow that no break may fall inside: a line box, an atomic inline box, a
/// replaced element, a box with `break-inside: avoid`. A column has to be tall enough to hold the
/// tallest of them, which is what stops balancing from cutting a single line into slivers, and a
/// fragmentainer that cannot hold one carries it to the next rather than slicing it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Unit {
    /// Where the stretch begins. For a flex item this is the room the container left before it —
    /// that space travels with the item — and otherwise the box's own edge.
    pub(crate) start: f32,
    pub(crate) end: f32,
    /// Where the box itself begins. Only a flex item's differs from [`Self::start`], and whether
    /// *some* fragmentainer could hold the box is a question about the box.
    pub(crate) box_start: f32,
    /// Whether this is an item of a flex container. A flex item no fragmentainer can hold is
    /// broken where the band ends rather than carried on, which is *not* what a box the author
    /// asked to keep whole does (`single-line-column-flex-fragmentation-071` against
    /// `table-page-break-inside-avoid-1`).
    pub(crate) flex_item: bool,
}

impl Unit {
    /// A stretch of the flow that moves as one.
    fn whole(start: f32, end: f32) -> Self {
        Self {
            start,
            end,
            box_start: start,
            flex_item: false,
        }
    }

    /// An item of a flex container running along the flow, with the room the container left before
    /// it. See [`Unit::start`].
    fn item(room: f32, start: f32, end: f32) -> Self {
        Self {
            start: room.min(start),
            end,
            box_start: start,
            flex_item: true,
        }
    }

    /// How tall the unit is along the flow, the room before it included.
    pub(crate) fn height(&self) -> f32 {
        (self.end - self.start).max(0.0)
    }

    /// How tall the box itself is along the flow.
    fn box_height(&self) -> f32 {
        (self.end - self.box_start).max(0.0)
    }
}

/// A box that clones the decoration of its fragments, and how much of a fragmentainer that takes
/// off the content.
///
/// `box-decoration-break: clone` draws the border and padding at *every* end of every fragment, so
/// a fragment of a box holds `depth - near - far` of content rather than the whole
/// fragmentainer. What the band above the break could not show is not lost: the band below starts
/// back at the flow line where the content was cut, which is why a cut inside one of these moves
/// the next band up by the insets it added.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CloneSpan {
    /// Where the box reaches along the flow, border box to border box.
    pub(crate) start: f32,
    pub(crate) end: f32,
    /// The border and padding a fragment of this box adds at both its ends.
    pub(crate) insets: f32,
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
    /// How deep the fragmentainer is along the flow, whether or not the band filled it: a column
    /// rule spans the whole fragmentainer, not the content that happens to sit in it.
    pub depth: f32,
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

/// The page a document starts on, as the document's `@page` rules describe it.
///
/// A document with no page rules prints on the window it was given, with no margin. The page's name
/// is the name the first box of its flow asks for (CSS Page 3 §6.5), and the rules matched are the
/// unnamed ones plus that name's, each carrying the pseudo-classes of the first page — Chrome
/// numbers a document's first page on the right-hand side of a spread, so `:first` and `:right`
/// both match it. Later pages are not resolved here: the page area is the containing block the
/// whole flow is laid out in, so another page's box could only be honoured by laying the flow out
/// again.
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
    let mut area = Size::default();
    let guard = doc.guard().read();
    let pseudos = style::stylesheets::PagePseudoClassFlags::FIRST
        | style::stylesheets::PagePseudoClassFlags::RIGHT;
    // A page inherits from the root element (CSS Page 3 §5.1.1), which is where a page rule's
    // `inherit` comes from — `page-margin-006-print` asks for the root's half-inch margin on every
    // side of the paper. The root box's own containing block is the page area, so a percentage of
    // its margin asks for the measure the page box has.
    let root_margin = doc
        .root_element()
        .layout_style()
        .margin()
        .resolve_or_zero(Some(page.size.width), resolve_calc_value);
    // A name carries more specificity than the plain `@page`, so the unnamed rules go first and
    // the page's own name after them; within each group a `:first` or `:right` selector outranks a
    // bare one, which is what sorting by specificity does.
    let mut keys = Vec::with_capacity(2);
    keys.push(style::Atom::default());
    if let Some(name) = starting_page_name(doc) {
        keys.push(name);
    }
    for key in keys {
        for (data, _) in doc.stylist.iter_extra_data_origins() {
            let Some(rules) = data.pages.rules.get(&key) else {
                continue;
            };
            let mut ordered = Vec::new();
            for entry in rules.iter() {
                let Some(spec) = entry.rule.read_with(&guard).match_specificity(pseudos) else {
                    continue;
                };
                ordered.push((spec, entry));
            }
            ordered.sort_by_key(|(spec, _)| *spec);
            for (_, entry) in ordered {
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
                        // `width` and `height` in a page rule size the page *area* — the box the
                        // content is laid out in — so the paper grows to fit them and their margins
                        // (`page-size-013-print`, whose `size: 500px; width: 200px; height: 300px;
                        // margin: 50px` prints on paper 300px by 400px).
                        PropertyDeclaration::Width(value) => {
                            area.width = page_edge(value, page.size.width).unwrap_or(area.width);
                        }
                        PropertyDeclaration::Height(value) => {
                            area.height = page_edge(value, page.size.height).unwrap_or(area.height);
                        }
                        // A CSS-wide keyword reaches an expanded declaration block one per longhand.
                        // `inherit` asks for the root element's margin, `initial` and `unset` for
                        // nothing, which is what a margin starts at; a reverted page rule is not
                        // read here, and leaves the margin as the rules before it made it.
                        PropertyDeclaration::CSSWideKeyword(keyword) => {
                            use style::properties::{
                                CSSWideKeyword as Keyword, LonghandId, PropertyDeclarationId,
                            };
                            let PropertyDeclarationId::Longhand(id) = declaration.id() else {
                                continue;
                            };
                            let (target, inherited): (&mut f32, f32) = match id {
                                LonghandId::MarginTop => (&mut page.margin.top, root_margin.top),
                                LonghandId::MarginRight => {
                                    (&mut page.margin.right, root_margin.right)
                                }
                                LonghandId::MarginBottom => {
                                    (&mut page.margin.bottom, root_margin.bottom)
                                }
                                LonghandId::MarginLeft => (&mut page.margin.left, root_margin.left),
                                _ => continue,
                            };
                            *target = match keyword.keyword {
                                Keyword::Inherit => inherited,
                                Keyword::Initial | Keyword::Unset => 0.0,
                                _ => continue,
                            };
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    if area.width > 0.0 {
        page.size.width = area.width + page.margin.left + page.margin.right;
    }
    if area.height > 0.0 {
        page.size.height = area.height + page.margin.top + page.margin.bottom;
    }
    page
}

/// The name of the page the document starts on: the first box of its flow that asks for a page of
/// its own, walking the flow in the order the page names are tracked in (`Planner::walk`).
fn starting_page_name(doc: &BaseDocument) -> Option<style::Atom> {
    page_name_in(doc, doc.root_element().id)
}

fn page_name_in(doc: &BaseDocument, node_id: NodeId) -> Option<style::Atom> {
    let node = &doc.nodes[node_id];
    if let Some(styles) = node.primary_styles() {
        if let PageName::PageName(name) = styles.clone_page() {
            return Some(name.0);
        }
    }
    let children = node.layout_children.borrow();
    let children = children.as_ref()?;
    for child in children.iter() {
        let id = *child;
        let child = &doc.nodes[id];
        if child.taffy_display() == taffy::Display::None || child.is_out_of_flow() {
            continue;
        }
        if let Some(name) = page_name_in(doc, id) {
            return Some(name);
        }
    }
    None
}

/// One edge of the page area as a page rule gives it, or `None` when the rule leaves it to the
/// paper. A percentage asks for the page box's measure in the same direction, as a page margin's
/// does; a length stands for itself.
fn page_edge(
    value: &style::values::specified::Size,
    basis: f32,
) -> Option<f32> {
    use style::values::specified::Size as S;
    match value {
        S::LengthPercentage(value) => positive(length(&value.0, basis)),
        S::Auto => None,
        _ => None,
    }
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

/// Does this box ask that each of its fragments be decorated as a box of its own?
fn clones_decoration(styles: &ComputedValues) -> bool {
    use style::properties::generated::longhands::box_decoration_break::computed_value::T as BoxDecorationBreak;
    matches!(styles.get_border().box_decoration_break, BoxDecorationBreak::Clone)
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
    /// The boxes that clone the decoration of their fragments (see [`CloneSpan`]).
    spans: Vec<CloneSpan>,
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
    /// box that must not be cut or that fragments its own content. `flex_items` says the children
    /// are items of a flex container whose main axis runs along this flow, which the flow moves on
    /// as a whole rather than halving. `inherited` is the page name the parent's content is printed
    /// on, which a child with `page: auto` keeps.
    fn walk(
        &mut self,
        parent: NodeId,
        offset: Point<f32>,
        inline_size: f32,
        breakable: bool,
        flex_items: bool,
        inherited: &PageName,
    ) {
        let doc = self.doc;
        let children = doc.nodes[parent].layout_children.borrow();
        let Some(children) = children.as_ref() else {
            return;
        };
        // The first item of a flex container starts at the container's own content edge; see the
        // note on where the break between two items falls.
        let mut first_item = true;
        // Where the item before this one ended, which is the gap the container left before it.
        let mut previous_item_end: Option<f32> = None;

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
            // The name of the box that holds the text is what counts: a chain of wrappers each
            // asking for a name breaks nothing when the content inside returns to the name the
            // flow already had (`page-name-inline-block-003`), and a box with no content of its
            // own is not what the page is named for (`page-name-propagated-001`,
            // `page-name-zero-height-001`) — which is why `layers-001-print`, whose named block
            // wraps nothing but a bordered box, still prints as one page.
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
                self.units.push(Unit::whole(outer_top, outer_bottom));
                self.page = self.last_page_name(child_id, &used);
                self.seen_content = true;
                continue;
            }
            let replaced = is_replaced_element(&data.name.local);
            if replaced {
                self.units.push(Unit::whole(outer_top, outer_bottom));
            }
            // A block-level replaced element is content too, and it has none of its own.
            let holds_content =
                in_flow_block && (child.flags.is_inline_root() || replaced);
            // The items of a flex container breaking along this flow take no part in the page-name
            // sequence: Chrome lets the container ask for the page, not the items inside it, so a
            // name on an item neither cuts the flow nor changes the name the flow goes on with
            // (`page-name-flex-001-print`, `page-name-flex-004-print`).
            let names_page = holds_content && !flex_items;
            let page_change = names_page && self.seen_content && used != self.page;
            if names_page {
                self.page = used.clone();
                self.seen_content = true;
            }

            // Between two items of a flex container the break falls at the end of the one that came
            // first: the room the container left between them is the space *before* the item that
            // follows, so it travels with that item's fragment and the fragment above it ends where
            // its own content stops (`flexbox-single-column-1a`, whose reference lays the gaps out
            // as boxes for exactly that reason). The first item's edge is the container's own
            // content edge, which is a legal point like any other box's; after the last item there
            // is no break between items to place (`flex-item-content-overflow-002a`).
            let legal_before = in_flow_block && !(flex_items && !first_item);
            let before = rank_between(styles.clone_break_before())
                .or_else(|| page_change.then_some(Rank::Forced))
                .or_else(|| legal_before.then_some(Rank::Legal));
            if let Some(rank) = before {
                self.push(outer_top, rank, breakable);
            }
            if let Some(rank) = rank_between(styles.clone_break_after()) {
                self.push(outer_bottom, rank, breakable);
            }
            if flex_items {
                let between_items = !matches!(
                    rank_between(styles.clone_break_before()),
                    Some(Rank::Forbidden)
                );
                if let Some(end) = previous_item_end.filter(|_| between_items) {
                    self.push(end, Rank::Legal, breakable);
                }
                first_item = false;
            }

            // Descend only into a box that shares its parent's fragmentainer: a scroll container
            // scrolls instead of breaking, a flex, grid or table container and a multi-column
            // container fragment their content by their own rules, and an atomic box does not
            // break at all.
            // A flex container whose main axis runs along this flow is the exception: its items
            // stand one after another along the flow exactly as two blocks do, so the flow cuts
            // between them and moves on item by item, which is how Chrome breaks it. Only a single
            // line of them: a wrapping container places its lines across the flow, so an item's
            // extent along the flow says nothing about where the line it belongs to ends.
            let flex_along_flow = display.inside() == DisplayInside::Flex
                && !styles
                    .get_position()
                    .flex_wrap
                    .intersects(FlexWrap::WRAP | FlexWrap::WRAP_REVERSE)
                && matches!(
                    styles.get_position().flex_direction,
                    FlexDirection::Column | FlexDirection::ColumnReverse
                );
            let own_fragmentainer = (!matches!(
                display.inside(),
                DisplayInside::Flow | DisplayInside::FlowRoot
            ) && !flex_along_flow)
                || styles.get_box().overflow_x != Overflow::Visible
                || styles.get_box().overflow_y != Overflow::Visible
                || is_multicol(&styles);
            let scroll_container = styles.get_box().overflow_x != Overflow::Visible
                || styles.get_box().overflow_y != Overflow::Visible;
            let breakable_inside = breakable
                && !avoids_inside(styles.clone_break_inside())
                && !own_fragmentainer;
            // A box the author asked not to be broken, and a box that scrolls or clips its
            // overflow, are carried whole into a fragmentainer rather than cut: the scrollport is
            // one box that cannot be in two places at once (`multicol-overflow-clip-auto-sized`).
            // A flex, grid or table container is not: Chrome breaks those item by item, and
            // treating them as unbreakable makes the columns around them too tall.
            if scroll_container || avoids_inside(styles.clone_break_inside()) {
                self.units.push(Unit::whole(outer_top, outer_bottom));
            }
            // A flex item is one thing the container sized and placed: where it does not fit the
            // fragmentainer, the container moves it on rather than halving it
            // (`flexbox-unbreakable-child`). The room the container left before the item is part of
            // what moves with it — that is why the break between two items falls at the end of the
            // one that came first — so an item that only fits once its gap is added to it is broken
            // where it stands rather than carried on (`flexbox-single-column-1a`).
            if flex_items {
                self.units.push(Unit::item(
                    previous_item_end.unwrap_or(outer_top),
                    outer_top,
                    outer_bottom,
                ));
                previous_item_end = Some(outer_bottom);
            }
            // A box that clones the decoration of its fragments holds less content in each one than
            // the fragmentainer is deep, and a cut inside it moves the next band back by that much.
            if breakable_inside && clones_decoration(&styles) {
                self.spans.push(CloneSpan {
                    start: top,
                    end: bottom,
                    insets: layout.border.top + layout.padding.top
                        + layout.border.bottom + layout.padding.bottom,
                });
            }
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
                    flex_along_flow,
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
            self.units.push(Unit::whole(top, *y));
            top = *y;
            let before = index + 1;
            // The last line's bottom is where the block ends, which is the break between two
            // blocks and not one inside this one.
            let after = lines.len() - before;
            if after == 0 {
                continue;
            }
            self.push(*y, line_rank(before, after, orphans, widows), true);
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
        self.units.sort_by(|a, b| a.start.total_cmp(&b.start));
        self.spans.sort_by(|a, b| a.start.total_cmp(&b.start));
        cut_bands(&self.edges, &self.units, &self.spans, start, extent, cross, content_end)
    }
}

/// Where a line box may be cut between, as the two counts a block carries decide it
/// (CSS Break 3 §3.3, and §3.4 for what they mean at the end of a fragmentainer): a break leaves
/// `before` lines behind and `after` ones to go, and neither side may be left with fewer lines than
/// the count that speaks for it — `orphans` for the fragment the lines came out of, `widows` for the
/// one they go to. A block's last line ends the block rather than breaking inside it, which the
/// caller skips before ever asking.
fn line_rank(before: usize, after: usize, orphans: usize, widows: usize) -> Rank {
    if after < widows || before < orphans {
        Rank::Forbidden
    } else {
        Rank::Legal
    }
}

/// The flow cannot be cut inside a box that must stay whole, so the box is carried to the next
/// fragmentainer when it fits there (CSS Break 3 §3.2: a forced break is placed at the last legal
/// break point before it). A box too tall for an empty fragmentainer is cut anyway — clipping it is
/// the only way the flow can go on — and a box that already starts at the fragmentainer's start
/// has nowhere later to go.
fn carry_before(units: &[Unit], limit: f32, cursor: f32, extent: f32) -> Option<f32> {
    units
        .iter()
        .find(|unit| unit.start > cursor && unit.start < limit && unit.end > limit && unit.height() <= extent)
        .map(|unit| unit.start)
}

/// Place the cuts: fill one fragmentainer, then start the next at the cut.
///
/// A forced break takes the band wherever it falls inside it. Otherwise the band ends at the
/// last legal point it can hold, and only when there is no legal point at all does the flow get
/// cut mid-box — which clips rather than loses the content, and is what an unbreakable box
/// taller than the page asks for.
///
/// A cut that falls inside a box cloning the decoration of its fragments costs that box the border
/// and padding the cut adds at both ends of the fragment, so the next band starts back by that much
/// and the content the band had no room for goes on below rather than being lost.
pub(crate) fn cut_bands(
    edges: &[Edge],
    units: &[Unit],
    spans: &[CloneSpan],
    start: f32,
    extent: f32,
    cross: f32,
    content_end: f32,
) -> ThinVec<Band> {
    let mut bands = ThinVec::new();
    // The most fragmentainers content could need if it broke at its own edges and nowhere else.
    let limit_pages = (content_end / extent.max(1.0)).ceil() as usize + 2;
    // Where a flex item starts that no empty fragmentainer could hold *and* that has no legal point
    // to break at inside it. Moving such an item on wins nothing — the next fragmentainer would
    // have to break it at its end anyway — so the point at its start is not taken and this
    // fragmentainer is filled (`single-line-column-flex-fragmentation-071`). An item with a legal
    // point inside it is still moved on (`flexbox-single-column-6`), and a box the author asked to
    // keep whole is carried even when nothing could hold it (`table-page-break-inside-avoid-1`).
    let inside: Vec<f32> = edges
        .iter()
        .filter(|edge| edge.rank == Rank::Legal)
        .map(|edge| edge.at)
        .collect();
    let has_inside = |unit: &Unit| {
        let from = match inside.binary_search_by(|at| at.total_cmp(&(unit.start + 0.5))) {
            Ok(at) => at,
            Err(at) => at,
        };
        inside
            .get(from..)
            .is_some_and(|rest| rest.first().is_some_and(|at| *at < unit.end - 0.5))
    };
    let mut too_tall: Vec<f32> = units
        .iter()
        .filter(|unit| unit.flex_item && unit.box_height() > extent && !has_inside(unit))
        .map(|unit| unit.start)
        .collect();
    too_tall.sort_by(|a, b| a.total_cmp(b));
    too_tall.dedup();
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
                Rank::Legal => {
                    if too_tall.binary_search_by(|at| at.total_cmp(&edge.at)).is_err() {
                        legal = Some(edge.at);
                    }
                }
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
            None => match legal {
                Some(at) => at,
                // Nothing in this fragmentainer may be cut at, so a box that must stay whole is
                // carried to the next one instead of being sliced here.
                None => carry_before(units, limit, cursor, extent).unwrap_or(limit),
            },
        };
        // The last fragmentainer is a whole page box: content may hang below the flow's own end
        // line (an out-of-flow box does), and the page it belongs to is still printed.
        let last = cut >= content_end || bands.len() + 1 >= limit_pages;
        bands.push(Band {
            start: cursor,
            end: if last { cursor + extent } else { cut },
            origin: Point::ZERO,
            cross,
            depth: extent,
        });
        if last {
            break;
        }
        // Only a band that filled its fragmentainer and was cut there lost content to the border and
        // padding a cloned fragment adds: a band that ended at a legal point, or carried a whole box
        // on, gave the next one everything it had. Every box the cut falls inside pays, and a band
        // always moves on: a flow that cannot advance is a fragmentainer that cannot end.
        //
        // The price is paid by moving the *next* band back, not by ending this one early, which
        // means the room between the cut and the fragmentainer's edge is shown twice: once as the
        // content that reached there and once as the border the clone draws. Shortening the band
        // instead — ending it at `cut - inset` so the next begins where this one's content stopped —
        // was measured and is not the answer: the `box-decoration-break` family stands at three
        // pairs passing and two failing either way, and `monolithic-overflow-031/032-print` stay
        // broken. What those two need is content inside a cloning box advancing *more slowly* than
        // the band — a box whose child overflows it paints that overflow across the following
        // fragmentainers, at the rate the band itself advances — which one flow coordinate cannot
        // express, since the same coordinate is the box's own content in one place and its
        // child's overflow in another.
        let inset: f32 = if cut >= limit - 0.5 {
            spans
                .iter()
                .filter(|span| span.start < cut - 0.5 && span.end > cut + 0.5)
                .map(|span| span.insets)
                .sum()
        } else {
            0.0
        };
        cursor = if inset > 0.0 {
            (cut - inset).max(cursor + 1.0)
        } else {
            cut
        };
    }
    bands
}

/// The break points of one container's flow, the boxes that clone their fragments' decoration, and
/// where that flow ends, read during layout.
///
/// A multi-column container needs this inside the layout pass: how tall a column has to be is
/// decided by where its flow may be cut, and that is not known until the flow has been laid out
/// at the column's width.
pub(crate) fn edges_of(
    doc: &BaseDocument,
    container_id: crate::NodeId,
    basis: f32,
) -> (Vec<Edge>, Vec<Unit>, Vec<CloneSpan>, f32) {
    let mut planner = Planner {
        doc,
        unrounded: true,
        edges: Vec::new(),
        units: Vec::new(),
        spans: Vec::new(),
        extent: 0.0,
        page: PageName::Auto,
        seen_content: false,
    };
    planner.line_edges(&doc.nodes[container_id], 0.0, true);
    planner.walk(container_id, Point::ZERO, basis, true, false, &PageName::Auto);
    planner.edges.sort_by(|a, b| a.at.total_cmp(&b.at));
    planner.units.sort_by(|a, b| a.start.total_cmp(&b.start));
    planner.spans.sort_by(|a, b| a.start.total_cmp(&b.start));
    let end = planner.extent;
    (planner.edges, planner.units, planner.spans, end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(at: f32, rank: Rank) -> Edge {
        Edge { at, rank }
    }

    /// A twelve-line block with `orphans: 7` and `widows: 3` — `widows-orphans-014`, whose assert
    /// asks for a break between the seventh line and the eighth. Three of its eleven interior points
    /// satisfy both counts, and they are the ones the two numbers leave room for.
    #[test]
    fn two_counts_leave_only_the_middle_of_a_twelve_line_block() {
        let legal: Vec<usize> = (1..12)
            .filter(|before| matches!(line_rank(*before, 12 - *before, 7, 3), Rank::Legal))
            .collect();
        assert_eq!(legal, vec![7, 8, 9], "seven must stay behind, three must arrive");
    }

    /// With both counts at 1 every line ends a legal break; `line_rank` is asked only about interior
    /// points, so the smallest thing it can be told about is one line on each side.
    #[test]
    fn a_pair_of_ones_breaks_between_every_line() {
        for before in 1..12 {
            assert_eq!(line_rank(before, 12 - before, 1, 1), Rank::Legal);
        }
        assert_eq!(line_rank(1, 1, 2, 2), Rank::Forbidden);
    }

    /// `orphans` guards the fragment the lines came out of and `widows` the one they arrive in:
    /// moving either count moves the allowed points to the matching end of the block.
    #[test]
    fn orphans_guards_the_end_left_behind() {
        assert_eq!(line_rank(2, 10, 3, 1), Rank::Forbidden, "two left, three asked for");
        assert_eq!(line_rank(3, 9, 3, 1), Rank::Legal);
        assert_eq!(line_rank(10, 2, 1, 3), Rank::Forbidden, "two to arrive, three asked for");
        assert_eq!(line_rank(9, 3, 1, 3), Rank::Legal);
    }

    /// The counts decide which points exist to be cut at, and the cut then takes the last of them
    /// before the fragmentainer's edge: twelve 26px lines in a 100px page with `orphans: 3` and
    /// `widows: 3` can only end after the third line, because the fourth would leave too few to
    /// arrive anywhere but the edge itself.
    #[test]
    fn the_cut_takes_the_last_point_both_counts_allow() {
        let edges: Vec<Edge> = (1..12)
            .map(|line| Edge {
                at: line as f32 * 26.0,
                rank: line_rank(line as usize, 12 - line as usize, 3, 3),
            })
            .collect();
        let bands = cut_bands(&edges, &[], &[], 0.0, 100.0, 100.0, 312.0);
        assert_eq!(bands[0].end, 78.0, "the last point that leaves three to arrive: {bands:?}");
    }

    /// A flow that fits one fragmentainer is one page, however short it is: the page box is the
    /// height of the paper, not of the content.
    #[test]
    fn one_page_when_it_fits() {
        let edges = [edge(0.0, Rank::Legal), edge(26.0, Rank::Legal)];
        let bands = cut_bands(&edges, &[], &[], 0.0, 600.0, 800.0, 120.0);
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
        let bands = cut_bands(&edges, &[], &[], 0.0, 600.0, 800.0, 200.0);
        assert_eq!(bands.len(), 1 + 1);
        assert_eq!((bands[0].start, bands[0].end), (0.0, 44.0));
        assert_eq!((bands[1].start, bands[1].end), (44.0, 644.0));
    }

    /// Content that overflows is cut at the last legal point, so no box is split.
    #[test]
    fn unforced_break_uses_the_last_legal_point() {
        let edges = [edge(26.0, Rank::Legal), edge(50.0, Rank::Legal)];
        let bands = cut_bands(&edges, &[], &[], 0.0, 60.0, 800.0, 120.0);
        assert_eq!((bands[0].start, bands[0].end), (0.0, 50.0));
        assert_eq!((bands[1].start, bands[1].end), (50.0, 110.0));
    }

    /// `break-after: avoid` on the box that follows cancels the point its predecessor offered.
    #[test]
    fn avoid_cancels_the_point_at_the_same_position() {
        let edges = [edge(26.0, Rank::Legal), edge(26.0, Rank::Forbidden)];
        let bands = cut_bands(&edges, &[], &[], 0.0, 60.0, 800.0, 120.0);
        // With no legal point left, the flow is cut at the page edge rather than at 26.
        assert_eq!((bands[0].start, bands[0].end), (0.0, 60.0));
    }

    /// A box that cannot be cut is carried to the next fragmentainer when it fits there, rather
    /// than being sliced at the edge of this one.
    #[test]
    fn an_unbreakable_box_is_carried() {
        let edges = [edge(0.0, Rank::Legal), edge(12.0, Rank::Legal)];
        let units = [Unit::whole(2.0, 12.0)];
        let bands = cut_bands(&edges, &units, &[], 0.0, 10.0, 800.0, 12.0);
        assert_eq!((bands[0].start, bands[0].end), (0.0, 2.0));
        assert_eq!((bands[1].start, bands[1].end), (2.0, 12.0));
    }

    /// A box too tall for an empty fragmentainer has nowhere later to go, so it is cut.
    #[test]
    fn too_tall_to_carry_is_cut() {
        let edges = [edge(0.0, Rank::Legal)];
        let units = [Unit::whole(2.0, 40.0)];
        let bands = cut_bands(&edges, &units, &[], 0.0, 10.0, 800.0, 40.0);
        assert_eq!((bands[0].start, bands[0].end), (0.0, 10.0));
    }

    /// A box taller than the page has no legal point inside it: the page is filled and the rest
    /// continues on the next one, clipped rather than dropped.
    #[test]
    fn unbreakable_box_longer_than_the_page_still_advances() {
        let edges = [edge(0.0, Rank::Legal)];
        let bands = cut_bands(&edges, &[], &[], 0.0, 60.0, 800.0, 200.0);
        assert_eq!(bands.len(), 4);
        assert_eq!((bands[0].start, bands[0].end), (0.0, 60.0));
        assert_eq!((bands[3].start, bands[3].end), (180.0, 240.0));
    }

    /// A fragment of a box cloning its decoration holds less content than the fragmentainer is
    /// deep, and the band below starts back at where this one's content was cut: `borders-008`
    /// splits a 240px content box into three columns of 80, not of 100, 100 and 40.
    #[test]
    fn a_cloned_fragment_pays_for_its_own_border() {
        let edges = [edge(0.0, Rank::Legal), edge(260.0, Rank::Legal)];
        let spans = [CloneSpan {
            start: 0.0,
            end: 260.0,
            insets: 20.0,
        }];
        let bands = cut_bands(&edges, &[], &spans, 0.0, 100.0, 800.0, 260.0);
        assert_eq!(bands.len(), 3);
        assert_eq!((bands[0].start, bands[0].end), (0.0, 100.0));
        assert_eq!((bands[1].start, bands[1].end), (80.0, 180.0));
        assert_eq!((bands[2].start, bands[2].end), (160.0, 260.0));
    }

    /// Every box a cut falls inside pays, but a band never starts before the content it has already
    /// placed: a box whose border and padding fill the fragmentainer still moves the flow on rather
    /// than cutting the same line for ever.
    #[test]
    fn a_cloned_band_still_advances() {
        let edges = [edge(0.0, Rank::Legal)];
        let spans = [CloneSpan {
            start: 0.0,
            end: 400.0,
            insets: 150.0,
        }];
        let bands = cut_bands(&edges, &[], &spans, 0.0, 100.0, 800.0, 400.0);
        assert_eq!((bands[0].start, bands[0].end), (0.0, 100.0));
        assert_eq!((bands[1].start, bands[1].end), (1.0, 101.0));
    }

    /// A band that stopped at a legal point of its own accord gave the next one all it had, so the
    /// cloned box whose edge it cut at takes nothing back: the flow goes on where it stopped.
    #[test]
    fn a_band_that_chose_its_end_does_not_step_back() {
        let edges = [
            edge(0.0, Rank::Legal),
            edge(40.0, Rank::Legal),
            edge(260.0, Rank::Legal),
        ];
        let spans = [CloneSpan {
            start: 0.0,
            end: 260.0,
            insets: 20.0,
        }];
        let bands = cut_bands(&edges, &[], &spans, 0.0, 100.0, 800.0, 260.0);
        assert_eq!((bands[0].start, bands[0].end), (0.0, 40.0));
        assert_eq!(bands[1].start, 40.0);
    }

    /// A flex item no fragmentainer could hold, and with no legal point inside it to break at, is
    /// broken where the band ends rather than moved on: the space it would have left behind is
    /// filled, because the point at its start buys nothing
    /// (`single-line-column-flex-fragmentation-071`).
    #[test]
    fn a_box_too_tall_to_move_on_is_not_moved() {
        let edges = [edge(0.0, Rank::Legal), edge(40.0, Rank::Legal)];
        let units = [Unit::item(40.0, 40.0, 200.0)];
        let bands = cut_bands(&edges, &units, &[], 0.0, 100.0, 800.0, 200.0);
        assert_eq!((bands[0].start, bands[0].end), (0.0, 100.0));
        assert_eq!((bands[1].start, bands[1].end), (100.0, 200.0));
    }

    /// An item that can be broken inside is still moved on whole when the band has no room for it:
    /// the point at its start is a legal break like any other (`flexbox-single-column-6`).
    #[test]
    fn an_item_with_a_point_inside_is_still_moved() {
        let edges = [
            edge(0.0, Rank::Legal),
            edge(40.0, Rank::Legal),
            edge(150.0, Rank::Legal),
        ];
        let units = [Unit::item(40.0, 40.0, 200.0)];
        let bands = cut_bands(&edges, &units, &[], 0.0, 100.0, 800.0, 200.0);
        assert_eq!((bands[0].start, bands[0].end), (0.0, 40.0));
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
        spans: Vec::new(),
        extent: 0.0,
        page: root_page.clone(),
        seen_content: false,
    };
    planner.walk(root_id, Point::ZERO, area.width, true, false, &root_page);
    // Where the content ends, and not the bottom of the first page: a flow shorter than a page
    // that asks for a break after its last box still gets one page, not two.
    let content_end = planner.extent;

    #[cfg(debug_assertions)]
    if std::env::var_os("CUOTUO_DEBUG_FRAGMENTATION").is_some() {
        let dump = planner
            .edges
            .iter()
            .map(|edge| format!("{:.1}:{}", edge.at, edge.rank_name()))
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
