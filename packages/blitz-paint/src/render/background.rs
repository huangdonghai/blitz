use super::{ElementCx, to_image_quality, to_peniko_image};
use crate::color::{Color, ToColorColor};
use crate::gradient::to_peniko_gradient;
use anyrender::PaintScene;
use blitz_dom::node::{ImageData, ImageResourceData, SpecialElementData};
use blitz_dom::{Node, NodeId};
use kurbo::{self, Affine, BezPath, Point, Rect, Shape, Size, Vec2};
use peniko::{self, Fill};
use std::collections::{HashMap, HashSet};
use style::{
    color::AbsoluteColor,
    computed_values::border_collapse::T as BorderCollapse,
    properties::{
        generated::longhands::{
            background_attachment::single_value::computed_value::T as StyloBackgroundAttachment,
            background_clip::single_value::computed_value::T as StyloBackgroundClip,
            background_origin::single_value::computed_value::T as StyloBackgroundOrigin,
            mask_origin::single_value::computed_value::T as StyloMaskOrigin,
        },
        style_structs::{Background, SVG},
    },
    values::{
        computed::{
            BackgroundRepeat, Gradient as StyloGradient, Image as ComputedImage, LengthPercentage,
            background::BackgroundSize,
        },
        generics::image::GenericImage,
        specified::background::BackgroundRepeatKeyword,
    },
};

#[cfg(feature = "tracing")]
use tracing::warn;

/// The block-axis extent of a caption's side of the table, in the table's
/// coordinates. `vertical_rl` decides which physical side is block-start.
fn caption_block_range(
    layout: &taffy::Layout,
    table: &taffy::Layout,
    vertical: bool,
    vertical_lr: bool,
) -> (f64, f64) {
    if vertical {
        let centre = layout.location.x as f64 + layout.size.width as f64 * 0.5;
        let at_start = if vertical_lr {
            centre < table.size.width as f64 * 0.5
        } else {
            centre >= table.size.width as f64 * 0.5
        };
        if at_start {
            if vertical_lr {
                (0.0, (layout.location.x + layout.size.width) as f64)
            } else {
                (layout.location.x as f64, table.size.width as f64)
            }
        } else if vertical_lr {
            (layout.location.x as f64, table.size.width as f64)
        } else {
            (0.0, (layout.location.x + layout.size.width) as f64)
        }
    } else {
        let at_start = (layout.location.y as f64) < table.size.height as f64 * 0.5;
        if at_start {
            (0.0, (layout.location.y + layout.size.height) as f64)
        } else {
            (layout.location.y as f64, table.size.height as f64)
        }
    }
}

/// A box from the CSS box model. Abstracts over the (structurally identical)
/// computed value types of the `background-clip`/`background-origin` and
/// `mask-clip`/`mask-origin` properties.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)] // The variants are named after the CSS keywords
pub(super) enum BoxModelBox {
    BorderBox,
    PaddingBox,
    ContentBox,
}

// Also covers MaskClip as the type is the same
impl From<StyloBackgroundClip> for BoxModelBox {
    fn from(value: StyloBackgroundClip) -> Self {
        match value {
            StyloBackgroundClip::BorderBox => Self::BorderBox,
            StyloBackgroundClip::PaddingBox => Self::PaddingBox,
            StyloBackgroundClip::ContentBox => Self::ContentBox,

            // TODO: support BorderArea
            StyloBackgroundClip::BorderArea => Self::BorderBox,
        }
    }
}

impl From<StyloBackgroundOrigin> for BoxModelBox {
    fn from(value: StyloBackgroundOrigin) -> Self {
        match value {
            StyloBackgroundOrigin::BorderBox => Self::BorderBox,
            StyloBackgroundOrigin::PaddingBox => Self::PaddingBox,
            StyloBackgroundOrigin::ContentBox => Self::ContentBox,
        }
    }
}

impl From<StyloMaskOrigin> for BoxModelBox {
    fn from(value: StyloMaskOrigin) -> Self {
        match value {
            StyloMaskOrigin::BorderBox => Self::BorderBox,
            StyloMaskOrigin::PaddingBox => Self::PaddingBox,
            StyloMaskOrigin::ContentBox => Self::ContentBox,
        }
    }
}

/// The styles and image data for a single layer of a CSS image layer list
/// (`background-image` or `mask-image`). The `background-*` and `mask-*`
/// properties share computed value types, which allows the layer painting code
/// to be shared.
pub(super) struct ImageLayerStyles<'a> {
    /// The computed value of the `background-image`/`mask-image` layer
    pub stylo_image: &'a ComputedImage,
    /// The loaded image resource if `stylo_image` is a `url()` image
    pub image_data: Option<&'a ImageResourceData>,
    pub position_x: &'a LengthPercentage,
    pub position_y: &'a LengthPercentage,
    pub repeat: &'a BackgroundRepeat,
    pub size: &'a BackgroundSize,
    pub clip: BoxModelBox,
    pub origin: BoxModelBox,
    pub attachment: StyloBackgroundAttachment,
}

impl<'a> ImageLayerStyles<'a> {
    pub(super) fn from_background(
        bg_styles: &'a Background,
        image_data: &'a [Option<ImageResourceData>],
        idx: usize,
    ) -> Self {
        Self {
            stylo_image: &bg_styles.background_image.0[idx],
            image_data: image_data.get(idx).and_then(Option::as_ref),
            position_x: get_cyclic(&bg_styles.background_position_x.0, idx),
            position_y: get_cyclic(&bg_styles.background_position_y.0, idx),
            repeat: get_cyclic(&bg_styles.background_repeat.0, idx),
            size: get_cyclic(&bg_styles.background_size.0, idx),
            clip: (*get_cyclic(&bg_styles.background_clip.0, idx)).into(),
            origin: (*get_cyclic(&bg_styles.background_origin.0, idx)).into(),
            attachment: *get_cyclic(&bg_styles.background_attachment.0, idx),
        }
    }

    pub(super) fn from_svg(
        svg_styles: &'a SVG,
        image_data: &'a [Option<ImageResourceData>],
        idx: usize,
    ) -> Self {
        Self {
            stylo_image: &svg_styles.mask_image.0[idx],
            image_data: image_data.get(idx).and_then(Option::as_ref),
            position_x: get_cyclic(&svg_styles.mask_position_x.0, idx),
            position_y: get_cyclic(&svg_styles.mask_position_y.0, idx),
            repeat: get_cyclic(&svg_styles.mask_repeat.0, idx),
            size: get_cyclic(&svg_styles.mask_size.0, idx),
            clip: (*get_cyclic(&svg_styles.mask_clip.0, idx)).into(),
            origin: (*get_cyclic(&svg_styles.mask_origin.0, idx)).into(),
            // There is no `mask-attachment` property
            attachment: StyloBackgroundAttachment::Scroll,
        }
    }
}

impl ElementCx<'_, '_> {
    pub(super) fn draw_background(&self, scene: &mut impl PaintScene) {
        // Table captions are painted once by the table, in grid order. Their
        // own box can be taller than the table fragment and would otherwise
        // paint a later caption over the current fragment's rows.
        if self
            .node
            .parent
            .and_then(|parent| self.context.dom.get_node(parent))
            .and_then(|parent| parent.element_data())
            .is_some_and(|parent| {
                matches!(
                    parent.special_data,
                    SpecialElementData::TableRoot(ref table)
                        if table.cells.iter().any(|cell| cell.caption && cell.node_id == self.node.id)
                )
            })
        {
            return;
        }
        if let Some(grid) = self.table_grid_cx() {
            return grid.draw_background_box(scene);
        }
        self.draw_background_box(scene);
    }

    fn draw_background_box(&self, scene: &mut impl PaintScene) {
        let bg_styles = &self.style.get_background();
        let image_data = &self.element.background_images;
        let layer_count = bg_styles.background_image.0.len();

        // The background color is clipped by the clip of the last layer in the list
        let background_clip: BoxModelBox =
            (*get_cyclic(&bg_styles.background_clip.0, layer_count - 1)).into();
        let background_clip_path = self.box_path(background_clip);

        // Draw background color (if any)
        self.draw_solid_bg(scene, &background_clip_path);

        for idx in (0..layer_count).rev() {
            let layer = ImageLayerStyles::from_background(bg_styles, image_data, idx);
            if layer_paints_nothing(&layer) {
                continue;
            }
            let background_clip_path = self.box_path(layer.clip);

            self.context.layer_manager.maybe_with_layer(
                scene,
                true,
                1.0,
                self.transform,
                &background_clip_path,
                None,
                None,
                |scene| {
                    self.draw_image_layer(scene, &layer);
                },
            );
        }
    }

    /// The path of the given CSS box model box for this element
    pub(super) fn box_path(&self, css_box: BoxModelBox) -> BezPath {
        match css_box {
            BoxModelBox::BorderBox => self.frame.border_box_path(),
            BoxModelBox::PaddingBox => self.frame.padding_box_path(),
            BoxModelBox::ContentBox => self.frame.content_box_path(),
        }
    }

    /// The rect of the given CSS box model box for this element
    fn box_rect(&self, css_box: BoxModelBox) -> Rect {
        match css_box {
            BoxModelBox::BorderBox => self.frame.border_box,
            BoxModelBox::PaddingBox => self.frame.padding_box,
            BoxModelBox::ContentBox => self.frame.content_box,
        }
    }

    /// Draw a single layer of a CSS image layer list (`background-image` or `mask-image`)
    pub(super) fn draw_image_layer(&self, scene: &mut impl PaintScene, layer: &ImageLayerStyles) {
        match layer.stylo_image {
            GenericImage::None => {
                // Do nothing
            }
            GenericImage::Gradient(gradient) => self.draw_gradient_layer(scene, gradient, layer),
            GenericImage::Url(_) => {
                self.draw_raster_image_layer(scene, layer);
                #[cfg(feature = "svg")]
                self.draw_svg_image_layer(scene, layer);
            }
            GenericImage::LightDark(_) => {
                #[cfg(feature = "tracing")]
                warn!("Implement image layer drawing for ImageLightDark")
            }
            GenericImage::PaintWorklet(_) => {
                #[cfg(feature = "tracing")]
                warn!("Implement image layer drawing for Image::PaintWorklet")
            }
            GenericImage::CrossFade(_) => {
                #[cfg(feature = "tracing")]
                warn!("Implement image layer drawing for Image::CrossFade")
            }
            GenericImage::Image(_) => {
                #[cfg(feature = "tracing")]
                warn!("Implement image layer drawing for Image::Image")
            }
            GenericImage::ImageSet(_) => {
                #[cfg(feature = "tracing")]
                warn!("Implement image layer drawing for Image::ImageSet")
            }
        }
    }

    /// Paint the backgrounds of the table's column groups, columns, row groups and rows
    /// (in that order, bottom to top: CSS 2.1 §17.5.1).
    ///
    /// These backgrounds are only painted behind the table's cells (not in the
    /// `border-spacing` between them, nor where a row has no cell), so they are
    /// painted cell by cell. A cell clips the background of each track it
    /// belongs to, while the track's own box positions the gradient so its
    /// phase stays continuous across the cells.
    pub(super) fn draw_table_row_backgrounds(&self, scene: &mut impl PaintScene) {
        let SpecialElementData::TableRoot(table) = &self.element.special_data else {
            return;
        };

        let cell_rect = |cell_id: NodeId| -> Option<Rect> {
            let layout = self.context.dom.get_node(cell_id)?.final_layout();
            Some(Rect::new(
                layout.location.x as f64,
                layout.location.y as f64,
                (layout.location.x + layout.size.width) as f64,
                (layout.location.y + layout.size.height) as f64,
            ))
        };
        let background_color = |node_id: NodeId| -> Option<Color> {
            let node = self.context.dom.get_node(node_id)?;
            let style = node.primary_styles()?;
            let current_color = style.clone_color();
            let color = style
                .get_background()
                .background_color
                .resolve_to_absolute(&current_color)
                .as_srgb_color();
            (color != Color::TRANSPARENT).then_some(color)
        };

        let has_background = |node_id: NodeId| -> bool {
            let Some(node) = self.context.dom.get_node(node_id) else {
                return false;
            };
            let Some(style) = node.primary_styles() else {
                return false;
            };
            let background = style.get_background();
            background_color(node_id).is_some()
                || background
                    .background_image
                    .0
                    .iter()
                    .any(|image| matches!(image, GenericImage::Gradient(_)))
        };

        let mut column_rects: HashMap<u16, Rect> = HashMap::new();
        let mut row_rects: HashMap<u16, Rect> = HashMap::new();
        let mut column_group_rects: HashMap<NodeId, Rect> = HashMap::new();
        let mut row_group_rects: HashMap<NodeId, Rect> = HashMap::new();
        let repeat_rows = |row: u16| {
            table.repeat_header_rows.contains(&row) || table.repeat_footer_rows.contains(&row)
        };
        for cell in table
            .cells
            .iter()
            .filter(|cell| !cell.caption && !repeat_rows(cell.row))
        {
            let Some(rect) = cell_rect(cell.node_id) else {
                continue;
            };
            column_rects
                .entry(cell.column)
                .and_modify(|known| *known = known.union(rect))
                .or_insert(rect);
            row_rects
                .entry(cell.row)
                .and_modify(|known| *known = known.union(rect))
                .or_insert(rect);
            if let Some(group) = table
                .columns
                .get(cell.column as usize)
                .and_then(|column| column.group)
            {
                column_group_rects
                    .entry(group)
                    .and_modify(|known| *known = known.union(rect))
                    .or_insert(rect);
            }
            if let Some(group) = table.rows.get(cell.row as usize).and_then(|row| row.group) {
                row_group_rects
                    .entry(group)
                    .and_modify(|known| *known = known.union(rect))
                    .or_insert(rect);
            }
        }

        // In the collapsed borders model the borders are laid out as gutters between the
        // cells, but a cell's border box (and so its background) extends to the middle of
        // the borders around it. The gutter is split on a whole pixel so that the
        // backgrounds of adjacent cells tile exactly.
        let collapsed = table.border_collapse == BorderCollapse::Collapse;
        let (gutter, bounds) = if collapsed {
            let gap = table.style.gap.map(|gap| gap.into_raw().value() as f64);
            let bounds = table
                .cells
                .iter()
                .filter_map(|cell| cell_rect(cell.node_id))
                .reduce(|a, b| a.union(b));
            (Size::new(gap.width, gap.height), bounds)
        } else {
            (Size::ZERO, None)
        };
        let before = Size::new((gutter.width / 2.0).ceil(), (gutter.height / 2.0).ceil());
        let after = gutter - before;

        for cell in &table.cells {
            if cell.caption || repeat_rows(cell.row) {
                continue;
            }
            let Some(mut rect) = cell_rect(cell.node_id) else {
                continue;
            };
            if let Some(bounds) = bounds {
                rect = Rect::new(
                    rect.x0 - before.width,
                    rect.y0 - before.height,
                    rect.x1 + after.width,
                    rect.y1 + after.height,
                )
                .intersect(bounds);
            }

            let clip = rect.scale_from_origin(self.scale);
            let column = table.columns.get(cell.column as usize);
            let row = table.rows.get(cell.row as usize);
            let column_rect = column_rects
                .get(&cell.column)
                .copied()
                .unwrap_or(rect)
                .scale_from_origin(self.scale);
            let row_rect = row_rects
                .get(&cell.row)
                .copied()
                .unwrap_or(rect)
                .scale_from_origin(self.scale);
            let column_group_rect = column
                .and_then(|column| column.group)
                .and_then(|group| column_group_rects.get(&group))
                .copied()
                .unwrap_or(rect)
                .scale_from_origin(self.scale);
            let row_group_rect = row
                .and_then(|row| row.group)
                .and_then(|group| row_group_rects.get(&group))
                .copied()
                .unwrap_or(rect)
                .scale_from_origin(self.scale);
            let tracks = [
                (column.and_then(|column| column.group), column_group_rect),
                (column.map(|column| column.node_id), column_rect),
                (row.and_then(|row| row.group), row_group_rect),
                (row.map(|row| row.node_id), row_rect),
            ];
            for (node_id, origin_rect) in tracks {
                let Some(node_id) = node_id else {
                    continue;
                };
                if !has_background(node_id) {
                    continue;
                }
                self.draw_table_track_background(scene, node_id, origin_rect, clip);
            }
        }

        // A table row whose children were wrapped in anonymous cells still
        // paints its background over the row track. The table's own grid
        // positions are the only place that height is available once empty
        // rows have been stretched by a specified table block size.
        let grid = table.computed_grid_info.borrow();
        let Some(grid) = grid.as_ref() else {
            return;
        };
        let caption_rows: HashSet<u16> = table
            .cells
            .iter()
            .filter(|cell| cell.caption)
            .map(|cell| cell.row)
            .collect();
        let table_layout = *self.node.final_layout();
        let writing_mode = self.node.writing_mode();
        let vertical = writing_mode.is_vertical();
        let table_bounds = Rect::new(
            0.0,
            0.0,
            table_layout.size.width as f64,
            table_layout.size.height as f64,
        )
        .scale_from_origin(self.scale);
        for (index, row) in table.rows.iter().enumerate() {
            let caption = caption_rows.contains(&(index as u16));
            if row.synthetic
                || row_rects.contains_key(&(index as u16))
                || repeat_rows(index as u16)
            {
                continue;
            }
            let Some(track) = grid.rows.positions.get(index) else {
                continue;
            };
            let block_range = caption.then(|| {
                self.context
                    .dom
                    .get_node(row.node_id)
                    .map(|node| {
                        caption_block_range(
                            node.final_layout(),
                            &table_layout,
                            vertical,
                            writing_mode.is_vertical_lr(),
                        )
                    })
            });
            let rect = if vertical {
                let (x0, x1) =
                    block_range
                        .flatten()
                        .unwrap_or((0.0, table_layout.size.width as f64));
                Rect::new(x0, track.start as f64, x1, track.end as f64)
            } else {
                let (y0, y1) = block_range
                    .flatten()
                    .unwrap_or((track.start as f64, track.end as f64));
                Rect::new(0.0, y0, table_layout.size.width as f64, y1)
            }
            .scale_from_origin(self.scale);
            for node_id in [row.group, Some(row.node_id)].into_iter().flatten() {
                if has_background(node_id) {
                    // A caption is the row's own element, but its 120px block size is the
                    // caption box, not a grid track that may spill outside the table.
                    let clip = if caption {
                        rect.intersect(table_bounds)
                    } else {
                        rect
                    };
                    self.draw_table_track_background(scene, node_id, rect, clip);
                }
            }
        }

        let header_cells: Vec<NodeId> = table
            .cells
            .iter()
            .filter(|cell| table.repeat_header_rows.contains(&cell.row))
            .map(|cell| cell.node_id)
            .collect();
        let footer_cells: Vec<NodeId> = table
            .cells
            .iter()
            .filter(|cell| table.repeat_footer_rows.contains(&cell.row))
            .map(|cell| cell.node_id)
            .collect();
        let body_height = table
            .cells
            .iter()
            .filter(|cell| !cell.caption && !repeat_rows(cell.row))
            .filter_map(|cell| cell_rect(cell.node_id))
            .reduce(|a, b| a.union(b))
            .map_or(0.0, |rect| rect.height());
        self.draw_repeated_table_sections(scene, &header_cells, &footer_cells, body_height);
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_repeated_table_sections(
        &self,
        scene: &mut impl PaintScene,
        header_cells: &[NodeId],
        footer_cells: &[NodeId],
        body_height: f64,
    ) {
        if header_cells.is_empty() && footer_cells.is_empty() {
            return;
        }
        let local_rect = |cell_id: NodeId| -> Option<Rect> {
            let layout = self.context.dom.get_node(cell_id)?.final_layout();
            Some(Rect::new(
                layout.location.x as f64,
                layout.location.y as f64,
                (layout.location.x + layout.size.width) as f64,
                (layout.location.y + layout.size.height) as f64,
            ))
        };
        let group_rect = |cells: &[NodeId]| -> Option<Rect> {
            cells
                .iter()
                .filter_map(|cell| local_rect(*cell))
                .reduce(|a, b| a.union(b))
        };
        let table = self.node.final_layout();
        let header_rect = group_rect(header_cells)
            .unwrap_or_else(|| Rect::new(0.0, 0.0, table.size.width as f64, 0.0));
        let header_height = header_rect.height();
        let footer_height = group_rect(footer_cells).map_or(0.0, |rect| rect.height());

        let clip_top = self.clip_rect.y0 / self.scale;
        let clip_bottom = self.clip_rect.y1 / self.scale;
        let natural_header_top = self
            .transform
            .transform_rect_bbox(header_rect.scale_from_origin(self.scale))
            .y0
            / self.scale;
        let (ordinal, fragment_depth) = self.repeat_fragment_ordinal();
        let start = if ordinal == 0 {
            natural_header_top.max(clip_top)
        } else {
            clip_top
        };
        let available = (clip_bottom - start - header_height - footer_height).max(0.0);
        let base_capacity = (fragment_depth - header_height - footer_height).max(0.0);
        let body_start = ordinal as f64 * base_capacity;
        let capacity = if ordinal == 0 {
            available
        } else {
            base_capacity
        };
        let body_slice = (body_height - body_start).clamp(0.0, capacity);
        if body_start >= body_height && body_height > 0.0 {
            return;
        }

        let natural_header_top = natural_header_top * self.scale;
        let target_top = start * self.scale;
        let header_transform =
            self.transform * Affine::translate(Vec2::new(0.0, target_top - natural_header_top));
        scene.push_clip_layer(Fill::NonZero, Affine::IDENTITY, &self.clip_rect);
        for cell in header_cells {
            self.context
                .render_node(scene, *cell, header_transform, self.clip_rect, self.frag);
        }

        if !footer_cells.is_empty() && body_slice > 0.0 {
            let natural_footer_top = self
                .transform
                .transform_rect_bbox(
                    group_rect(footer_cells)
                        .unwrap_or(header_rect)
                        .scale_from_origin(self.scale),
                )
                .y0;
            let footer_top = (start + header_height + body_slice) * self.scale - natural_footer_top;
            let footer_transform = self.transform * Affine::translate(Vec2::new(0.0, footer_top));
            for cell in footer_cells {
                self.context
                    .render_node(scene, *cell, footer_transform, self.clip_rect, self.frag);
            }
        }
        scene.pop_layer();
    }

    fn repeat_fragment_ordinal(&self) -> (usize, f64) {
        let mut current = self.node.parent;
        let mut plans = Vec::new();
        while let Some(id) = current {
            let Some(node) = self.context.dom.get_node(id) else {
                break;
            };
            if node.is_multicol_box()
                && let Some(plan) = node.break_plan.as_deref()
            {
                plans.push((id, plan));
            }
            current = node.parent;
        }
        let Some((outer_id, outer)) = plans.last().copied() else {
            return (
                self.frag.map_or(0, |frag| frag.index),
                self.clip_rect.height() / self.scale,
            );
        };
        if outer.stride <= 0.0 {
            return (
                self.frag.map_or(0, |frag| frag.index),
                self.clip_rect.height() / self.scale,
            );
        }
        let mut x = 0.0;
        let mut current = Some(outer_id);
        while let Some(id) = current {
            let Some(node) = self.context.dom.get_node(id) else {
                break;
            };
            x += node.final_layout().location.x as f64;
            current = node.parent;
        }
        let clip_x = self.clip_rect.x0 / self.scale;
        let stride = self.clip_rect.width() / self.scale;
        if stride <= 0.0 {
            return (
                self.frag.map_or(0, |frag| frag.index),
                self.clip_rect.height() / self.scale,
            );
        }
        let ordinal = ((clip_x - x) / stride).floor().max(0.0) as usize;
        let depth = outer
            .bands
            .iter()
            .map(|band| band.depth as f64)
            .fold(self.clip_rect.height() / self.scale, f64::max);
        (ordinal, depth)
    }

    fn draw_table_track_background(
        &self,
        scene: &mut impl PaintScene,
        node_id: NodeId,
        origin_rect: Rect,
        clip_rect: Rect,
    ) {
        let Some(node) = self.context.dom.get_node(node_id) else {
            return;
        };
        let Some(style) = node.primary_styles() else {
            return;
        };
        let Some(element) = node.element_data() else {
            return;
        };
        let background = style.get_background();
        let current_color = style.clone_color();
        let color = background
            .background_color
            .resolve_to_absolute(&current_color)
            .as_srgb_color();
        if color != Color::TRANSPARENT {
            scene.fill(Fill::NonZero, self.transform, color, None, &clip_rect);
        }

        let layer_count = background.background_image.0.len();
        for index in (0..layer_count).rev() {
            let layer =
                ImageLayerStyles::from_background(background, &element.background_images, index);
            let GenericImage::Gradient(gradient) = layer.stylo_image else {
                continue;
            };
            let clip = self.transform.transform_rect_bbox(clip_rect);
            scene.push_clip_layer(Fill::NonZero, Affine::IDENTITY, &clip);
            self.draw_gradient_in(
                scene,
                gradient,
                &layer,
                origin_rect,
                self.transform,
                clip_rect,
                origin_rect,
                &current_color,
            );
            scene.pop_layer();
        }
    }

    pub(super) fn draw_table_caption_backgrounds(&self, scene: &mut impl PaintScene) {
        let SpecialElementData::TableRoot(table) = &self.element.special_data else {
            return;
        };
        let captions: Vec<_> = table.cells.iter().filter(|cell| cell.caption).collect();
        if captions.is_empty() {
            return;
        }
        let background = |node: &Node| -> Option<Color> {
            let style = node.primary_styles()?;
            let current = style.clone_color();
            let color = style
                .get_background()
                .background_color
                .resolve_to_absolute(&current)
                .as_srgb_color();
            (color != Color::TRANSPARENT).then_some(color)
        };
        let mut fallback = None;
        let mut current = self.node.parent;
        while let Some(id) = current {
            let Some(node) = self.context.dom.get_node(id) else {
                break;
            };
            if let Some(color) = background(node) {
                fallback = Some(color);
                break;
            }
            current = node.parent;
        }
        let fallback = fallback.unwrap_or(Color::WHITE);
        let table = *self.node.final_layout();
        let writing_mode = self.node.writing_mode();
        for caption in captions {
            let Some(node) = self.context.dom.get_node(caption.node_id) else {
                continue;
            };
            let color = background(node).unwrap_or(fallback);
            let layout = node.final_layout();
            let rect = if writing_mode.is_vertical() {
                let (x0, x1) = caption_block_range(
                    layout,
                    &table,
                    true,
                    writing_mode.is_vertical_lr(),
                );
                Rect::new(x0, 0.0, x1, table.size.height as f64)
            } else {
                let (y0, y1) = caption_block_range(layout, &table, false, false);
                Rect::new(0.0, y0, table.size.width as f64, y1)
            }
            .scale_from_origin(self.scale);
            scene.fill(Fill::NonZero, self.transform, color, None, &rect);
        }
    }

    fn draw_solid_bg(&self, scene: &mut impl PaintScene, shape: &BezPath) {
        let current_color = self.style.clone_color();
        let background_color = &self.style.get_background().background_color;
        let bg_color = background_color
            .resolve_to_absolute(&current_color)
            .as_srgb_color();

        if bg_color != Color::TRANSPARENT {
            // Fill the color
            scene.fill(Fill::NonZero, self.transform, bg_color, None, shape);
        }
    }

    /// Whether the layer is positioned against the viewport
    /// (`background-attachment: fixed`) rather than the element's origin box.
    /// `fixed` behaves as `scroll` on elements affected by a CSS transform
    /// (the transformed element acts as the layer's containing block).
    fn layer_is_fixed(&self, layer: &ImageLayerStyles) -> bool {
        layer.attachment == StyloBackgroundAttachment::Fixed && !self.is_transformed()
    }

    /// The background positioning area and the transform from its coordinate
    /// space to the scene for a fixed layer: the viewport, unaffected by any
    /// scrolling.
    fn fixed_positioning_area(&self) -> (Rect, Affine) {
        let viewport_rect = Rect::new(
            0.0,
            0.0,
            self.context.width as f64,
            self.context.height as f64,
        );
        let transform = Affine::translate((self.context.initial_x, self.context.initial_y));
        (viewport_rect, transform)
    }

    /// Whether this element or any of its ancestors has a CSS transform
    ///
    /// TODO: this misses transformed elements whose resolved transform is the
    /// identity (e.g. `transform: translate(0)`), and elements with
    /// `will-change: transform`, both of which should also degrade `fixed`
    /// to `scroll` (see WPT css/css-transforms/transform-fixed-bg-005/008)
    fn is_transformed(&self) -> bool {
        let mut current = Some(self.node.id);
        while let Some(node) = current.and_then(|id| self.context.dom.get_node(id)) {
            if node.transform().is_some() {
                return true;
            }
            current = node.parent;
        }
        false
    }

    #[cfg(feature = "svg")]
    fn draw_svg_image_layer(&self, scene: &mut impl PaintScene, layer: &ImageLayerStyles) {
        let Some(bg_image) = layer.image_data else {
            return;
        };
        let ImageData::Svg(svg) = &bg_image.image else {
            return;
        };

        // A zero-sized `viewBox` disables rendering of the SVG
        if svg.intrinsic_dimensions.degenerate_view_box {
            return;
        }

        let (origin_rect, base_transform) = if self.layer_is_fixed(layer) {
            self.fixed_positioning_area()
        } else {
            (self.box_rect(layer.origin), self.transform)
        };

        let frame_w = (origin_rect.width() / self.scale) as f32;
        let frame_h = (origin_rect.height() / self.scale) as f32;

        // Size the SVG per the CSS default sizing algorithm
        // (https://drafts.csswg.org/css-images/#default-sizing). An SVG image
        // may lack an intrinsic width, height, and/or aspect ratio, so each is
        // passed separately (usvg's resolved `Tree::size` is only used as the
        // source coordinate space of the rendered tree).
        let intrinsic_width = svg.intrinsic_width().filter(|w| w.is_finite() && *w > 0.0);
        let intrinsic_height = svg.intrinsic_height().filter(|h| h.is_finite() && *h > 0.0);
        let aspect_ratio = match (intrinsic_width, intrinsic_height) {
            (Some(w), Some(h)) => Some(w / h),
            _ => svg.viewbox_aspect_ratio(),
        }
        .filter(|r| r.is_finite() && *r > 0.0);

        let bg_size = compute_layer_size(
            layer,
            frame_w,
            frame_h,
            BackgroundSizeComputeMode::Intrinsic {
                width: intrinsic_width,
                height: intrinsic_height,
                ratio: aspect_ratio,
            },
        );

        if bg_size.width <= 0.0 || bg_size.height <= 0.0 {
            return;
        }

        let viewport = super::svg_viewport_affine(
            svg,
            (bg_size.width * self.scale) as f32,
            (bg_size.height * self.scale) as f32,
        );

        let bg_pos = compute_layer_position(
            layer,
            frame_w - bg_size.width as f32,
            frame_h - bg_size.height as f32,
        );

        let transform = base_transform
            * kurbo::Affine::translate((
                origin_rect.x0 + bg_pos.x * self.scale,
                origin_rect.y0 + bg_pos.y * self.scale,
            ))
            * viewport;

        anyrender_svg::render_svg_tree(scene, &svg.tree, transform);
    }

    fn draw_raster_image_layer(&self, scene: &mut impl PaintScene, layer: &ImageLayerStyles) {
        let Some(bg_image) = layer.image_data else {
            return;
        };
        let ImageData::Raster(image_data) = &bg_image.image else {
            return;
        };

        let image_rendering = self.style.clone_image_rendering();
        let quality = to_image_quality(image_rendering);

        let (origin_rect, base_transform) = if self.layer_is_fixed(layer) {
            self.fixed_positioning_area()
        } else {
            (self.box_rect(layer.origin), self.transform)
        };

        let image_width = image_data.width as f64;
        let image_height = image_data.height as f64;

        let (bg_pos, bg_size) = compute_layer_position_and_size(
            layer,
            origin_rect.width() / self.scale,
            origin_rect.height() / self.scale,
            BackgroundSizeComputeMode::Size(image_width as f32, image_height as f32),
        );

        let bg_pos = (bg_pos.to_vec2() * self.scale).to_point();
        let bg_size = bg_size * self.scale;

        let x_ratio = bg_size.width / image_width;
        let y_ratio = bg_size.height / image_height;

        let BackgroundRepeat(repeat_x, repeat_y) = layer.repeat;

        let x = raster_axis_tiling(
            *repeat_x,
            origin_rect.x0,
            origin_rect.width(),
            bg_pos.x,
            bg_size.width,
            image_width,
            x_ratio,
        );
        let y = raster_axis_tiling(
            *repeat_y,
            origin_rect.y0,
            origin_rect.height(),
            bg_pos.y,
            bg_size.height,
            image_height,
            y_ratio,
        );

        let transform = base_transform
            .pre_scale_non_uniform(x_ratio, y_ratio)
            .then_translate(Vec2 {
                x: x.translate,
                y: y.translate,
            });
        let tile_rect = Rect::new(0.0, 0.0, x.rect_len, y.rect_len);

        for hc in 0..y.count {
            for wc in 0..x.count {
                let transform = transform.then_translate(Vec2 {
                    x: wc as f64 * x.stride,
                    y: hc as f64 * y.stride,
                });

                scene.fill(
                    peniko::Fill::NonZero,
                    transform,
                    to_peniko_image(image_data, quality).as_ref(),
                    None,
                    &tile_rect,
                );
            }
        }
    }

    fn draw_gradient_layer(
        &self,
        scene: &mut impl PaintScene,
        gradient: &StyloGradient,
        layer: &ImageLayerStyles,
    ) {
        // For a fixed layer the positioning area (the viewport) already covers
        // everything visible, so it also serves as the clip rect (no extension
        // towards the clip box is needed).
        let (origin_rect, base_transform, clip_rect) = if self.layer_is_fixed(layer) {
            let (viewport_rect, transform) = self.fixed_positioning_area();
            (viewport_rect, transform, viewport_rect)
        } else {
            (
                self.box_rect(layer.origin),
                self.transform,
                self.box_rect(layer.clip),
            )
        };

        let bounding_box = self.frame.border_box.bounding_box();
        let current_color = self.style.clone_color();
        self.draw_gradient_in(
            scene,
            gradient,
            layer,
            origin_rect,
            base_transform,
            clip_rect,
            bounding_box,
            &current_color,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_gradient_in(
        &self,
        scene: &mut impl PaintScene,
        gradient: &StyloGradient,
        layer: &ImageLayerStyles,
        origin_rect: Rect,
        base_transform: Affine,
        clip_rect: Rect,
        bounding_box: Rect,
        current_color: &AbsoluteColor,
    ) {
        let (bg_pos, bg_size) = compute_layer_position_and_size(
            layer,
            origin_rect.width() / self.scale,
            origin_rect.height() / self.scale,
            BackgroundSizeComputeMode::Auto,
        );

        let bg_pos = (bg_pos.to_vec2() * self.scale).to_point();
        let bg_size = bg_size * self.scale;

        let BackgroundRepeat(repeat_x, repeat_y) = layer.repeat;

        let x = gradient_axis_tiling(
            *repeat_x,
            origin_rect.x0,
            origin_rect.width(),
            clip_rect.x0,
            clip_rect.width(),
            bg_pos.x,
            bg_size.width,
        );
        let y = gradient_axis_tiling(
            *repeat_y,
            origin_rect.y0,
            origin_rect.height(),
            clip_rect.y0,
            clip_rect.height(),
            bg_pos.y,
            bg_size.height,
        );

        // FIXME: https://wpt.live/css/css-backgrounds/background-size/background-size-near-zero-gradient.html
        if x.count as u64 * y.count as u64 > 500 {
            return;
        }

        let tile_rect = Rect::new(0.0, 0.0, x.rect_len, y.rect_len);

        let (gradient, gradient_transform) =
            to_peniko_gradient(gradient, tile_rect, bounding_box, self.scale, current_color);
        let brush = anyrender::Paint::Gradient(&gradient);

        let transform = base_transform.then_translate(Vec2 {
            x: x.translate,
            y: y.translate,
        });

        for hc in 0..y.count {
            for wc in 0..x.count {
                let transform = transform.then_translate(Vec2 {
                    x: wc as f64 * x.stride,
                    y: hc as f64 * y.stride,
                });

                scene.fill(
                    peniko::Fill::NonZero,
                    transform,
                    brush.clone(),
                    gradient_transform,
                    &tile_rect,
                );
            }
        }
    }
}

fn compute_layer_position_and_size(
    layer: &ImageLayerStyles,
    container_w: f64,
    container_h: f64,
    size_mode: BackgroundSizeComputeMode,
) -> (Point, Size) {
    use BackgroundRepeatKeyword::*;

    let bg_size = compute_layer_size(layer, container_w as f32, container_h as f32, size_mode);

    let bg_pos = compute_layer_position(
        layer,
        (container_w - bg_size.width) as f32,
        (container_h - bg_size.height) as f32,
    );

    let BackgroundRepeat(repeat_x, repeat_y) = layer.repeat;

    let bg_size = if matches!(repeat_x, Round) && matches!(repeat_y, Round) {
        let count = (container_w / bg_size.width).round();
        let width = container_w / count;

        let count = (container_h / bg_size.height).round();
        let height = container_h / count;

        Size::new(width, height)
    } else if matches!(repeat_x, Round) {
        let count = (container_w / bg_size.width).round();
        let width = container_w / count;
        Size::new(width, bg_size.height)
    } else if matches!(repeat_y, Round) {
        let count = (container_h / bg_size.height).round();
        let height = container_h / count;
        Size::new(bg_size.width, height)
    } else {
        bg_size
    };

    (bg_pos, bg_size)
}

#[inline]
fn compute_layer_position(layer: &ImageLayerStyles, width: f32, height: f32) -> Point {
    use style::values::computed::Length;

    let bg_pos_x = layer.position_x.resolve(Length::new(width)).px() as f64;
    let bg_pos_y = layer.position_y.resolve(Length::new(height)).px() as f64;

    Point::new(bg_pos_x, bg_pos_y)
}

fn compute_layer_size(
    layer: &ImageLayerStyles,
    container_w: f32,
    container_h: f32,
    mode: BackgroundSizeComputeMode,
) -> kurbo::Size {
    use style::values::computed::Length;
    use style::values::generics::length::GenericLengthPercentageOrAuto as Lpa;

    let (width, height): (f32, f32) = match layer.size {
        BackgroundSize::ExplicitSize { width, height } => {
            let width = width.map(|w| w.0.resolve(Length::new(container_w)));
            let height = height.map(|h| h.0.resolve(Length::new(container_h)));

            match (width, height) {
                (Lpa::LengthPercentage(width), Lpa::LengthPercentage(height)) => {
                    let width = width.px();
                    let height = height.px();
                    match mode {
                        BackgroundSizeComputeMode::Auto => (width, height),
                        BackgroundSizeComputeMode::Size(_, _) => (width, height),
                        BackgroundSizeComputeMode::Intrinsic { .. } => (width, height),
                    }
                }
                (Lpa::LengthPercentage(width), Lpa::Auto) => {
                    let width = width.px();
                    let height = match mode {
                        BackgroundSizeComputeMode::Auto => container_h,
                        BackgroundSizeComputeMode::Size(bg_w, bg_h) => bg_h / bg_w * width,
                        BackgroundSizeComputeMode::Intrinsic { height, ratio, .. } => ratio
                            .map(|ratio| width / ratio)
                            .or(height)
                            .unwrap_or(container_h),
                    };
                    (width, height)
                }
                (Lpa::Auto, Lpa::LengthPercentage(height)) => {
                    let height = height.px();
                    let width = match mode {
                        BackgroundSizeComputeMode::Auto => container_w,
                        BackgroundSizeComputeMode::Size(bg_w, bg_h) => bg_w / bg_h * height,
                        BackgroundSizeComputeMode::Intrinsic { width, ratio, .. } => ratio
                            .map(|ratio| height * ratio)
                            .or(width)
                            .unwrap_or(container_w),
                    };
                    (width, height)
                }
                (Lpa::Auto, Lpa::Auto) => match mode {
                    BackgroundSizeComputeMode::Auto => (container_w, container_h),
                    BackgroundSizeComputeMode::Size(bg_w, bg_h) => (bg_w, bg_h),
                    BackgroundSizeComputeMode::Intrinsic {
                        width,
                        height,
                        ratio,
                    } => default_sizing(width, height, ratio, container_w, container_h),
                },
            }
        }
        BackgroundSize::Cover => match mode {
            BackgroundSizeComputeMode::Auto => (container_w, container_h),
            BackgroundSizeComputeMode::Size(bg_w, bg_h) => {
                // Scale to the smallest size that covers both axes
                let ratio = (container_w / bg_w).max(container_h / bg_h);
                (bg_w * ratio, bg_h * ratio)
            }
            BackgroundSizeComputeMode::Intrinsic { ratio, .. } => match ratio {
                // Scale the aspect ratio to the smallest size that covers both axes
                Some(ratio) => {
                    let scale = (container_w / ratio).max(container_h);
                    (scale * ratio, scale)
                }
                // No intrinsic aspect ratio: fill the positioning area
                None => (container_w, container_h),
            },
        },
        BackgroundSize::Contain => match mode {
            BackgroundSizeComputeMode::Auto => (container_w, container_h),
            BackgroundSizeComputeMode::Size(bg_w, bg_h) => {
                // Scale to the largest size contained by both axes
                let ratio = (container_w / bg_w).min(container_h / bg_h);
                (bg_w * ratio, bg_h * ratio)
            }
            BackgroundSizeComputeMode::Intrinsic { ratio, .. } => match ratio {
                // Scale the aspect ratio to the largest size contained by both axes
                Some(ratio) => {
                    let scale = (container_w / ratio).min(container_h);
                    (scale * ratio, scale)
                }
                // No intrinsic aspect ratio: fill the positioning area
                None => (container_w, container_h),
            },
        },
    };

    kurbo::Size {
        width: width as f64,
        height: height as f64,
    }
}

enum BackgroundSizeComputeMode {
    Auto,
    Size(f32, f32),
    /// Intrinsic dimensions of an image which may lack an intrinsic width,
    /// height, and/or aspect ratio (e.g. SVG), sized per the CSS default
    /// sizing algorithm (https://drafts.csswg.org/css-images/#default-sizing)
    Intrinsic {
        width: Option<f32>,
        height: Option<f32>,
        ratio: Option<f32>,
    },
}

/// The CSS default sizing algorithm for the unconstrained (`auto auto`) case:
/// resolve the concrete object size from whichever intrinsic dimensions exist,
/// falling back to the default object size (the background positioning area).
fn default_sizing(
    width: Option<f32>,
    height: Option<f32>,
    ratio: Option<f32>,
    container_w: f32,
    container_h: f32,
) -> (f32, f32) {
    match (width, height) {
        (Some(w), Some(h)) => (w, h),
        (Some(w), None) => (w, ratio.map(|r| w / r).unwrap_or(container_h)),
        (None, Some(h)) => (ratio.map(|r| h * r).unwrap_or(container_w), h),
        (None, None) => match ratio {
            // Intrinsic aspect ratio only: size as if `contain` were specified
            Some(ratio) => {
                let scale = (container_w / ratio).min(container_h);
                (scale * ratio, scale)
            }
            None => (container_w, container_h),
        },
    }
}

/// The placement and tiling of a background layer along one axis: a
/// translation applied to the layer as a whole, the length of each filled
/// rect, and the number of explicit tiles with the stride between them.
struct AxisTiling {
    /// Translation (in device pixels) positioning the first tile
    translate: f64,
    /// Length of each filled rect, in the coordinate space of the fill's transform
    rect_len: f64,
    /// Number of explicitly drawn tiles
    count: u32,
    /// Stride (in device pixels) between the starts of consecutive tiles
    stride: f64,
}

/// Per-axis placement and tiling for a raster image layer. `Repeat`/`Round`
/// produce a single fill covering the whole positioning area (relying on the
/// image brush repeating), while `Space` produces `count` explicit tiles
/// spaced `stride` apart.
///
/// The fill rect is in image pixel coordinates (the drawing transform is
/// pre-scaled by `ratio`), while translations are in device pixels.
fn raster_axis_tiling(
    repeat: BackgroundRepeatKeyword,
    origin_start: f64,
    origin_len: f64,
    bg_pos: f64,
    tile_len: f64,
    image_len: f64,
    ratio: f64,
) -> AxisTiling {
    use BackgroundRepeatKeyword::*;

    match repeat {
        Repeat | Round => {
            let extend_len = extend(bg_pos, tile_len);
            AxisTiling {
                translate: origin_start - extend_len,
                rect_len: (origin_len + extend_len) / ratio,
                count: 1,
                stride: 0.0,
            }
        }
        Space => {
            let (count, stride) = compute_space_count_and_stride(origin_len, tile_len);
            AxisTiling {
                translate: origin_start + if count == 1 { bg_pos } else { 0.0 },
                rect_len: image_len,
                count,
                stride,
            }
        }
        NoRepeat => AxisTiling {
            translate: origin_start + bg_pos,
            rect_len: image_len,
            count: 1,
            stride: 0.0,
        },
    }
}

/// Per-axis placement and tiling for a gradient layer. Unlike raster images,
/// gradients cannot rely on brush repetition, so `Repeat`/`Round` also produce
/// explicit tiles. When the clip box extends beyond the origin box, tiling
/// starts from the clip box edge so the pattern covers the whole clipped area.
fn gradient_axis_tiling(
    repeat: BackgroundRepeatKeyword,
    origin_start: f64,
    origin_len: f64,
    clip_start: f64,
    clip_len: f64,
    bg_pos: f64,
    tile_len: f64,
) -> AxisTiling {
    use BackgroundRepeatKeyword::*;

    match repeat {
        Repeat | Round => {
            // The clip and origin boxes are nested, so the clip box extends
            // beyond the origin box iff it does so at either end
            let clip_is_outer =
                clip_start < origin_start || clip_start + clip_len > origin_start + origin_len;
            let (area_start, area_len) = if clip_is_outer {
                (clip_start, clip_len)
            } else {
                (origin_start, origin_len)
            };
            let extend_len = extend((origin_start - area_start) + bg_pos, tile_len);
            let count = ((area_len + extend_len) / tile_len).ceil() as u32;
            AxisTiling {
                translate: area_start - extend_len,
                rect_len: tile_len,
                count,
                stride: tile_len,
            }
        }
        Space => {
            let (count, stride) = compute_space_count_and_stride(origin_len, tile_len);
            AxisTiling {
                translate: origin_start + if count == 1 { bg_pos } else { 0.0 },
                rect_len: tile_len,
                count,
                stride,
            }
        }
        NoRepeat => AxisTiling {
            translate: origin_start + bg_pos,
            rect_len: tile_len,
            count: 1,
            stride: 0.0,
        },
    }
}

fn compute_space_count_and_stride(bg_size: f64, size: f64) -> (u32, f64) {
    let modulo = bg_size % size;
    let count = (((bg_size - modulo) / size) as u32).max(1);
    let stride = if count > 1 {
        modulo / (count - 1) as f64
    } else {
        0.0
    } + size;

    (count, stride)
}

/// Whether drawing the layer is guaranteed to paint nothing, making its clip
/// layer unnecessary
fn layer_paints_nothing(layer: &ImageLayerStyles) -> bool {
    match layer.stylo_image {
        GenericImage::None => true,
        GenericImage::Url(_) => layer.image_data.is_none(),
        _ => false,
    }
}

#[inline]
pub(super) fn get_cyclic<T>(values: &[T], layer_index: usize) -> &T {
    &values[layer_index % values.len()]
}

fn extend(offset: f64, length: f64) -> f64 {
    let extend_length = offset % length;
    if extend_length > 0.0 {
        length - extend_length
    } else {
        -extend_length
    }
}
