//! Incremental execution: tile grids, dirty sets, dependency
//! propagation, and the invalidation rules the graph executor
//! applies between frames.
//!
//! A frame is divided into tiles of [`TILE_SIZE`] pixels. The
//! executor compares each tile with the previous frame's tile
//! (exact byte comparison, no threshold) and marks changed tiles
//! dirty. The dirty set propagates through the graph in
//! topological order: each node's declared spatial dependency
//! decides how its input dirty set maps to its output dirty
//! set, and a node whose output dirty set is empty is skipped
//! entirely — its cached output is already correct. A node with
//! a non-empty dirty set reruns whole-frame, unless it is a
//! tile-safe local node, which reruns only its dirty tiles from
//! cropped inputs. A global node turns any change into a
//! whole-frame dirty set; Contour stays global, so any change
//! reruns it whole-frame and only a fully static frame skips it.
//!
//! The rules here are pure functions over tile grids, so the
//! executor's per-node decisions are testable without running a
//! graph. The executor itself lives in [`crate::graph`].

use crate::frame::Frame;
use crate::graph::{SpatialDependency, TemporalDependency};

/// Tile edge in pixels. A graph-level setting, not a per-node
/// choice, so every node's cache lines up. 16x16 source pixels
/// is the default from the section 9 specification.
pub const TILE_SIZE: u32 = 16;

/// The tile grid of one frame: `cols` by `rows` tiles of
/// [`TILE_SIZE`] pixels, the edge tiles clamped to the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileGrid {
    /// Frame width in pixels.
    width: u32,
    /// Frame height in pixels.
    height: u32,
    /// Tiles across.
    cols: u32,
    /// Tiles down.
    rows: u32,
}

impl TileGrid {
    /// The grid for a frame of `width` by `height` pixels. A
    /// zero-sized frame has zero tiles in that dimension.
    #[must_use]
    pub fn from_dims(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            cols: width.div_ceil(TILE_SIZE),
            rows: height.div_ceil(TILE_SIZE),
        }
    }

    /// Frame width in pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Frame height in pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Tiles across.
    #[must_use]
    pub fn cols(&self) -> u32 {
        self.cols
    }

    /// Tiles down.
    #[must_use]
    pub fn rows(&self) -> u32 {
        self.rows
    }

    /// Total tiles in the grid.
    #[must_use]
    pub fn tile_count(&self) -> usize {
        self.cols as usize * self.rows as usize
    }

    /// Linear index of tile (`tx`, `ty`), row-major. Callers
    /// use in-range coordinates only.
    #[must_use]
    pub fn tile_index(&self, tx: u32, ty: u32) -> usize {
        ty as usize * self.cols as usize + tx as usize
    }

    /// Pixel rectangle of tile (`tx`, `ty`), half-open and
    /// clamped to the frame: `(x0, y0, x1, y1)`. An out-of-range
    /// tile gives an empty rectangle at the frame edge.
    #[must_use]
    pub fn tile_rect(&self, tx: u32, ty: u32) -> (u32, u32, u32, u32) {
        let x0 = tx.saturating_mul(TILE_SIZE).min(self.width);
        let y0 = ty.saturating_mul(TILE_SIZE).min(self.height);
        let x1 = x0.saturating_add(TILE_SIZE).min(self.width);
        let y1 = y0.saturating_add(TILE_SIZE).min(self.height);
        (x0, y0, x1, y1)
    }

    /// Tile range overlapping the half-open pixel rectangle
    /// `(x0, y0, x1, y1)`, as `(tx0, ty0, tx1, ty1)` with the
    /// ends exclusive. An empty rectangle overlaps no tiles.
    fn tiles_for_rect(&self, x0: u32, y0: u32, x1: u32, y1: u32) -> (u32, u32, u32, u32) {
        if x1 <= x0 || y1 <= y0 {
            return (0, 0, 0, 0);
        }
        let tx0 = x0 / TILE_SIZE;
        let ty0 = y0 / TILE_SIZE;
        let tx1 = ((x1 - 1) / TILE_SIZE + 1).min(self.cols);
        let ty1 = ((y1 - 1) / TILE_SIZE + 1).min(self.rows);
        (tx0, ty0, tx1, ty1)
    }
}

/// Which tiles of one frame changed: one flag per tile in a
/// [`TileGrid`]. The executor computes one per graph input per
/// frame and one per node output per frame; a tile outside the
/// dirty set is provably unchanged, so its cached output stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirtySet {
    /// The grid the flags index into.
    grid: TileGrid,
    /// One flag per tile, row-major.
    dirty: Vec<bool>,
}

impl DirtySet {
    /// No tile dirty.
    #[must_use]
    pub fn empty(grid: TileGrid) -> Self {
        Self {
            grid,
            dirty: vec![false; grid.tile_count()],
        }
    }

    /// Every tile dirty.
    #[must_use]
    pub fn whole(grid: TileGrid) -> Self {
        Self {
            grid,
            dirty: vec![true; grid.tile_count()],
        }
    }

    /// The grid this set indexes.
    #[must_use]
    pub fn grid(&self) -> TileGrid {
        self.grid
    }

    /// True when no tile is dirty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.dirty.iter().any(|flag| *flag)
    }

    /// How many tiles are dirty.
    #[must_use]
    pub fn count(&self) -> usize {
        self.dirty.iter().filter(|flag| **flag).count()
    }

    /// True when tile (`tx`, `ty`) is dirty. Out-of-range tiles
    /// are never dirty.
    #[must_use]
    pub fn is_dirty(&self, tx: u32, ty: u32) -> bool {
        if tx >= self.grid.cols || ty >= self.grid.rows {
            return false;
        }
        self.dirty[self.grid.tile_index(tx, ty)]
    }

    /// Mark tile (`tx`, `ty`) dirty. Out-of-range tiles are
    /// ignored.
    pub fn mark(&mut self, tx: u32, ty: u32) {
        if tx >= self.grid.cols || ty >= self.grid.rows {
            return;
        }
        let index = self.grid.tile_index(tx, ty);
        self.dirty[index] = true;
    }

    /// Union of two sets. Sets on different grids union to the
    /// whole of this set's grid, which is the conservative
    /// answer when the grids do not line up.
    #[must_use]
    pub fn union(&self, other: &Self) -> Self {
        if self.grid != other.grid {
            return Self::whole(self.grid);
        }
        let dirty = self
            .dirty
            .iter()
            .zip(other.dirty.iter())
            .map(|(a, b)| *a || *b)
            .collect();
        Self {
            grid: self.grid,
            dirty,
        }
    }

    /// Expand each dirty tile by `radius` pixels in every
    /// direction, rounded out to tile boundaries, clamped to the
    /// frame. A neighborhood node's output tile is dirty when
    /// any input pixel within its radius changed.
    #[must_use]
    pub fn expand_by_pixels(&self, radius: u32) -> Self {
        let mut out = Self::empty(self.grid);
        for ty in 0..self.grid.rows {
            for tx in 0..self.grid.cols {
                if !self.is_dirty(tx, ty) {
                    continue;
                }
                let (x0, y0, x1, y1) = self.grid.tile_rect(tx, ty);
                let rx0 = x0.saturating_sub(radius);
                let ry0 = y0.saturating_sub(radius);
                let rx1 = x1.saturating_add(radius).min(self.grid.width);
                let ry1 = y1.saturating_add(radius).min(self.grid.height);
                let (tx0, ty0, tx1, ty1) = self.grid.tiles_for_rect(rx0, ry0, rx1, ry1);
                for y in ty0..ty1 {
                    for x in tx0..tx1 {
                        out.mark(x, y);
                    }
                }
            }
        }
        out
    }

    /// Map this set through an integer scale factor into
    /// `out_grid`: a dirty input tile marks every output tile
    /// its pixels scale into. The caller must have established
    /// the scale with [`integer_scale`]; the mapping is by pixel
    /// rectangles, so a partial edge tile maps to exactly the
    /// output tiles its pixels cover. A zero scale maps to the
    /// whole output grid, which is the conservative answer for
    /// a scale that cannot exist.
    #[must_use]
    pub fn map_scale(&self, scale: u32, out_grid: TileGrid) -> Self {
        let mut out = Self::empty(out_grid);
        if scale == 0 {
            return Self::whole(out_grid);
        }
        for ty in 0..self.grid.rows {
            for tx in 0..self.grid.cols {
                if !self.is_dirty(tx, ty) {
                    continue;
                }
                let (x0, y0, x1, y1) = self.grid.tile_rect(tx, ty);
                let sx0 = (u64::from(x0) * u64::from(scale)).min(u64::from(out_grid.width));
                let sy0 = (u64::from(y0) * u64::from(scale)).min(u64::from(out_grid.height));
                let sx1 = (u64::from(x1) * u64::from(scale)).min(u64::from(out_grid.width));
                let sy1 = (u64::from(y1) * u64::from(scale)).min(u64::from(out_grid.height));
                let (tx0, ty0, tx1, ty1) = out_grid.tiles_for_rect(
                    u32::try_from(sx0).unwrap_or(u32::MAX),
                    u32::try_from(sy0).unwrap_or(u32::MAX),
                    u32::try_from(sx1).unwrap_or(u32::MAX),
                    u32::try_from(sy1).unwrap_or(u32::MAX),
                );
                for y in ty0..ty1 {
                    for x in tx0..tx1 {
                        out.mark(x, y);
                    }
                }
            }
        }
        out
    }

    /// Exact per-tile comparison of two frames: a tile is dirty
    /// when any of its pixel bytes differ. Frames of different
    /// dimensions or formats compare as wholly dirty — a
    /// resolution or format change invalidates everything. Row
    /// padding is ignored: only pixel bytes compare.
    #[must_use]
    pub fn diff(prev: &Frame, curr: &Frame) -> Self {
        let grid = TileGrid::from_dims(curr.width(), curr.height());
        if prev.width() != curr.width()
            || prev.height() != curr.height()
            || prev.format() != curr.format()
        {
            return Self::whole(grid);
        }
        let bpp = curr.bytes_per_pixel();
        let mut out = Self::empty(grid);
        for ty in 0..grid.rows {
            for tx in 0..grid.cols {
                let (x0, y0, x1, y1) = grid.tile_rect(tx, ty);
                let mut changed = false;
                for y in y0..y1 {
                    let start = x0 as usize * bpp;
                    let end = x1 as usize * bpp;
                    if prev.row(y)[start..end] != curr.row(y)[start..end] {
                        changed = true;
                        break;
                    }
                }
                if changed {
                    out.mark(tx, ty);
                }
            }
        }
        out
    }
}

/// The integer scale factor from `input_dims` to `output_dims`,
/// when the output is exactly the input times that factor in
/// both dimensions: `Some(s)` with `s >= 1`, or `None` when the
/// sizes do not scale by one integer factor (including any zero
/// dimension). The executor uses it to map dirty tiles across a
/// node; `None` means the mapping is unknown and the node
/// invalidates whole-frame.
#[must_use]
pub(crate) fn integer_scale(input_dims: (u32, u32), output_dims: (u32, u32)) -> Option<u32> {
    let (iw, ih) = input_dims;
    let (ow, oh) = output_dims;
    if iw == 0 || ih == 0 || ow == 0 || oh == 0 {
        return None;
    }
    if ow % iw != 0 || oh % ih != 0 {
        return None;
    }
    let scale = ow / iw;
    if scale == 0 || oh / ih != scale {
        return None;
    }
    Some(scale)
}

/// Map one input dirty set through a node's declared
/// dependencies into its output tile grid. Local passes the set
/// through the integer scale; Neighborhood expands it by the
/// declared radius first; Global turns any change into a
/// whole-frame dirty set. A stateful node has no invalidation
/// rule in the node trait, so any evaluation dirties its whole
/// output — its history can change the output even when the
/// input did not. An empty input set propagates to an empty
/// output set for stateless nodes: no input change, no output
/// change.
#[must_use]
pub(crate) fn propagate(
    spatial: SpatialDependency,
    temporal: TemporalDependency,
    input: &DirtySet,
    input_dims: (u32, u32),
    output_grid: TileGrid,
) -> DirtySet {
    if temporal == TemporalDependency::Stateful {
        return DirtySet::whole(output_grid);
    }
    if input.is_empty() {
        return DirtySet::empty(output_grid);
    }
    let output_dims = (output_grid.width(), output_grid.height());
    match spatial {
        SpatialDependency::Global => DirtySet::whole(output_grid),
        SpatialDependency::Local => match integer_scale(input_dims, output_dims) {
            Some(scale) => input.map_scale(scale, output_grid),
            None => DirtySet::whole(output_grid),
        },
        SpatialDependency::Neighborhood { radius } => {
            let expanded = input.expand_by_pixels(radius);
            match integer_scale(input_dims, output_dims) {
                Some(scale) => expanded.map_scale(scale, output_grid),
                None => DirtySet::whole(output_grid),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::PixelFormat;

    fn filled_frame(width: u32, height: u32, value: u8) -> Result<Frame, String> {
        Frame::filled(width, height, PixelFormat::Rgb8, value).map_err(|e| e.to_string())
    }

    fn frame_with_pixel(
        width: u32,
        height: u32,
        x: u32,
        y: u32,
        rgb: [u8; 3],
    ) -> Result<Frame, String> {
        let mut frame = filled_frame(width, height, 0)?;
        frame.pixel_mut(x, y).copy_from_slice(&rgb);
        Ok(frame)
    }

    #[test]
    fn grid_dimensions_round_up() {
        assert_eq!(TileGrid::from_dims(32, 32).cols(), 2);
        assert_eq!(TileGrid::from_dims(32, 32).rows(), 2);
        assert_eq!(TileGrid::from_dims(20, 20).cols(), 2);
        assert_eq!(TileGrid::from_dims(17, 16).cols(), 2);
        assert_eq!(TileGrid::from_dims(17, 16).rows(), 1);
        assert_eq!(TileGrid::from_dims(16, 16).tile_count(), 1);
        assert_eq!(TileGrid::from_dims(0, 0).tile_count(), 0);
        assert_eq!(TileGrid::from_dims(256, 224).tile_count(), 16 * 14);
    }

    #[test]
    fn tile_rect_clamps_edge_tiles() {
        let grid = TileGrid::from_dims(20, 20);
        assert_eq!(grid.tile_rect(0, 0), (0, 0, 16, 16));
        assert_eq!(grid.tile_rect(1, 1), (16, 16, 20, 20));
        assert_eq!(grid.tile_rect(2, 0), (20, 0, 20, 16));
    }

    #[test]
    fn diff_identical_frames_is_empty() -> Result<(), String> {
        let a = filled_frame(32, 32, 7)?;
        let b = filled_frame(32, 32, 7)?;
        assert!(DirtySet::diff(&a, &b).is_empty());
        Ok(())
    }

    #[test]
    fn diff_single_pixel_marks_one_tile() -> Result<(), String> {
        let a = filled_frame(32, 32, 0)?;
        let b = frame_with_pixel(32, 32, 20, 5, [9, 9, 9])?;
        let set = DirtySet::diff(&a, &b);
        assert_eq!(set.count(), 1);
        assert!(set.is_dirty(1, 0));
        Ok(())
    }

    #[test]
    fn diff_pixel_on_tile_boundary_marks_its_own_tile() -> Result<(), String> {
        let a = filled_frame(32, 32, 0)?;
        let b = frame_with_pixel(32, 32, 16, 0, [9, 9, 9])?;
        let set = DirtySet::diff(&a, &b);
        assert_eq!(set.count(), 1);
        assert!(set.is_dirty(1, 0));
        assert!(!set.is_dirty(0, 0));
        Ok(())
    }

    #[test]
    fn diff_different_dimensions_is_whole() -> Result<(), String> {
        let a = filled_frame(32, 32, 0)?;
        let b = filled_frame(48, 32, 0)?;
        let set = DirtySet::diff(&a, &b);
        assert_eq!(set.count(), set.grid().tile_count());
        Ok(())
    }

    #[test]
    fn diff_ignores_row_padding() -> Result<(), String> {
        let tight = filled_frame(2, 1, 5)?;
        let strided = Frame::new_with_stride(2, 1, PixelFormat::Rgb8, 8, vec![5; 8])
            .map_err(|e| e.to_string())?;
        assert!(DirtySet::diff(&tight, &strided).is_empty());
        Ok(())
    }

    #[test]
    fn expand_by_one_pixel_reaches_neighbors() {
        let grid = TileGrid::from_dims(64, 64);
        let mut set = DirtySet::empty(grid);
        set.mark(1, 1);
        let expanded = set.expand_by_pixels(1);
        assert_eq!(expanded.count(), 9);
        for ty in 0..3 {
            for tx in 0..3 {
                assert!(expanded.is_dirty(tx, ty));
            }
        }
        assert!(!expanded.is_dirty(3, 3));
    }

    #[test]
    fn expand_by_one_pixel_clamps_at_frame_edge() {
        let grid = TileGrid::from_dims(64, 64);
        let mut set = DirtySet::empty(grid);
        set.mark(0, 0);
        let expanded = set.expand_by_pixels(1);
        assert_eq!(expanded.count(), 4);
        assert!(expanded.is_dirty(1, 1));
        assert!(!expanded.is_dirty(2, 2));
    }

    #[test]
    fn map_scale_doubles_tiles() {
        let in_grid = TileGrid::from_dims(32, 32);
        let out_grid = TileGrid::from_dims(64, 64);
        let mut set = DirtySet::empty(in_grid);
        set.mark(1, 0);
        let mapped = set.map_scale(2, out_grid);
        assert_eq!(mapped.count(), 4);
        assert!(mapped.is_dirty(2, 0));
        assert!(mapped.is_dirty(3, 0));
        assert!(mapped.is_dirty(2, 1));
        assert!(mapped.is_dirty(3, 1));
    }

    #[test]
    fn map_scale_partial_edge_tile_maps_exactly() {
        let in_grid = TileGrid::from_dims(20, 20);
        let out_grid = TileGrid::from_dims(40, 40);
        let mut set = DirtySet::empty(in_grid);
        set.mark(1, 1);
        let mapped = set.map_scale(2, out_grid);
        assert_eq!(mapped.count(), 1);
        assert!(mapped.is_dirty(2, 2));
    }

    #[test]
    fn integer_scale_cases() {
        assert_eq!(integer_scale((32, 32), (64, 64)), Some(2));
        assert_eq!(integer_scale((32, 32), (32, 32)), Some(1));
        assert_eq!(integer_scale((32, 32), (128, 128)), Some(4));
        assert_eq!(integer_scale((32, 32), (64, 32)), None);
        assert_eq!(integer_scale((20, 20), (40, 41)), None);
        assert_eq!(integer_scale((0, 32), (0, 64)), None);
    }

    #[test]
    fn propagate_rules() {
        let in_grid = TileGrid::from_dims(32, 32);
        let out_grid = TileGrid::from_dims(64, 64);
        let mut set = DirtySet::empty(in_grid);
        set.mark(1, 0);
        let local = propagate(
            SpatialDependency::Local,
            TemporalDependency::Stateless,
            &set,
            (32, 32),
            out_grid,
        );
        assert_eq!(local.count(), 4);
        let global = propagate(
            SpatialDependency::Global,
            TemporalDependency::Stateless,
            &set,
            (32, 32),
            out_grid,
        );
        assert_eq!(global.count(), global.grid().tile_count());
        let empty = propagate(
            SpatialDependency::Global,
            TemporalDependency::Stateless,
            &DirtySet::empty(in_grid),
            (32, 32),
            out_grid,
        );
        assert!(empty.is_empty());
        let stateful = propagate(
            SpatialDependency::Local,
            TemporalDependency::Stateful,
            &DirtySet::empty(in_grid),
            (32, 32),
            out_grid,
        );
        assert_eq!(stateful.count(), stateful.grid().tile_count());
        let near = propagate(
            SpatialDependency::Neighborhood { radius: 1 },
            TemporalDependency::Stateless,
            &set,
            (32, 32),
            TileGrid::from_dims(32, 32),
        );
        assert_eq!(near.count(), 4);
    }
}
