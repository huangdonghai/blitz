mod background;
mod border;
mod box_shadow;
mod clip_path;
mod form_controls;
mod mask;

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use super::kurbo_css::CssBox;
use crate::color::{Color, ToColorColor};
use crate::debug_overlay::render_debug_overlay;
use crate::filters::convert_filters;
use crate::kurbo_css::NonUniformRoundedRectRadii;
use crate::layers::LayerManager;
use crate::sizing::compute_object_fit;
use crate::text::DrawTextContext;
use crate::{CustomWidgetSceneMap, SELECTION_COLOR};
use anyrender::{PaintScene, Scene};
use blitz_dom::node::{
    ListItemLayout, ListItemLayoutPosition, Marker, NodeData, RasterImageData, TextInputData,
    TextNodeData,
};
use blitz_dom::{BaseDocument, ElementData, FlowAxis, Node, NodeId, local_name};
use blitz_traits::devtools::DevtoolSettings;

use style::values::computed::{BorderCornerRadius, ColorOrAuto};
use style::{
    dom::TElement,
    properties::{
        ComputedValues, generated::longhands::visibility::computed_value::T as StyloVisibility,
        style_structs::Font,
    },
    values::{
        computed::{CSSPixelLength, Contain, Overflow},
        specified::box_::{DisplayInside, DisplayOutside},
        specified::image::ImageRendering,
    },
};

use kurbo::{self, Affine, Insets, Point, Rect, Shape, Size, Stroke, Vec2};
use peniko::{self, Fill, ImageData, ImageSampler};
use style::values::generics::color::GenericColor;
use taffy::Layout;

/// A view of the track positions reported by taffy in physical (left-to-right /
/// top-to-bottom) order. Taffy reports tracks in logical order, which for RTL grid
/// containers is physically reversed.
#[derive(Clone, Copy)]
pub(crate) struct PhysicalTracks<'a> {
    positions: &'a [taffy::Line<f32>],
    reversed: bool,
}

impl<'a> PhysicalTracks<'a> {
    pub(crate) fn from_tracks<S: taffy::CheapCloneStr>(
        tracks: &'a taffy::DetailedGridTracksInfo<S>,
    ) -> Self {
        let positions = tracks.positions.as_slice();
        let reversed = positions
            .first()
            .zip(positions.last())
            .is_some_and(|(first, last)| first.start > last.start);
        Self {
            positions,
            reversed,
        }
    }

    fn get(self, index: usize) -> taffy::Line<f32> {
        if self.reversed {
            self.positions[self.positions.len() - 1 - index]
        } else {
            self.positions[index]
        }
    }

    /// Iterate track positions in physical order
    pub(crate) fn iter(self) -> impl ExactSizeIterator<Item = taffy::Line<f32>> + 'a {
        (0..self.positions.len()).map(move |index| self.get(index))
    }

    /// The physical start position of the first track
    pub(crate) fn origin(self) -> f32 {
        if self.positions.is_empty() {
            0.0
        } else {
            self.get(0).start
        }
    }

    /// Total distance from the start of the first track to the end of the last track
    pub(crate) fn span(self) -> f32 {
        if self.positions.is_empty() {
            0.0
        } else {
            self.get(self.positions.len() - 1).end - self.get(0).start
        }
    }
}

/// A short-lived struct which holds a bunch of parameters for rendering a scene so
/// that we don't have to pass them down as parameters
pub struct BlitzDomPainter<'dom, 'a> {
    /// Input parameters (read only) for generating the Scene
    pub(crate) dom: &'dom BaseDocument,
    pub(crate) scale: f64,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) initial_x: f64,
    pub(crate) initial_y: f64,
    /// The id of the document's root element (cached to avoid re-resolving it for every element)
    pub(crate) root_element_id: Option<NodeId>,
    /// Scrollbar hover/drag state, resolved once per scene like the root element
    #[cfg(feature = "scrollbars")]
    pub(crate) hovered_scrollbar: Option<blitz_dom::node::ScrollbarRef>,
    #[cfg(feature = "scrollbars")]
    pub(crate) scrollbar_drag_target: Option<blitz_dom::node::ScrollbarRef>,
    pub(crate) layer_manager: LayerManager,
    /// Reusable scratch allocations shared by all text layouts painted for this document.
    pub(crate) draw_text_context: RefCell<DrawTextContext>,
    /// Cached selection ranges for O(1) lookup: node_id -> (start_offset, end_offset)
    pub(crate) selection_ranges: HashMap<NodeId, (usize, usize)>,

    // Pre-computed `Scene`s for each CustomWidget
    pub(crate) custom_widget_scenes: &'a CustomWidgetSceneMap,
}

impl<'dom, 'a> BlitzDomPainter<'dom, 'a> {
    /// Create a new BlitzDomPainter for the given document
    pub fn new(
        dom: &'dom BaseDocument,
        scale: f64,
        width: u32,
        height: u32,
        initial_x: f64,
        initial_y: f64,
        custom_widget_scenes: &'a CustomWidgetSceneMap,
    ) -> Self {
        let selection_ranges: HashMap<NodeId, (usize, usize)> = dom
            .get_text_selection_ranges()
            .into_iter()
            .map(|(node_id, start, end)| (node_id, (start, end)))
            .collect();

        let layer_manager = LayerManager::default();
        let root_element_id = dom.try_root_element().map(|el| el.id);

        Self {
            dom,
            scale,
            width,
            height,
            initial_x,
            initial_y,
            root_element_id,
            #[cfg(feature = "scrollbars")]
            hovered_scrollbar: dom.hovered_scrollbar(),
            #[cfg(feature = "scrollbars")]
            scrollbar_drag_target: dom.scrollbar_drag_target(),
            layer_manager,
            draw_text_context: RefCell::new(DrawTextContext::default()),
            selection_ranges,
            custom_widget_scenes,
        }
    }

    /// Draw the current tree to current render surface
    /// Eventually we'll want the surface itself to be passed into the render function, along with things like the viewport
    ///
    /// This assumes styles are resolved and layout is complete.
    /// Make sure you do those before trying to render
    pub fn paint_scene(&self, scene: &mut impl PaintScene) {
        if self.dom.has_pending_critical_resources() {
            return;
        }

        // Simply render the document (the root element (note that this is not the same as the root node)))
        // scene.reset();
        let viewport_scroll = self.dom.as_ref().viewport_scroll();

        // A document without a root element (e.g. an empty iframe sub-document) has
        // nothing to paint.
        let Some(root_element) = self.dom.as_ref().try_root_element() else {
            return;
        };
        let root_id = root_element.id;
        let bg_width = (self.width as f32).max(root_element.final_layout().size.width);
        let bg_height = (self.height as f32).max(root_element.final_layout().size.height);

        let background_color = {
            let html_color = root_element
                .primary_styles()
                .map(|s| s.clone_background_color())
                .unwrap_or(GenericColor::TRANSPARENT_BLACK);
            if html_color == GenericColor::TRANSPARENT_BLACK {
                root_element
                    .children
                    .iter()
                    .find_map(|id| {
                        self.dom
                            .as_ref()
                            .get_node(*id)
                            .filter(|node| node.data.is_element_with_tag_name(&local_name!("body")))
                    })
                    .and_then(|body| body.primary_styles())
                    .map(|style| {
                        let current_color = style.clone_color();
                        style
                            .clone_background_color()
                            .resolve_to_absolute(&current_color)
                    })
            } else {
                let current_color = root_element.primary_styles().unwrap().clone_color();
                Some(html_color.resolve_to_absolute(&current_color))
            }
        };

        if let Some(bg_color) = background_color {
            let bg_color = bg_color.as_srgb_color();
            let rect = Rect::from_origin_size(
                (self.initial_x, self.initial_y),
                (bg_width as f64, bg_height as f64),
            );
            scene.fill(Fill::NonZero, Affine::IDENTITY, bg_color, None, &rect);
        }

        // The root clip rectangle is the viewport (in screen coordinates, with the
        // initial offset already subtracted). Elements outside of this are culled, and
        // scrollports narrow this rectangle further for their descendants.
        let viewport_clip_rect = Rect::new(0.0, 0.0, self.width as f64, self.height as f64);

        let scroll = Vec2 {
            x: self.initial_x - (viewport_scroll.x * self.scale),
            y: self.initial_y - (viewport_scroll.y * self.scale),
        };
        match root_element.break_plan.as_deref() {
            None => self.render_element(
                scene,
                root_id,
                Affine::translate(scroll),
                viewport_clip_rect,
                false,
                None,
            ),
            Some(plan) => {
                // A fragmented flow is painted once per fragmentainer: the band's slice of the
                // flow shifted to its page box and clipped to it. Shifting and clipping is all
                // fragmentation needs of paint — a box broken across two pages loses its far-edge
                // decorations to the clip, which is `box-decoration-break: slice`.
                let last = plan.bands.len().saturating_sub(1);
                for (index, band) in plan.bands.iter().enumerate() {
                    let shift = plan.translate(band);
                    let page = plan.fragment_rect(band);
                    let band_rect = Rect::new(
                        page.left as f64 * self.scale,
                        page.top as f64 * self.scale,
                        page.right as f64 * self.scale,
                        page.bottom as f64 * self.scale,
                    );
                    let clip = band_rect.intersect(viewport_clip_rect);
                    if clip.area() <= 0.0 {
                        continue;
                    }
                    // The clip has to be a real layer: culling only drops boxes that fall
                    // entirely outside, and a box that straddles the break line paints whole
                    // unless it is cut.
                    scene.push_clip_layer(Fill::NonZero, Affine::IDENTITY, &clip);
                    self.render_element(
                        scene,
                        root_id,
                        Affine::translate(Vec2 {
                            x: scroll.x + shift.x as f64 * self.scale,
                            y: scroll.y + shift.y as f64 * self.scale,
                        }),
                        clip,
                        true,
                        Some(Fragmentainer {
                            rect: band_rect,
                            axis: plan.axis,
                            outer_start: index == 0,
                            outer_end: index == last,
                        }),
                    );
                    scene.pop_layer();
                }
            }
        }

        // Render debug overlay
        if self.dom.devtools().highlight_hover {
            if let Some(node_id) = self.dom.as_ref().get_hover_node_id() {
                render_debug_overlay(
                    scene,
                    self.dom,
                    node_id,
                    self.scale,
                    self.initial_x,
                    self.initial_y,
                );
            }
        }
        if let Some(node_id) = self.dom.devtools().highlight_node {
            // Only nodes with element data have a layout to visualise
            if self
                .dom
                .as_ref()
                .get_node(node_id)
                .is_some_and(|node| node.element_data().is_some())
            {
                render_debug_overlay(
                    scene,
                    self.dom,
                    node_id,
                    self.scale,
                    self.initial_x,
                    self.initial_y,
                );
            }
        }
    }

    /// Renders a node, but is guaranteed that the node is an element
    /// This is because the font_size is calculated from layout resolution and all text is rendered directly here, instead
    /// of a separate text stroking phase.
    ///
    /// In Blitz, text styling gets its attributes from its container element/resolved styles
    /// In other libraries, text gets its attributes from a `text` element - this is not how HTML works.
    ///
    /// Approaching rendering this way guarantees we have all the styles we need when rendering text with not having
    /// to traverse back to the parent for its styles, or needing to pass down styles
    fn render_element(
        &self,
        scene: &mut impl PaintScene,
        node_id: NodeId,
        parent_style_transform: Affine,
        clip_rect: Rect,
        band_applied: bool,
        frag: Option<Fragmentainer>,
    ) {
        let node = &self.dom.as_ref().tree()[node_id];

        // Early return if the element is hidden
        if matches!(node.taffy_display(), taffy::Display::None) {
            return;
        }

        // Only draw elements with a style
        let Some(styles) = node.primary_styles() else {
            return;
        };

        // Hide inputs with type=hidden
        // Implemented here rather than using the style engine for performance reasons
        if node.local_name() == "input" && node.attr(local_name!("type")) == Some("hidden") {
            return;
        }

        // Hide elements with a visibility style other than visible
        if styles.get_inherited_box().visibility != StyloVisibility::Visible {
            return;
        }

        let effects = styles.get_effects();
        let opacity = effects.opacity;
        if opacity == 0.0 {
            return;
        }
        let has_opacity = opacity < 1.0;

        // TODO: account for overflow_x vs overflow_y
        let overflow_x = styles.get_box().overflow_x;
        let overflow_y = styles.get_box().overflow_y;
        // `contain: paint` (and stronger values like `strict`/`content`) clips the element's
        // contents to its padding box. Paint containment does not apply to non-atomic inlines
        // or internal table boxes other than table-cell.
        let contain_paint = styles.get_box().clone_contain().contains(Contain::PAINT) && {
            let display = styles.clone_display();
            let is_internal_table_box_other_than_cell = display.outside()
                == DisplayOutside::InternalTable
                && display.inside() != DisplayInside::TableCell;
            !display.is_inline_flow() && !is_internal_table_box_other_than_cell
        };
        let is_image = node
            .element_data()
            .and_then(|e| e.raster_image_data())
            .is_some();
        let is_sub_doc = node
            .element_data()
            .and_then(|el| el.sub_doc_data())
            .is_some();
        let is_text_input = node
            .element_data()
            .and_then(|el| el.text_input_data())
            .is_some();
        // The root element's overflow is propagated to the viewport (which is clipped by the
        // window/surface bounds), so the root element must not clip its own overflow.
        let is_root_element = self.root_element_id == Some(node_id);
        let mut should_clip = !is_root_element
            && (is_image
                || is_sub_doc
                || is_text_input
                || contain_paint
                || !matches!(overflow_x, Overflow::Visible)
                || !matches!(overflow_y, Overflow::Visible));

        // Apply padding/border offset to inline root
        let taffy::Layout {
            size,
            border,
            padding,
            location,
            ..
        } = *node.final_layout();
        let box_position = Vec2::new(location.x as f64, location.y as f64) * self.scale;
        let box_size = Size::new(size.width as f64, size.height as f64);
        let border_box = Rect::from_origin_size(box_position.to_point(), box_size);
        let scaled_pb = (padding + border).map(f64::from);
        let content_position = kurbo::Point {
            x: scaled_pb.left,
            y: scaled_pb.top,
        };
        let content_box_size = kurbo::Size {
            width: (size.width as f64 - scaled_pb.left - scaled_pb.right) * self.scale,
            height: (size.height as f64 - scaled_pb.top - scaled_pb.bottom) * self.scale,
        };

        // Don't render things that are out of view
        let overflow = *node.scrollable_overflow();
        // CSS Writing Modes 3 §9.1.3: a `text-combine-upright` composition may
        // advance no more than one em. Its contents are laid out at their
        // natural width inside the one-em square, so contents that overflow are
        // compressed — geometrically here — and centred in the square (§9.1.2).
        // The adjustment is expressed in the element's own coordinates (CSS px,
        // origin at its border box), so it composes to the right of the box's
        // own transform.
        let combine_transform = match text_combine_adjustment(node, self.scale) {
            Some((factor, dx)) => {
                let centre = Vec2 {
                    x: box_size.width * 0.5,
                    y: box_size.height * 0.5,
                };
                // Only the advance has to fit one em (CSS Writing Modes 3
                // 9.1.3), so the contents are compressed horizontally and keep
                // their height — the same as Chrome, whose composition is a
                // one-em advance at the natural glyph height rather than a
                // uniformly shrunk square.
                Affine::translate(centre)
                    * Affine::scale_non_uniform(factor, 1.0)
                    * Affine::translate(-centre)
                    * Affine::translate(Vec2 { x: dx, y: 0.0 })
            }
            None => Affine::IDENTITY,
        };

        let transform = parent_style_transform
            * Affine::translate(box_position)
            * node.transform().as_deref().copied().unwrap_or_default()
            * combine_transform;

        // A box that a fragmentainer cuts, and that asks for `box-decoration-break: clone`, is drawn
        // as its fragment: its own border box shortened to the band, and its content moved inside
        // the border and padding the break added.
        let piece = clone_fragment(node, node.final_layout(), transform, frag, self.scale);
        let transform = match &piece {
            Some(piece) => transform.then_translate(piece.moved),
            None => transform,
        };
        // A fragment cuts its own content to what stands inside the border and padding the break
        // added, whatever the box's overflow asked for. The root element's overflow belongs to the
        // viewport rather than to itself, and the band already cuts it.
        if piece.is_some() && !is_root_element {
            should_clip = true;
        }

        let screen_transform = Affine::translate(Vec2 {
            x: -self.initial_x,
            y: -self.initial_y,
        }) * transform;
        let screen_bbox = screen_transform.transform_rect_bbox(overflow.union(border_box));

        // Cull elements that fall entirely outside the current clip rectangle. In addition to
        // the viewport, `clip_rect` is narrowed by any ancestor scrollport (see below), so this
        // also culls elements scrolled out of view inside a clipping/scrolling container.
        //
        // An inline root is left to the per-line cull in `text::stroke_text`, which knows how far
        // a line's ink reaches. Its box says nothing about that: a `position: relative` offset
        // moves text out of its box, and a decoration is as thick as the page asks (`4em` is a
        // real value), so judging the element by its border box and scrollable overflow dropped
        // text that does paint inside the clip. That is what a WPT case with a thick underline
        // inside `overflow: hidden` was catching.
        let cull_by_box = !node.flags.is_inline_root();
        if std::env::var_os("CUOTUO_DEBUG_CULL").is_some() {
            eprintln!(
                "node {:?} inline_root={} cull_by_box={} bbox={:?} clip={:?}",
                node.id,
                node.flags.is_inline_root(),
                cull_by_box,
                screen_bbox,
                clip_rect
            );
        }
        if cull_by_box
            && (screen_bbox.x1 < clip_rect.x0
                || screen_bbox.x0 > clip_rect.x1
                || screen_bbox.y1 < clip_rect.y0
                || screen_bbox.y0 > clip_rect.y1)
        {
            return;
        }

        // Cull elements whose transformed geometry is not representable in f32 (e.g.
        // `transform: scale(99e99)`): the renderer works in f32 and flattening such paths
        // can attempt to allocate an unbounded number of line segments.
        let f32_max = f64::from(f32::MAX);
        if !screen_bbox.is_finite()
            || [
                screen_bbox.x0,
                screen_bbox.y0,
                screen_bbox.x1,
                screen_bbox.y1,
            ]
            .iter()
            .any(|c| c.abs() > f32_max)
        {
            return;
        }

        // Optimise zero-area (/very small area) clips by not rendering at all
        let clip_area = content_box_size.width * content_box_size.height;
        let overflow_area =
            node.scrollable_overflow().width() * node.scrollable_overflow().height();
        if should_clip && clip_area < 0.01 && overflow_area < 0.01 {
            return;
        }

        #[cfg(feature = "custom-widget")]
        let custom_widget_scene = self.custom_widget_scenes.get(&(self.dom.id(), node_id));
        #[cfg(not(feature = "custom-widget"))]
        let custom_widget_scene = None;

        // Apply CSS transform property (where transforms are 2d)

        let mut cx = self.element_cx(
            node,
            *node.final_layout(),
            transform,
            clip_rect,
            band_applied,
            frag,
            custom_widget_scene,
        );

        // The fragment is a box: its border box is what the band left of the original, so every
        // decoration is drawn around it, and its content is cut to what stands inside them.
        if let Some(piece) = &piece {
            cx.frame = CssBox::new(
                piece.rect,
                cx.frame.border_width,
                cx.frame.padding_width,
                cx.frame.outline_width,
                cx.frame.border_radii.clone(),
            );
            cx.frag = Some(piece.frag);
        }

        // If this element clips its overflow it establishes a scrollport: narrow the clip
        // rectangle passed to descendants to the visible (clipped) region so that content
        // scrolled out of view is culled rather than drawn and clipped away. The box used
        // here matches the clip applied to the content below.
        let child_clip_rect = if should_clip {
            let clip_box = if is_text_input || piece.is_some() {
                cx.frame.content_box_path()
            } else {
                cx.frame.padding_box_path()
            };
            clip_rect.intersect(screen_transform.transform_rect_bbox(clip_box.bounding_box()))
        } else {
            clip_rect
        };
        cx.clip_rect = child_clip_rect;

        // Compute clip-path (if any) and wrap all rendering in a clip layer
        let clip_path_shape = cx.clip_path_shape();
        let has_clip_path = clip_path_shape.is_some();
        let default_clip = cx.frame.border_box_path();
        let mut clip_path_for_layer = clip_path_shape.unwrap_or(default_clip);
        clip_path_for_layer.apply_affine(Affine::scale(self.scale));

        // CSS 2 `clip` property (absolutely positioned elements only). Applies to the whole of
        // the element's rendering (including outlines and shadows), so it is the outermost layer.
        let css_clip_rect = cx.css_clip_rect();
        if css_clip_rect.is_some_and(|rect| rect.is_zero_area()) {
            return;
        }
        let css_clip_layer_pushed = self.layer_manager.maybe_push_layer(
            scene,
            css_clip_rect.is_some(),
            1.0,
            cx.transform,
            &css_clip_rect.unwrap_or(Rect::ZERO),
            None,
            None,
        );

        cx.draw_outline(scene);
        cx.draw_outset_box_shadow(scene);

        // clip-path clip ayer
        self.layer_manager.maybe_with_layer(
            scene,
            has_clip_path,
            1.0,
            cx.transform,
            &clip_path_for_layer,
            None,
            None,
            |scene| {
                // If the element has a CSS `mask`, then push an isolation layer for the
                // masked content. The mask is applied when the layer is popped below.
                let mask_layer_pushed = cx.maybe_push_css_mask_layer(scene);
                // `cx.transform` is mutated to apply scroll offsets while drawing content.
                // Save it so that the mask can be drawn untransformed by scroll offsets.
                let unscrolled_transform = cx.transform;

                let filter = convert_filters(&effects.filter.0).map(Arc::new);
                let backdrop_filter = convert_filters(&effects.backdrop_filter.0).map(Arc::new);

                // Adjust effect layer clip by filter expansion area
                //
                // Returns a rectangle centered at the origin representing how much the filter
                // expands the processing region in each direction. The rect coordinates are:
                // - x0: negative left expansion
                // - y0: negative top expansion
                // - x1: positive right expansion
                // - y1: positive bottom expansion
                let filter_expansion_area = filter
                    .as_ref()
                    .map(|f| f.expansion_rect())
                    .unwrap_or(Rect::ZERO);

                let mut effect_layer_clip = cx.frame.border_box_path().bounding_box();
                effect_layer_clip.x0 += filter_expansion_area.x0;
                effect_layer_clip.y0 += filter_expansion_area.y0;
                effect_layer_clip.x1 += filter_expansion_area.x1;
                effect_layer_clip.y1 += filter_expansion_area.y1;

                // Opacity/Filter layer if box has opacity or a filter.
                // Clipped to border-box as it needs to include the background and borders.
                self.layer_manager.maybe_with_layer(
                    scene,
                    has_opacity || filter.is_some() || backdrop_filter.is_some(),
                    opacity,
                    cx.transform,
                    &effect_layer_clip,
                    filter,
                    backdrop_filter,
                    |scene| {
                        cx.draw_background(scene);
                        cx.draw_inset_box_shadow(scene);
                        cx.draw_table_row_backgrounds(scene);
                        cx.draw_table_borders(scene);
                        cx.draw_border(scene);
                        cx.stroke_devtools(scene);

                        // TODO: allow layers with opacity to be unclipped (overflow: visible)
                        // A fragment is cut to its own content box: the border and padding the break
                        // cloned sit between that box and the band, and content belongs to neither.
                        let clip = if is_text_input || piece.is_some() {
                            &cx.frame.content_box_path()
                        } else {
                            &cx.frame.padding_box_path()
                        };

                        // Clip layer if box requires clipping. Opacity set to 1.0
                        self.layer_manager.maybe_with_layer(
                            scene,
                            should_clip,
                            1.0, // opacity
                            cx.transform,
                            clip,
                            None,
                            None,
                            |scene| {
                                // Now that background has been drawn, offset pos and cx in order to draw our contents scrolled
                                let content_position = Point {
                                    x: content_position.x - node.scroll_offset().x,
                                    y: content_position.y - node.scroll_offset().y,
                                };

                                cx.transform = cx.transform.then_translate(Vec2 {
                                    x: -node.scroll_offset().x * self.scale,
                                    y: -node.scroll_offset().y * self.scale,
                                });
                                // The content of a fragment stands inside the border and padding
                                // its cut added, so it slides along the flow from where the box
                                // would have drawn it.
                                if let Some(piece) = &piece {
                                    cx.transform = cx.transform.then_translate(piece.shift);
                                }
                                cx.draw_content_fragmented(
                                    scene,
                                    content_position,
                                    child_clip_rect,
                                );
                            },
                        );

                        // Overlay scrollbars, drawn unscrolled above the
                        // clipped content.
                        #[cfg(feature = "scrollbars")]
                        {
                            cx.transform = unscrolled_transform;
                            cx.draw_scrollbars(scene);
                        }
                    },
                );

                // Apply the CSS `mask` (if any) to the content drawn above
                cx.transform = unscrolled_transform;
                cx.maybe_pop_css_mask_layer(scene, mask_layer_pushed);
            },
        );

        self.layer_manager
            .maybe_pop_layer(scene, css_clip_layer_pushed);
    }

    fn render_node(
        &self,
        scene: &mut impl PaintScene,
        node_id: NodeId,
        parent_style_transform: Affine,
        clip_rect: Rect,
        frag: Option<Fragmentainer>,
    ) {
        let node = &self.dom.as_ref().tree()[node_id];

        match &node.data {
            NodeData::Element(_) | NodeData::AnonymousBlock(_) => self.render_element(
                scene,
                node_id,
                parent_style_transform,
                clip_rect,
                false,
                frag,
            ),
            NodeData::Text(TextNodeData { .. }) => {
                // Text nodes should never be rendered directly
                // (they should always be rendered as part of an inline layout)
                // unreachable!()
            }
            NodeData::Document(_) => {}
            // NodeData::Doctype => {}
            NodeData::Comment { .. } => {} // NodeData::ProcessingInstruction { .. } => {}
        }
    }

    fn element_cx(
        &'dom self,
        node: &'dom Node,
        layout: Layout,
        transform: Affine,
        clip_rect: Rect,
        band_applied: bool,
        frag: Option<Fragmentainer>,
        custom_widget_scene: Option<&'a Scene>,
    ) -> ElementCx<'dom, 'a> {
        let style = node
            .stylo_element_data()
            .primary_styles()
            .as_ref()
            .map(|styles| (*styles).clone())
            .unwrap_or_else(|| {
                ComputedValues::initial_values_with_font_override(Font::initial_values()).to_arc()
            });

        let scale = self.scale;

        // todo: maybe cache this so we don't need to constantly be figuring it out
        // It is quite a bit of math to calculate during render/traverse
        // Also! we can cache the bezpaths themselves, saving us a bunch of work
        let frame = create_css_rect(&style, &layout, scale);

        let element = node.element_data().unwrap();

        ElementCx {
            context: self,
            frame,
            scale,
            style,
            node,
            element,
            transform,
            clip_rect,
            band_applied,
            frag,
            #[cfg(feature = "svg")]
            svg: match element.image_data() {
                Some(blitz_dom::node::ImageData::Svg(svg)) => Some(svg),
                _ => None,
            },
            text_input: element.text_input_data(),
            list_item: element.list_item_data.as_deref(),
            devtools: self.dom.devtools(),
            custom_widget_scene,
        }
    }
}

fn to_image_quality(image_rendering: ImageRendering) -> peniko::ImageQuality {
    match image_rendering {
        ImageRendering::Auto => peniko::ImageQuality::Medium,
        ImageRendering::CrispEdges => peniko::ImageQuality::Low,
        ImageRendering::Pixelated => peniko::ImageQuality::Low,
    }
}

/// The transform from an SVG's rendered tree to a viewport of the given size.
#[cfg(feature = "svg")]
fn svg_viewport_affine(svg: &blitz_dom::node::SvgImageData, width: f32, height: f32) -> Affine {
    let t = svg.viewport_transform(width, height);
    Affine::new([t.sx, t.ky, t.kx, t.sy, t.tx, t.ty].map(f64::from))
}

/// Ensure that the `resized_image` field has a correctly sized image
fn to_peniko_image(image: &RasterImageData, quality: peniko::ImageQuality) -> peniko::ImageBrush {
    peniko::ImageBrush {
        image: ImageData {
            data: image.data.clone(),
            format: peniko::ImageFormat::Rgba8,
            width: image.width,
            height: image.height,
            alpha_type: peniko::ImageAlphaType::Alpha,
        },
        sampler: ImageSampler {
            x_extend: peniko::Extend::Repeat,
            y_extend: peniko::Extend::Repeat,
            quality,
            alpha: 1.0,
        },
    }
}

/// A context of loaded and hot data to draw the element from
struct ElementCx<'dom, 'a> {
    context: &'dom BlitzDomPainter<'dom, 'a>,
    frame: CssBox,
    style: style::servo_arc::Arc<ComputedValues>,
    scale: f64,
    node: &'dom Node,
    element: &'dom ElementData,
    transform: Affine,
    /// The visible region, in screen coordinates: what descendants may be culled
    /// against (see [`ElementCx::local_cull_rect`]).
    clip_rect: Rect,
    /// Whether the caller already placed this box in one of its fragmentainers. A page sequence
    /// is cut by [`BlitzDomPainter::paint_scene`] and a column box by
    /// [`ElementCx::draw_content_fragmented`]; either way the content below is drawn once per
    /// fragment, and cutting it again would move and clip it twice.
    band_applied: bool,
    /// The fragmentainer this box's content is cut by, when the box stands in a fragmented flow.
    /// A box that cloned its own decoration replaces it with its fragment's content box.
    frag: Option<Fragmentainer>,
    #[cfg(feature = "svg")]
    svg: Option<&'dom blitz_dom::node::SvgImageData>,
    text_input: Option<&'dom TextInputData>,
    list_item: Option<&'dom ListItemLayout>,
    devtools: &'dom DevtoolSettings,
    #[cfg_attr(not(feature = "custom-widget"), expect(unused))]
    custom_widget_scene: Option<&'a Scene>,
}

/// Converts parley BoundingBox into peniko Rect
fn convert_rect(rect: &parley::BoundingBox) -> kurbo::Rect {
    peniko::kurbo::Rect::new(rect.x0, rect.y0, rect.x1, rect.y1)
}

/// Compression for a `text-combine-upright` composition (CSS Writing Modes 3
/// §9.1.3).
///
/// The style adjuster lays the composition out as a one-em inline-block with a
/// horizontal writing mode, so its contents keep their natural width and
/// overflow the square. Returns the scale that fits them into one em, plus the
/// horizontal offset (CSS px) that centres them in the square.
fn text_combine_adjustment(node: &Node, scale: f64) -> Option<(f64, f64)> {
    if !node.is_text_combined() {
        return None;
    }
    let natural = node
        .element_data()?
        .inline_layout_data
        .as_ref()?
        .layout
        .lines()
        .next()?
        .metrics()
        .advance as f64;
    let one_em = node
        .primary_styles()?
        .get_font()
        .clone_font_size()
        .computed_size
        .0
        .px() as f64
        // The line advance is in layout units, which scale with the device
        // pixel ratio; the computed font size is in CSS pixels.
        * scale;
    // The offset is applied in the element's own coordinates (CSS pixels),
    // while the advance is in layout units.
    (natural > one_em).then(|| (one_em / natural, -(natural - one_em) / 2.0 / scale))
}

/// How far outside its own border box an element draws: an outline sits beyond the box by its
/// width plus the offset that moves it out, and an outset shadow reaches by its spread plus the
/// blur that softens it, further on the side its offset points to.
///
/// A fragmentainer cuts the flow where a break falls; it does not cut the decoration of a box the
/// break left whole, which is why Chrome shows a spanning box's outline over the gutter it would
/// otherwise be clipped to, and the sides of a box's shadow past the edge of the column that holds
/// it (`box-shadow-002`). The insets are outward, in CSS pixels, and none is ever negative.
fn decoration_bleed(node: &Node) -> Insets {
    use style::values::computed::{BorderStyle, OutlineStyle};
    let Some(styles) = node.primary_styles() else {
        return Insets::ZERO;
    };
    let mut bleed = Insets::ZERO;
    let outline = styles.get_outline();
    if let OutlineStyle::BorderStyle(style) = outline.outline_style {
        if !matches!(style, BorderStyle::None | BorderStyle::Hidden) {
            let amount = (f64::from(outline.outline_width.0.to_f32_px())
                + outline.outline_offset.to_f64_px())
            .max(0.0);
            bleed = widest(bleed, Insets::uniform(amount));
        }
    }
    for shadow in styles
        .get_effects()
        .box_shadow
        .0
        .iter()
        .filter(|shadow| !shadow.inset)
    {
        // The same reach the shadow is drawn with: a blur spreads two and a half times its radius.
        let reach = f64::from(shadow.spread.px()) + f64::from(shadow.base.blur.px()) * 2.5;
        let x = f64::from(shadow.base.horizontal.px());
        let y = f64::from(shadow.base.vertical.px());
        bleed = widest(
            bleed,
            Insets::new(
                (reach - x).max(0.0),
                (reach - y).max(0.0),
                (reach + x).max(0.0),
                (reach + y).max(0.0),
            ),
        );
    }
    bleed
}

/// The reach two decorations have outside the box, edge by edge.
fn widest(one: Insets, other: Insets) -> Insets {
    Insets::new(
        one.x0.max(other.x0),
        one.y0.max(other.y0),
        one.x1.max(other.x1),
        one.y1.max(other.y1),
    )
}

/// Whether the out-of-flow children of a box take part in a fragmentainer.
///
/// A box positioned against a multi-column container as a whole is not part of the flow the
/// container cut into columns, so it belongs to no band: sliding it along with each band paints one
/// box once per column at a different place, and clipping it to a band hides the rest of it
/// (`nested-oofs-in-relative-multicol`, `out-of-flow-in-multicolumn-094`). Such a box is drawn
/// against the container itself — while a box that stands inside a *fragmented* box's own flow,
/// whose containing block is one of its bands, does move with that band and is left in place by
/// [`OofChildren::All`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum OofChildren {
    /// Draw them with everything else, against the band the parent was drawn into.
    All,
    /// Leave them to the fragmented box's own pass.
    Skip,
    /// Draw only them, against the box as a whole.
    Only,
}

/// Whether a box asks that the decoration of each of its fragments be drawn whole — which is why
/// such a box may bleed past a break: wherever the band cut it, it draws its own edge there.
fn clones_decorations(node: &Node) -> bool {
    use style::properties::generated::longhands::box_decoration_break::computed_value::T as BoxDecorationBreak;
    node.primary_styles().is_some_and(|styles| {
        matches!(
            styles.get_border().box_decoration_break,
            BoxDecorationBreak::Clone
        )
    })
}

/// One fragmentainer, as the paint of the flow it holds needs to know it: where it cuts, and where
/// the flow simply ends.
#[derive(Clone, Copy)]
struct Fragmentainer {
    /// The band's own rectangle on the canvas, before any decoration is allowed past it.
    rect: Rect,
    /// The axis the flow advances along, which is the axis a box is cut on.
    axis: FlowAxis,
    /// Whether the near edge along the flow is where the flow starts, and the same at the far edge.
    /// A box may bleed what it draws outside its border box past an end of the flow, but never past
    /// a break, because the next fragmentainer is what lies there.
    outer_start: bool,
    outer_end: bool,
}

impl Fragmentainer {
    /// The rectangle one box of the flow is cut to: the fragmentainer widened by what that box
    /// draws outside its own border box along the cross axis, and along the flow only where the
    /// flow ends — or, for a box that clones its decoration, wherever it was cut.
    fn clip_for(&self, bleed: Insets, scale: f64, cloned: bool) -> Rect {
        let Insets { x0, y0, x1, y1 } = bleed;
        let (x0, y0, x1, y1) = (x0 * scale, y0 * scale, x1 * scale, y1 * scale);
        let (start, end) = if cloned {
            (true, true)
        } else {
            (self.outer_start, self.outer_end)
        };
        match self.axis {
            FlowAxis::Y => Rect::new(
                self.rect.x0 - x0,
                self.rect.y0 - if start { y0 } else { 0.0 },
                self.rect.x1 + x1,
                self.rect.y1 + if end { y1 } else { 0.0 },
            ),
            // A vertical flow runs left to right for now; `vertical-rl` will ask for the other end.
            FlowAxis::X => Rect::new(
                self.rect.x0 - if start { x0 } else { 0.0 },
                self.rect.y0 - y0,
                self.rect.x1 + if end { x1 } else { 0.0 },
                self.rect.y1 + y1,
            ),
        }
    }
}

/// What one box looks like cut by a fragmentainer when it asks for `box-decoration-break: clone`:
/// the fragment is decorated as its own box, and its content moves inside the border and padding
/// the cut adds.
struct Piece {
    /// The fragment's own border box, in coordinates whose origin is the fragment's near corner.
    rect: Rect,
    /// How far the box's own drawing origin moves to the fragment's.
    moved: Vec2,
    /// How much further its content moves in from the cut, along the flow.
    shift: Vec2,
    /// The fragmentainer the content is cut to: the fragment's content box, because the border and
    /// padding the clone adds at the cut sit between the fragment's own edge and its content.
    frag: Fragmentainer,
}

/// The fragment a fragmentainer cuts out of a box that asks for its decorations cloned, or `None`
/// when the box is drawn as itself.
///
/// `transform` maps the box's own coordinates — its border box standing at the origin — on to the
/// canvas, where the band has already slid it. A box the band leaves whole needs nothing of this:
/// its own decorations are its fragment's.
fn clone_fragment(
    node: &Node,
    layout: &Layout,
    transform: Affine,
    frag: Option<Fragmentainer>,
    scale: f64,
) -> Option<Piece> {
    if !clones_decorations(node) {
        return None;
    }
    let frag = frag?;
    // Only a box that reaches the canvas by sliding its own coordinates along it has a fragment
    // that is a box: under a rotated or scaled ancestor the cut would have to be taken in that
    // space rather than this one.
    let [sx, ky, kx, sy, _, _] = transform.as_coeffs();
    if sx != 1.0 || ky != 0.0 || kx != 0.0 || sy != 1.0 {
        return None;
    }
    let on_canvas = transform.transform_rect_bbox(Rect::from_origin_size(
        Point::ZERO,
        Size::new(
            layout.size.width as f64 * scale,
            layout.size.height as f64 * scale,
        ),
    ));
    let hit = on_canvas.intersect(frag.rect);
    // A fragmentainer cuts along the flow only: a box wider than the column it stands in is not
    // broken for that, so the cross axis keeps the box's own measure.
    let piece = match frag.axis {
        FlowAxis::Y => Rect::new(on_canvas.x0, hit.y0, on_canvas.x1, hit.y1),
        FlowAxis::X => Rect::new(hit.x0, on_canvas.y0, hit.x1, on_canvas.y1),
    };
    // Where the band's edges fell inside the box. A break adds a border and padding to the
    // fragment; an end of the flow adds nothing, because the box's own edge is already there.
    let (cut_start, cut_end) = match frag.axis {
        FlowAxis::Y => (
            on_canvas.y0 < frag.rect.y0 - 0.5,
            on_canvas.y1 > frag.rect.y1 + 0.5,
        ),
        FlowAxis::X => (
            on_canvas.x0 < frag.rect.x0 - 0.5,
            on_canvas.x1 > frag.rect.x1 + 0.5,
        ),
    };
    // A box the band does not reach along the flow is not fragmenting here at all: some other band
    // holds it, and this one must not draw a line where its edge clipped the box to nothing.
    let depth = match frag.axis {
        FlowAxis::Y => piece.height(),
        FlowAxis::X => piece.width(),
    };
    if depth <= 0.5 {
        return None;
    }
    if !(cut_start || cut_end) {
        return None;
    }
    // The border and padding the box puts at the near end of its own extent along the flow.
    let near = match frag.axis {
        FlowAxis::Y => (layout.border.top + layout.padding.top) as f64 * scale,
        FlowAxis::X => (layout.border.left + layout.padding.left) as f64 * scale,
    };
    // The content that stood at the flow line the cut fell on moves in by the border and padding
    // the clone adds there — but not past the box's own edge, which already had them.
    let into = piece.origin() - on_canvas.origin();
    let shift = match frag.axis {
        FlowAxis::Y => Vec2::new(0.0, into.y.min(near)),
        FlowAxis::X => Vec2::new(into.x.min(near), 0.0),
    };
    let moved = piece.origin() - on_canvas.origin();
    // The fragment's content box on the canvas: the decoration transform slides the fragment's own
    // origin, which is where `piece` already sits.
    let (left, top) = (
        (layout.border.left + layout.padding.left) as f64 * scale,
        (layout.border.top + layout.padding.top) as f64 * scale,
    );
    let (right, bottom) = (
        (layout.border.right + layout.padding.right) as f64 * scale,
        (layout.border.bottom + layout.padding.bottom) as f64 * scale,
    );
    let content = Rect::new(
        piece.x0 + left,
        piece.y0 + top,
        piece.x1 - right,
        piece.y1 - bottom,
    );
    Some(Piece {
        rect: Rect::from_origin_size(Point::ZERO, piece.size()),
        moved,
        shift,
        frag: Fragmentainer {
            // Never wider than the band: the fragment's content box may reach past a column the box
            // itself overflows, and its children are cut to the column, not to the box.
            rect: content.intersect(frag.rect),
            axis: frag.axis,
            // An edge of the content box that is not a cut leads into the box's own padding, which
            // its children may overflow; an edge the break put there cuts them.
            outer_start: !cut_start,
            outer_end: !cut_end,
        },
    })
}

impl ElementCx<'_, '_> {
    /// Paint overlay scrollbar thumbs for scroll containers: `overflow:
    /// scroll`, or `auto` when the content overflows (never `hidden`/`clip`,
    /// which scroll only programmatically). Thumbs appear on scroll and fade
    /// out after a delay ([`BaseDocument::scrollbar_opacity`]); never-scrolled
    /// containers paint nothing, keeping thumbs out of static reftest
    /// screenshots.
    ///
    /// Geometry comes from [`Node::scrollbar_thumb`], shared with the
    /// thumb-drag hit testing in blitz-dom.
    #[cfg(feature = "scrollbars")]
    fn draw_scrollbars(&self, scene: &mut impl PaintScene) {
        // css-scrollbars-1 scrollbar-color: author thumb/track colors
        use blitz_dom::node::{ScrollbarColor, ScrollbarRef};
        use taffy::AbsoluteAxis;
        let (custom_thumb, custom_track) = match self.node.scrollbar_color() {
            ScrollbarColor::Auto => (None, None),
            ScrollbarColor::Colors { thumb, track } => {
                (Some(thumb.as_srgb_color()), Some(track.as_srgb_color()))
            }
        };

        let drag_target = self.context.scrollbar_drag_target;
        let hovered_thumb = self.context.hovered_scrollbar;

        // scrollbar-color doesn't affect overlay visibility: persistence is
        // UA policy, not author styling.
        let node_id = self.node.id;
        let opacity = self.context.dom.scrollbar_opacity(node_id);
        if opacity == 0.0 {
            return;
        }

        // Default thumb palette for the used color scheme; thumbs paint as
        // fill plus a thin contrast stroke so they read over same-colored
        // content.
        let dark_scheme =
            self.context.dom.viewport().color_scheme == blitz_traits::shell::ColorScheme::Dark;
        let (thumb_rest, thumb_hover, thumb_active, stroke_color) = if dark_scheme {
            (
                Color::from_rgba8(214, 214, 214, 178),
                Color::from_rgba8(190, 190, 190, 222),
                Color::from_rgba8(172, 172, 172, 255),
                Color::from_rgba8(0, 0, 0, 102),
            )
        } else {
            (
                Color::from_rgba8(128, 128, 128, 178),
                Color::from_rgba8(152, 152, 152, 222),
                Color::from_rgba8(170, 170, 170, 255),
                Color::from_rgba8(255, 255, 255, 102),
            )
        };

        // Chromium's hovered/pressed scrollbar contrast ratios.
        const HOVER_CONTRAST: f32 = 1.8;
        const ACTIVE_CONTRAST: f32 = 1.3;

        for axis in [AbsoluteAxis::Vertical, AbsoluteAxis::Horizontal] {
            if !self.node.wants_scrollbar(axis) {
                continue;
            }
            let Some(thumb) = self.node.scrollbar_thumb(axis) else {
                continue;
            };

            let rect = thumb.scale_from_origin(self.scale);

            // Track (only when the author specified a track color)
            if let Some(track_color) = custom_track {
                let padding_box = self.frame.padding_box;
                let track_rect = match axis {
                    AbsoluteAxis::Horizontal => {
                        Rect::new(padding_box.x0, rect.y0, padding_box.x1, rect.y1)
                    }
                    AbsoluteAxis::Vertical => {
                        Rect::new(rect.x0, padding_box.y0, rect.x1, padding_box.y1)
                    }
                };
                scene.fill(
                    Fill::NonZero,
                    self.transform,
                    track_color.multiply_alpha(opacity),
                    None,
                    &track_rect,
                );
            }

            let this = ScrollbarRef { node_id, axis };
            let is_active = drag_target == Some(this);
            let is_hovered = hovered_thumb == Some(this);
            let color = match custom_thumb {
                Some(base) if is_active => crate::color::blend_for_contrast(base, ACTIVE_CONTRAST),
                Some(base) if is_hovered => crate::color::blend_for_contrast(base, HOVER_CONTRAST),
                Some(base) => base,
                None if is_active => thumb_active,
                None if is_hovered => thumb_hover,
                None => thumb_rest,
            };
            let radius = match axis {
                AbsoluteAxis::Horizontal => rect.height() / 2.0,
                AbsoluteAxis::Vertical => rect.width() / 2.0,
            };
            scene.fill(
                Fill::NonZero,
                self.transform,
                color.multiply_alpha(opacity),
                None,
                &rect.to_rounded_rect(radius),
            );
            // Contrast stroke, default thumbs only: an author-specified
            // scrollbar-color is rendered exactly as given.
            if custom_thumb.is_none() {
                let stroke_width = self.scale;
                let stroke_rect = rect.inset(-stroke_width / 2.0);
                scene.stroke(
                    &Stroke::new(stroke_width),
                    self.transform,
                    stroke_color.multiply_alpha(opacity),
                    None,
                    &stroke_rect.to_rounded_rect(radius - stroke_width / 2.0),
                );
            }
        }
    }

    /// The visible region in the coordinates this element's glyphs are painted
    /// in, so whole lines of text outside it can be skipped.
    ///
    /// `transform` is the one the glyphs are drawn with — the inline root's own
    /// coordinates to content coordinates — and `clip_rect` is in screen
    /// coordinates, so the region has to be mapped back through the same screen
    /// offset the culling in `render_element` uses.
    fn local_cull_rect(&self, transform: Affine) -> Rect {
        let screen = Affine::translate(Vec2 {
            x: -self.context.initial_x,
            y: -self.context.initial_y,
        }) * transform;
        // A degenerate transform has no visible region to speak of; cull nothing
        // rather than guess at one.
        if screen.determinant().abs() < f64::EPSILON {
            return Rect::new(
                f64::NEG_INFINITY,
                f64::NEG_INFINITY,
                f64::INFINITY,
                f64::INFINITY,
            );
        }
        screen.inverse().transform_rect_bbox(self.clip_rect)
    }

    fn draw_inline_layout(&self, scene: &mut impl PaintScene, pos: Point) {
        if self.node.flags.is_inline_root() {
            let text_layout = self.element
                .inline_layout_data
                .as_ref()
                .unwrap_or_else(|| {
                    panic!("Tried to render node marked as inline root that does not have an inline layout: {:?}", self.node);
                });

            let pos = Point {
                x: pos.x,
                y: pos.y + text_layout.block_offset as f64,
            };
            let transform =
                self.transform * Affine::translate((pos.x * self.scale, pos.y * self.scale));

            // Render inline element backgrounds (e.g. `<span style="background: ...">`)
            // behind the text and selection highlight.
            #[cfg(feature = "writing-mode")]
            let vertical = crate::text::vertical_text(self.context.dom, self.node.id, self.scale);
            #[cfg(not(feature = "writing-mode"))]
            let vertical = None;

            crate::text::draw_inline_backgrounds(
                scene,
                text_layout.layout.lines(),
                self.context.dom,
                transform,
                self.node.id,
                vertical.as_ref(),
            );

            // Render text selection highlight (if any) using cached selection ranges
            if let Some(&(sel_start, sel_end)) = self.context.selection_ranges.get(&self.node.id) {
                // Selection geometry comes from Parley in the writing mode's own
                // axes, so vertical inline roots need the same transpose the
                // glyphs get below.
                #[cfg(feature = "writing-mode")]
                let selection_vertical =
                    crate::text::vertical_text(self.context.dom, self.node.id, self.scale);
                #[cfg(not(feature = "writing-mode"))]
                let selection_vertical = None;

                crate::text::draw_text_selection(
                    scene,
                    &text_layout.layout,
                    transform,
                    sel_start,
                    sel_end,
                    selection_vertical,
                );
            }

            // Render text
            let mut draw_text_context = self.context.draw_text_context.borrow_mut();
            crate::text::stroke_text(
                scene,
                text_layout.layout.lines(),
                self.context.dom,
                transform,
                self.scale,
                self.node.id,
                self.local_cull_rect(transform),
                &mut draw_text_context,
            );
        }
    }

    fn draw_text_input_text(&self, scene: &mut impl PaintScene, pos: Point) {
        // Render the text in text inputs
        if let Some(input_data) = self.text_input {
            // For single-line inputs, add an offset to vertically center the text input layout
            // within the content box of it's node.
            let y_offset = self.node.text_input_v_centering_offset(self.scale);
            let pos = Point {
                x: pos.x,
                y: pos.y + y_offset,
            };

            // Apply the scroll offset (stored in CSS pixels, scaled here to device pixels) so
            // that the caret stays visible. Single-line inputs scroll horizontally; multi-line
            // inputs scroll vertically.
            let scroll_offset = input_data.scroll_offset as f64 * self.scale;
            let (scroll_x, scroll_y) = if input_data.is_multiline {
                (0.0, scroll_offset)
            } else {
                (scroll_offset, 0.0)
            };

            let transform = self.transform
                * Affine::translate((pos.x * self.scale - scroll_x, pos.y * self.scale - scroll_y));

            if self.node.is_focussed() {
                // Render selection/caret
                for (rect, _line_idx) in input_data.editor.selection_geometry().iter() {
                    scene.fill(
                        Fill::NonZero,
                        transform,
                        SELECTION_COLOR,
                        None,
                        &convert_rect(rect),
                    );
                }
                if let Some(cursor) = input_data.editor.cursor_geometry(1.5) {
                    let color = self.style.get_inherited_text().color;
                    let caret_color = match &self.style.get_inherited_ui().caret_color.0 {
                        ColorOrAuto::Auto => color,
                        ColorOrAuto::Color(caret_color) => caret_color.resolve_to_absolute(&color),
                    };

                    scene.fill(
                        Fill::NonZero,
                        transform,
                        caret_color.as_srgb_color(),
                        None,
                        &convert_rect(&cursor),
                    );
                };
            }

            // Render text
            let mut draw_text_context = self.context.draw_text_context.borrow_mut();
            crate::text::stroke_text(
                scene,
                input_data.editor.try_layout().unwrap().lines(),
                self.context.dom,
                transform,
                self.scale,
                self.node.id,
                self.local_cull_rect(transform),
                &mut draw_text_context,
            );
        }
    }

    fn draw_marker(&self, scene: &mut impl PaintScene, pos: Point) {
        if let Some(ListItemLayout {
            marker,
            position: ListItemLayoutPosition::Outside(layout),
        }) = self.list_item
        {
            // Right align and pad the bullet when rendering outside
            let x_padding = match marker {
                Marker::Char(_) => 8.0,
                Marker::String(_) => 0.0,
            };
            // Outside markers are placed outside the list item's border box
            // (`pos` is the origin of its content box)
            let item_layout = self.node.final_layout();
            let x_offset = -(layout.full_width() / layout.scale()
                + x_padding
                + item_layout.padding.left
                + item_layout.border.left);

            // Align the marker with the baseline of the first line of text in the list item
            let y_offset = if let Some((text_layout, first_text_line)) = &self
                .element
                .inline_layout_data
                .as_ref()
                .and_then(|text_layout| Some((text_layout, text_layout.layout.lines().next()?)))
            {
                (first_text_line.metrics().baseline
                    - layout.lines().next().unwrap().metrics().baseline)
                    / layout.scale()
                    + text_layout.block_offset
            } else {
                0.0
            };

            let pos = Point {
                x: pos.x + x_offset as f64,
                y: pos.y + y_offset as f64,
            };

            let transform =
                self.transform * Affine::translate((pos.x * self.scale, pos.y * self.scale));

            let mut draw_text_context = self.context.draw_text_context.borrow_mut();
            crate::text::stroke_text(
                scene,
                layout.lines(),
                self.context.dom,
                transform,
                self.scale,
                self.node.id,
                self.local_cull_rect(transform),
                &mut draw_text_context,
            );
        }
    }

    /// The rule in the gutter of every column that has one.
    ///
    /// A rule is centred on the middle of the gutter — Chrome draws a 4px rule in a 20px gap at
    /// x 98..102 of a column that ends at 90 — and it is as deep as the fragmentainer rather than
    /// as deep as the content that happens to fill it. It belongs to the gutter and not to a
    /// column, so it is drawn outside the per-band clips, which would cut it in half.
    fn draw_column_rules(&self, scene: &mut impl PaintScene) {
        use style::values::computed::BorderStyle;
        // Only a multi-column container has gutters to rule; the plan of a page sequence puts
        // its pages one below the other and its `stride` is the paper, not a column pitch.
        if !self.node.is_multicol_box() {
            return;
        }
        let column = self.style.get_column();
        if matches!(
            column.column_rule_style,
            BorderStyle::None | BorderStyle::Hidden
        ) {
            return;
        }
        let width = column.column_rule_width.0.to_f32_px().max(0.0);
        if width <= 0.0 {
            return;
        }
        let color = column
            .column_rule_color
            .resolve_to_absolute(&self.style.clone_color())
            .as_srgb_color();
        if color == peniko::Color::TRANSPARENT {
            return;
        }
        let Some(plan) = self.node.break_plan.as_deref() else {
            return;
        };
        let left = self.frame.content_box.origin().x as f32;
        for band in plan.bands.iter() {
            // The leftmost fragmentainer of a row — and a box that spans every column — has no
            // gutter to its left.
            let gutter = plan.stride - band.cross;
            if band.origin.x <= left + 0.5 || gutter <= 0.0 {
                continue;
            }
            let centre = band.origin.x - gutter / 2.0;
            let rect = Rect::new(
                (centre - width / 2.0) as f64 * self.scale,
                band.origin.y as f64 * self.scale,
                (centre + width / 2.0) as f64 * self.scale,
                (band.origin.y + band.depth) as f64 * self.scale,
            );
            scene.fill(Fill::NonZero, self.transform, color, None, &rect);
        }
    }

    /// The content of a box — its text, its replaced parts, its children — drawn once, or once
    /// per fragmentainer when the box has a break plan. A fragmented box's own background and
    /// border are *not* repeated: a multi-column container is one box with the columns inside it.
    /// What repeats is the content, shifted and cut to each column.
    fn draw_content_fragmented(
        &mut self,
        scene: &mut impl PaintScene,
        content_position: Point,
        clip_rect: Rect,
    ) {
        let plan = if self.band_applied {
            None
        } else {
            self.node.break_plan.as_deref()
        };
        let Some(plan) = plan else {
            self.draw_content(scene, content_position, clip_rect);
            return;
        };
        let base = self.transform;
        self.draw_column_rules(scene);
        let last = plan.bands.len().saturating_sub(1);
        for (index, band) in plan.bands.iter().enumerate() {
            let shift = plan.translate(band);
            let fragment = plan.fragment_rect(band);
            let own = Rect::new(
                fragment.left as f64 * self.scale,
                fragment.top as f64 * self.scale,
                fragment.right as f64 * self.scale,
                fragment.bottom as f64 * self.scale,
            );
            // Measured against `base`, not the running transform: the loop moves `self.transform`
            // for the band it draws, and a later band's clip would inherit the earlier shift.
            let band_rect = base.transform_rect_bbox(own);
            let clip = band_rect.intersect(clip_rect);
            if clip.area() <= 0.0 {
                continue;
            }
            self.transform = base.then_translate(Vec2 {
                x: shift.x as f64 * self.scale,
                y: shift.y as f64 * self.scale,
            });
            // Culling alone cannot do this: it drops boxes that fall entirely outside the clip,
            // and a box that straddles the column edge would paint whole in both columns.
            scene.push_clip_layer(Fill::NonZero, Affine::IDENTITY, &clip);
            self.draw_own_content(scene, content_position);
            scene.pop_layer();
            // Each child is cut to the fragmentainer in a layer of its own, widened by whatever it
            // draws outside its own box: a break that left the child whole must not shave the
            // outline it draws around itself, nor the side of a shadow that reaches into the gutter.
            let frag = Fragmentainer {
                rect: band_rect,
                axis: plan.axis,
                outer_start: index == 0,
                outer_end: index == last,
            };
            self.draw_children(scene, self.transform, clip_rect, Some(frag), OofChildren::Skip);
        }
        // The boxes positioned against this container as a whole, once, where they were placed —
        // after the bands, so that they cover the columns they are painted over as they do in
        // Chrome. A negative-z out-of-flow child is drawn in the same pass, which is a known
        // simplification.
        self.transform = base;
        self.draw_children(scene, base, clip_rect, None, OofChildren::Only);
    }

    fn draw_content(&self, scene: &mut impl PaintScene, content_position: Point, clip_rect: Rect) {
        self.draw_own_content(scene, content_position);
        // A box of a fragmented flow cuts its own children by the band that cuts it, so that a
        // decoration they draw outside their boxes is not shaved by an ancestor's clip. Its
        // out-of-flow children move with it: their containing block is a fragment of this flow.
        self.draw_children(scene, self.transform, clip_rect, self.frag, OofChildren::All);
    }

    /// What a box draws itself: its text, its replaced parts, its marker. Its children are not
    /// part of this, because a fragmented box cuts them one by one.
    fn draw_own_content(&self, scene: &mut impl PaintScene, content_position: Point) {
        self.draw_image(scene);
        #[cfg(feature = "svg")]
        self.draw_svg(scene);
        #[cfg(feature = "custom-widget")]
        self.draw_custom_widget(scene);
        self.draw_sub_document(scene);
        self.draw_input(scene);
        self.draw_text_input_text(scene, content_position);
        self.draw_inline_layout(scene, content_position);
        self.draw_marker(scene, content_position);
    }

    /// One child of a fragmented box, cut to the fragmentainer the child stands in, widened by the
    /// decoration that child draws outside its own border box.
    fn render_fragment_child(
        &self,
        scene: &mut impl PaintScene,
        child_id: NodeId,
        transform: Affine,
        frag: Fragmentainer,
        clip_rect: Rect,
    ) {
        let child = &self.context.dom.as_ref().tree()[child_id];
        let bleed = decoration_bleed(child);
        let clip = frag
            .clip_for(bleed, self.scale, clones_decorations(child))
            .intersect(clip_rect);
        if clip.area() <= 0.0 {
            return;
        }
        scene.push_clip_layer(Fill::NonZero, Affine::IDENTITY, &clip);
        self.render_node(scene, child_id, transform, clip, Some(frag));
        scene.pop_layer();
    }

    /// The children of a box, in paint order.
    ///
    /// `band` is the fragmentainer a fragmented box is being drawn into: with one, every child is
    /// clipped to it in a layer of its own, so that a child the break left whole keeps the outline
    /// it draws outside its own box. `oof` says what becomes of the children that stand outside the
    /// flow — see [`OofChildren`].
    fn draw_children(
        &self,
        scene: &mut impl PaintScene,
        parent_style_transform: Affine,
        clip_rect: Rect,
        band: Option<Fragmentainer>,
        oof: OofChildren,
    ) {
        let takes_part = |child: &Node| match oof {
            OofChildren::All => true,
            OofChildren::Skip => !child.is_out_of_flow(),
            OofChildren::Only => child.is_out_of_flow(),
        };

        // Negative z_index hoisted nodes

        if let Some(hoisted) = &self.node.stacking_context {
            for hoisted_child in hoisted.neg_z_hoisted_children() {
                if !takes_part(&self.context.dom.as_ref().tree()[hoisted_child.node_id]) {
                    continue;
                }
                let position = hoisted_child.position(self.node.tree(), self.node.id);
                let pos = kurbo::Vec2 {
                    x: position.x as f64 * self.scale,
                    y: position.y as f64 * self.scale,
                };
                let transform = parent_style_transform.pre_translate(pos);
                match band {
                    Some(frag) => self.render_fragment_child(
                        scene,
                        hoisted_child.node_id,
                        transform,
                        frag,
                        clip_rect,
                    ),
                    None => {
                        self.render_node(scene, hoisted_child.node_id, transform, clip_rect, band)
                    }
                }
            }
        }

        // Regular children
        if let Some(children) = &*self.node.paint_children.borrow() {
            for child_id in children {
                // Fixed-position children of the root element are positioned against
                // the viewport and do not scroll with it, so cancel out the viewport
                // scroll (applied in `paint_scene`). A fixed box contained by a
                // transformed ancestor scrolls with it like any out-of-flow box.
                let child = &self.context.dom.as_ref().tree()[*child_id];
                if !takes_part(child) {
                    continue;
                }
                let child_transform = if child.taffy_position() == taffy::Position::Fixed
                    && Some(self.node.id) == self.context.root_element_id
                {
                    let scroll = self.context.dom.as_ref().viewport_scroll();
                    parent_style_transform.pre_translate(kurbo::Vec2 {
                        x: scroll.x * self.scale,
                        y: scroll.y * self.scale,
                    })
                } else {
                    parent_style_transform
                };
                match band {
                    Some(frag) => self.render_fragment_child(
                        scene,
                        *child_id,
                        child_transform,
                        frag,
                        clip_rect,
                    ),
                    None => self.render_node(scene, *child_id, child_transform, clip_rect, band),
                }
            }
        }

        // Positive z_index hoisted nodes
        if let Some(hoisted) = &self.node.stacking_context {
            for hoisted_child in hoisted.pos_z_hoisted_children() {
                if !takes_part(&self.context.dom.as_ref().tree()[hoisted_child.node_id]) {
                    continue;
                }
                let position = hoisted_child.position(self.node.tree(), self.node.id);
                let pos = kurbo::Vec2 {
                    x: position.x as f64 * self.scale,
                    y: position.y as f64 * self.scale,
                };
                let transform = parent_style_transform.pre_translate(pos);
                match band {
                    Some(frag) => self.render_fragment_child(
                        scene,
                        hoisted_child.node_id,
                        transform,
                        frag,
                        clip_rect,
                    ),
                    None => {
                        self.render_node(scene, hoisted_child.node_id, transform, clip_rect, band)
                    }
                }
            }
        }
    }

    #[cfg(feature = "svg")]
    fn draw_svg(&self, scene: &mut impl PaintScene) {
        use style::properties::generated::longhands::object_fit::computed_value::T as ObjectFit;

        let Some(svg) = self.svg else {
            return;
        };

        let width = self.frame.content_box.width() as f32;
        let height = self.frame.content_box.height() as f32;
        let svg_size = svg.tree.size();

        let x = self.frame.content_box.origin().x;
        let y = self.frame.content_box.origin().y;

        // An SVG with an aspect ratio but no natural size is sized by the default sizing
        // algorithm, which for `none` and `scale-down` contains it within the content box.
        let object_fit = match self.style.clone_object_fit() {
            ObjectFit::None | ObjectFit::ScaleDown
                if svg.intrinsic_width().is_none()
                    && svg.intrinsic_height().is_none()
                    && svg.viewbox_aspect_ratio().is_some() =>
            {
                ObjectFit::Contain
            }
            object_fit => object_fit,
        };
        let object_position = self.style.clone_object_position();

        // Apply object-fit algorithm
        let container_size = taffy::Size { width, height };
        let object_size = taffy::Size {
            width: svg_size.width(),
            height: svg_size.height(),
        };
        let paint_size = compute_object_fit(container_size, Some(object_size), object_fit);

        // Compute object-position
        let x_offset = object_position.horizontal.resolve(
            CSSPixelLength::new(container_size.width - paint_size.width) / self.scale as f32,
        ) * self.scale as f32;
        let y_offset = object_position.vertical.resolve(
            CSSPixelLength::new(container_size.height - paint_size.height) / self.scale as f32,
        ) * self.scale as f32;
        let x = x + x_offset.px() as f64;
        let y = y + y_offset.px() as f64;

        let viewport = svg_viewport_affine(svg, paint_size.width, paint_size.height);
        let transform = (self.transform * viewport).then_translate(Vec2 { x, y });

        self.with_replaced_content_clip(
            scene,
            container_size,
            paint_size,
            x_offset.px(),
            y_offset.px(),
            |scene| {
                anyrender_svg::render_svg_tree(scene, &svg.tree, transform);
            },
        );
    }

    /// Clip replaced content to the content box when its object-fit/object-position
    /// rect overflows it.
    fn with_replaced_content_clip<S: PaintScene>(
        &self,
        scene: &mut S,
        container_size: taffy::Size<f32>,
        paint_size: taffy::Size<f32>,
        x_offset: f32,
        y_offset: f32,
        paint: impl FnOnce(&mut S),
    ) {
        let overflows = x_offset < 0.0
            || y_offset < 0.0
            || x_offset + paint_size.width > container_size.width
            || y_offset + paint_size.height > container_size.height;
        self.context.layer_manager.maybe_with_layer(
            scene,
            overflows,
            1.0,
            self.transform,
            &self.frame.content_box_path(),
            None,
            None,
            paint,
        );
    }

    fn draw_image(&self, scene: &mut impl PaintScene) {
        if let Some(image) = self.element.raster_image_data() {
            let width = self.frame.content_box.width() as u32;
            let height = self.frame.content_box.height() as u32;
            let x = self.frame.content_box.origin().x;
            let y = self.frame.content_box.origin().y;

            let object_fit = self.style.clone_object_fit();
            let object_position = self.style.clone_object_position();
            let image_rendering = self.style.clone_image_rendering();
            let quality = to_image_quality(image_rendering);

            // Apply object-fit algorithm
            let container_size = taffy::Size {
                width: width as f32,
                height: height as f32,
            };
            let object_size = taffy::Size {
                width: image.width as f32,
                height: image.height as f32,
            };
            let paint_size = compute_object_fit(container_size, Some(object_size), object_fit);

            // Compute object-position
            let x_offset = object_position.horizontal.resolve(
                CSSPixelLength::new(container_size.width - paint_size.width) / self.scale as f32,
            ) * self.scale as f32;
            let y_offset = object_position.vertical.resolve(
                CSSPixelLength::new(container_size.height - paint_size.height) / self.scale as f32,
            ) * self.scale as f32;
            let x = x + x_offset.px() as f64;
            let y = y + y_offset.px() as f64;

            let x_scale = paint_size.width as f64 / object_size.width as f64;
            let y_scale = paint_size.height as f64 / object_size.height as f64;
            let transform = self
                .transform
                .pre_translate(Vec2 { x, y })
                .pre_scale_non_uniform(x_scale, y_scale);

            self.with_replaced_content_clip(
                scene,
                container_size,
                paint_size,
                x_offset.px(),
                y_offset.px(),
                |scene| {
                    scene.draw_image(to_peniko_image(image, quality).as_ref(), transform);
                },
            );
        }
    }

    #[cfg(feature = "custom-widget")]
    fn draw_custom_widget(&self, scene: &mut impl PaintScene) {
        if let Some(widget_scene) = self.custom_widget_scene {
            let x = self.frame.content_box.origin().x;
            let y = self.frame.content_box.origin().y;
            let transform = self.transform.then_translate(Vec2 { x, y });

            // TODO: eliminate clone
            scene.append_scene(widget_scene.clone(), transform);
        }
    }

    fn draw_sub_document(&self, scene: &mut impl PaintScene) {
        if let Some(sub_doc) = self.element.sub_doc_data().map(|doc| doc.inner()) {
            let scale = self.scale;
            let width = self.frame.content_box.width() as u32;
            let height = self.frame.content_box.height() as u32;

            // TODO: Support arbitrary transforms of subdocuments
            let translation = self.transform.translation();
            let initial_x = translation.x + self.frame.content_box.origin().x;
            let initial_y = translation.y + self.frame.content_box.origin().y;
            // let transform = self.transform.then_translate(Vec2 { x, y });

            let painter = BlitzDomPainter::new(
                &sub_doc,
                scale,
                width,
                height,
                initial_x,
                initial_y,
                self.custom_widget_scenes,
            );
            painter.paint_scene(scene);
        }
    }

    fn stroke_devtools(&self, scene: &mut impl PaintScene) {
        if self.devtools.show_layout {
            let shape = &self.frame.border_box;
            let stroke = Stroke::new(self.scale);

            let stroke_color = match self.node.taffy_display() {
                taffy::Display::Block | taffy::Display::FlowRoot => {
                    Color::new([1.0, 0.0, 0.0, 1.0])
                }
                taffy::Display::Flex => Color::new([0.0, 1.0, 0.0, 1.0]),
                taffy::Display::Grid => Color::new([0.0, 0.0, 1.0, 1.0]),
                taffy::Display::None => Color::new([0.0, 0.0, 1.0, 1.0]),
            };

            scene.stroke(&stroke, self.transform, stroke_color, None, &shape);
        }
    }
}
impl<'dom, 'a> std::ops::Deref for ElementCx<'dom, 'a> {
    type Target = BlitzDomPainter<'dom, 'a>;
    fn deref(&self) -> &Self::Target {
        self.context
    }
}

fn insets_from_taffy_rect(input: taffy::Rect<f64>) -> Insets {
    Insets {
        x0: input.left,
        y0: input.top,
        x1: input.right,
        y1: input.bottom,
    }
}

/// Convert Stylo and Taffy types into Kurbo types
fn create_css_rect(style: &ComputedValues, layout: &Layout, scale: f64) -> CssBox {
    // Resolve and rescale
    // We have to scale since document pixels are not same same as rendered pixels
    let width: f64 = layout.size.width as f64;
    let height: f64 = layout.size.height as f64;
    let border_box = Rect::new(0.0, 0.0, width * scale, height * scale);
    let border = insets_from_taffy_rect(layout.border.map(|p| p as f64 * scale));
    let padding = insets_from_taffy_rect(layout.padding.map(|p| p as f64 * scale));
    let outline_width = style.get_outline().outline_width.0.to_f64_px() * scale;

    // Resolve the radii to a length. need to downscale since the radii are in document pixels
    let resolve_w = CSSPixelLength::new(width as _);
    let resolve_h = CSSPixelLength::new(height as _);
    let resolve_radii = |radius: &BorderCornerRadius| -> Vec2 {
        Vec2 {
            x: scale * radius.0.width.0.resolve(resolve_w).px() as f64,
            y: scale * radius.0.height.0.resolve(resolve_h).px() as f64,
        }
    };
    let s_border = style.get_border();
    let border_radii = NonUniformRoundedRectRadii {
        top_left: resolve_radii(&s_border.border_top_left_radius),
        top_right: resolve_radii(&s_border.border_top_right_radius),
        bottom_right: resolve_radii(&s_border.border_bottom_right_radius),
        bottom_left: resolve_radii(&s_border.border_bottom_left_radius),
    };

    CssBox::new(border_box, border, padding, outline_width, border_radii)
}
