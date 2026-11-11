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
//!
//! ## What this model cannot express
//!
//! Laying the flow out once and cutting afterwards means the flow has **one** coordinate system,
//! shared by every fragment. Four things in the specs ask for more, and each is a registered
//! difference rather than an untried idea — the attempts are recorded in the commits that were
//! reverted (`borders-003..008`, `monolithic-overflow-031/032`, `float-page-break-inside-avoid-3/4`,
//! `multi-line-row-flex-fragmentation-068`):
//!
//! - content that must **move** at a break: a flex row gap truncated where a line breaks, a float
//!   that carries the lines beside it to the next fragmentainer;
//! - content that must advance at a **different rate** inside a box that clones its decoration,
//!   whose fragments each pay for their own borders;
//! - a nested flow on **its own axis**: a vertical subtree inside a horizontal document, or a
//!   `vertical-rl` document paginating along x (`page-name-orthogonal-writing-003-print`);
//! - an out-of-box anchored to the page sequence whose **own content** decides how many pages there
//!   are (the extent now counts what it hoists; fragmenting that content does not).
//!
//! A fifth limit was reached from the other direction and has since been paid. The width a columned
//! box reports when it is measured for its own size is the number of columns its content *needs* —
//! four 26px lines with `widows: 2` shrink to two columns (`widows-orphans-002`), which needs to know
//! *why* a fragment end is barred, since a point barred by `widows` and one barred by `break-inside`
//! are not the same instruction to balancing. [`Rank::Forbidden`] now records that
//! ([`Edge::by_counts`], set from [`line_edges`]), and a box that chooses its own width — auto, whether
//! stretched or shrink-to-fit — is now divided by the columns its flow filled (`9840577f`, `92e4027f`,
//! `eaf6b803`). What the work exposed is a sixth limit, and it is not the coordinate system:
//! [`cut_bands`] will not place a boundary inside a [`Unit`] at all, so a box taller than its
//! fragmentainer is carried whole to the next one, while Chrome lets it overflow and keeps the column
//! count. That is where `break-inside-avoid-min-block-size-1` and the `column-box-alignment-rtl`
//! family sit, and two hypotheses about it have now been measured false: the balancing floor is not the
//! lever (`avoid`-only units excluded — no pixel moved), nor is a line-only floor (empty boxes have no
//! line units at all). The lever is the carry-versus-overflow decision in the cut, and it is
//! load-bearing against `single-line-column-flex-fragmentation-071`, `flexbox-unbreakable-child` and
//! `table-page-break-inside-avoid-1`, where carrying an item whole is exactly right.
//!
//! The one change that would reach the first four is running the flow layout **per band** rather than
//! once — pass one of [`multicol`](super::multicol) already measures the flow at a column's width, so
//! the shape is: let `cut_bands` return the ranges to re-measure, re-measure each at its fragmentainer's
//! cross size and depth, and give each fragment its own coordinates. That is a layout-architecture
//! change, not a rule in this file, and it should be attempted with `gecko/flexbox`, `css-multicol`,
//! `-print` and the full corpus as its gate: the model as it stands is worth 5061 pairs, and only a
//! measurement can say whether the change keeps them. The model is worth 5062 pairs today. The sixth
//! limit above does not need it — the cut
//! already knows where the boundary would fall; what is missing is that a box taller than the
//! fragmentainer should sometimes overflow it rather than be carried.
//!
//! Six attempts at the shape above were made **through the existing fields** and every one measured
//! dead, and the finding recorded here now reads backwards from the first two of them: what those
//! attempts were chasing was never the rule. Drawing a columned container's out-of-flow children per
//! column, or counting the container's own hoisted boxes into its flow: no pixel moved. Counting a
//! *descendant's* hoisted reach into the flow: one pair gained, another lost —
//! `out-of-flow-in-multicolumn-004` wants an abspos box to lengthen the flow while `bfc-004-print`
//! wants the same shape not to, and nothing in the current inputs tells those apart (the containing
//! block and the hoisting are the same kind in both). Setting the flow's measure to the column depth,
//! and re-laying each child at it: worse, in the way described above.
//!
//! **A percentage height inside a column asks for the whole multicol, not for one column's depth.**
//! `multicol-span-all-children-height-002/-003` say so in their own `assert` ("compute the percentage
//! relative to the entire multicol"), and their references write the children as `height: 200px` in a
//! box whose content is 200px and whose columns are 100px deep — the child is *spread over* the two
//! columns, not one. The reading that sent seven rounds into the door below came from `-001`, whose
//! children are `50%` in the test and `25%` in its reference: true as far as it goes, but `-001`'s
//! column depth happens to be half its container, and a pair that cannot tell two rules apart is not
//! evidence. The probe that pinned the old reading used `columns: 1` for exactly the same reason and
//! proved nothing; it now uses two columns, and asserts the container rule the references give.
//!
//! So what was wrong was in [`multicol`](super::multicol)'s own arithmetic, and no new input was
//! needed. Four things, three of them the same shape — a number counted twice, or counted as the
//! wrong box:
//!
//! 1. The height the author wrote is the **content** box under the default `box-sizing`, while the
//!    block algorithm is handed a **border box** — which the width in the same call already was
//!    (`columns.width + insets`). A container of `height: 200px; border: 5px` therefore measured its
//!    own content at 190px, its columns at 95, and every percentage inside at 95 instead of 100. This
//!    is also why the seven attempts above "found no channel": they were re-basing a percentage whose
//!    basis was already wrong by the border, and each one moved the box and the basis together, which
//!    is what makes a change look inert.
//! 2. A run of columns after a `column-span: all` box may use only the depth the container has left,
//!    which is what gives `-002`'s last row four columns of 50px, two of them outside the box's own
//!    width, exactly as its reference draws them. Under `column-fill: auto` each row still gets the
//!    whole content box (`multicol-gap-decorations-020`).
//! 3. `spanner_bands` started its running offset at `insets.top` and then advanced it by the first
//!    run's flow start, which already carries that inset — so the container's insets were counted
//!    twice along the flow, and a spanned page sat 5px below where the unfragmented pass had put its
//!    content (`-002`), or 64px below (`multicol-span-all-rule-001`, whose two halves run through this
//!    same path and so hid the error from each other while the pair passed).
//! 4. The same double-count across the flow: a band's `origin` includes the container's cross inset,
//!    and the subtree drawn into the band was laid out from that inset too, so
//!    [`BreakPlan::translate`] adding `origin` as a shift put every column of a bordered container
//!    one inset too far right — `-002`'s yellow began at x 18 where its reference has x 13. Hence
//!    [`BreakPlan::cross_start`], which the page sequence leaves at zero because a page's bands
//!    already carry the page's own position.
//!
//! Measured, with the two `multicol-span-all-button` claims retired because the engine now draws them
//! identically: 5061 → 5068, five pairs fixed and none lost — `-002` and `-003` (from 39720 and
//! 51537px), `out-of-flow-in-multicolumn-093`, `gecko/pagination/border-breaking-001-cols` and
//! `css-break/flexbox/flex-item-content-overflow-002b`.
//!
//! Two shapes were tried part-way through and rejected, both because a column **rule** is drawn to the
//! container's content edge whatever the run — the assert of `multicol-span-all-rule-001`, which each
//! of them lost: stretching the last run to the room left rather than balancing it, and setting
//! [`Band::depth`] from the run instead of the container. Read [`BreakPlan::fragment_rect`] before
//! trying the second again: the clip is cut at `end - start`, so `depth` is *only* the rule's measure.
//! An earlier draft of this note said the painting past the bottom of the box needed the rule and the
//! clip separated first; they are separate, and what was painting there was double-count 4.
//!
//! The stretch was taken back out and measured again once the insets stopped being counted twice,
//! because that rejection had been decided on the old arithmetic. It still fails, and it now fails for
//! a different, larger reason: no pair changes state anywhere in the multicol, break, gaps or paged
//! families (48 / 11 / 83 / 1 / 126, exactly as without it), `-001` stays at 209px and `-007` at
//! 8883px, and `multicol-span-all-rule-002` goes from 337260 to 372520px. So it buys nothing and costs
//! 35260px on an already-failing pair: rejected on measurement, not on memory, and the tree it was
//! measured in was reverted back to `6d415d74` byte for byte.
//!
//! What still waits behind the per-fragmentainer measure is unchanged: float carrying across columns
//! and pages, abspos content fragmented onto the pages it spans, the flex row gap truncated at a line
//! break, and the nested writing mode once its own box is measured rightly (see below). `cut_bands`
//! handing back the ranges to re-measure is the first step of that, and it is a larger change than
//! anything named above.
//!
//! Two different things sit under "the nested writing mode", and conflating them sends a reader to the
//! wrong branch:
//!
//! - **Vertical multicol** is what `gecko/css-break/vertical-wm-001/-002` (7320px, 2720px) ask for.
//!   Their markup is `div.col.vl`: the **columned box is itself** `vertical-lr`, with
//!   `block-size: 90px` and `inline-size: 140px`. Measured off the frame, its border box comes out
//!   142px in *both* axes — `inline-size` became the width as well as the height, and `block-size`,
//!   the depth a vertical flow's columns would be cut from, was never applied. So the first thing to
//!   transpose is the container's own measure in [`multicol`](super::multicol), ahead of any question
//!   about band axes. The logical properties *inside* that box already follow the writing mode (its
//!   25px `border-block-start` is a vertical bar, 40px in from the edge), which is why the pair reads
//!   as "the columns are wrong" rather than "vertical text is broken".
//! - **The orthogonal island** — a horizontal subtree in a vertical flow, or its mirror — is the walk's
//!   `foreign_axis`: [`edges_of`] asks each child whether its block axis is the one this plan reads,
//!   and when it is not, the child becomes a single [`Unit::whole`] with no break point inside it.
//!   Right for a page name, unfinished for a nested flow that should go on breaking; the plan holds
//!   one axis for the whole tree, so the change is a per-level axis.
//!
//! Each is pinned by its own canvas probe, and each probe asserts what happens **today**, so neither
//! can be quietly edited to match a change in the other:
//! `a_vertical_columned_box_still_measures_itself_horizontally` (the 142px-in-both-axes measure) and
//! `an_orthogonal_island_is_held_whole_by_the_parent` (ink stops at y 60 in column two, column one's
//! remaining 40px empty). The first fails when the columned box is measured on its own axes; the
//! second, when the island stops being whole.

use blitz_traits::node_id::NodeId;
use markup5ever::local_name;
use std::collections::{HashMap, HashSet};
use style::computed_values::flex_direction::T as FlexDirection;
use style::computed_values::flex_wrap::T as FlexWrap;
use style::media_queries::MediaType;
use style::properties::ComputedValues;
use style::values::computed::length::NonNegativeLengthOrAuto;
use style::properties::PropertyDeclaration;
use style::values::computed::{
    BorderStyle, BreakBetween, BreakWithin, ColumnCount, Float, OutlineStyle, Overflow, PageName,
    TextDecorationLine,
};
use style::values::specified::box_::{DisplayInside, DisplayOutside};
use taffy::{CoreStyle as _, Point, Rect, ResolveOrZero as _, Size};
use thin_vec::ThinVec;

use super::resolve_calc_value;
use crate::layout::is_replaced_element;
use crate::node::SpecialElementData;
use crate::{BaseDocument, Node, NodeData};

/// The physical direction a fragmented flow advances along.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlowAxis {
    /// Down the page: the block axis of `horizontal-tb`.
    Y,
    /// Along x: the block axis of a vertical writing mode. Not produced yet.
    X,
}

impl FlowAxis {
    /// Where a box's border box begins **in the flow's own coordinates**, given the physical `offset`
    /// of its container and `measure`, the far edge the flow's coordinates count from. Margins are
    /// added by the caller: the outer edge is the box's own begin less the room before it, while how
    /// far the flow reaches is measured to the border box — a margin left at the bottom of a
    /// fragmentainer asks for no page.
    ///
    /// A horizontal flow and a `vertical-lr` one read the physical coordinate directly. A
    /// `vertical-rl` flow starts at the containing space's *right* edge and advances leftwards, so a
    /// box's place along the flow is how far its own right edge sits from that far edge. Flipping
    /// here, once, keeps every cut rule — legal points in increasing order, widows, `carry_before` —
    /// blind to the direction; painting converts the other way, in [`BreakPlan::translate`].
    pub(crate) fn along(
        self,
        reversed: bool,
        measure: f32,
        offset: Point<f32>,
        layout: &taffy::Layout,
    ) -> f32 {
        match self {
            FlowAxis::Y => offset.y + layout.location.y,
            FlowAxis::X if reversed => measure - (offset.x + layout.location.x + layout.size.width),
            FlowAxis::X => offset.x + layout.location.x,
        }
    }

    /// The room a box leaves before it along the flow — its *right* margin for a reversed flow, whose
    /// first edge is the right one. Paired with [`Self::end_edge`]: these two with [`Self::along`]
    /// are the whole of what the direction does to where a break may fall.
    pub(crate) fn start_edge(self, reversed: bool, margin: Rect<f32>) -> f32 {
        match self {
            FlowAxis::Y => margin.top,
            FlowAxis::X if reversed => margin.right,
            FlowAxis::X => margin.left,
        }
    }

    pub(crate) fn end_edge(self, reversed: bool, margin: Rect<f32>) -> f32 {
        match self {
            FlowAxis::Y => margin.bottom,
            FlowAxis::X if reversed => margin.left,
            FlowAxis::X => margin.right,
        }
    }

    /// How far a box reaches along this axis.
    pub(crate) fn measure(self, layout: &taffy::Layout) -> f32 {
        match self {
            FlowAxis::Y => layout.size.height,
            FlowAxis::X => layout.size.width,
        }
    }

    /// The axis the flow runs against, given the box's own writing mode: a vertical writing mode
    /// blocks along x. Horizontal flows are the only ones cut today.
    pub(crate) fn of(vertical: bool) -> Self {
        if vertical {
            FlowAxis::X
        } else {
            FlowAxis::Y
        }
    }
}

/// What a position in the flow is worth as a place to cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Rank {
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
    pub(crate) rank: Rank,
    /// Whether it is a `widows`/`orphans` count that bars this point, rather than the content itself
    /// (a `break-inside`, a box that cannot be cut). Balancing may have to end a fragment where the
    /// content bars the cut — nothing else fits — but a height that forces a cut the counts forbid
    /// is not a balanced height, and the two look identical without this.
    pub(crate) by_counts: bool,
}

#[cfg(debug_assertions)]
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

#[cfg(debug_assertions)]
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
    /// A table row that CSS Tables 3 calls freely fragmentable. It may be cut
    /// at the fragmentainer edge even when none of its descendants offers a
    /// break point, so `carry_before` must not move the row whole.
    freely_fragmentable: bool,
}

impl Unit {
    /// A stretch of the flow that moves as one.
    pub(crate) fn whole(start: f32, end: f32) -> Self {
        Self {
            start,
            end,
            box_start: start,
            flex_item: false,
            freely_fragmentable: false,
        }
    }

    /// A stretch a CSS layout model allows slicing when no finer break point
    /// exists, as opposed to a line box or an `avoid` box that must be carried.
    fn splittable(start: f32, end: f32) -> Self {
        Self {
            start,
            end,
            box_start: start,
            flex_item: false,
            freely_fragmentable: true,
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
            freely_fragmentable: false,
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
    /// Whether the flow runs against the axis (`vertical-rl`). Such a flow *enters* its
    /// fragmentainers at the far edge, which is what [`Self::fragment_span`] and [`Self::translate`]
    /// account for.
    pub reversed: bool,
    /// The far edge a reversed flow's coordinates were counted from — what the walk measured against.
    pub measure: f32,
    /// How deep one fragmentainer is along the axis, which is what a reversed flow right-aligns its
    /// content inside.
    pub depth: f32,
    /// Where the flow's content actually stops. The last band is drawn to its fragmentainer's depth,
    /// so without this its content would be measured longer than it is, and every reversed page would
    /// mis-align by exactly the difference.
    pub flow_end: f32,
    /// The coordinate the flow's own children start at, along the axis *across* the flow. A band's
    /// `origin` is the far edge of the fragmentainer's content box and already includes it, while the
    /// subtree drawn into the band was laid out from it, so the shift is `origin - cross_start`.
    /// Taking the origin alone paints a bordered columned box one border-width away from where the
    /// unfragmented pass put its content: the yellow of
    /// `multicol-span-all-children-height-002` began at x 18 where its reference has x 13, five times
    /// the container's `border: 5px`. A page sequence sets `0.0`: its bands carry the page's own
    /// absolute position, and the content box the children live in is already relative to that.
    pub cross_start: f32,
}

impl BreakPlan {
    /// The flow lines one band really holds: `(start, end)`, with the last band's end brought back to
    /// where the content stops. A band's `end` records the depth of its fragmentainer — where the
    /// next break would have fallen — which is right for the clip and wrong for measuring content.
    fn content_slice(&self, band: &Band) -> (f32, f32) {
        let end = if self.reversed {
            band.end.min(self.flow_end)
        } else {
            band.end
        };
        (band.start, end.max(band.start))
    }

    /// Where one band's content sits on the canvas along the flow: its `(near, far)` edges.
    ///
    /// Normally content begins at its fragmentainer's near edge. A reversed flow *enters* the
    /// fragmentainer at its far edge, so its first content is what sits against the right: the same
    /// span, grown from the other side. Both the clip and [`Self::translate`] start here, so they
    /// cannot drift apart.
    pub fn fragment_span(&self, band: &Band) -> (f32, f32) {
        let (start, end) = self.content_slice(band);
        let span = (end - start).max(0.0);
        let near = match self.axis {
            FlowAxis::Y => band.origin.y,
            FlowAxis::X if self.reversed => band.origin.x + (self.depth - span).max(0.0),
            FlowAxis::X => band.origin.x,
        };
        (near, near + span)
    }

    /// The canvas rectangle one band's content is clipped to: the fragmentainer's content box, cut
    /// short at the flow line where the next break was placed. Painting the whole page box would show
    /// the content that belongs to the following fragmentainer as well.
    ///
    /// The clip is measured by the fragmentainer's **depth**, not by [`Self::content_slice`]: a band's
    /// own page is as deep as the page is, and a last band's overflowing tail must not be shaved.
    pub fn fragment_rect(&self, band: &Band) -> Rect<f32> {
        let depth = (band.end - band.start).max(0.0);
        let near = match self.axis {
            FlowAxis::Y => band.origin.y,
            FlowAxis::X if self.reversed => band.origin.x + (self.depth - depth).max(0.0),
            FlowAxis::X => band.origin.x,
        };
        let far = near + depth;
        match self.axis {
            FlowAxis::Y => Rect {
                left: band.origin.x,
                top: near,
                right: band.origin.x + band.cross,
                bottom: far,
            },
            FlowAxis::X => Rect {
                left: near,
                top: band.origin.y,
                right: far,
                bottom: band.origin.y + band.cross,
            },
        }
    }

    /// How far the fragmentainer's subtree shifts to draw one band: from where layout physically put
    /// the flow's slice, to where [`Self::fragment_span`] says it belongs. A reversed flow's
    /// coordinates count from the far edge of the space they were measured in, so its slice begins at
    /// `measure - end` rather than at `start`.
    pub fn translate(&self, band: &Band) -> Point<f32> {
        let (near, _) = self.fragment_span(band);
        let (_, end) = self.content_slice(band);
        match self.axis {
            FlowAxis::Y => Point {
                x: band.origin.x - self.cross_start,
                y: near - band.start,
            },
            FlowAxis::X if self.reversed => Point {
                x: near - (self.measure - end),
                y: band.origin.y - self.cross_start,
            },
            FlowAxis::X => Point {
                x: near - band.start,
                y: band.origin.y - self.cross_start,
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
            width: match plan.axis {
                FlowAxis::Y => page.size.width,
                FlowAxis::X => plan.canvas_extent(),
            },
            height: match plan.axis {
                FlowAxis::Y => plan.canvas_extent(),
                FlowAxis::X => page.size.height,
            },
        })
    }

    /// Number of page fragmentainers in the current print flow.
    pub fn page_count(&self) -> usize {
        self.root_element()
            .break_plan
            .as_deref()
            .map_or(0, |plan| plan.bands.len())
    }

    /// Whether the page sequence advances horizontally, as it does for a
    /// vertical writing mode, rather than down the paper.
    pub fn page_flow_is_vertical(&self) -> bool {
        self.root_element()
            .break_plan
            .as_deref()
            .is_some_and(|plan| plan.axis == FlowAxis::X)
    }

    /// Size of one sheet of paper, including its page margins.
    pub fn page_box_size(&self) -> Option<Size<f32>> {
        self.page_box.map(|page| page.size)
    }

    /// The flow coordinate under the top-left corner of page `index`'s paper.
    ///
    /// A page is drawn from the paper's edge, which sits outside the page area
    /// the flow was laid out in, and a reversed flow (`vertical-rl`) enters its
    /// pages from the far edge. Painting moves each band by
    /// [`BreakPlan::translate`], so the inverse of that translation plus the
    /// page's position on the canvas is the document coordinate a page image's
    /// top-left pixel stands for — the offset pointer coordinates need before
    /// hit testing.
    pub fn page_flow_origin(&self, index: usize) -> Option<(f32, f32)> {
        let page = self.page_box?;
        let plan = self.root_element().break_plan.as_deref()?;
        let band = plan.bands.get(index)?;
        let shift = plan.translate(band);
        let (x, y) = match plan.axis {
            FlowAxis::Y => (0.0, index as f32 * page.size.height),
            FlowAxis::X => (index as f32 * page.size.width, 0.0),
        };
        Some((x - shift.x, y - shift.y))
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

    /// Where the page area of page `index` begins on the canvas. Pages stack along the axis the flow
    /// runs along: down the paper for a horizontal flow, across it for a vertical one.
    pub fn area_origin(&self, index: usize, axis: FlowAxis) -> Point<f32> {
        match axis {
            FlowAxis::Y => Point {
                x: self.margin.left,
                y: self.margin.top + index as f32 * self.size.height,
            },
            FlowAxis::X => Point {
                x: self.margin.left + index as f32 * self.size.width,
                y: self.margin.top,
            },
        }
    }
}

/// The state a membership index was built from. The epoch covers layout and
/// scroll damage; the page fields make the cache safe if a caller changes the
/// paper without rebuilding the document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PageIndexKey {
    epoch: u64,
    page_count: usize,
    paper: Option<[u32; 4]>,
    axis: FlowAxis,
    reversed: bool,
    measure: u32,
    depth: u32,
    stride: u32,
    flow_end: u32,
    cross_start: u32,
    band_hash: u64,
}

impl PageIndexKey {
    fn new(doc: &BaseDocument, plan: &BreakPlan) -> Self {
        let paper = doc.page_box.map(|page| {
            [
                page.size.width.to_bits(),
                page.size.height.to_bits(),
                page.margin.left.to_bits(),
                page.margin.top.to_bits(),
            ]
        });
        let mut band_hash = 0xcbf2_9ce4_8422_2325u64;
        let mut mix = |bits: u32| {
            band_hash ^= u64::from(bits);
            band_hash = band_hash.wrapping_mul(0x0000_0100_0000_01b3);
        };
        for band in &plan.bands {
            mix(band.start.to_bits());
            mix(band.end.to_bits());
            mix(band.origin.x.to_bits());
            mix(band.origin.y.to_bits());
            mix(band.cross.to_bits());
            mix(band.depth.to_bits());
        }
        Self {
            epoch: doc.page_paint_epoch,
            page_count: plan.bands.len(),
            paper,
            axis: plan.axis,
            reversed: plan.reversed,
            measure: plan.measure.to_bits(),
            depth: plan.depth.to_bits(),
            stride: plan.stride.to_bits(),
            flow_end: plan.flow_end.to_bits(),
            cross_start: plan.cross_start.to_bits(),
            band_hash,
        }
    }
}

/// A conservative index of the nodes paint may need on each page.
///
/// This is deliberately cheaper and coarser than a fragment tree: a node is
/// listed on a page when its painted box, shifted to that page, reaches the
/// paper; every listed node also pulls in its ancestors so paint can reach it.
/// Nodes absent from the page cannot paint there, so paint skips their whole
/// subtree instead of walking it only to clip it away.
#[derive(Debug)]
pub struct PagePaintMembership {
    key: PageIndexKey,
    pages: Vec<HashSet<NodeId>>,
}

impl PagePaintMembership {
    pub(crate) fn is_current(&self, doc: &BaseDocument) -> bool {
        let Some(root) = doc.try_root_element() else {
            return false;
        };
        let Some(plan) = root.break_plan.as_deref() else {
            return false;
        };
        self.key == PageIndexKey::new(doc, plan)
    }

    /// Whether `node_id`, or an ancestor needed to reach painted content below
    /// it, belongs to `page_index`.
    pub fn contains(&self, page_index: usize, node_id: NodeId) -> bool {
        self.pages
            .get(page_index)
            .is_some_and(|nodes| nodes.contains(&node_id))
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Bleed {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

impl Bleed {
    fn uniform(amount: f32) -> Self {
        Self {
            left: amount,
            top: amount,
            right: amount,
            bottom: amount,
        }
    }

    fn widen(self, other: Self) -> Self {
        Self {
            left: self.left.max(other.left),
            top: self.top.max(other.top),
            right: self.right.max(other.right),
            bottom: self.bottom.max(other.bottom),
        }
    }
}

/// How far this box can paint outside its border box, without consulting its
/// descendants. The paint crate has the same rules; keeping the conservative
/// answer here avoids a page missing a shadow or outline whose box ends on the
/// previous page.
fn paint_bleed(node: &Node) -> Bleed {
    let Some(styles) = node.primary_styles() else {
        return Bleed::default();
    };
    let mut bleed = Bleed::default();

    let outline = styles.get_outline();
    if let OutlineStyle::BorderStyle(style) = outline.outline_style
        && !matches!(style, BorderStyle::None | BorderStyle::Hidden)
    {
        let amount = (f64::from(outline.outline_width.0.to_f32_px())
            + outline.outline_offset.to_f64_px())
        .max(0.0) as f32;
        bleed = bleed.widen(Bleed::uniform(amount));
    }

    let effects = styles.get_effects();
    for shadow in effects.box_shadow.0.iter().filter(|shadow| !shadow.inset) {
        let reach = f64::from(shadow.spread.px()) + f64::from(shadow.base.blur.px()) * 2.5;
        let x = f64::from(shadow.base.horizontal.px());
        let y = f64::from(shadow.base.vertical.px());
        bleed = bleed.widen(Bleed {
            left: (reach - x).max(0.0) as f32,
            top: (reach - y).max(0.0) as f32,
            right: (reach + x).max(0.0) as f32,
            bottom: (reach + y).max(0.0) as f32,
        });
    }

    let font_size = styles.get_font().font_size.computed_size().px();
    let text = styles.get_inherited_text();
    for shadow in text.text_shadow.0.iter() {
        let blur = shadow.blur.px();
        let dx = shadow.horizontal.px();
        let dy = shadow.vertical.px();
        // Text shadows are painted as blurred glyph copies. A glyph is at
        // most about one em wide, and the layer's own bounds add the blur.
        bleed = bleed.widen(Bleed::uniform(font_size + blur + dx.abs().max(dy.abs())));
    }

    let drawn = TextDecorationLine::UNDERLINE
        | TextDecorationLine::OVERLINE
        | TextDecorationLine::LINE_THROUGH;
    if styles.get_text().text_decoration_line.intersects(drawn) {
        bleed = bleed.widen(Bleed::uniform(font_size));
    }
    if node
        .element_data()
        .is_some_and(|element| element.list_item_data.is_some())
    {
        bleed = bleed.widen(Bleed::uniform(font_size));
    }

    bleed
}

/// A node's painted extent in continuous document coordinates.
fn box_paint_bounds(doc: &BaseDocument, node: &Node) -> Option<(f32, f32, f32, f32)> {
    let bleed = paint_bleed(node);
    let (pos, layout) = doc.physical_unrounded_geometry(node.id);
    let overflow = layout.scrollable_overflow_rect;
    let (left, top, right, bottom) = (
        pos.x + overflow.left.min(0.0),
        pos.y + overflow.top.min(0.0),
        pos.x + layout.size.width.max(overflow.right),
        pos.y + layout.size.height.max(overflow.bottom),
    );

    let left = left - bleed.left;
    let top = top - bleed.top;
    let right = right + bleed.right;
    let bottom = bottom + bleed.bottom;
    (left.is_finite() && top.is_finite() && right.is_finite() && bottom.is_finite())
        .then_some((left, top, right, bottom))
}

/// The paper rectangle one page is drawn on. Content may live in its margin
/// (margin notes), so membership is tested against the paper rather than the
/// body's narrower fragment clip.
fn page_paper_rect(doc: &BaseDocument, plan: &BreakPlan, band: &Band) -> Rect<f32> {
    let Some(page) = doc.page_box else {
        return plan.fragment_rect(band);
    };
    let left = band.origin.x - page.margin.left;
    let top = band.origin.y - page.margin.top;
    Rect {
        left,
        top,
        right: left + page.size.width,
        bottom: top + page.size.height,
    }
}

/// Whether a box, drawn with this page's translation, can be seen on its
/// paper. This mirrors paint's placement exactly: a box crossing a break may
/// be cut by a page but still reach the next page's margin.
fn band_contains(bounds: (f32, f32, f32, f32), shift: Point<f32>, paper: Rect<f32>) -> bool {
    let (left, top, right, bottom) = bounds;
    right + shift.x > paper.left
        && left + shift.x < paper.right
        && bottom + shift.y > paper.top
        && top + shift.y < paper.bottom
}

/// Build the conservative page-member index for the current geometry.
///
/// The page plan knows the flow's cuts but not which box produced each
/// fragment yet. Rather than duplicate paint's fragment walk per page, this
/// uses each node's absolute box, shifted by every page's translation. It is
/// conservative: transforms, fixed/sticky positioning and filters may list a
/// node on extra pages, never omit one it can paint on.
pub(crate) fn build_page_membership(doc: &BaseDocument) -> Option<PagePaintMembership> {
    let root = doc.try_root_element()?;
    let plan = root.break_plan.as_deref()?;
    if plan.bands.is_empty() {
        return None;
    }

    let page_count = plan.bands.len();
    let key = PageIndexKey::new(doc, plan);
    let mut pages = vec![HashSet::new(); page_count];
    let page_geometry: Vec<_> = plan
        .bands
        .iter()
        .map(|band| (plan.translate(band), page_paper_rect(doc, plan, band)))
        .collect();
    let mut inline_root_bounds = HashMap::new();

    for (_, node) in doc.tree().iter() {
        if !matches!(
            node.data,
            NodeData::Element(_) | NodeData::AnonymousBlock(_)
        ) {
            continue;
        }
        if node.taffy_display() == taffy::Display::None {
            continue;
        }
        let conservative_pages = {
            let mut current = Some(node.id);
            let mut all = false;
            while let Some(id) = current {
                let Some(ancestor) = doc.get_node(id) else {
                    break;
                };
                if !matches!(
                    ancestor.data,
                    NodeData::Element(_) | NodeData::AnonymousBlock(_)
                ) {
                    current = ancestor.parent;
                    continue;
                }
                if matches!(
                    ancestor.taffy_position(),
                    taffy::Position::Fixed | taffy::Position::Sticky
                ) || (ancestor.element_data().is_some() && ancestor.transform().is_some())
                    || ancestor.primary_styles().is_some_and(|styles| {
                        let effects = styles.get_effects();
                        !effects.filter.0.is_empty() || !effects.backdrop_filter.0.is_empty()
                    })
                {
                    all = true;
                    break;
                }
                current = ancestor.containing_block().or(ancestor.parent);
            }
            all
        };

        if conservative_pages {
            for page in &mut pages {
                page.insert(node.id);
            }
            continue;
        }

        // A clone box's descendants may paint on a page their own box does not
        // reach: the clone's padding at a break shifts the continuation. Use
        // the clone root's bounds for the whole subtree so membership remains
        // conservative without teaching this index the paint-time shift.
        let mut clone_root = None;
        let mut current = node.parent;
        while let Some(id) = current {
            let Some(ancestor) = doc.get_node(id) else {
                break;
            };
            if ancestor
                .primary_styles()
                .is_some_and(|styles| clones_decoration(&styles))
            {
                clone_root = Some(ancestor);
                break;
            }
            current = ancestor.parent.or(ancestor.containing_block());
        }

        // A non-atomic inline has no layout box of its own. Its lines are part
        // of its inline root, whose box already covers every page those lines
        // can reach; using that root avoids rescanning the whole paragraph for
        // every span in it.
        let bounds = if let Some(clone_root) = clone_root {
            box_paint_bounds(doc, clone_root)
        } else if node.is_non_atomic_inline() {
            let root_id = node.inline_root_ancestor().map(|root| root.id);
            root_id.and_then(|root_id| {
                *inline_root_bounds.entry(root_id).or_insert_with(|| {
                    doc.get_node(root_id)
                        .and_then(|root| box_paint_bounds(doc, root))
                })
            })
        } else {
            box_paint_bounds(doc, node)
        };
        let Some(bounds) = bounds else {
            continue;
        };

        for (page_index, &(shift, paper)) in page_geometry.iter().enumerate() {
            if band_contains(bounds, shift, paper) {
                pages[page_index].insert(node.id);
            }
        }

        // A margin note is painted by the inline root that owns its text
        // layout, not by its anchor box. A long note can continue onto pages
        // past that root's own box, so the fragmentainer that holds each note
        // fragment must retain the root even when no line of body text does.
        if let Some(layout) = node
            .element_data()
            .and_then(|element| element.inline_layout_data.as_ref())
        {
            for group in &layout.margin_note_groups {
                for fragment in &group.page_fragments {
                    if let Some(page) = pages.get_mut(fragment.page_index) {
                        page.insert(node.id);
                    }
                }
            }
        }
    }

    // Paint reaches a fragment through its DOM/containing-block ancestry, and
    // hoisted stacking-context children through their containing chain too.
    // Close over both chains transitively so traversal never stops before a
    // visible descendant, even when the two chains interleave.
    for page in &mut pages {
        let mut pending: Vec<NodeId> = page.iter().copied().collect();
        while let Some(id) = pending.pop() {
            let Some(node) = doc.get_node(id) else {
                continue;
            };
            let mut ancestors = vec![node.parent, node.containing_block()];
            while let Some(parent) = ancestors.pop().flatten() {
                if !matches!(
                    doc.get_node(parent).map(|node| &node.data),
                    Some(NodeData::Element(_) | NodeData::AnonymousBlock(_))
                ) {
                    continue;
                }
                if page.insert(parent) {
                    pending.push(parent);
                }
            }
        }
    }

    Some(PagePaintMembership { key, pages })
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
        BreakBetween::Always
        | BreakBetween::Page
        | BreakBetween::Left
        | BreakBetween::Right
        | BreakBetween::Column
        | BreakBetween::Region => Some(Rank::Forced),
        BreakBetween::Avoid | BreakBetween::AvoidColumn | BreakBetween::AvoidRegion => {
            Some(Rank::Forbidden)
        }
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
    /// The axis the flow runs along: see [`FlowAxis::along`]. Only [`FlowAxis::Y`] is cut today; the
    /// walk already measures through this field so that a vertical document has one place left to
    /// settle rather than a coordinate convention repeated through the file.
    axis: FlowAxis,
    /// Whether the flow runs against the axis (`vertical-rl`), and the far edge its coordinates count
    /// from. A horizontal flow sets neither.
    reversed: bool,
    measure: f32,
    /// The fragmentainer's block and cross sizes, when the caller knows them before
    /// planning. Table rows use them for the CSS Tables “preserve the row whole”
    /// test; column balancing learns its depth later, so it leaves these unset.
    fragmentainer_depth: Option<f32>,
    fragmentainer_cross: Option<f32>,
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
            // A percentage margin resolves against the containing block's inline size; `auto`
            // resolves to nothing, which is what it means in a block flow.
            let margin = child
                .layout_style()
                .margin()
                .resolve_or_zero(Some(inline_size), resolve_calc_value);
            // Where the box sits along the flow, and how far it reaches — both asked of
            // [`FlowAxis`], so that one place decides what "further along the flow" means. See
            // [`FlowAxis::along`] for the coordinate choice a vertical document has to settle.
            let top = self
                .axis
                .along(self.reversed, self.measure, offset, &layout);
            let bottom = top + self.axis.measure(&layout);
            let outer_top = top - self.axis.start_edge(self.reversed, margin);
            let outer_bottom = bottom + self.axis.end_edge(self.reversed, margin);
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
            // A wrapping flex container whose main axis runs *across* this flow stacks lines down
            // it, and the flow is cut between the lines and never inside one (CSS Flexbox 1 §8: a
            // multi-line container breaks at its line boundaries). Taffy does not tell us where its
            // lines are, but the items it placed do: items of one line overlap along the flow, and
            // items of the next do not, so the groups of overlapping spans are the lines. Their
            // interiors are made unbreakable and their starts are the points the flow may cut at,
            // exactly as an item's edges are for a single-line container; nothing inside a line is
            // offered (`multi-line-row-flex-fragmentation-082a-print`, three pages tall here and
            // two in its reference, because the container was cut through instead of between).
            let lines_across_flow = display.inside() == DisplayInside::Flex
                && styles
                    .get_position()
                    .flex_wrap
                    .intersects(FlexWrap::WRAP | FlexWrap::WRAP_REVERSE)
                && matches!(
                    styles.get_position().flex_direction,
                    FlexDirection::Row | FlexDirection::RowReverse
                );
            let own_fragmentainer = (!matches!(
                display.inside(),
                DisplayInside::Flow | DisplayInside::FlowRoot
            ) && !flex_along_flow
                && !lines_across_flow)
                || styles.get_box().overflow_x != Overflow::Visible
                || styles.get_box().overflow_y != Overflow::Visible
                || is_multicol(&styles);
            let scroll_container = styles.get_box().overflow_x != Overflow::Visible
                || styles.get_box().overflow_y != Overflow::Visible;
            // A box whose own block axis is not the one this flow is being measured along — the
            // `horizontal-tb` subtree inside a `vertical-rl` document, or its mirror — cannot have
            // its interior recorded here at all: its break points lie along an axis this walk is not
            // reading. Until the plan can carry an axis per level (see the module docs), such a box
            // is held whole and its content is left unmeasured; recording positions from it as if
            // they were along this axis is what put a nested page name at the wrong place
            // (`page-name-orthogonal-writing-001/-003-print`).
            #[cfg(feature = "writing-mode")]
            let foreign_axis = FlowAxis::of(child.writing_mode().is_vertical()) != self.axis;
            #[cfg(not(feature = "writing-mode"))]
            let foreign_axis = false;
            let breakable_inside = breakable
                && !avoids_inside(styles.clone_break_inside())
                && !own_fragmentainer
                && !foreign_axis;
            // A box the author asked not to be broken, a box that scrolls or clips its
            // overflow, and a box on another axis are carried whole into a fragmentainer rather than
            // cut: the scrollport is one box that cannot be in two places at once
            // (`multicol-overflow-clip-auto-sized`), and a nested flow cannot be cut where the cut
            // is not being taken.
            if scroll_container || avoids_inside(styles.clone_break_inside()) || foreign_axis {
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
                let insets = match self.axis {
                    FlowAxis::Y => {
                        layout.border.top
                            + layout.padding.top
                            + layout.border.bottom
                            + layout.padding.bottom
                    }
                    FlowAxis::X => {
                        layout.border.left
                            + layout.padding.left
                            + layout.border.right
                            + layout.padding.right
                    }
                };
                self.spans.push(CloneSpan {
                    start: top,
                    end: bottom,
                    insets,
                });
                self.units.push(Unit::splittable(top, bottom));
            }
            // A line box ends where the flow may be cut (CSS Break 3 §4.1): a text block taller
            // than the fragmentainer is otherwise cut rather than carried on.
            self.line_edges(child, top, breakable_inside);
            let table = child.element_data().and_then(|element| {
                let SpecialElementData::TableRoot(table) = &element.special_data else {
                    return None;
                };
                Some(table.clone())
            });
            if let Some(table) = table {
                let table_breakable = breakable
                    && !foreign_axis
                    && !scroll_container
                    && !avoids_inside(styles.clone_break_inside());
                let forced_before = table
                    .cells
                    .iter()
                    .any(|cell| self.subtree_has_forced_break(cell.node_id, true))
                    || table
                        .rows
                        .iter()
                        .any(|row| self.subtree_has_forced_break(row.node_id, true));
                if forced_before {
                    self.push(outer_top, Rank::Forced, breakable);
                }
                let forced_after = table
                    .cells
                    .iter()
                    .any(|cell| self.subtree_has_forced_break(cell.node_id, false))
                    || table
                        .rows
                        .iter()
                        .any(|row| self.subtree_has_forced_break(row.node_id, false));
                if forced_after {
                    self.push(outer_bottom, Rank::Forced, breakable);
                }
                self.table_edges(&table, table_breakable);
                if table_breakable && clones_decoration(&styles) {
                    let insets = match self.axis {
                        FlowAxis::Y => {
                            layout.border.top
                                + layout.padding.top
                                + layout.border.bottom
                                + layout.padding.bottom
                        }
                        FlowAxis::X => {
                            layout.border.left
                                + layout.padding.left
                                + layout.border.right
                                + layout.padding.right
                        }
                    };
                    self.spans.push(CloneSpan {
                        start: top,
                        end: bottom,
                        insets,
                    });
                    self.units.push(Unit::splittable(top, bottom));
                }
            }
            // A block container with no break point inside cannot push content
            // to the next line or row, but its background and border still
            // fragment. Treat the empty stretch as splittable so a page can
            // fill up to its edge instead of stopping at the box's start.
            let empty_block = breakable_inside
                && in_flow_block
                && !child.flags.is_inline_root()
                && !replaced
                && self.fragmentainer_depth.is_some()
                && self.axis == FlowAxis::Y
                && child
                    .layout_children
                    .borrow()
                    .as_ref()
                    .is_none_or(|children| children.is_empty());
            if empty_block {
                self.units.push(Unit::splittable(top, bottom));
                self.push(bottom, Rank::Legal, breakable);
            }

            if breakable_inside {
                let border = layout.border;
                let padding = layout.padding;
                let content = Point {
                    x: offset.x + layout.location.x,
                    y: top,
                };
                let inner_inline = (layout.size.width
                    - border.left
                    - border.right
                    - padding.left
                    - padding.right)
                    .max(0.0);
                if lines_across_flow && breakable {
                    self.flex_lines(child_id, content, inner_inline, &used);
                } else {
                    self.walk(
                        child_id,
                        content,
                        inner_inline,
                        breakable_inside && !lines_across_flow,
                        flex_along_flow,
                        &used,
                    );
                }
            }
        }
    }

    /// Record the lines of a wrapping flex container, and descend into them so that nothing inside a
    /// line is offered as a break point. `offset` is the container's border box in flow coordinates.
    fn flex_lines(&mut self, container: NodeId, offset: Point<f32>, inline_size: f32, inherited: &PageName) {
        let doc = self.doc;
        let children = doc.nodes[container].layout_children.borrow();
        let Some(children) = children.as_ref() else {
            return;
        };
        // One line's span along the flow, and the boxes that sit in it.
        let mut lines: Vec<(f32, f32, Vec<(NodeId, Point<f32>, f32)>)> = Vec::new();
        for &child_id in children.iter() {
            let child = &doc.nodes[child_id];
            if child.taffy_display() == taffy::Display::None {
                continue;
            }
            let layout = if self.unrounded {
                child.unrounded_layout()
            } else {
                child.final_layout()
            };
            let top = offset.y + layout.location.y;
            let bottom = top + layout.size.height;
            if bottom <= top {
                continue;
            }
            match lines.last_mut() {
                // Overlapping spans, or a line that begins where the last one ended with no room
                // between them, belong to the same line.
                Some(last) if last.1 >= top - 0.5 => {
                    last.1 = last.1.max(bottom);
                    last.2.push((child_id, layout.location, layout.size.width));
                }
                _ => lines.push((top, bottom, vec![(child_id, layout.location, layout.size.width)])),
            }
        }
        for (index, (start, end, items)) in lines.iter().enumerate() {
            self.units.push(Unit::whole(*start, *end));
            // The break between two lines falls at the end of the line that came first: the room the
            // container left between them travels with the line below, as it does between items.
            if index > 0 {
                self.push(*start, Rank::Legal, true);
            }
            for &(child_id, location, _) in items {
                let child_offset = Point {
                    x: offset.x + location.x,
                    y: *start,
                };
                self.extent = self.extent.max(*end);
                // A percentage margin of an item resolves against the container's content width,
                // which is what `inline_size` is here.
                self.walk(child_id, child_offset, inline_size, false, false, inherited);
            }
        }
        self.extent = self.extent.max(lines.last().map(|line| line.1).unwrap_or(offset.y));
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

    fn inline_origin(
        &self,
        node: &Node,
        layout: &taffy::Layout,
        text: &crate::node::TextLayout,
        top: f32,
    ) -> f32 {
        // Read the line's place from the box's physical position rather than
        // reconstructing it from `top` and the rounded border box. A vertical
        // inline root's block extent comes from the unrounded inline layout,
        // while `final_layout` has whole-pixel size; adding the line metrics to
        // the rounded edge shifted every line by the difference (about 0.6px on
        // the 32-kai sample) and left a sliver of the line on the previous page.
        let physical = node.absolute_position(0.0, 0.0);
        let content_left = physical.x + layout.border.left + layout.padding.left;
        let content_top = physical.y + layout.border.top + layout.padding.top;
        match (self.axis, self.reversed, text.vertical_text) {
            (FlowAxis::Y, _, None) => content_top + text.block_offset,
            (FlowAxis::X, true, Some(vertical)) if vertical.block_axis_starts_at_right => {
                self.measure - (content_left + vertical.block_extent) + text.block_offset
            }
            (FlowAxis::X, false, Some(_)) => content_left + text.block_offset,
            _ => top + text.block_offset,
        }
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
        let origin = self.inline_origin(node, &layout, text, top);
        let scale = text.layout.scale();
        let widows = styles.clone_widows().max(1) as usize;
        let orphans = styles.clone_orphans().max(1) as usize;
        if !text.interline_lines.is_empty() {
            let body_lines = text.layout.lines().collect::<Vec<_>>();
            let content_end = origin + text.layout.height() / scale;
            let mut groups = Vec::<(f32, f32)>::new();
            for (index, line) in body_lines.iter().enumerate() {
                let metrics = line.metrics();
                let body_top = origin
                    + metrics
                        .block_min_coord
                        .min(metrics.content_block_min_coord)
                        / scale;
                let body_bottom = origin
                    + metrics
                        .block_max_coord
                        .max(metrics.content_block_max_coord)
                        / scale;
                let note_top = text
                    .interline_lines
                    .iter()
                    .filter(|note| note.slot_index == index)
                    .map(|note| origin + note.block_offset)
                    .min_by(f32::total_cmp)
                    .unwrap_or(body_top);
                groups.push((note_top.min(body_top), body_bottom));
            }
            let mut synthetic = text
                .interline_lines
                .iter()
                .filter(|note| note.slot_index >= body_lines.len())
                .collect::<Vec<_>>();
            synthetic.sort_by(|left, right| left.block_offset.total_cmp(&right.block_offset));
            for (index, note) in synthetic.iter().enumerate() {
                let start = origin + note.block_offset;
                let end = synthetic
                    .get(index + 1)
                    .map(|next| origin + next.block_offset)
                    .unwrap_or(content_end);
                groups.push((start, end.max(start)));
            }
            groups.sort_by(|left, right| left.0.total_cmp(&right.0));

            let mut cursor = origin;
            let mut ends = Vec::with_capacity(groups.len() + 1);
            for (start, end) in groups {
                let start = cursor.max(start);
                let end = end.max(start);
                self.units.push(Unit::whole(start, end));
                ends.push(end);
                cursor = end;
            }
            if cursor < content_end {
                self.units.push(Unit::whole(cursor, content_end));
                ends.push(content_end);
            }
            for (index, end) in ends.iter().enumerate() {
                let before = index + 1;
                let after = ends.len() - before;
                if after == 0 {
                    continue;
                }
                self.push_at(*end, line_rank(before, after, orphans, widows), true, true);
            }
            return;
        }
        let lines: Vec<(f32, f32)> = text
            .layout
            .lines()
            .map(|line| {
                let metrics = line.metrics();
                (
                    origin
                        + metrics
                            .block_min_coord
                            .min(metrics.content_block_min_coord)
                            / scale,
                    origin
                        + metrics
                            .block_max_coord
                            .max(metrics.content_block_max_coord)
                            / scale,
                )
            })
            .collect();
        let mut top = origin;
        for (index, (start, end)) in lines.iter().enumerate() {
            // A line box is the smallest thing a flow can be cut between: no break falls inside
            // one, so a fragmentainer has to be tall enough to hold the tallest of them.
            self.units.push(Unit::whole(top.min(*start), *end));
            top = *end;
            let before = index + 1;
            // The last line's bottom is where the block ends, which is the break between two
            // blocks and not one inside this one.
            let after = lines.len() - before;
            if after == 0 {
                continue;
            }
            // Whether the counts bar this point is the question balancing asks: a column height that
            // leaves a fragment fewer lines than `widows`/`orphans` demands is not a balanced one.
            self.push_at(*end, line_rank(before, after, orphans, widows), true, true);
        }
    }

    fn flow_box(&self, node: &Node, layout: &taffy::Layout) -> (f32, f32) {
        let physical = node.absolute_position(0.0, 0.0);
        match self.axis {
            FlowAxis::Y => (physical.y, physical.y + layout.size.height),
            FlowAxis::X if self.reversed => (
                self.measure - (physical.x + layout.size.width),
                self.measure - physical.x,
            ),
            FlowAxis::X => (physical.x, physical.x + layout.size.width),
        }
    }

    fn collect_table_line_boxes(&self, node_id: NodeId, out: &mut Vec<(f32, f32)>) {
        let node = &self.doc.nodes[node_id];
        if let Some(text) = node
            .element_data()
            .and_then(|element| element.inline_layout_data.as_deref())
        {
            let layout = node.final_layout();
            let origin = self.inline_origin(node, &layout, text, 0.0);
            let scale = text.layout.scale();
            out.extend(text.layout.lines().filter_map(|line| {
                let metrics = line.metrics();
                let start = origin
                    + metrics
                        .block_min_coord
                        .min(metrics.content_block_min_coord)
                        / scale;
                let end = origin
                    + metrics
                        .block_max_coord
                        .max(metrics.content_block_max_coord)
                        / scale;
                (end > start).then_some((start, end))
            }));
        }

        let children = node.layout_children.borrow();
        if let Some(children) = children.as_ref() {
            for &child in children.iter() {
                self.collect_table_line_boxes(child, out);
            }
        }
    }

    /// A table lays its cells out itself, so the page walk does not see the
    /// line boxes inside them. Cut the table only where every cell in one row
    /// has finished a line: otherwise one cell's glyph column is sliced even
    /// though another cell happened to offer a legal edge.
    fn table_edges(
        &mut self,
        table: &crate::layout::table::TableContext,
        breakable: bool,
    ) {
        if !breakable {
            return;
        }
        let mut row_cells = vec![Vec::new(); table.rows.len()];
        for cell in &table.cells {
            if let Some(cells) = row_cells.get_mut(cell.row as usize) {
                cells.push(cell);
            }
        }
        for (row_index, cells) in row_cells.into_iter().enumerate() {
            let mut lines = Vec::new();
            let mut row_start = f32::INFINITY;
            let mut row_end = f32::NEG_INFINITY;
            for cell in cells.iter() {
                let node = &self.doc.nodes[cell.node_id];
                let layout = node.final_layout();
                let (start, end) = self.flow_box(node, &layout);
                row_start = row_start.min(start);
                row_end = row_end.max(end);
            }
            if !row_start.is_finite() || row_end <= row_start {
                continue;
            }

            // CSS Tables 3 §6.1 keeps a row whole when no cell spans a later row
            // and the row is at most half the fragmentainer in both directions.
            // A rowspan is read from the DOM because the table context keeps only
            // the cell's origin row, not the cells it covers later.
            let has_rowspan = cells.iter().any(|cell| {
                self.doc.nodes[cell.node_id]
                    .attr(local_name!("rowspan"))
                    .and_then(|span| span.trim().parse::<u16>().ok())
                    .is_some_and(|span| span != 1)
            });
            let has_caption = cells.iter().any(|cell| cell.caption);
            let row_group = table.rows.get(row_index).and_then(|row| row.group);
            let group_avoids = row_group
                .and_then(|group| self.doc.nodes[group].primary_styles())
                .is_some_and(|styles| avoids_inside(styles.clone_break_inside()));
            let preserve = has_caption
                || group_avoids
                || (!has_rowspan
                    && self.fragmentainer_depth.zip(self.fragmentainer_cross).is_some_and(
                        |(depth, cross)| {
                            let row_size = row_end - row_start;
                            row_size <= depth * 0.5 && row_size <= cross * 0.5
                        },
                    ));
            if preserve {
                self.units.push(Unit::whole(row_start, row_end));
                self.push(row_end, Rank::Legal, breakable);
                continue;
            }

            for cell in cells {
                self.collect_table_line_boxes(cell.node_id, &mut lines);
            }
            lines.retain_mut(|(start, end)| {
                *start = start.clamp(row_start, row_end);
                *end = end.clamp(row_start, row_end);
                *end > *start
            });
            lines.sort_by(|left, right| left.0.total_cmp(&right.0));
            let mut merged = Vec::<(f32, f32)>::new();
            for (start, end) in lines {
                match merged.last_mut() {
                    Some(last) if start <= last.1 + 0.01 => last.1 = last.1.max(end),
                    _ => merged.push((start, end)),
                }
            }

            let mut cursor = row_start;
            for (start, end) in merged {
                if cursor < start {
                    self.units.push(Unit::splittable(cursor, start));
                }
                self.push(start, Rank::Legal, breakable);
                self.units.push(Unit::whole(start.max(cursor), end));
                self.push(end, Rank::Legal, breakable);
                cursor = cursor.max(end);
            }
            if cursor < row_end {
                self.units.push(Unit::splittable(cursor, row_end));
            }
            self.push(row_end, Rank::Legal, breakable);
        }

        // A table with only captions still has its own box between them. That
        // box is not a row, so none of the edges above describe it; without
        // this unit a column would jump from the top caption straight to the
        // bottom one and the table box would lose its middle fragment.
        if !table.cells.iter().any(|cell| !cell.caption) {
            let mut grid_start = f32::NEG_INFINITY;
            let mut grid_end = f32::INFINITY;
            for cell in table.cells.iter().filter(|cell| cell.caption) {
                let node = &self.doc.nodes[cell.node_id];
                let layout = node.final_layout();
                let (start, end) = self.flow_box(node, &layout);
                if cell.caption_top {
                    grid_start = grid_start.max(end);
                } else {
                    grid_end = grid_end.min(start);
                }
            }
            if grid_start.is_finite() && grid_end.is_finite() && grid_end > grid_start {
                self.units.push(Unit::splittable(grid_start, grid_end));
                self.push(grid_start, Rank::Legal, breakable);
                self.push(grid_end, Rank::Legal, breakable);
            }
        }
    }

    fn subtree_has_forced_break(&self, node_id: NodeId, before: bool) -> bool {
        let node = &self.doc.nodes[node_id];
        let forced = node.primary_styles().is_some_and(|styles| {
            let value = if before {
                styles.clone_break_before()
            } else {
                styles.clone_break_after()
            };
            rank_between(value) == Some(Rank::Forced)
        });
        if forced {
            return true;
        }
        let children = node.layout_children.borrow();
        children.as_ref().is_some_and(|children| {
            children
                .iter()
                .any(|&child| self.subtree_has_forced_break(child, before))
        })
    }

    fn push(&mut self, at: f32, rank: Rank, breakable: bool) {
        self.push_at(at, rank, breakable, false);
    }

    /// As [`Self::push`], recording that a `widows`/`orphans` count is what bars the point.
    /// Balancing asks which candidate heights would force such a cut.
    fn push_at(&mut self, at: f32, rank: Rank, breakable: bool, by_counts: bool) {
        if breakable || rank == Rank::Forced {
            self.edges.push(Edge {
                at,
                rank,
                by_counts,
            });
        }
    }

    /// Cut the recorded flow into fragmentainers `extent` deep.
    fn bands(mut self, start: f32, extent: f32, cross: f32, content_end: f32) -> ThinVec<Band> {
        self.edges.sort_by(|a, b| a.at.total_cmp(&b.at));
        self.units.sort_by(|a, b| a.start.total_cmp(&b.start));
        self.spans.sort_by(|a, b| a.start.total_cmp(&b.start));
        #[cfg(debug_assertions)]
        if std::env::var_os("CUOTUO_DEBUG_FRAGMENTATION").is_some() {
            eprintln!("fragmentation units: {:?}", self.units);
            eprintln!("fragmentation spans: {:?}", self.spans);
        }
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
        .find(|unit| {
            !unit.freely_fragmentable
                && unit.start > cursor
                && unit.start < limit
                && unit.end > limit
                && unit.height() <= extent
        })
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
    // Each pass advances to at least the next recorded edge or whole unit, so
    // their count bounds the number of fragmentainers. Counting pages by
    // `content_end / extent` is not an upper bound: a page may stop well before
    // its edge to keep a line whole, and a long vertical flow then hit the cap
    // with content still to print.
    let limit_pages = ((content_end / extent.max(1.0)).ceil() as usize + 2)
        .max(edges.len() + units.len() + 2);
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
    let too_tall: Vec<f32> = units
        .iter()
        .filter(|unit| unit.flex_item && unit.box_height() > extent && !has_inside(unit))
        .map(|unit| unit.start)
        .collect();
    let mut dangling_starts: Vec<f32> = units
        .iter()
        .filter(|unit| {
            !unit.flex_item
                && !unit.freely_fragmentable
                && unit.box_height() > extent
                && !has_inside(unit)
        })
        .map(|unit| unit.start)
        .collect();
    dangling_starts.sort_by(|a, b| a.total_cmp(b));
    dangling_starts.dedup();
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
                    let dangling = dangling_starts
                        .binary_search_by(|at| at.total_cmp(&edge.at))
                        .is_ok()
                        && !units.iter().any(|unit| {
                            unit.start < edge.at - 0.5 && unit.end > cursor + 0.5
                        });
                    if too_tall.binary_search_by(|at| at.total_cmp(&edge.at)).is_err()
                        && !dangling
                    {
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

        // A break model that can fragment inside a freely splittable stretch
        // (today a freely fragmentable table row) may use the page's full
        // extent. A legal box edge earlier in the flow must not move such a
        // row whole just because it happens to be the last recorded edge.
        let split_ahead = units.iter().any(|unit| {
            unit.freely_fragmentable
                && unit.start < limit
                && unit.end > limit
        });
        let cut = match forced {
            Some(at) => at,
            None if content_end <= limit => content_end,
            None if split_ahead => limit,
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
    axis: FlowAxis,
    reversed: bool,
    measure: f32,
    fragmentainer_depth: Option<f32>,
) -> (Vec<Edge>, Vec<Unit>, Vec<CloneSpan>, f32) {
    let mut planner = Planner {
        doc,
        unrounded: true,
        axis,
        // A column sequence is measured along its own block axis and direction.
        reversed,
        measure,
        fragmentainer_depth,
        fragmentainer_cross: None,
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
        Edge {
            at,
            rank,
            by_counts: false,
        }
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
                by_counts: true,
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

    /// A too-tall box's start is not a useful break point when it cannot fit an empty
    /// fragmentainer either. Taking it would print an empty page instead of filling this one.
    #[test]
    fn too_tall_to_carry_ignores_its_start_edge() {
        let edges = [edge(0.0, Rank::Legal), edge(2.0, Rank::Legal)];
        let units = [Unit::whole(2.0, 40.0)];
        let bands = cut_bands(&edges, &units, &[], 0.0, 10.0, 800.0, 40.0);
        assert_eq!((bands[0].start, bands[0].end), (0.0, 10.0));
        assert_eq!((bands[1].start, bands[1].end), (10.0, 20.0));
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

    /// Two units that **overlap** along the flow are what a wrapping flex container's parallel lines
    /// look like to the cut: a box per line, at the same extent. One of them could be moved on, both
    /// cannot, so the cut goes through them. That is what `flexbox-unbreakable-child-2` depends on
    /// today, and the reason a wrapping container cannot be fragmented by treating its items as
    /// blocks along the flow (round 57 measured the two variants: each fixed two pairs and broke this
    /// one). A per-band layout has to carry or cut the lines together; until then this stands, and
    /// the test says which decision the corpus was waiting on rather than only that pairs got worse.
    #[test]
    fn parallel_units_are_cut_rather_than_one_being_carried() {
        let edges = [edge(0.0, Rank::Legal)];
        let units = [Unit::whole(0.0, 40.0), Unit::whole(4.0, 44.0)];
        let bands = cut_bands(&edges, &units, &[], 0.0, 30.0, 800.0, 44.0);
        assert_eq!((bands[0].start, bands[0].end), (0.0, 30.0));
        assert_eq!(bands.len(), 2);
    }
}

/// Compute the page plan for the flow rooted at `root_id`, or `None` when the flow cannot be
/// fragmented yet.
fn page_plan(doc: &BaseDocument, root_id: NodeId) -> Option<BreakPlan> {
    // Which way the document's own flow runs. A vertical writing mode blocks along x, so its pages are
    // cut across the paper and stack sideways; `vertical-rl` runs *against* x, which the walk settles
    // once (see [`FlowAxis::along`]) and painting answers by right-aligning each page's content (see
    // [`BreakPlan::fragment_span`]). A sideways mode is neither, and is not paginated yet.
    //
    // EPUBs commonly put `writing-mode: vertical-rl` on `<body>` while leaving
    // `<html>` horizontal. The body is then the principal block flow that has to
    // be cut, even though the root reports the initial horizontal mode.
    #[cfg(feature = "writing-mode")]
    let (axis, reversed) = {
        let root_mode = doc.nodes[root_id].writing_mode();
        let mode = if root_mode.is_vertical() {
            root_mode
        } else {
            doc.nodes[root_id]
                .children
                .iter()
                .find(|&&child| {
                    doc.nodes[child]
                        .element_data()
                        .is_some_and(|element| &*element.name.local == "body")
                })
                .map(|&body| doc.nodes[body].writing_mode())
                .filter(|mode| mode.is_vertical())
                .unwrap_or(root_mode)
        };
        if mode.is_vertical_rl() {
            (FlowAxis::X, true)
        } else if mode.is_vertical_lr() {
            (FlowAxis::X, false)
        } else if mode.is_vertical() {
            return None;
        } else {
            (FlowAxis::Y, false)
        }
    };
    #[cfg(not(feature = "writing-mode"))]
    let (axis, reversed) = (FlowAxis::Y, false);

    let page = doc.page_box?;
    let area = page.area();
    #[cfg(debug_assertions)]
    if std::env::var_os("CUOTUO_DEBUG_FRAGMENTATION").is_some() {
        eprintln!("page box: {:?} area {:?} axis {:?}", page, area, axis);
    }
    // The fragmentainer's depth is the measure the flow runs along; the page's other dimension is
    // what it crosses.
    let depth = match axis {
        FlowAxis::Y => area.height,
        FlowAxis::X => area.width,
    };
    let cross = match axis {
        FlowAxis::Y => area.width,
        FlowAxis::X => area.height,
    };
    if depth <= 1.0 {
        return None;
    }

    let root_page = doc.nodes[root_id]
        .primary_styles()
        .map(|styles| styles.clone_page())
        .unwrap_or(PageName::Auto);
    // What a reversed flow's coordinates count from: the **far edge**, in absolute layout
    // coordinates, of the space the root was laid out in — not the root's own width. A
    // `vertical-rl` root is itself placed against the far side of that space (240px of content in an
    // 800px viewport sits at x 560), so counting from the width instead shifts the whole document off
    // the canvas and paints nothing at all.
    let root_layout = doc.nodes[root_id].final_layout();
    let flow_measure = match axis {
        FlowAxis::Y => root_layout.size.height,
        FlowAxis::X => root_layout.location.x + root_layout.size.width,
    };
    let mut planner = Planner {
        doc,
        unrounded: false,
        axis,
        reversed,
        measure: flow_measure,
        fragmentainer_depth: Some(depth),
        fragmentainer_cross: Some(cross),
        edges: Vec::new(),
        units: Vec::new(),
        spans: Vec::new(),
        extent: 0.0,
        page: root_page.clone(),
        seen_content: false,
    };
    // A percentage asks for the container's *inline* measure: the page area's width for a horizontal
    // flow, its height for a vertical one.
    let inline_size = match axis {
        FlowAxis::Y => area.width,
        FlowAxis::X => area.height,
    };
    // The walk starts where the root box actually sits, not at the origin: a `vertical-rl` root is
    // placed against the far side of the space (x 560 above), and the coordinates the plan cuts must
    // be the same ones painting sees — paint shifts absolute positions, so a flow coordinate measured
    // from the root's own corner would put every page off the canvas. For a horizontal root, at y 0,
    // this is the origin it has always been given.
    planner.walk(
        root_id,
        root_layout.location,
        inline_size,
        true,
        false,
        &root_page,
    );
    // Where the content ends, and not the bottom of the first page: a flow shorter than a page
    // that asks for a break after its last box still gets one page, not two.
    //
    // The boxes anchored to this container count too. A box positioned against the page sequence is
    // not one of the block sequence — the walk passes it by — but it stands in the flow the pages
    // fragment, and its content is printed on as many of them as it needs: a document whose only
    // content is a four-inch abspos box holding eight inches of green prints on the pages the green
    // fills (`monolithic-overflow-027-print`, "four green pages", and `overflowing-block-002-print`,
    // whose abspos content "is printed in two pages").
    let content_end = planner
        .extent
        .max(anchored_extent(doc, root_id, axis, reversed, flow_measure));

    /// How far along the flow the boxes anchored to `container` reach, and their own anchored
    /// descendants with them. `measure` is the far edge a reversed flow counts from; unused otherwise.
    fn anchored_extent(
        doc: &BaseDocument,
        container: crate::NodeId,
        axis: FlowAxis,
        reversed: bool,
        measure: f32,
    ) -> f32 {
        let mut far: f32 = 0.0;
        let hoisted = doc.nodes[container].hoisted_children.borrow();
        for &child in hoisted.iter() {
            let node = &doc.nodes[child];
            let layout = node.final_layout();
            let margin = node
                .layout_style()
                .margin()
                .resolve_or_zero(None, resolve_calc_value);
            let (start, own, ink) = match axis {
                FlowAxis::Y => (
                    layout.location.y,
                    layout.size.height,
                    layout.scrollable_overflow_rect.bottom,
                ),
                FlowAxis::X => (
                    layout.location.x,
                    layout.size.width,
                    layout.scrollable_overflow_rect.right,
                ),
            };
            let reached = (start + own + axis.end_edge(reversed, margin)).max(ink);
            // Content that leaves the box travels with it, so the ink is what has to be printed. A
            // reversed flow reads its coordinates from the far edge, so the box reaching furthest
            // *along* it is the one whose physical extent lies furthest *back*.
            let along_flow = if reversed { measure - start } else { reached };
            far = far
                .max(along_flow)
                .max(anchored_extent(doc, node.id, axis, reversed, measure));
        }
        far
    }

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

    let mut bands = planner.bands(0.0, depth, cross, content_end);
    for (index, band) in bands.iter_mut().enumerate() {
        band.origin = page.area_origin(index, axis);
    }

    #[cfg(debug_assertions)]
    if std::env::var_os("CUOTUO_DEBUG_FRAGMENTATION").is_some() {
        let dump = bands
            .iter()
            .map(|b| {
                let at = match axis {
                    FlowAxis::Y => b.origin.y,
                    FlowAxis::X => b.origin.x,
                };
                format!("[{:.1}..{:.1}]@{:.1}", b.start, b.end, at)
            })
            .collect::<Vec<_>>()
            .join(" ");
        eprintln!("fragmentation bands: {dump} (content end {content_end:.1})");
    }

    Some(BreakPlan {
        axis,
        stride: match axis {
            FlowAxis::Y => page.size.height,
            FlowAxis::X => page.size.width,
        },
        bands,
        reversed,
        measure: flow_measure,
        depth,
        flow_end: content_end,
        // A page's bands already carry the page's absolute position, and the content laid out into a
        // page starts at that band, so there is nothing to subtract: the shift is the origin itself.
        cross_start: 0.0,
    })
}
