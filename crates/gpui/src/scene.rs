// todo("windows"): remove
#![cfg_attr(windows, allow(dead_code))]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    AtlasTextureId, AtlasTile, Background, Bounds, ContentMask, Corners, DevicePixels, Edges, Hsla,
    Pixels, Point, Radians, Rgba, ScaledPixels, Size, bounds_tree::BoundsTree, point,
};
use std::{
    fmt::Debug,
    iter::Peekable,
    ops::{Add, Range, Sub},
    slice,
};

#[allow(non_camel_case_types, unused)]
#[expect(missing_docs)]
pub type PathVertex_ScaledPixels = PathVertex<ScaledPixels>;

#[expect(missing_docs)]
pub type DrawOrder = u32;

/// A boolean stored as a `u32` so that GPU-facing structs contain no
/// compiler-inserted padding bytes, which would be undefined behavior to
/// reinterpret as `&[u8]` when writing instance buffers. Guaranteed to be
/// `0` or `1` by construction; shaders read it as a `u32`/`uint`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct PaddedBool32(u32);

impl From<bool> for PaddedBool32 {
    fn from(value: bool) -> Self {
        PaddedBool32(value as u32)
    }
}

#[derive(Default)]
#[expect(missing_docs)]
pub struct Scene {
    pub(crate) paint_operations: Vec<PaintOperation>,
    primitive_bounds: BoundsTree<ScaledPixels>,
    layer_stack: Vec<Layer>,
    /// Draw orders assigned by `primitive_bounds` start after this order. A backdrop
    /// filter reads everything painted before it, so it ends the current interval.
    interval_start_order: DrawOrder,
    max_order: DrawOrder,
    pub backdrop_filters: Vec<BackdropFilter>,
    pub shadows: Vec<Shadow>,
    pub quads: Vec<Quad>,
    pub paths: Vec<Path<ScaledPixels>>,
    pub underlines: Vec<Underline>,
    pub monochrome_sprites: Vec<MonochromeSprite>,
    pub subpixel_sprites: Vec<SubpixelSprite>,
    pub polychrome_sprites: Vec<PolychromeSprite>,
    pub surfaces: Vec<PaintSurface>,
}

#[expect(missing_docs)]
impl Scene {
    pub fn clear(&mut self) {
        self.paint_operations.clear();
        self.primitive_bounds.clear();
        self.layer_stack.clear();
        self.interval_start_order = 0;
        self.max_order = 0;
        self.backdrop_filters.clear();
        self.paths.clear();
        self.shadows.clear();
        self.quads.clear();
        self.underlines.clear();
        self.monochrome_sprites.clear();
        self.subpixel_sprites.clear();
        self.polychrome_sprites.clear();
        self.surfaces.clear();
    }

    pub fn len(&self) -> usize {
        self.paint_operations.len()
    }

    pub fn push_layer(&mut self, bounds: Bounds<ScaledPixels>) {
        let order = self.insert_bounds(bounds);
        self.layer_stack.push(Layer { bounds, order });
        self.paint_operations
            .push(PaintOperation::StartLayer(bounds));
    }

    pub fn pop_layer(&mut self) {
        self.layer_stack.pop();
        self.paint_operations.push(PaintOperation::EndLayer);
    }

    pub fn insert_primitive(&mut self, primitive: impl Into<Primitive>) {
        let mut primitive = primitive.into();
        let clipped_bounds = primitive
            .bounds()
            .intersect(&primitive.content_mask().bounds);

        if clipped_bounds.is_empty() {
            return;
        }

        if let Primitive::BackdropFilter(filter) = &mut primitive {
            // A filter is a barrier: it draws after everything painted so far and
            // before everything painted later, regardless of spatial overlap.
            self.max_order += 1;
            filter.order = self.max_order;
            self.backdrop_filters.push(*filter);
            self.paint_operations
                .push(PaintOperation::Primitive(primitive));
            self.start_interval_after(self.max_order);
            return;
        }

        let order = match self.layer_stack.last() {
            Some(layer) => layer.order,
            None => self.insert_bounds(clipped_bounds),
        };
        match &mut primitive {
            Primitive::Shadow(shadow) => {
                shadow.order = order;
                self.shadows.push(*shadow);
            }
            Primitive::Quad(quad) => {
                quad.order = order;
                self.quads.push(*quad);
            }
            Primitive::Path(path) => {
                path.order = order;
                path.id = PathId(self.paths.len());
                self.paths.push(path.clone());
            }
            Primitive::Underline(underline) => {
                underline.order = order;
                self.underlines.push(*underline);
            }
            Primitive::MonochromeSprite(sprite) => {
                sprite.order = order;
                self.monochrome_sprites.push(*sprite);
            }
            Primitive::SubpixelSprite(sprite) => {
                sprite.order = order;
                self.subpixel_sprites.push(*sprite);
            }
            Primitive::PolychromeSprite(sprite) => {
                sprite.order = order;
                self.polychrome_sprites.push(*sprite);
            }
            Primitive::Surface(surface) => {
                surface.order = order;
                self.surfaces.push(surface.clone());
            }
            Primitive::BackdropFilter(_) => unreachable!("backdrop filters are ordered above"),
        }
        self.paint_operations
            .push(PaintOperation::Primitive(primitive));
    }

    pub fn replay(&mut self, range: Range<usize>, prev_scene: &Scene) {
        for operation in &prev_scene.paint_operations[range] {
            match operation {
                PaintOperation::Primitive(primitive) => self.insert_primitive(primitive.clone()),
                PaintOperation::StartLayer(bounds) => self.push_layer(*bounds),
                PaintOperation::EndLayer => self.pop_layer(),
            }
        }
    }

    pub fn finish(&mut self) {
        self.backdrop_filters.sort_by_key(|filter| filter.order);
        self.shadows.sort_by_key(|shadow| shadow.order);
        self.quads.sort_by_key(|quad| quad.order);
        self.paths.sort_by_key(|path| path.order);
        self.underlines.sort_by_key(|underline| underline.order);
        self.monochrome_sprites
            .sort_by_key(|sprite| (sprite.order, sprite.tile.tile_id));
        self.subpixel_sprites
            .sort_by_key(|sprite| (sprite.order, sprite.tile.tile_id));
        self.polychrome_sprites
            .sort_by_key(|sprite| (sprite.order, sprite.tile.tile_id));
        self.surfaces.sort_by_key(|surface| surface.order);
    }

    #[cfg_attr(
        all(
            any(target_os = "linux", target_os = "freebsd"),
            not(any(feature = "x11", feature = "wayland"))
        ),
        allow(dead_code)
    )]
    pub fn batches(&self) -> impl Iterator<Item = PrimitiveBatch> + '_ {
        BatchIterator {
            backdrop_filters_start: 0,
            backdrop_filters_iter: self.backdrop_filters.iter().peekable(),
            shadows_start: 0,
            shadows_iter: self.shadows.iter().peekable(),
            quads_start: 0,
            quads_iter: self.quads.iter().peekable(),
            paths_start: 0,
            paths_iter: self.paths.iter().peekable(),
            underlines_start: 0,
            underlines_iter: self.underlines.iter().peekable(),
            monochrome_sprites_start: 0,
            monochrome_sprites_iter: self.monochrome_sprites.iter().peekable(),
            subpixel_sprites_start: 0,
            subpixel_sprites_iter: self.subpixel_sprites.iter().peekable(),
            polychrome_sprites_start: 0,
            polychrome_sprites_iter: self.polychrome_sprites.iter().peekable(),
            surfaces_start: 0,
            surfaces_iter: self.surfaces.iter().peekable(),
        }
    }

    fn insert_bounds(&mut self, bounds: Bounds<ScaledPixels>) -> DrawOrder {
        let order = self.interval_start_order + self.primitive_bounds.insert(bounds);
        self.max_order = self.max_order.max(order);
        order
    }

    /// Starts a new ordering interval after `order`. Active layers are reinserted so
    /// primitives painted into them after the barrier still draw above it.
    fn start_interval_after(&mut self, order: DrawOrder) {
        self.primitive_bounds.clear();
        self.interval_start_order = order;
        for layer in &mut self.layer_stack {
            layer.order = self.interval_start_order + self.primitive_bounds.insert(layer.bounds);
            self.max_order = self.max_order.max(layer.order);
        }
    }
}

#[derive(Clone, Copy)]
struct Layer {
    bounds: Bounds<ScaledPixels>,
    order: DrawOrder,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Default)]
#[cfg_attr(
    all(
        any(target_os = "linux", target_os = "freebsd"),
        not(any(feature = "x11", feature = "wayland"))
    ),
    allow(dead_code)
)]
pub(crate) enum PrimitiveKind {
    BackdropFilter,
    Shadow,
    #[default]
    Quad,
    Path,
    Underline,
    MonochromeSprite,
    SubpixelSprite,
    PolychromeSprite,
    Surface,
}

pub(crate) enum PaintOperation {
    Primitive(Primitive),
    StartLayer(Bounds<ScaledPixels>),
    EndLayer,
}

#[derive(Clone)]
#[expect(missing_docs)]
pub enum Primitive {
    BackdropFilter(BackdropFilter),
    Shadow(Shadow),
    Quad(Quad),
    Path(Path<ScaledPixels>),
    Underline(Underline),
    MonochromeSprite(MonochromeSprite),
    SubpixelSprite(SubpixelSprite),
    PolychromeSprite(PolychromeSprite),
    Surface(PaintSurface),
}

#[expect(missing_docs)]
impl Primitive {
    pub fn bounds(&self) -> &Bounds<ScaledPixels> {
        match self {
            Primitive::BackdropFilter(filter) => &filter.bounds,
            Primitive::Shadow(shadow) => &shadow.bounds,
            Primitive::Quad(quad) => &quad.bounds,
            Primitive::Path(path) => &path.bounds,
            Primitive::Underline(underline) => &underline.bounds,
            Primitive::MonochromeSprite(sprite) => &sprite.bounds,
            Primitive::SubpixelSprite(sprite) => &sprite.bounds,
            Primitive::PolychromeSprite(sprite) => &sprite.bounds,
            Primitive::Surface(surface) => &surface.bounds,
        }
    }

    pub fn content_mask(&self) -> &ContentMask<ScaledPixels> {
        match self {
            Primitive::BackdropFilter(filter) => &filter.content_mask,
            Primitive::Shadow(shadow) => &shadow.content_mask,
            Primitive::Quad(quad) => &quad.content_mask,
            Primitive::Path(path) => &path.content_mask,
            Primitive::Underline(underline) => &underline.content_mask,
            Primitive::MonochromeSprite(sprite) => &sprite.content_mask,
            Primitive::SubpixelSprite(sprite) => &sprite.content_mask,
            Primitive::PolychromeSprite(sprite) => &sprite.content_mask,
            Primitive::Surface(surface) => &surface.content_mask,
        }
    }
}

#[cfg_attr(
    all(
        any(target_os = "linux", target_os = "freebsd"),
        not(any(feature = "x11", feature = "wayland"))
    ),
    allow(dead_code)
)]
struct BatchIterator<'a> {
    backdrop_filters_start: usize,
    backdrop_filters_iter: Peekable<slice::Iter<'a, BackdropFilter>>,
    shadows_start: usize,
    shadows_iter: Peekable<slice::Iter<'a, Shadow>>,
    quads_start: usize,
    quads_iter: Peekable<slice::Iter<'a, Quad>>,
    paths_start: usize,
    paths_iter: Peekable<slice::Iter<'a, Path<ScaledPixels>>>,
    underlines_start: usize,
    underlines_iter: Peekable<slice::Iter<'a, Underline>>,
    monochrome_sprites_start: usize,
    monochrome_sprites_iter: Peekable<slice::Iter<'a, MonochromeSprite>>,
    subpixel_sprites_start: usize,
    subpixel_sprites_iter: Peekable<slice::Iter<'a, SubpixelSprite>>,
    polychrome_sprites_start: usize,
    polychrome_sprites_iter: Peekable<slice::Iter<'a, PolychromeSprite>>,
    surfaces_start: usize,
    surfaces_iter: Peekable<slice::Iter<'a, PaintSurface>>,
}

impl<'a> Iterator for BatchIterator<'a> {
    type Item = PrimitiveBatch;

    fn next(&mut self) -> Option<Self::Item> {
        let mut orders_and_kinds = [
            (
                self.backdrop_filters_iter.peek().map(|filter| filter.order),
                PrimitiveKind::BackdropFilter,
            ),
            (
                self.shadows_iter.peek().map(|s| s.order),
                PrimitiveKind::Shadow,
            ),
            (self.quads_iter.peek().map(|q| q.order), PrimitiveKind::Quad),
            (self.paths_iter.peek().map(|q| q.order), PrimitiveKind::Path),
            (
                self.underlines_iter.peek().map(|u| u.order),
                PrimitiveKind::Underline,
            ),
            (
                self.monochrome_sprites_iter.peek().map(|s| s.order),
                PrimitiveKind::MonochromeSprite,
            ),
            (
                self.subpixel_sprites_iter.peek().map(|s| s.order),
                PrimitiveKind::SubpixelSprite,
            ),
            (
                self.polychrome_sprites_iter.peek().map(|s| s.order),
                PrimitiveKind::PolychromeSprite,
            ),
            (
                self.surfaces_iter.peek().map(|s| s.order),
                PrimitiveKind::Surface,
            ),
        ];
        orders_and_kinds.sort_by_key(|(order, kind)| (order.unwrap_or(u32::MAX), *kind));

        let first = orders_and_kinds[0];
        let second = orders_and_kinds[1];
        let (batch_kind, max_order_and_kind) = if first.0.is_some() {
            (first.1, (second.0.unwrap_or(u32::MAX), second.1))
        } else {
            return None;
        };

        match batch_kind {
            PrimitiveKind::BackdropFilter => {
                // Each filter reads the output of everything before it, including
                // an adjacent filter, so filters never share a batch.
                let start = self.backdrop_filters_start;
                self.backdrop_filters_iter.next();
                self.backdrop_filters_start = start + 1;
                Some(PrimitiveBatch::BackdropFilters(start..start + 1))
            }
            PrimitiveKind::Shadow => {
                let shadows_start = self.shadows_start;
                let mut shadows_end = shadows_start + 1;
                self.shadows_iter.next();
                while self
                    .shadows_iter
                    .next_if(|shadow| (shadow.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    shadows_end += 1;
                }
                self.shadows_start = shadows_end;
                Some(PrimitiveBatch::Shadows(shadows_start..shadows_end))
            }
            PrimitiveKind::Quad => {
                let quads_start = self.quads_start;
                let mut quads_end = quads_start + 1;
                self.quads_iter.next();
                while self
                    .quads_iter
                    .next_if(|quad| (quad.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    quads_end += 1;
                }
                self.quads_start = quads_end;
                Some(PrimitiveBatch::Quads(quads_start..quads_end))
            }
            PrimitiveKind::Path => {
                let paths_start = self.paths_start;
                let mut paths_end = paths_start + 1;
                self.paths_iter.next();
                while self
                    .paths_iter
                    .next_if(|path| (path.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    paths_end += 1;
                }
                self.paths_start = paths_end;
                Some(PrimitiveBatch::Paths(paths_start..paths_end))
            }
            PrimitiveKind::Underline => {
                let underlines_start = self.underlines_start;
                let mut underlines_end = underlines_start + 1;
                self.underlines_iter.next();
                while self
                    .underlines_iter
                    .next_if(|underline| (underline.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    underlines_end += 1;
                }
                self.underlines_start = underlines_end;
                Some(PrimitiveBatch::Underlines(underlines_start..underlines_end))
            }
            PrimitiveKind::MonochromeSprite => {
                let texture_id = self.monochrome_sprites_iter.peek().unwrap().tile.texture_id;
                let sprites_start = self.monochrome_sprites_start;
                let mut sprites_end = sprites_start + 1;
                self.monochrome_sprites_iter.next();
                while self
                    .monochrome_sprites_iter
                    .next_if(|sprite| {
                        (sprite.order, batch_kind) < max_order_and_kind
                            && sprite.tile.texture_id == texture_id
                    })
                    .is_some()
                {
                    sprites_end += 1;
                }
                self.monochrome_sprites_start = sprites_end;
                Some(PrimitiveBatch::MonochromeSprites {
                    texture_id,
                    range: sprites_start..sprites_end,
                })
            }
            PrimitiveKind::SubpixelSprite => {
                let texture_id = self.subpixel_sprites_iter.peek().unwrap().tile.texture_id;
                let sprites_start = self.subpixel_sprites_start;
                let mut sprites_end = sprites_start + 1;
                self.subpixel_sprites_iter.next();
                while self
                    .subpixel_sprites_iter
                    .next_if(|sprite| {
                        (sprite.order, batch_kind) < max_order_and_kind
                            && sprite.tile.texture_id == texture_id
                    })
                    .is_some()
                {
                    sprites_end += 1;
                }
                self.subpixel_sprites_start = sprites_end;
                Some(PrimitiveBatch::SubpixelSprites {
                    texture_id,
                    range: sprites_start..sprites_end,
                })
            }
            PrimitiveKind::PolychromeSprite => {
                let texture_id = self.polychrome_sprites_iter.peek().unwrap().tile.texture_id;
                let sprites_start = self.polychrome_sprites_start;
                let mut sprites_end = sprites_start + 1;
                self.polychrome_sprites_iter.next();
                while self
                    .polychrome_sprites_iter
                    .next_if(|sprite| {
                        (sprite.order, batch_kind) < max_order_and_kind
                            && sprite.tile.texture_id == texture_id
                    })
                    .is_some()
                {
                    sprites_end += 1;
                }
                self.polychrome_sprites_start = sprites_end;
                Some(PrimitiveBatch::PolychromeSprites {
                    texture_id,
                    range: sprites_start..sprites_end,
                })
            }
            PrimitiveKind::Surface => {
                let surfaces_start = self.surfaces_start;
                let mut surfaces_end = surfaces_start + 1;
                self.surfaces_iter.next();
                while self
                    .surfaces_iter
                    .next_if(|surface| (surface.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    surfaces_end += 1;
                }
                self.surfaces_start = surfaces_end;
                Some(PrimitiveBatch::Surfaces(surfaces_start..surfaces_end))
            }
        }
    }
}

#[derive(Debug)]
#[cfg_attr(
    all(
        any(target_os = "linux", target_os = "freebsd"),
        not(any(feature = "x11", feature = "wayland"))
    ),
    allow(dead_code)
)]
#[allow(missing_docs)]
pub enum PrimitiveBatch {
    BackdropFilters(Range<usize>),
    Shadows(Range<usize>),
    Quads(Range<usize>),
    Paths(Range<usize>),
    Underlines(Range<usize>),
    MonochromeSprites {
        texture_id: AtlasTextureId,
        range: Range<usize>,
    },
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    SubpixelSprites {
        texture_id: AtlasTextureId,
        range: Range<usize>,
    },
    PolychromeSprites {
        texture_id: AtlasTextureId,
        range: Range<usize>,
    },
    Surfaces(Range<usize>),
}

impl PrimitiveBatch {
    #[expect(missing_docs)]
    pub fn label(&self) -> String {
        match self {
            Self::BackdropFilters(range) => format!("backdrop filters ({})", range.len()),
            Self::Shadows(range) => format!("shadows ({})", range.len()),
            Self::Quads(range) => format!("quads ({})", range.len()),
            Self::Paths(range) => format!("paths ({})", range.len()),
            Self::Underlines(range) => format!("underlines ({})", range.len()),
            Self::MonochromeSprites { texture_id, range } => {
                format!(
                    "monochrome sprites ({}) on atlas {}",
                    range.len(),
                    texture_id.index
                )
            }
            Self::SubpixelSprites { texture_id, range } => {
                format!(
                    "subpixel sprites ({}) on atlas {}",
                    range.len(),
                    texture_id.index
                )
            }
            Self::PolychromeSprites { texture_id, range } => {
                format!(
                    "polychrome sprites ({}) on atlas {}",
                    range.len(),
                    texture_id.index
                )
            }
            Self::Surfaces(range) => format!("surfaces ({})", range.len()),
        }
    }
}

/// Filters the pixels already painted beneath an element's rounded bounds.
#[derive(Debug, Copy, Clone)]
pub struct BackdropFilter {
    /// Assigned by the scene. Every filter draws after all earlier primitives.
    pub order: DrawOrder,
    /// The element's bounds.
    pub bounds: Bounds<ScaledPixels>,
    /// Clips the filtered output.
    pub content_mask: ContentMask<ScaledPixels>,
    /// Rounds the filtered output.
    pub corner_radii: Corners<ScaledPixels>,
    /// The Gaussian blur sigma. Zero skips the blur.
    pub radius: ScaledPixels,
    /// Interpolates between the original and filtered pixels.
    pub opacity: f32,
    /// Constrains filtered premultiplied color to the range this source-over color admits
    /// without changing alpha. A transparent tone leaves color unchanged.
    pub tone: Rgba,
    /// The maximum filtered alpha. Premultiplied color scales with alpha.
    pub alpha_limit: f32,
}

impl Default for BackdropFilter {
    fn default() -> Self {
        Self {
            order: 0,
            bounds: Bounds::default(),
            content_mask: ContentMask::default(),
            corner_radii: Corners::default(),
            radius: ScaledPixels::default(),
            opacity: 0.0,
            tone: Rgba::default(),
            alpha_limit: 1.0,
        }
    }
}

impl BackdropFilter {
    /// The blur's downsampling factor. A box prefilter is only used once the
    /// Gaussian sigma is wide enough to hide its footprint.
    pub fn blur_downsample(&self) -> u32 {
        // At 2.5 sigma per reduced pixel, aliased box-prefilter energy stays below one 8-bit step.
        const MIN_SIGMA_PER_REDUCED_PIXEL: f32 = 2.5;
        if self.radius.0 >= MIN_SIGMA_PER_REDUCED_PIXEL * 4.0 {
            4
        } else if self.radius.0 >= MIN_SIGMA_PER_REDUCED_PIXEL * 2.0 {
            2
        } else {
            1
        }
    }

    /// The device pixels a renderer must copy to filter this primitive: the clipped output,
    /// expanded by the blur kernel and downsampling footprint when blurring, and clipped to
    /// the viewport. `None` when nothing is visible.
    pub fn snapshot_bounds(&self, viewport: Size<DevicePixels>) -> Option<Bounds<DevicePixels>> {
        let viewport_bounds = Bounds::new(
            point(ScaledPixels(0.0), ScaledPixels(0.0)),
            Size::new(
                ScaledPixels(viewport.width.0 as f32),
                ScaledPixels(viewport.height.0 as f32),
            ),
        );
        let output = self
            .bounds
            .intersect(&self.content_mask.bounds)
            .intersect(&viewport_bounds);
        if output.is_empty() {
            return None;
        }
        let halo = if self.radius.0 > 0.0 {
            (3.0 * self.radius.0).ceil() + 2.0 * self.blur_downsample() as f32
        } else {
            0.0
        };
        let left = (output.origin.x.0 - halo).floor().max(0.0) as i32;
        let top = (output.origin.y.0 - halo).floor().max(0.0) as i32;
        let right = (output.right().0 + halo)
            .ceil()
            .min(viewport.width.0 as f32) as i32;
        let bottom = (output.bottom().0 + halo)
            .ceil()
            .min(viewport.height.0 as f32) as i32;
        Some(Bounds::new(
            point(DevicePixels(left), DevicePixels(top)),
            Size::new(DevicePixels(right - left), DevicePixels(bottom - top)),
        ))
    }
}

impl Scene {
    /// The scratch texture size that fits the snapshot of every backdrop filter in this
    /// scene and the blur target at each filter's downsampling factor.
    /// `None` when no filter is visible.
    pub fn backdrop_scratch_size(
        &self,
        viewport: Size<DevicePixels>,
    ) -> Option<BackdropScratchSize> {
        self.backdrop_filters
            .iter()
            .filter_map(|filter| {
                let snapshot = filter.snapshot_bounds(viewport)?;
                Some(BackdropScratchSize {
                    snapshot: snapshot.size,
                    blur: if filter.radius.0 > 0.0 {
                        downsampled(snapshot.size, filter.blur_downsample())
                    } else {
                        Size::new(DevicePixels(1), DevicePixels(1))
                    },
                })
            })
            .reduce(|required, size| BackdropScratchSize {
                snapshot: required.snapshot.max(&size.snapshot),
                blur: required.blur.max(&size.blur),
            })
    }
}

/// Sizes of the snapshot and each blur scratch texture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackdropScratchSize {
    /// Full-resolution pixels copied from the render target.
    pub snapshot: Size<DevicePixels>,
    /// Pixels in each blur target.
    pub blur: Size<DevicePixels>,
}

/// One render pass of a backdrop filter.
///
/// A blurring filter copies its snapshot into a scratch texture, blurs it horizontally
/// into a texture downsampled by [`BackdropFilter::blur_downsample`], blurs that
/// vertically into a second downsampled texture, and composites the result into the
/// target. A filter without blur composites straight from the snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum BackdropPass {
    /// Reads the snapshot and writes the first downsampled texture.
    Horizontal = 0,
    /// Reads the first downsampled texture and writes the second.
    Vertical = 1,
    /// Reads the blurred or snapshot texture and the snapshot, and writes the target.
    Composite = 2,
}

/// Shader parameters of one backdrop pass. The layout matches the Metal, WGSL uniform,
/// and HLSL constant buffer declarations: `vec2`s start at 8-byte offsets and `vec4`s at
/// 16-byte offsets.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct BackdropUniforms {
    /// The size of the texture the pass writes.
    pub target_size: [f32; 2],
    /// The size of the texture the pass blurs or composites.
    pub source_size: [f32; 2],
    /// The part of the source texture that holds this filter's pixels.
    pub source_active_size: [f32; 2],
    /// The size of the snapshot texture.
    pub snapshot_size: [f32; 2],
    /// The part of the snapshot texture that holds this filter's pixels.
    pub snapshot_active_size: [f32; 2],
    /// The target position of the snapshot's top-left pixel.
    pub snapshot_origin: [f32; 2],
    /// The element's bounds as origin and size.
    pub bounds: [f32; 4],
    /// The content mask as origin and size.
    pub content_mask: [f32; 4],
    /// Top-left, top-right, bottom-right, and bottom-left radii.
    pub corner_radii: [f32; 4],
    /// The blur sigma in pixels of the pass's target.
    pub sigma: f32,
    /// See [`BackdropFilter::opacity`].
    pub opacity: f32,
    /// A [`BackdropPass`] value.
    pub pass: u32,
    /// See [`BackdropFilter::alpha_limit`].
    pub alpha_limit: f32,
    /// The source pixels represented by each horizontal-pass target pixel.
    pub downsample_factor: f32,
    /// Explicit padding before `tone` in all shader layouts.
    pub padding: [f32; 3],
    /// See [`BackdropFilter::tone`].
    pub tone: [f32; 4],
}

const _: () = assert!(size_of::<BackdropUniforms>() == 144);

impl BackdropFilter {
    /// The passes that render this filter, in order.
    pub fn passes(&self) -> &'static [BackdropPass] {
        if self.radius.0 > 0.0 {
            &[
                BackdropPass::Horizontal,
                BackdropPass::Vertical,
                BackdropPass::Composite,
            ]
        } else {
            &[BackdropPass::Composite]
        }
    }

    /// The pixels `pass` writes, in the coordinates of its target. `snapshot` is this
    /// filter's [`Self::snapshot_bounds`]. `None` when the pass writes nothing.
    pub fn pass_scissor(
        &self,
        pass: BackdropPass,
        snapshot: Bounds<DevicePixels>,
        viewport: Size<DevicePixels>,
    ) -> Option<Bounds<DevicePixels>> {
        let bounds = match pass {
            BackdropPass::Horizontal | BackdropPass::Vertical => Bounds::new(
                point(DevicePixels(0), DevicePixels(0)),
                downsampled(snapshot.size, self.blur_downsample()),
            ),
            BackdropPass::Composite => {
                let output = self.bounds.intersect(&self.content_mask.bounds);
                let clamp = |value: f32, max: i32| value.clamp(0.0, max as f32) as i32;
                let left = clamp(output.origin.x.0.floor(), viewport.width.0);
                let top = clamp(output.origin.y.0.floor(), viewport.height.0);
                let right = clamp(output.right().0.ceil(), viewport.width.0).max(left);
                let bottom = clamp(output.bottom().0.ceil(), viewport.height.0).max(top);
                Bounds::from_corners(
                    point(DevicePixels(left), DevicePixels(top)),
                    point(DevicePixels(right), DevicePixels(bottom)),
                )
            }
        };
        (bounds.size.width.0 > 0 && bounds.size.height.0 > 0).then_some(bounds)
    }

    /// The uniforms of `pass`. `snapshot` is this filter's [`Self::snapshot_bounds`] and
    /// `scratch_size` comes from [`Scene::backdrop_scratch_size`].
    pub fn uniforms(
        &self,
        pass: BackdropPass,
        snapshot: Bounds<DevicePixels>,
        scratch_size: BackdropScratchSize,
        viewport: Size<DevicePixels>,
    ) -> BackdropUniforms {
        let size = |size: Size<DevicePixels>| [size.width.0 as f32, size.height.0 as f32];
        let downsample_factor = self.blur_downsample();
        let active_blur = downsampled(snapshot.size, downsample_factor);
        let (target_size, source_size, source_active_size) = match pass {
            BackdropPass::Horizontal => (scratch_size.blur, scratch_size.snapshot, snapshot.size),
            BackdropPass::Vertical => (scratch_size.blur, scratch_size.blur, active_blur),
            BackdropPass::Composite if self.radius.0 > 0.0 => {
                (viewport, scratch_size.blur, active_blur)
            }
            BackdropPass::Composite => (viewport, scratch_size.snapshot, snapshot.size),
        };
        let sigma = match pass {
            BackdropPass::Horizontal | BackdropPass::Vertical => {
                let factor = downsample_factor as f32;
                (self.radius.0.powi(2) - (factor.powi(2) - 1.) / 12.).sqrt() / factor
            }
            BackdropPass::Composite => 0.0,
        };
        let bounds = |bounds: Bounds<ScaledPixels>| {
            [
                bounds.origin.x.0,
                bounds.origin.y.0,
                bounds.size.width.0,
                bounds.size.height.0,
            ]
        };
        BackdropUniforms {
            target_size: size(target_size),
            source_size: size(source_size),
            source_active_size: size(source_active_size),
            snapshot_size: size(scratch_size.snapshot),
            snapshot_active_size: size(snapshot.size),
            snapshot_origin: [snapshot.origin.x.0 as f32, snapshot.origin.y.0 as f32],
            bounds: bounds(self.bounds),
            content_mask: bounds(self.content_mask.bounds),
            corner_radii: [
                self.corner_radii.top_left.0,
                self.corner_radii.top_right.0,
                self.corner_radii.bottom_right.0,
                self.corner_radii.bottom_left.0,
            ],
            sigma,
            opacity: self.opacity,
            pass: pass as u32,
            alpha_limit: self.alpha_limit,
            downsample_factor: downsample_factor as f32,
            padding: [0.0; 3],
            tone: [self.tone.r, self.tone.g, self.tone.b, self.tone.a],
        }
    }
}

fn downsampled(size: Size<DevicePixels>, factor: u32) -> Size<DevicePixels> {
    let divide =
        |value: DevicePixels| DevicePixels(value.0.max(0).cast_unsigned().div_ceil(factor) as i32);
    Size {
        width: divide(size.width),
        height: divide(size.height),
    }
}

impl From<BackdropFilter> for Primitive {
    fn from(filter: BackdropFilter) -> Self {
        Primitive::BackdropFilter(filter)
    }
}

#[derive(Default, Debug, Copy, Clone)]
#[repr(C)]
#[expect(missing_docs)]
pub struct Quad {
    pub order: DrawOrder,
    pub border_style: BorderStyle,
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub background: Background,
    pub border_color: Hsla,
    pub corner_radii: Corners<ScaledPixels>,
    pub border_widths: Edges<ScaledPixels>,
}

impl From<Quad> for Primitive {
    fn from(quad: Quad) -> Self {
        Primitive::Quad(quad)
    }
}

#[derive(Debug, Copy, Clone)]
#[repr(C)]
#[expect(missing_docs)]
pub struct Underline {
    pub order: DrawOrder,
    pub pad: u32, // align to 8 bytes
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub color: Hsla,
    pub thickness: ScaledPixels,
    pub wavy: PaddedBool32,
}

impl From<Underline> for Primitive {
    fn from(underline: Underline) -> Self {
        Primitive::Underline(underline)
    }
}

#[derive(Debug, Copy, Clone)]
#[repr(C)]
#[expect(missing_docs)]
pub struct Shadow {
    pub order: DrawOrder,
    pub blur_radius: ScaledPixels,
    pub bounds: Bounds<ScaledPixels>,
    pub corner_radii: Corners<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub color: Hsla,
    pub element_bounds: Bounds<ScaledPixels>,
    pub element_corner_radii: Corners<ScaledPixels>,
    /// 0 = drop shadow (rendered outside the element), 1 = inset shadow (rendered inside).
    pub inset: u32,
    /// 1 = clip a drop shadow out of the element's rounded bounds. Also aligns to 8 bytes.
    pub outside_only: u32,
}

impl From<Shadow> for Primitive {
    fn from(shadow: Shadow) -> Self {
        Primitive::Shadow(shadow)
    }
}

/// The style of a border.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[repr(C)]
pub enum BorderStyle {
    /// A solid border.
    #[default]
    Solid = 0,
    /// A dashed border.
    Dashed = 1,
}

/// A data type representing a 2 dimensional transformation that can be applied to an element.
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(C)]
pub struct TransformationMatrix {
    /// 2x2 matrix containing rotation and scale,
    /// stored row-major
    pub rotation_scale: [[f32; 2]; 2],
    /// translation vector
    pub translation: [f32; 2],
}

impl Eq for TransformationMatrix {}

impl TransformationMatrix {
    /// The unit matrix, has no effect.
    pub fn unit() -> Self {
        Self {
            rotation_scale: [[1.0, 0.0], [0.0, 1.0]],
            translation: [0.0, 0.0],
        }
    }

    /// Move the origin by a given point
    pub fn translate(mut self, point: Point<ScaledPixels>) -> Self {
        self.compose(Self {
            rotation_scale: [[1.0, 0.0], [0.0, 1.0]],
            translation: [point.x.0, point.y.0],
        })
    }

    /// Clockwise rotation in radians around the origin
    pub fn rotate(self, angle: Radians) -> Self {
        self.compose(Self {
            rotation_scale: [
                [angle.0.cos(), -angle.0.sin()],
                [angle.0.sin(), angle.0.cos()],
            ],
            translation: [0.0, 0.0],
        })
    }

    /// Scale around the origin
    pub fn scale(self, size: Size<f32>) -> Self {
        self.compose(Self {
            rotation_scale: [[size.width, 0.0], [0.0, size.height]],
            translation: [0.0, 0.0],
        })
    }

    /// Perform matrix multiplication with another transformation
    /// to produce a new transformation that is the result of
    /// applying both transformations: first, `other`, then `self`.
    #[inline]
    pub fn compose(self, other: TransformationMatrix) -> TransformationMatrix {
        if other == Self::unit() {
            return self;
        }
        // Perform matrix multiplication
        TransformationMatrix {
            rotation_scale: [
                [
                    self.rotation_scale[0][0] * other.rotation_scale[0][0]
                        + self.rotation_scale[0][1] * other.rotation_scale[1][0],
                    self.rotation_scale[0][0] * other.rotation_scale[0][1]
                        + self.rotation_scale[0][1] * other.rotation_scale[1][1],
                ],
                [
                    self.rotation_scale[1][0] * other.rotation_scale[0][0]
                        + self.rotation_scale[1][1] * other.rotation_scale[1][0],
                    self.rotation_scale[1][0] * other.rotation_scale[0][1]
                        + self.rotation_scale[1][1] * other.rotation_scale[1][1],
                ],
            ],
            translation: [
                self.translation[0]
                    + self.rotation_scale[0][0] * other.translation[0]
                    + self.rotation_scale[0][1] * other.translation[1],
                self.translation[1]
                    + self.rotation_scale[1][0] * other.translation[0]
                    + self.rotation_scale[1][1] * other.translation[1],
            ],
        }
    }

    /// Apply transformation to a point, mainly useful for debugging
    pub fn apply(&self, point: Point<Pixels>) -> Point<Pixels> {
        let input = [point.x.0, point.y.0];
        let mut output = self.translation;
        for (i, output_cell) in output.iter_mut().enumerate() {
            for (k, input_cell) in input.iter().enumerate() {
                *output_cell += self.rotation_scale[i][k] * *input_cell;
            }
        }
        Point::new(output[0].into(), output[1].into())
    }
}

impl Default for TransformationMatrix {
    fn default() -> Self {
        Self::unit()
    }
}

#[derive(Copy, Clone, Debug)]
#[repr(C)]
#[expect(missing_docs)]
pub struct MonochromeSprite {
    pub order: DrawOrder,
    pub pad: u32,
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub color: Hsla,
    pub tile: AtlasTile,
    pub transformation: TransformationMatrix,
}

impl From<MonochromeSprite> for Primitive {
    fn from(sprite: MonochromeSprite) -> Self {
        Primitive::MonochromeSprite(sprite)
    }
}

#[derive(Copy, Clone, Debug)]
#[repr(C)]
#[expect(missing_docs)]
pub struct SubpixelSprite {
    pub order: DrawOrder,
    pub pad: u32, // align to 8 bytes
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub color: Hsla,
    pub tile: AtlasTile,
    pub transformation: TransformationMatrix,
}

impl From<SubpixelSprite> for Primitive {
    fn from(sprite: SubpixelSprite) -> Self {
        Primitive::SubpixelSprite(sprite)
    }
}

#[derive(Copy, Clone, Debug)]
#[repr(C)]
#[expect(missing_docs)]
pub struct PolychromeSprite {
    pub order: DrawOrder,
    pub pad: u32,
    pub grayscale: PaddedBool32,
    pub opacity: f32,
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub corner_radii: Corners<ScaledPixels>,
    pub tile: AtlasTile,
}

impl From<PolychromeSprite> for Primitive {
    fn from(sprite: PolychromeSprite) -> Self {
        Primitive::PolychromeSprite(sprite)
    }
}

#[derive(Clone, Debug)]
#[allow(missing_docs)]
pub struct PaintSurface {
    pub order: DrawOrder,
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    #[cfg(target_os = "macos")]
    pub image_buffer: core_video::pixel_buffer::CVPixelBuffer,
}

impl From<PaintSurface> for Primitive {
    fn from(surface: PaintSurface) -> Self {
        Primitive::Surface(surface)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[expect(missing_docs)]
pub struct PathId(pub usize);

/// A line made up of a series of vertices and control points.
#[derive(Clone, Debug)]
#[expect(missing_docs)]
pub struct Path<P: Clone + Debug + Default + PartialEq> {
    pub id: PathId,
    pub order: DrawOrder,
    pub bounds: Bounds<P>,
    pub content_mask: ContentMask<P>,
    pub vertices: Vec<PathVertex<P>>,
    pub color: Background,
    start: Point<P>,
    current: Point<P>,
    contour_count: usize,
}

impl Path<Pixels> {
    /// Create a new path with the given starting point.
    pub fn new(start: Point<Pixels>) -> Self {
        Self {
            id: PathId(0),
            order: DrawOrder::default(),
            vertices: Vec::new(),
            start,
            current: start,
            bounds: Bounds {
                origin: start,
                size: Default::default(),
            },
            content_mask: Default::default(),
            color: Default::default(),
            contour_count: 0,
        }
    }

    /// Scale this path by the given factor.
    pub fn scale(&self, factor: f32) -> Path<ScaledPixels> {
        Path {
            id: self.id,
            order: self.order,
            bounds: self.bounds.scale(factor),
            content_mask: self.content_mask.scale(factor),
            vertices: self
                .vertices
                .iter()
                .map(|vertex| vertex.scale(factor))
                .collect(),
            start: self.start.map(|start| start.scale(factor)),
            current: self.current.scale(factor),
            contour_count: self.contour_count,
            color: self.color,
        }
    }

    /// Move the start, current point to the given point.
    pub fn move_to(&mut self, to: Point<Pixels>) {
        self.contour_count += 1;
        self.start = to;
        self.current = to;
    }

    /// Draw a straight line from the current point to the given point.
    pub fn line_to(&mut self, to: Point<Pixels>) {
        self.contour_count += 1;
        if self.contour_count > 1 {
            self.push_triangle(
                (self.start, self.current, to),
                (point(0., 1.), point(0., 1.), point(0., 1.)),
            );
        }
        self.current = to;
    }

    /// Draw a curve from the current point to the given point, using the given control point.
    pub fn curve_to(&mut self, to: Point<Pixels>, ctrl: Point<Pixels>) {
        self.contour_count += 1;
        if self.contour_count > 1 {
            self.push_triangle(
                (self.start, self.current, to),
                (point(0., 1.), point(0., 1.), point(0., 1.)),
            );
        }

        self.push_triangle(
            (self.current, ctrl, to),
            (point(0., 0.), point(0.5, 0.), point(1., 1.)),
        );
        self.current = to;
    }

    /// Push a triangle to the Path.
    pub fn push_triangle(
        &mut self,
        xy: (Point<Pixels>, Point<Pixels>, Point<Pixels>),
        st: (Point<f32>, Point<f32>, Point<f32>),
    ) {
        self.bounds = self
            .bounds
            .union(&Bounds {
                origin: xy.0,
                size: Default::default(),
            })
            .union(&Bounds {
                origin: xy.1,
                size: Default::default(),
            })
            .union(&Bounds {
                origin: xy.2,
                size: Default::default(),
            });

        self.vertices.push(PathVertex {
            xy_position: xy.0,
            st_position: st.0,
            content_mask: Default::default(),
        });
        self.vertices.push(PathVertex {
            xy_position: xy.1,
            st_position: st.1,
            content_mask: Default::default(),
        });
        self.vertices.push(PathVertex {
            xy_position: xy.2,
            st_position: st.2,
            content_mask: Default::default(),
        });
    }
}

impl<T> Path<T>
where
    T: Clone + Debug + Default + PartialEq + PartialOrd + Add<T, Output = T> + Sub<Output = T>,
{
    #[allow(unused)]
    #[expect(missing_docs)]
    pub fn clipped_bounds(&self) -> Bounds<T> {
        self.bounds.intersect(&self.content_mask.bounds)
    }
}

impl From<Path<ScaledPixels>> for Primitive {
    fn from(path: Path<ScaledPixels>) -> Self {
        Primitive::Path(path)
    }
}

#[derive(Clone, Debug)]
#[repr(C)]
#[expect(missing_docs)]
pub struct PathVertex<P: Clone + Debug + Default + PartialEq> {
    pub xy_position: Point<P>,
    pub st_position: Point<f32>,
    pub content_mask: ContentMask<P>,
}

#[expect(missing_docs)]
impl PathVertex<Pixels> {
    pub fn scale(&self, factor: f32) -> PathVertex<ScaledPixels> {
        PathVertex {
            xy_position: self.xy_position.scale(factor),
            st_position: self.st_position,
            content_mask: self.content_mask.scale(factor),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bounds, size};

    fn test_bounds(x: f32, y: f32, width: f32, height: f32) -> Bounds<ScaledPixels> {
        bounds(
            point(ScaledPixels(x), ScaledPixels(y)),
            size(ScaledPixels(width), ScaledPixels(height)),
        )
    }

    fn test_quad(bounds: Bounds<ScaledPixels>) -> Quad {
        Quad {
            bounds,
            content_mask: ContentMask { bounds },
            ..Default::default()
        }
    }

    fn test_shadow(bounds: Bounds<ScaledPixels>) -> Shadow {
        Shadow {
            order: 0,
            blur_radius: ScaledPixels::default(),
            bounds,
            corner_radii: Corners::default(),
            content_mask: ContentMask { bounds },
            color: Hsla::default(),
            element_bounds: bounds,
            element_corner_radii: Corners::default(),
            inset: 0,
            outside_only: 0,
        }
    }

    fn test_filter(bounds: Bounds<ScaledPixels>) -> BackdropFilter {
        BackdropFilter {
            bounds,
            content_mask: ContentMask { bounds },
            radius: ScaledPixels(8.),
            opacity: 1.,
            ..Default::default()
        }
    }

    fn batch_orders(scene: &Scene) -> Vec<(PrimitiveKind, Vec<DrawOrder>)> {
        fn orders<T>(items: &[T], order: impl Fn(&T) -> DrawOrder) -> Vec<DrawOrder> {
            items.iter().map(order).collect()
        }
        scene
            .batches()
            .map(|batch| match batch {
                PrimitiveBatch::BackdropFilters(range) => (
                    PrimitiveKind::BackdropFilter,
                    orders(&scene.backdrop_filters[range], |filter| filter.order),
                ),
                PrimitiveBatch::Shadows(range) => (
                    PrimitiveKind::Shadow,
                    orders(&scene.shadows[range], |shadow| shadow.order),
                ),
                PrimitiveBatch::Quads(range) => (
                    PrimitiveKind::Quad,
                    orders(&scene.quads[range], |quad| quad.order),
                ),
                other => panic!("unexpected batch {}", other.label()),
            })
            .collect()
    }

    #[test]
    fn backdrop_snapshot_bounds_clip_output_and_expand_only_for_blur() {
        let viewport = size(DevicePixels(400), DevicePixels(300));
        let mut filter = test_filter(test_bounds(80., 64., 48., 40.));
        filter.content_mask.bounds = test_bounds(88., 72., 28., 24.);
        filter.radius = ScaledPixels(0.);
        assert_eq!(
            filter.snapshot_bounds(viewport),
            Some(bounds(
                point(DevicePixels(88), DevicePixels(72)),
                size(DevicePixels(28), DevicePixels(24))
            ))
        );
        filter.radius = ScaledPixels(4.);
        assert_eq!(
            filter.snapshot_bounds(viewport),
            Some(bounds(
                point(DevicePixels(74), DevicePixels(58)),
                size(DevicePixels(56), DevicePixels(52))
            ))
        );
    }

    #[test]
    fn backdrop_downsampling_follows_sigma_and_sizes_each_scratch_target() {
        let viewport = size(DevicePixels(200), DevicePixels(200));
        let bounds = test_bounds(80., 80., 20., 20.);
        let mut filter = test_filter(bounds);
        for (radius, factor) in [
            (0., 1),
            (0.01, 1),
            (1.9999, 1),
            (2., 1),
            (4., 1),
            (4.9999, 1),
            (5., 2),
            (9.9999, 2),
            (10., 4),
        ] {
            filter.radius = ScaledPixels(radius);
            assert_eq!(filter.blur_downsample(), factor);
            let snapshot = filter.snapshot_bounds(viewport).unwrap();
            let scratch = BackdropScratchSize {
                snapshot: snapshot.size,
                blur: downsampled(snapshot.size, factor),
            };
            let horizontal = filter.uniforms(BackdropPass::Horizontal, snapshot, scratch, viewport);
            assert_eq!(horizontal.downsample_factor, factor as f32);
            assert_eq!(
                horizontal.target_size,
                [scratch.blur.width.0 as f32, scratch.blur.height.0 as f32]
            );
            let factor = factor as f32;
            let expected_sigma = (radius * radius - (factor * factor - 1.) / 12.).sqrt() / factor;
            assert!(
                (horizontal.sigma - expected_sigma).abs() < 0.00001,
                "radius {radius}, factor {factor}: got {} instead of {expected_sigma}",
                horizontal.sigma
            );
            let vertical = filter.uniforms(BackdropPass::Vertical, snapshot, scratch, viewport);
            assert_eq!(vertical.sigma, horizontal.sigma);
            let composite = filter.uniforms(BackdropPass::Composite, snapshot, scratch, viewport);
            assert_eq!(composite.sigma, 0.);
            if radius > 0. {
                assert_eq!(
                    filter
                        .pass_scissor(BackdropPass::Horizontal, snapshot, viewport)
                        .unwrap()
                        .size,
                    scratch.blur
                );
            }
        }

        let mut scene = Scene::default();
        filter.radius = ScaledPixels(0.);
        scene.insert_primitive(filter);
        assert_eq!(
            scene.backdrop_scratch_size(viewport).unwrap(),
            BackdropScratchSize {
                snapshot: size(DevicePixels(20), DevicePixels(20)),
                blur: size(DevicePixels(1), DevicePixels(1)),
            }
        );
        scene.clear();
        filter.radius = ScaledPixels(0.01);
        scene.insert_primitive(filter);
        filter.radius = ScaledPixels(4.);
        scene.insert_primitive(filter);
        let required = scene.backdrop_scratch_size(viewport).unwrap();
        assert_eq!(required.snapshot, size(DevicePixels(48), DevicePixels(48)));
        assert_eq!(required.blur, size(DevicePixels(48), DevicePixels(48)));
    }

    #[test]
    fn backdrop_snapshot_bounds_round_outward_and_stop_at_viewport_edges() {
        let viewport = size(DevicePixels(100), DevicePixels(80));
        let mut filter = test_filter(test_bounds(-3.5, 65.5, 30.75, 30.));
        filter.radius = ScaledPixels(0.);
        assert_eq!(
            filter.snapshot_bounds(viewport),
            Some(bounds(
                point(DevicePixels(0), DevicePixels(65)),
                size(DevicePixels(28), DevicePixels(15))
            ))
        );
        filter.radius = ScaledPixels(4.);
        assert_eq!(
            filter.snapshot_bounds(viewport),
            Some(bounds(
                point(DevicePixels(0), DevicePixels(51)),
                size(DevicePixels(42), DevicePixels(29))
            ))
        );
        filter.bounds = test_bounds(101., 0., 10., 10.);
        filter.content_mask.bounds = filter.bounds;
        assert!(filter.snapshot_bounds(viewport).is_none());
    }

    #[test]
    fn scene_without_filters_keeps_spatial_batch_ordering() {
        let mut scene = Scene::default();
        scene.insert_primitive(test_quad(test_bounds(0., 0., 10., 10.)));
        scene.insert_primitive(test_shadow(test_bounds(100., 100., 10., 10.)));
        scene.finish();

        assert_eq!(
            batch_orders(&scene),
            vec![
                (PrimitiveKind::Shadow, vec![1]),
                (PrimitiveKind::Quad, vec![1]),
            ]
        );
    }

    #[test]
    fn backdrop_filter_orders_disjoint_primitives_around_it() {
        let mut scene = Scene::default();
        scene.insert_primitive(test_quad(test_bounds(0., 0., 10., 10.)));
        scene.insert_primitive(test_filter(test_bounds(100., 100., 10., 10.)));
        scene.insert_primitive(test_shadow(test_bounds(200., 200., 10., 10.)));
        scene.finish();

        assert_eq!(
            batch_orders(&scene),
            vec![
                (PrimitiveKind::Quad, vec![1]),
                (PrimitiveKind::BackdropFilter, vec![2]),
                (PrimitiveKind::Shadow, vec![3]),
            ]
        );
    }

    #[test]
    fn backdrop_filter_splits_active_nested_layers() {
        let mut scene = Scene::default();
        scene.push_layer(test_bounds(0., 0., 100., 100.));
        scene.insert_primitive(test_quad(test_bounds(1., 1., 5., 5.)));
        scene.push_layer(test_bounds(10., 10., 50., 50.));
        scene.insert_primitive(test_shadow(test_bounds(12., 12., 5., 5.)));
        scene.insert_primitive(test_filter(test_bounds(20., 20., 10., 10.)));
        scene.insert_primitive(test_quad(test_bounds(30., 30., 5., 5.)));
        scene.pop_layer();
        scene.pop_layer();
        scene.finish();

        assert_eq!(
            batch_orders(&scene),
            vec![
                (PrimitiveKind::Quad, vec![1]),
                (PrimitiveKind::Shadow, vec![2]),
                (PrimitiveKind::BackdropFilter, vec![3]),
                (PrimitiveKind::Quad, vec![5]),
            ]
        );
    }

    #[test]
    fn adjacent_backdrop_filters_use_separate_batches() {
        let mut scene = Scene::default();
        scene.insert_primitive(test_filter(test_bounds(0., 0., 100., 100.)));
        scene.insert_primitive(test_filter(test_bounds(25., 25., 50., 50.)));
        scene.finish();

        assert_eq!(
            batch_orders(&scene),
            vec![
                (PrimitiveKind::BackdropFilter, vec![1]),
                (PrimitiveKind::BackdropFilter, vec![2]),
            ]
        );
    }

    #[test]
    fn replayed_backdrop_filter_keeps_its_barrier() {
        let mut cached_scene = Scene::default();
        cached_scene.insert_primitive(test_quad(test_bounds(0., 0., 10., 10.)));
        cached_scene.insert_primitive(test_filter(test_bounds(100., 100., 10., 10.)));
        cached_scene.insert_primitive(test_shadow(test_bounds(200., 200., 10., 10.)));

        let mut scene = Scene::default();
        scene.insert_primitive(test_shadow(test_bounds(300., 300., 10., 10.)));
        scene.replay(0..cached_scene.len(), &cached_scene);
        scene.insert_primitive(test_quad(test_bounds(400., 400., 10., 10.)));
        scene.finish();

        assert_eq!(
            batch_orders(&scene),
            vec![
                (PrimitiveKind::Shadow, vec![1]),
                (PrimitiveKind::Quad, vec![1]),
                (PrimitiveKind::BackdropFilter, vec![2]),
                (PrimitiveKind::Shadow, vec![3]),
                (PrimitiveKind::Quad, vec![3]),
            ]
        );
    }
}
