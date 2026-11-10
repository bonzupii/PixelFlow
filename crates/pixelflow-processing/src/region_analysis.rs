//! Region analysis: deterministic content classification of a frame,
//! one class per tile, with confidence scores.
//!
//! This is the section 8 stage of
//! `docs/incremental-region-aware-architecture.md`, built as a
//! standalone capability. It produces metadata, not pixels: a
//! [`RegionMap`] holding one [`TileClassification`] per tile of the
//! [`TileGrid`]. The run path computes one map per frame with
//! [`RegionAnalyzer::analyze_dirty`], restricted to the tiles
//! whose pixels changed, and hands the map's
//! [`RegionMap::routes`] to the graph, which resolves every
//! route to a node. The run path builds one graph per
//! algorithm the routing table names (see
//! [`ROUTING_TABLE`] and [`resolve_algorithm`]) and runs
//! only the graphs the frame's tiles need, compositing the
//! outputs tile by tile, so each tile is processed by the
//! algorithm its class routes to.
//!
//! The analysis is color and edge features only. It does not do
//! semantic object recognition: it cannot tell what a shape
//! depicts, only what its colors and edges look like. A sprite
//! can look like a background element, text can sit inside a
//! sprite, and a flattened frame has no layer structure. The
//! confidence threshold and the [`ContentClass::Unknown`] class
//! are the answer to that, not better features.
//!
//! # Features
//!
//! Every feature is computed from the source pixels of one tile,
//! in fixed order, with no randomness and no learned weights, so
//! the same frame always produces the same map:
//!
//! - palette size: distinct RGB triples in the tile.
//! - color variance and range, per channel.
//! - edge density: fraction of pixels that are edges under a
//!   fixed operator. A pixel is an edge pixel when the largest
//!   per-channel absolute difference to its right or lower
//!   neighbor exceeds [`EDGE_THRESHOLD`].
//! - connected components: count and sizes of same-color
//!   connected regions, 4-connected, exact color equality.
//! - contour geometry: length and straightness of the dominant
//!   contour, the boundary of the largest component. Length is
//!   the count of boundary pixels; straightness is the
//!   bounding-box perimeter divided by that length, clamped to
//!   1, so an axis-aligned rectangle scores 1 and a wiggly
//!   boundary scores less.
//! - repeating patterns: the smallest row and column period
//!   whose shifted pixels match on at least
//!   [`PERIOD_MATCH_THRESHOLD`] of informative pairs. A pair
//!   is informative when at least one of its pixels is not
//!   the dominant color: background-background pairs match
//!   at every shift and carry no evidence of repetition.
//!   A period only counts when the tile actually varies
//!   along that axis: a tile that is constant along an axis
//!   has no period on that axis, so a horizontal gradient
//!   is not misread as a vertical pattern.
//! - texture complexity: mean absolute adjacent-pixel
//!   difference, normalized to [0, 1].
//! - gradient characteristics: a least-squares plane fit of
//!   luminance on (x, y), giving fit quality, direction, and
//!   magnitude in luminance levels per pixel.
//! - foreground edge ratio: the fraction of non-dominant-color
//!   pixels that are edge pixels. Thin strokes score near 1;
//!   solid bodies score low, which is what separates text from
//!   sprites.
//!
//! The feature set is fixed by [`FEATURE_VERSION`]. A saved
//! profile's region routing names this version, so a detector
//! change does not silently change what a routing means.
//!
//! # Classification
//!
//! The rule is a documented decision procedure over the
//! features, not a tuned black box. Each class gets a score in
//! [0, 1] from its rule below; the highest score wins, ties
//! breaking in the order Flat, Gradient, Patterned, Text,
//! Sprite. Scores of 0.85 and above are primary evidence: a
//! clean exemplar of the class. Scores of 0.55 to 0.6 are weak
//! evidence and never clear the default confidence threshold
//! on their own, so weak evidence classifies as
//! [`ContentClass::Unknown`].
//!
//! - Flat: one color scores 1.0. Two colors within a range of 8
//!   and almost no edges score 0.9. Near-zero variance with
//!   almost no edges scores 0.7.
//! - Gradient: almost no edges, a plane fit of 0.8 or better,
//!   and a color range of 32 or more scores 0.9. A looser fit
//!   of 0.6 with a range of 16 scores 0.6.
//! - Patterned: a row or column period with at least two colors
//!   scores 0.9.
//! - Text: edge density 0.12 or more, at least 4 components, a
//!   foreground edge ratio of 0.7 or more, and at most 16
//!   colors scores 0.85. Edge density 0.08 or more with at
//!   least 3 components and at most 16 colors scores 0.55.
//! - Sprite: a dominant component between a quarter and 85% of
//!   the tile, 2 to 8 components, 2 to 64 colors, edge density
//!   between 0.02 and 0.5, and a foreground edge ratio below
//!   0.7 scores 0.85. A dominant component of a quarter or
//!   more with at least 2 colors scores 0.55.
//! - Unknown: the best score is below the confidence threshold.
//!   Its confidence is 1 minus the best score: confidence that
//!   no class fits.
//!
//! # Temporal stability
//!
//! Two mechanisms, both from section 10 of the specification:
//!
//! - Reclassification gating. A tile keeps its previous class
//!   when its feature vector moved less than
//!   [`DEFAULT_RECLASSIFY_DISTANCE`] since the last analysis.
//!   The stored features are the last analyzed ones, so a
//!   slow drift accumulates across frames until it crosses the
//!   threshold.
//! - Hysteresis. When a tile is re-analyzed and a new class
//!   wins, the new class must beat the previous class's score
//!   on the current features by
//!   [`DEFAULT_HYSTERESIS_MARGIN`], not merely exceed it.
//!   Hysteresis only applies while the previous class still
//!   scores at or above the confidence threshold on the
//!   current features; a class that evaporated does not hold
//!   the tile.
//!
//! A tile that changes class is routing-dirty even when its
//! pixels did not change: see
//! [`RegionMap::changed_tiles_since`]. The flicker count for a
//! frame is [`RegionMap::class_changes_since`].
//!
//! # Overrides and routing
//!
//! [`TileOverride`] forces a rectangle of tiles to a class, or
//! to [`ContentClass::Unknown`]. An override always wins over
//! the detector for its tiles, and the map reports which tiles
//! were overridden. A tile that was overridden has no trusted
//! previous detector state, so it is freshly analyzed once the
//! override is removed.
//!
//! [`RoutingProfile`] maps each class to a branch name, or to
//! the general path. The default routes every class to its
//! branch; [`ContentClass::Unknown`] always uses the general
//! path. [`ROUTING_TABLE`] maps each branch's class to the
//! algorithm that processes it, so a branch name and an
//! algorithm id are different things: the profile says where
//! a tile goes, the table says what processes it there.
//! [`RegionMap::halo_for`] and [`RegionMap::halos`] give the
//! overlap tiles around routed tiles, so a branch that reads
//! across a tile edge does not seam against its neighbor.

use pixelflow_core::{DirtySet, Frame, TileGrid};

/// Feature-set version. The classification rules and the
/// feature definitions above are version 2. Bump this when
/// either changes, so a saved routing names the detector it
/// was built against.
pub const FEATURE_VERSION: u32 = 2;

/// Default confidence threshold: the best class score must
/// reach this for the tile to take that class. Below it the
/// tile is [`ContentClass::Unknown`].
pub const DEFAULT_CONFIDENCE_THRESHOLD: f64 = 0.6;

/// Default hysteresis margin: a new class must beat the
/// previous class's score by this much to take the tile.
pub const DEFAULT_HYSTERESIS_MARGIN: f64 = 0.10;

/// Default reclassification gate: a tile is only re-analyzed
/// when its feature vector moved at least this far since the
/// last analysis. Feature distance is the largest normalized
/// per-feature change, so 0.05 is a 5% move on any one feature.
pub const DEFAULT_RECLASSIFY_DISTANCE: f64 = 0.05;

/// Default halo radius in source pixels around tiles routed
/// to a branch, so branch processing that reads across a tile
/// edge overlaps its neighbor instead of seaming against it.
pub const DEFAULT_HALO_RADIUS_PX: u32 = 1;

/// Edge operator threshold: the largest per-channel absolute
/// difference to a right or lower neighbor, in levels, above
/// which a pixel counts as an edge pixel. 32 is a quarter of
/// the channel range: well above sensor and dither noise, well
/// below a real stroke or silhouette boundary.
pub const EDGE_THRESHOLD: u8 = 32;

/// Period acceptance: the fraction of shifted pixel pairs
/// that must match for a row or column period to count.
/// 0.9 tolerates a few pixels of detail on top of a repeating
/// structure without accepting a coincidence.
pub const PERIOD_MATCH_THRESHOLD: f64 = 0.9;

/// Smallest period considered. Period 1 means constant along
/// the axis, which is not a repeating structure.
pub const MIN_PERIOD: u32 = 2;

/// Largest period considered. A 16-pixel tile holds at most
/// two repeats of an 8-pixel period, which is the smallest
/// repetition worth detecting.
pub const MAX_PERIOD: u32 = 8;

/// Largest variance one u8 channel can have: a two-point
/// distribution at 0 and 255 has variance 255 squared over 4.
/// Feature distance normalizes channel variance by this.
const MAX_CHANNEL_VARIANCE: f64 = 16256.25;

/// Flat rule: the largest per-channel range allowed for the
/// two-color flat score.
const FLAT_MAX_RANGE: u8 = 8;

/// Flat rule: edge density below this counts as almost no
/// edges.
const FLAT_MAX_EDGE_DENSITY: f64 = 0.01;

/// Flat rule: the largest per-channel variance allowed for
/// the near-flat score.
const FLAT_MAX_VARIANCE: f64 = 4.0;

/// Gradient rule, primary: edge density below this, with a
/// plane fit and range at least the constants below.
const GRADIENT_MAX_EDGE_DENSITY: f64 = 0.02;

/// Gradient rule, primary: the plane fit must reach this.
const GRADIENT_MIN_FIT: f64 = 0.8;

/// Gradient rule, primary: the color range must reach this.
const GRADIENT_MIN_RANGE: u8 = 32;

/// Gradient rule, secondary: looser edge, fit, and range
/// bounds for a gradient with slight noise or banding.
const GRADIENT_MAX_EDGE_DENSITY_LOOSE: f64 = 0.05;

/// Gradient rule, secondary: the plane fit must reach this.
const GRADIENT_MIN_FIT_LOOSE: f64 = 0.6;

/// Gradient rule, secondary: the color range must reach this.
const GRADIENT_MIN_RANGE_LOOSE: u8 = 16;

/// Text rule, primary: edge density at least this.
const TEXT_MIN_EDGE_DENSITY: f64 = 0.12;

/// Text rule, primary: at least this many components.
const TEXT_MIN_COMPONENTS: usize = 4;

/// Text rule, primary: foreground edge ratio at least this.
/// Thin strokes are almost all edge; solid bodies are not.
const TEXT_MIN_FOREGROUND_EDGE_RATIO: f64 = 0.7;

/// Text rule: at most this many colors. Text is ink on a
/// background, not a photograph.
const TEXT_MAX_PALETTE: usize = 16;

/// Text rule, secondary: looser edge and component bounds.
const TEXT_MIN_EDGE_DENSITY_LOOSE: f64 = 0.08;

/// Text rule, secondary: at least this many components.
const TEXT_MIN_COMPONENTS_LOOSE: usize = 3;

/// Sprite rule, primary: the largest component holds at
/// least this fraction of the tile.
const SPRITE_MIN_LARGEST_FRACTION: f64 = 0.25;

/// Sprite rule, primary: the largest component holds at most
/// this fraction. Above it the tile is a background with a
/// speck, not a bounded shape.
const SPRITE_MAX_LARGEST_FRACTION: f64 = 0.85;

/// Sprite rule, primary: at most this many components. A
/// sprite is one shape with a few details, not a field of
/// fragments.
const SPRITE_MAX_COMPONENTS: usize = 8;

/// Sprite rule, primary: at most this many colors.
const SPRITE_MAX_PALETTE: usize = 64;

/// Sprite rule, primary: edge density between these bounds.
/// Below the lower bound there is no silhouette; above the
/// upper bound the tile is texture, not a shape.
const SPRITE_MIN_EDGE_DENSITY: f64 = 0.02;

/// Sprite rule, primary: edge density at most this.
const SPRITE_MAX_EDGE_DENSITY: f64 = 0.5;

/// What kind of visual content a tile holds, from color and
/// edge features only. No class claims semantic recognition:
/// `Sprite` means a bounded shape with a silhouette, not a
/// recognized character, and `Text` means thin strokes, not
/// recognized glyphs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContentClass {
    /// Text and UI: thin strokes, sharp corners, glyph shapes.
    Text,
    /// Sprites and characters: bounded shapes with silhouettes.
    Sprite,
    /// Flat backgrounds: solid colors, near-zero variance.
    Flat,
    /// Patterned backgrounds: repeating structures.
    Patterned,
    /// Gradients: smooth directional color transitions.
    Gradient,
    /// Unknown or mixed: use the general-purpose path.
    Unknown,
}

/// The measured features of one tile. Every field is a
/// deterministic function of the tile's source pixels, as
/// documented at the top of this module.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TileFeatures {
    /// Distinct RGB triples in the tile.
    pub palette_size: usize,
    /// Per-channel color variance, in squared levels.
    pub channel_variance: [f64; 3],
    /// Per-channel color range (max minus min), in levels.
    pub channel_range: [u8; 3],
    /// Fraction of pixels that are edge pixels.
    pub edge_density: f64,
    /// Count of same-color connected regions.
    pub component_count: usize,
    /// Fraction of the tile the largest component covers.
    pub largest_component_fraction: f64,
    /// Mean component size, in pixels.
    pub mean_component_size: f64,
    /// Length of the dominant contour, in boundary pixels.
    pub dominant_contour_length: usize,
    /// Straightness of the dominant contour: bounding-box
    /// perimeter over contour length, clamped to [0, 1].
    pub dominant_contour_straightness: f64,
    /// Smallest row period accepted, if any.
    pub row_period: Option<u32>,
    /// Smallest column period accepted, if any.
    pub col_period: Option<u32>,
    /// Match fraction of the accepted row period, or 0.0.
    pub row_period_score: f64,
    /// Match fraction of the accepted column period, or 0.0.
    pub col_period_score: f64,
    /// Mean absolute adjacent-pixel difference, normalized
    /// to [0, 1]. High-frequency energy.
    pub texture_complexity: f64,
    /// Plane-fit quality of luminance on (x, y), in [0, 1].
    /// 1.0 is a perfect linear gradient.
    pub gradient_fit: f64,
    /// Unit direction of the luminance gradient, when the
    /// gradient magnitude is non-zero.
    pub gradient_direction: Option<(f64, f64)>,
    /// Luminance slope magnitude, in levels per pixel.
    pub gradient_magnitude: f64,
    /// Fraction of non-dominant-color pixels that are edge
    /// pixels. Near 1 for thin strokes, low for solid bodies.
    pub foreground_edge_ratio: f64,
}

impl TileFeatures {
    /// Largest per-channel color variance.
    fn max_channel_variance(&self) -> f64 {
        self.channel_variance
            .iter()
            .fold(0.0_f64, |acc, value| acc.max(*value))
    }

    /// Largest per-channel color range.
    fn max_channel_range(&self) -> u8 {
        self.channel_range
            .iter()
            .fold(0_u8, |acc, value| acc.max(*value))
    }

    /// Feature distance to another tile's features: the
    /// largest normalized per-feature change, in [0, 1].
    /// Each feature is normalized to its own range before
    /// the comparison, so a 5% move on any one feature is a
    /// distance of 0.05. Periods are not part of the
    /// distance: a period appearing or disappearing moves
    /// the features it comes from.
    #[must_use]
    pub fn distance(&self, other: &Self) -> f64 {
        let mut distance =
            (normalized_count(self.palette_size) - normalized_count(other.palette_size)).abs();
        distance = distance.max(
            ((self.max_channel_variance() - other.max_channel_variance()).abs())
                / MAX_CHANNEL_VARIANCE,
        );
        distance = distance.max((self.edge_density - other.edge_density).abs());
        distance = distance
            .max((self.largest_component_fraction - other.largest_component_fraction).abs());
        distance = distance.max((self.texture_complexity - other.texture_complexity).abs());
        distance = distance.max((self.gradient_fit - other.gradient_fit).abs());
        distance = distance.max((self.foreground_edge_ratio - other.foreground_edge_ratio).abs());
        distance
    }
}

/// A count of tile pixels, normalized by the largest tile
/// size, for feature distance. Counts above the tile size
/// clamp at the top of the range by construction of the
/// caller: no tile holds more than 256 pixels.
fn normalized_count(count: usize) -> f64 {
    f64::from(u32::try_from(count).unwrap_or(u32::MAX)) / 256.0
}

/// One tile's classification: its class, the confidence in
/// that class, the features the decision was made from, and
/// whether an override set it.
#[derive(Debug, Clone, PartialEq)]
pub struct TileClassification {
    /// The class assigned to the tile.
    pub class: ContentClass,
    /// Confidence in the class, in [0, 1]. For
    /// [`ContentClass::Unknown`] this is 1 minus the best
    /// class score: confidence that no class fits.
    pub confidence: f64,
    /// The features this classification was made from. When
    /// reclassification gating holds a class, these are the
    /// last analyzed features, not the current frame's.
    pub features: TileFeatures,
    /// True when a [`TileOverride`] set this tile's class.
    pub overridden: bool,
}

/// A user override: force a rectangle of tiles to a class,
/// or to [`ContentClass::Unknown`]. Tile coordinates are
/// half-open, `[tx0, tx1)` by `[ty0, ty1)`. Coordinates
/// outside the grid match no tiles; no clamping is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileOverride {
    /// First tile column covered.
    pub tx0: u32,
    /// First tile row covered.
    pub ty0: u32,
    /// Tile column after the last covered, exclusive.
    pub tx1: u32,
    /// Tile row after the last covered, exclusive.
    pub ty1: u32,
    /// The class to force these tiles to.
    pub class: ContentClass,
}

impl TileOverride {
    /// Build an override for the half-open tile rectangle
    /// `[tx0, tx1)` by `[ty0, ty1)`, forcing `class`.
    #[must_use]
    pub const fn new(tx0: u32, ty0: u32, tx1: u32, ty1: u32, class: ContentClass) -> Self {
        Self {
            tx0,
            ty0,
            tx1,
            ty1,
            class,
        }
    }

    /// True when tile (`tx`, `ty`) falls inside this
    /// override's rectangle.
    #[must_use]
    pub const fn covers(&self, tx: u32, ty: u32) -> bool {
        tx >= self.tx0 && tx < self.tx1 && ty >= self.ty0 && ty < self.ty1
    }
}

/// Tunable settings for the analyzer. All thresholds are
/// named constants with their reasons in this module; the
/// config exists so a profile can tune them without
/// forking the rules.
#[derive(Debug, Clone, PartialEq)]
pub struct RegionConfig {
    /// The best class score must reach this for the tile to
    /// take that class. Below it the tile is
    /// [`ContentClass::Unknown`].
    pub confidence_threshold: f64,
    /// A new class must beat the previous class's score by
    /// this much to take the tile.
    pub hysteresis_margin: f64,
    /// A tile is only re-analyzed when its feature vector
    /// moved at least this far since the last analysis.
    pub reclassify_distance: f64,
}

impl Default for RegionConfig {
    fn default() -> Self {
        Self {
            confidence_threshold: DEFAULT_CONFIDENCE_THRESHOLD,
            hysteresis_margin: DEFAULT_HYSTERESIS_MARGIN,
            reclassify_distance: DEFAULT_RECLASSIFY_DISTANCE,
        }
    }
}

/// The region analyzer: stateless over frames except for the
/// previous map the caller passes in. Pure computation over
/// frame data: no capture, no portal, no presentation, no IO.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RegionAnalyzer {
    /// The thresholds this analyzer classifies with.
    config: RegionConfig,
}

impl RegionAnalyzer {
    /// Build an analyzer with the given config.
    #[must_use]
    pub const fn new(config: RegionConfig) -> Self {
        Self { config }
    }

    /// The config this analyzer classifies with.
    #[must_use]
    pub const fn config(&self) -> &RegionConfig {
        &self.config
    }

    /// Analyze a frame into a region map: one
    /// classification per tile.
    ///
    /// The frame is converted to RGB8 first, so any format
    /// analyzes the same way. `overrides` apply after the
    /// detector, in slice order, later overrides winning
    /// overlaps; `previous` is the last frame's map, used
    /// for reclassification gating and hysteresis, and only
    /// when it is on the same grid. A tile whose previous
    /// classification was overridden has no trusted detector
    /// state and is freshly analyzed.
    ///
    /// This analyzes every tile of the frame: it is
    /// [`RegionAnalyzer::analyze_dirty`] with the whole grid
    /// dirty.
    #[must_use]
    pub fn analyze(
        &self,
        frame: &Frame,
        overrides: &[TileOverride],
        previous: Option<&RegionMap>,
    ) -> RegionMap {
        let grid = TileGrid::from_dims(frame.width(), frame.height());
        self.analyze_dirty(frame, &DirtySet::whole(grid), overrides, previous)
    }

    /// Analyze a frame into a region map, re-analyzing only
    /// the tiles in `dirty`: the section 11 dirty-restricted
    /// analysis. A clean tile carries its previous
    /// classification forward unchanged, which is exactly
    /// what reclassification gating would return for it: its
    /// pixels did not change, so its features did not move.
    /// A tile is re-analyzed when it is dirty, when it has
    /// no previous classification on the same grid, when its
    /// previous classification was overridden, or when an
    /// override covers it. Re-analyzed tiles go through
    /// [`RegionAnalyzer::classify`], so gating and hysteresis
    /// still apply to them. A `dirty` set on a different grid
    /// than the frame analyzes every tile, the conservative
    /// answer when the grids do not line up.
    #[must_use]
    pub fn analyze_dirty(
        &self,
        frame: &Frame,
        dirty: &DirtySet,
        overrides: &[TileOverride],
        previous: Option<&RegionMap>,
    ) -> RegionMap {
        let rgb = frame.to_rgb8();
        let grid = TileGrid::from_dims(rgb.width(), rgb.height());
        let whole = DirtySet::whole(grid);
        let dirty = if dirty.grid() == grid { dirty } else { &whole };
        let previous_same_grid = previous.filter(|map| map.grid == grid);
        let mut tiles = Vec::with_capacity(grid.tile_count());
        for ty in 0..grid.rows() {
            for tx in 0..grid.cols() {
                let previous_tile = previous_same_grid
                    .and_then(|map| map.tile(tx, ty))
                    .filter(|tile| !tile.overridden);
                let override_here = overrides.iter().rev().find(|o| o.covers(tx, ty));
                let reanalyze =
                    dirty.is_dirty(tx, ty) || previous_tile.is_none() || override_here.is_some();
                // A tile an override covers is always
                // re-analyzed, so the carried-forward branch
                // below never holds an overridden tile.
                let classification = if let Some(tile) = previous_tile.filter(|_| !reanalyze) {
                    tile.clone()
                } else {
                    let features = extract(&rgb, grid, tx, ty);
                    let mut classified = self.classify(&features, previous_tile);
                    if let Some(tile_override) = override_here {
                        classified.class = tile_override.class;
                        classified.confidence = 1.0;
                        classified.features = features;
                        classified.overridden = true;
                    }
                    classified
                };
                tiles.push(classification);
            }
        }
        RegionMap {
            grid,
            tiles,
            feature_version: FEATURE_VERSION,
        }
    }

    /// Classify one tile from its features, applying
    /// reclassification gating and hysteresis against a
    /// previous classification.
    ///
    /// Gating runs first: when the feature vector moved
    /// less than the configured distance since the last
    /// analysis, the previous classification is returned
    /// unchanged, with its stored features. Otherwise the
    /// class scores are computed and hysteresis decides
    /// whether a winning new class takes the tile.
    #[must_use]
    pub fn classify(
        &self,
        features: &TileFeatures,
        previous: Option<&TileClassification>,
    ) -> TileClassification {
        if let Some(prev) = previous
            && prev.features.distance(features) < self.config.reclassify_distance
        {
            return prev.clone();
        }
        let scores = class_scores(features);
        let mut best = (ContentClass::Unknown, 0.0_f64);
        for (class, score) in scores {
            if score > best.1 {
                best = (class, score);
            }
        }
        if let Some(prev) = previous
            && !prev.overridden
            && prev.class != ContentClass::Unknown
        {
            let prev_score = scores
                .iter()
                .find(|(class, _)| *class == prev.class)
                .map_or(0.0, |(_, score)| *score);
            if prev_score >= self.config.confidence_threshold
                && best.0 != prev.class
                && best.1 <= prev_score + self.config.hysteresis_margin
            {
                return TileClassification {
                    class: prev.class,
                    confidence: prev_score,
                    features: features.clone(),
                    overridden: false,
                };
            }
        }
        if best.1 < self.config.confidence_threshold {
            TileClassification {
                class: ContentClass::Unknown,
                confidence: 1.0 - best.1,
                features: features.clone(),
                overridden: false,
            }
        } else {
            TileClassification {
                class: best.0,
                confidence: best.1,
                features: features.clone(),
                overridden: false,
            }
        }
    }
}

/// One score per class, in the tie-break order Flat,
/// Gradient, Patterned, Text, Sprite. The rules and their
/// reasons are documented at the top of this module.
fn class_scores(features: &TileFeatures) -> [(ContentClass, f64); 5] {
    let max_variance = features.max_channel_variance();
    let max_range = features.max_channel_range();
    let flat = if features.palette_size <= 1 {
        1.0
    } else if features.palette_size <= 2
        && max_range <= FLAT_MAX_RANGE
        && features.edge_density < FLAT_MAX_EDGE_DENSITY
    {
        0.9
    } else if max_variance < FLAT_MAX_VARIANCE && features.edge_density < FLAT_MAX_EDGE_DENSITY {
        0.7
    } else {
        0.0
    };
    let gradient = if features.edge_density < GRADIENT_MAX_EDGE_DENSITY
        && features.gradient_fit >= GRADIENT_MIN_FIT
        && max_range >= GRADIENT_MIN_RANGE
    {
        0.9
    } else if features.edge_density < GRADIENT_MAX_EDGE_DENSITY_LOOSE
        && features.gradient_fit >= GRADIENT_MIN_FIT_LOOSE
        && max_range >= GRADIENT_MIN_RANGE_LOOSE
    {
        0.6
    } else {
        0.0
    };
    let patterned = if (features.row_period.is_some() || features.col_period.is_some())
        && features.palette_size >= 2
    {
        0.9
    } else {
        0.0
    };
    let text = if features.edge_density >= TEXT_MIN_EDGE_DENSITY
        && features.component_count >= TEXT_MIN_COMPONENTS
        && features.foreground_edge_ratio >= TEXT_MIN_FOREGROUND_EDGE_RATIO
        && features.palette_size <= TEXT_MAX_PALETTE
    {
        0.85
    } else if features.edge_density >= TEXT_MIN_EDGE_DENSITY_LOOSE
        && features.component_count >= TEXT_MIN_COMPONENTS_LOOSE
        && features.palette_size <= TEXT_MAX_PALETTE
    {
        0.55
    } else {
        0.0
    };
    let sprite_shape = features.largest_component_fraction >= SPRITE_MIN_LARGEST_FRACTION
        && features.largest_component_fraction <= SPRITE_MAX_LARGEST_FRACTION
        && features.component_count >= 2
        && features.component_count <= SPRITE_MAX_COMPONENTS;
    let sprite_surface = features.palette_size >= 2
        && features.palette_size <= SPRITE_MAX_PALETTE
        && features.edge_density >= SPRITE_MIN_EDGE_DENSITY
        && features.edge_density <= SPRITE_MAX_EDGE_DENSITY
        && features.foreground_edge_ratio < TEXT_MIN_FOREGROUND_EDGE_RATIO;
    let sprite = if sprite_shape && sprite_surface {
        0.85
    } else if features.largest_component_fraction >= SPRITE_MIN_LARGEST_FRACTION
        && features.palette_size >= 2
    {
        0.55
    } else {
        0.0
    };
    [
        (ContentClass::Flat, flat),
        (ContentClass::Gradient, gradient),
        (ContentClass::Patterned, patterned),
        (ContentClass::Text, text),
        (ContentClass::Sprite, sprite),
    ]
}

/// The region map of one frame: one classification per tile,
/// row-major, over the frame's [`TileGrid`].
#[derive(Debug, Clone, PartialEq)]
pub struct RegionMap {
    /// The tile grid these classifications index into.
    grid: TileGrid,
    /// One classification per tile, row-major.
    tiles: Vec<TileClassification>,
    /// The feature-set version that produced this map.
    feature_version: u32,
}

impl RegionMap {
    /// The tile grid this map indexes into.
    #[must_use]
    pub const fn grid(&self) -> TileGrid {
        self.grid
    }

    /// The feature-set version that produced this map.
    #[must_use]
    pub const fn feature_version(&self) -> u32 {
        self.feature_version
    }

    /// All tile classifications, row-major.
    #[must_use]
    pub fn tiles(&self) -> &[TileClassification] {
        &self.tiles
    }

    /// The classification of tile (`tx`, `ty`), or `None`
    /// when the tile is outside the grid.
    #[must_use]
    pub fn tile(&self, tx: u32, ty: u32) -> Option<&TileClassification> {
        if tx >= self.grid.cols() || ty >= self.grid.rows() {
            return None;
        }
        self.tiles.get(self.grid.tile_index(tx, ty))
    }

    /// How many tiles were set by a [`TileOverride`].
    #[must_use]
    pub fn overridden_count(&self) -> usize {
        self.tiles.iter().filter(|tile| tile.overridden).count()
    }

    /// The flicker count against a previous map: how many
    /// tiles changed class. Tiles with no counterpart in the
    /// previous map count as changed, so a grid change
    /// counts every tile that has no match.
    #[must_use]
    pub fn class_changes_since(&self, previous: &RegionMap) -> usize {
        self.changed_tiles_since(previous).len()
    }

    /// The tiles that changed class against a previous map.
    /// A tile that changes class is routing-dirty even when
    /// its pixels did not change: its branch assignment
    /// changed, so its output must be recomputed.
    #[must_use]
    pub fn changed_tiles_since(&self, previous: &RegionMap) -> Vec<(u32, u32)> {
        let mut changed = Vec::new();
        for ty in 0..self.grid.rows() {
            for tx in 0..self.grid.cols() {
                let changed_here = match previous.tile(tx, ty) {
                    Some(prev_tile) => self
                        .tile(tx, ty)
                        .is_some_and(|tile| tile.class != prev_tile.class),
                    None => true,
                };
                if changed_here {
                    changed.push((tx, ty));
                }
            }
        }
        changed
    }

    /// The halo around tiles of one class: those tiles plus
    /// every tile within `radius` source pixels of them.
    /// A branch that reads across a tile edge declares its
    /// halo radius, and the executor treats it like a
    /// neighborhood radius for invalidation.
    #[must_use]
    pub fn halo_for(&self, class: ContentClass, radius: u32) -> DirtySet {
        let mut set = DirtySet::empty(self.grid);
        for ty in 0..self.grid.rows() {
            for tx in 0..self.grid.cols() {
                if self.tile(tx, ty).is_some_and(|tile| tile.class == class) {
                    set.mark(tx, ty);
                }
            }
        }
        set.expand_by_pixels(radius)
    }

    /// The halo around every tile routed to a branch by the
    /// profile: the union of each routed class's halo at the
    /// profile's halo radius. Tiles on the general path get
    /// no halo.
    #[must_use]
    pub fn halos(&self, profile: &RoutingProfile) -> DirtySet {
        let mut out = DirtySet::empty(self.grid);
        for class in [
            ContentClass::Text,
            ContentClass::Sprite,
            ContentClass::Flat,
            ContentClass::Patterned,
            ContentClass::Gradient,
        ] {
            if profile.branch_name(class).is_some() {
                out = out.union(&self.halo_for(class, profile.halo_radius_px()));
            }
        }
        out
    }

    /// The halo-expanded invalidation for tiles that changed
    /// class against `previous`: the changed tiles that route
    /// to a branch under the profile, expanded by the
    /// profile's halo radius. A tile that changes class is
    /// routing-dirty even when its pixels did not change, and
    /// its neighbors are dirty with it: a branch reads across
    /// the tile edge, so a neighbor it overlaps must be
    /// re-processed too. This is the part of [`RegionMap::halos`]
    /// this frame's class changes account for; tiles whose
    /// class did not change contribute no invalidation here.
    /// Tiles that changed into a class on the general path
    /// get no halo.
    #[must_use]
    pub fn class_change_halos(&self, previous: &RegionMap, profile: &RoutingProfile) -> DirtySet {
        let mut changed = DirtySet::empty(self.grid);
        for (tx, ty) in self.changed_tiles_since(previous) {
            if self
                .tile(tx, ty)
                .is_some_and(|tile| profile.branch_name(tile.class).is_some())
            {
                changed.mark(tx, ty);
            }
        }
        changed.expand_by_pixels(profile.halo_radius_px())
    }

    /// The routing decision for every tile under a profile:
    /// each tile's class, confidence, and route.
    #[must_use]
    pub fn routes(&self, profile: &RoutingProfile) -> Vec<TileRoute> {
        self.tiles
            .iter()
            .map(|tile| TileRoute {
                class: tile.class,
                confidence: tile.confidence,
                route: profile.route_for(tile.class),
            })
            .collect()
    }
}

/// Where a tile is processed: the general-purpose path, or a
/// named branch. Branch names are metadata: the graph maps
/// them to nodes with `GraphBuilder::route_branch`, and the
/// run path builds one graph per algorithm the routing table
/// names, so each branch is processed by its tabled
/// algorithm, not by the general-path node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// The general-purpose path. This is where
    /// [`ContentClass::Unknown`] always goes.
    General,
    /// A named branch, such as `"text"` or `"sprite"`.
    Branch(&'static str),
}

/// One tile's routing decision: its class, its confidence,
/// and where it routes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TileRoute {
    /// The tile's class.
    pub class: ContentClass,
    /// Confidence in the class, in [0, 1].
    pub confidence: f64,
    /// Where the tile routes.
    pub route: Route,
}

/// Which branch each class routes to. A class with no branch
/// assigned falls back to the general path, and
/// [`ContentClass::Unknown`] always uses the general path:
/// it cannot be assigned a branch.
///
/// The default profile routes every class to its branch:
/// Text to `"text"`, Sprite to `"sprite"`, Flat to
/// `"flat"`, Patterned to `"patterned"`, Gradient to
/// `"gradient"`. What processes a branch is
/// [`ROUTING_TABLE`], not this profile: the profile says
/// where a tile goes, the table says what processes it
/// there.
#[derive(Debug, Clone, PartialEq)]
pub struct RoutingProfile {
    /// Branch for text tiles, or `None` for the general path.
    pub text_branch: Option<&'static str>,
    /// Branch for sprite tiles, or `None` for the general path.
    pub sprite_branch: Option<&'static str>,
    /// Branch for flat tiles, or `None` for the general path.
    pub flat_branch: Option<&'static str>,
    /// Branch for patterned tiles, or `None` for the general path.
    pub patterned_branch: Option<&'static str>,
    /// Branch for gradient tiles, or `None` for the general path.
    pub gradient_branch: Option<&'static str>,
    /// Halo radius in source pixels around tiles routed to a
    /// branch. One radius covers every branch for now; a
    /// branch that needs its own radius declares it at
    /// wiring time.
    pub halo_radius_px: u32,
}

impl Default for RoutingProfile {
    fn default() -> Self {
        Self {
            text_branch: Some("text"),
            sprite_branch: Some("sprite"),
            flat_branch: Some("flat"),
            patterned_branch: Some("patterned"),
            gradient_branch: Some("gradient"),
            halo_radius_px: DEFAULT_HALO_RADIUS_PX,
        }
    }
}

impl RoutingProfile {
    /// The branch assigned to a class, or `None` when the
    /// class uses the general path.
    #[must_use]
    pub const fn branch_name(&self, class: ContentClass) -> Option<&'static str> {
        match class {
            ContentClass::Text => self.text_branch,
            ContentClass::Sprite => self.sprite_branch,
            ContentClass::Flat => self.flat_branch,
            ContentClass::Patterned => self.patterned_branch,
            ContentClass::Gradient => self.gradient_branch,
            ContentClass::Unknown => None,
        }
    }

    /// The route for a class: its branch, or
    /// [`Route::General`].
    #[must_use]
    pub const fn route_for(&self, class: ContentClass) -> Route {
        match self.branch_name(class) {
            Some(name) => Route::Branch(name),
            None => Route::General,
        }
    }

    /// Assign a branch to a class, or `None` for the general
    /// path. Assigning a branch to
    /// [`ContentClass::Unknown`] does nothing: it always
    /// uses the general path.
    pub fn set_branch(&mut self, class: ContentClass, branch: Option<&'static str>) {
        match class {
            ContentClass::Text => self.text_branch = branch,
            ContentClass::Sprite => self.sprite_branch = branch,
            ContentClass::Flat => self.flat_branch = branch,
            ContentClass::Patterned => self.patterned_branch = branch,
            ContentClass::Gradient => self.gradient_branch = branch,
            ContentClass::Unknown => {}
        }
    }

    /// The halo radius in source pixels around tiles routed
    /// to a branch.
    #[must_use]
    pub const fn halo_radius_px(&self) -> u32 {
        self.halo_radius_px
    }

    /// The algorithms a routed run needs, given the
    /// general-path algorithm id: one entry per distinct
    /// registry id [`resolve_algorithm`] returns over all
    /// classes, in first-use class order, each with the
    /// branch names it serves and whether it serves the
    /// general path. The run path builds one graph per
    /// entry and runs only the entries the frame's tiles
    /// actually need.
    #[must_use]
    pub fn routed_algorithms<'a>(&self, general_algorithm: &'a str) -> Vec<RoutedAlgorithm<'a>> {
        let mut algorithms: Vec<RoutedAlgorithm<'a>> = Vec::new();
        for class in ALL_CLASSES {
            let id = resolve_algorithm(class, general_algorithm);
            let index = if let Some(index) = algorithms.iter().position(|entry| entry.id == id) {
                index
            } else {
                algorithms.push(RoutedAlgorithm {
                    id,
                    branches: Vec::new(),
                    general: id == general_algorithm,
                });
                algorithms.len() - 1
            };
            let slot = &mut algorithms[index];
            if let Some(branch) = self.branch_name(class)
                && !slot.branches.contains(&branch)
            {
                slot.branches.push(branch);
            }
        }
        algorithms
    }
}

/// Every content class, in the order the routing table
/// lists the classified classes, with
/// [`ContentClass::Unknown`] last.
pub const ALL_CLASSES: [ContentClass; 6] = [
    ContentClass::Text,
    ContentClass::Sprite,
    ContentClass::Flat,
    ContentClass::Patterned,
    ContentClass::Gradient,
    ContentClass::Unknown,
];

/// The routing table: which registry algorithm processes
/// each content class. One table, read by
/// [`resolve_algorithm`]; no per-class match exists
/// anywhere else in the run path. The ids are registry ids:
/// `"contour"`, `"nearest"`, `"bilinear"`.
///
/// - Text: Contour. Thin strokes and sharp corners are what
///   Contour reconstructs: its edge follower traces the
///   stroke and rebuilds it at scale. Nearest would step
///   the stroke into blocks; bilinear would blur it.
/// - Sprite: Contour. A bounded shape with a silhouette
///   needs the same edge reconstruction as text: the
///   silhouette is the shape.
/// - Flat: Nearest. A flat tile is one color, or two colors
///   within a few levels, so nearest copies the color
///   exactly. Contour would move it: its hard-light stage
///   shifts flat mid-tones by up to just under 16 levels,
///   which is a defect on a background that must stay
///   solid. Bilinear would blend the tile's edge pixels
///   with its neighbor. Nearest is exact and cheapest here.
/// - Patterned: Nearest. A repeating structure is preserved
///   exactly when each source pixel is replicated: the
///   period and the colors survive the scale unchanged. Any
///   reconstruction risks changing the period or the phase,
///   and bilinear blurs the pattern's edges.
/// - Gradient: Bilinear. A smooth directional ramp is what
///   bilinear interpolation reproduces: linear between
///   source pixels, which matches a linear gradient up to
///   rounding, with no steps and no banding. Nearest would
///   step the ramp; Contour would shift it with hard light.
/// - Unknown: no table entry. It always takes the general
///   path, the algorithm the run was started with: section 9
///   of the specification gives unclassified content the
///   general-purpose algorithm, not a guess.
pub const ROUTING_TABLE: [(ContentClass, &str); 5] = [
    (ContentClass::Text, "contour"),
    (ContentClass::Sprite, "contour"),
    (ContentClass::Flat, "nearest"),
    (ContentClass::Patterned, "nearest"),
    (ContentClass::Gradient, "bilinear"),
];

/// Resolve a tile's class to the registry id of the
/// algorithm that processes it, given the general-path
/// algorithm id of the run. The table answers for the five
/// classified classes; [`ContentClass::Unknown`] takes
/// `general_algorithm`, the algorithm the run was started
/// with. A table entry naming `"nearest"` resolves to
/// `"nearest-gpu"` when the general path is
/// `"nearest-gpu"`: the GPU node is the same algorithm on
/// the GPU backend, and a run started on the GPU keeps
/// every nearest tile on it instead of splitting nearest
/// between backends.
#[must_use]
pub fn resolve_algorithm(class: ContentClass, general_algorithm: &str) -> &str {
    if class == ContentClass::Unknown {
        return general_algorithm;
    }
    let algorithm = ROUTING_TABLE
        .iter()
        .find(|(candidate, _)| *candidate == class)
        .map_or(general_algorithm, |(_, algorithm)| *algorithm);
    if algorithm == "nearest" && general_algorithm == "nearest-gpu" {
        "nearest-gpu"
    } else {
        algorithm
    }
}

/// One algorithm's share of a routed run: its registry id,
/// the branch names it serves, and whether it serves the
/// general path. Built by
/// [`RoutingProfile::routed_algorithms`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutedAlgorithm<'a> {
    /// Registry id of the algorithm.
    pub id: &'a str,
    /// Branch names routed to this algorithm, in class order.
    pub branches: Vec<&'static str>,
    /// True when this algorithm serves the general path.
    pub general: bool,
}

/// The pixels of one tile, row-major, as RGB triples. The
/// frame is RGB8 here, so each pixel is three bytes; the
/// frame's row stride is honored through [`Frame::row`].
fn tile_pixels(frame: &Frame, x0: u32, y0: u32, x1: u32, y1: u32) -> Vec<[u8; 3]> {
    let width = (x1 - x0) as usize;
    let height = (y1 - y0) as usize;
    let mut pixels = Vec::with_capacity(width * height);
    for y in y0..y1 {
        let row = frame.row(y);
        for x in x0..x1 {
            let start = x as usize * 3;
            pixels.push([row[start], row[start + 1], row[start + 2]]);
        }
    }
    pixels
}

/// Palette size, dominant color, and per-channel variance
/// and range of a tile's pixels. `pixels` is non-empty.
struct PaletteStats {
    /// Distinct colors in the tile.
    size: usize,
    /// The most frequent color.
    dominant: [u8; 3],
    /// How many pixels hold the dominant color.
    dominant_count: usize,
    /// Per-channel color variance, in squared levels.
    channel_variance: [f64; 3],
    /// Per-channel color range, in levels.
    channel_range: [u8; 3],
}

/// Palette size, dominant color, and per-channel variance
/// and range of a tile's pixels. `pixels` is non-empty.
fn palette_stats(pixels: &[[u8; 3]]) -> PaletteStats {
    let mut sorted = pixels.to_vec();
    sorted.sort_unstable();
    let mut size = 0_usize;
    let mut dominant = [0_u8; 3];
    let mut dominant_count = 0_usize;
    let mut index = 0_usize;
    while index < sorted.len() {
        let color = sorted[index];
        let mut count = 0_usize;
        while index < sorted.len() && sorted[index] == color {
            count += 1;
            index += 1;
        }
        size += 1;
        if count > dominant_count {
            dominant = color;
            dominant_count = count;
        }
    }
    let count = u32::try_from(pixels.len()).unwrap_or(u32::MAX);
    let mut channel_variance = [0.0_f64; 3];
    let mut channel_range = [0_u8; 3];
    for (channel, variance_slot) in channel_variance.iter_mut().enumerate() {
        let mut sum = 0_u32;
        let mut sum_squares = 0_u32;
        let mut min = u8::MAX;
        let mut max = u8::MIN;
        for pixel in pixels {
            let value = pixel[channel];
            sum += u32::from(value);
            sum_squares += u32::from(value) * u32::from(value);
            min = min.min(value);
            max = max.max(value);
        }
        let n = f64::from(count);
        let mean = f64::from(sum) / n;
        *variance_slot = (f64::from(sum_squares) / n - mean * mean).max(0.0);
        channel_range[channel] = max - min;
    }
    PaletteStats {
        size,
        dominant,
        dominant_count,
        channel_variance,
        channel_range,
    }
}

/// Largest per-channel absolute difference between two
/// pixels, in levels.
fn channel_diff(a: [u8; 3], b: [u8; 3]) -> u32 {
    let mut diff = 0_u8;
    for (ca, cb) in a.iter().zip(b.iter()) {
        diff = diff.max(ca.abs_diff(*cb));
    }
    u32::from(diff)
}

/// Edge flags and texture of a tile's pixels.
struct EdgeStats {
    /// One flag per pixel: true when the pixel is an edge
    /// pixel under the fixed operator.
    edge: Vec<bool>,
    /// Fraction of pixels that are edge pixels.
    edge_density: f64,
    /// Mean absolute adjacent-pixel difference, normalized
    /// to [0, 1].
    texture_complexity: f64,
    /// True when any pixel differs from its left neighbor.
    /// A row period needs variation along x.
    columns_vary: bool,
    /// True when any pixel differs from its upper neighbor.
    /// A column period needs variation along y.
    rows_vary: bool,
}

/// Edge flags and texture of a tile's pixels, `width` by
/// `height`, row-major. A pixel is an edge pixel when the
/// largest per-channel absolute difference to its right or
/// lower neighbor exceeds [`EDGE_THRESHOLD`].
fn edge_stats(pixels: &[[u8; 3]], width: usize, height: usize) -> EdgeStats {
    let total = pixels.len();
    let mut edge = vec![false; total];
    let mut texture_sum = 0.0_f64;
    let mut pair_count = 0_u32;
    let mut columns_vary = false;
    let mut rows_vary = false;
    for ly in 0..height {
        for lx in 0..width {
            let index = ly * width + lx;
            if lx + 1 < width {
                let diff = channel_diff(pixels[index], pixels[index + 1]);
                pair_count += 1;
                texture_sum += f64::from(diff) / 255.0;
                if diff > 0 {
                    columns_vary = true;
                }
                if diff > u32::from(EDGE_THRESHOLD) {
                    edge[index] = true;
                }
            }
            if ly + 1 < height {
                let diff = channel_diff(pixels[index], pixels[index + width]);
                pair_count += 1;
                texture_sum += f64::from(diff) / 255.0;
                if diff > 0 {
                    rows_vary = true;
                }
                if diff > u32::from(EDGE_THRESHOLD) {
                    edge[index] = true;
                }
            }
        }
    }
    let mut edge_count = 0_u32;
    for flag in &edge {
        if *flag {
            edge_count += 1;
        }
    }
    let total_u = u32::try_from(total).unwrap_or(u32::MAX);
    EdgeStats {
        edge_density: f64::from(edge_count) / f64::from(total_u),
        texture_complexity: if pair_count > 0 {
            texture_sum / f64::from(pair_count)
        } else {
            0.0
        },
        edge,
        columns_vary,
        rows_vary,
    }
}

/// Connected-component counts of a tile's pixels.
struct ComponentStats {
    /// Count of same-color connected regions.
    count: usize,
    /// Fraction of the tile the largest component covers.
    largest_fraction: f64,
    /// Mean component size, in pixels.
    mean_size: f64,
    /// Pixel indices of the largest component.
    largest_pixels: Vec<usize>,
}

/// Push a neighbor onto the flood-fill stack when it is
/// unvisited and holds the component's color.
fn push_neighbor(
    stack: &mut Vec<usize>,
    visited: &mut [bool],
    pixels: &[[u8; 3]],
    color: [u8; 3],
    index: usize,
) {
    if !visited[index] && pixels[index] == color {
        visited[index] = true;
        stack.push(index);
    }
}

/// Same-color connected regions of a tile's pixels,
/// 4-connected, exact color equality. `pixels` is non-empty.
fn components(pixels: &[[u8; 3]], width: usize, height: usize) -> ComponentStats {
    let total = pixels.len();
    let mut visited = vec![false; total];
    let mut count = 0_usize;
    let mut largest: Vec<usize> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    for start in 0..total {
        if visited[start] {
            continue;
        }
        count += 1;
        let color = pixels[start];
        let mut component: Vec<usize> = Vec::new();
        stack.push(start);
        visited[start] = true;
        while let Some(index) = stack.pop() {
            component.push(index);
            let lx = index % width;
            let ly = index / width;
            if lx > 0 {
                push_neighbor(&mut stack, &mut visited, pixels, color, index - 1);
            }
            if lx + 1 < width {
                push_neighbor(&mut stack, &mut visited, pixels, color, index + 1);
            }
            if ly > 0 {
                push_neighbor(&mut stack, &mut visited, pixels, color, index - width);
            }
            if ly + 1 < height {
                push_neighbor(&mut stack, &mut visited, pixels, color, index + width);
            }
        }
        if component.len() > largest.len() {
            largest = component;
        }
    }
    let total_u = u32::try_from(total).unwrap_or(u32::MAX);
    let count_u = u32::try_from(count).unwrap_or(u32::MAX);
    let largest_u = u32::try_from(largest.len()).unwrap_or(u32::MAX);
    ComponentStats {
        count,
        largest_fraction: f64::from(largest_u) / f64::from(total_u),
        mean_size: f64::from(total_u) / f64::from(count_u),
        largest_pixels: largest,
    }
}

/// Length and straightness of the dominant contour: the
/// boundary of the largest component. Length is the count of
/// component pixels with a neighbor outside the component;
/// straightness is the bounding-box perimeter over that
/// length, clamped to [0, 1].
fn contour_geometry(largest: &[usize], width: usize, height: usize) -> (usize, f64) {
    if largest.is_empty() {
        return (0, 1.0);
    }
    let mut inside = vec![false; width * height];
    for index in largest {
        inside[*index] = true;
    }
    let mut perimeter = 0_usize;
    let mut min_x = width;
    let mut max_x = 0_usize;
    let mut min_y = height;
    let mut max_y = 0_usize;
    for index in largest {
        let lx = *index % width;
        let ly = *index / width;
        min_x = min_x.min(lx);
        max_x = max_x.max(lx);
        min_y = min_y.min(ly);
        max_y = max_y.max(ly);
        let mut boundary = lx == 0 || ly == 0 || lx + 1 >= width || ly + 1 >= height;
        if !boundary {
            boundary = !inside[index - 1]
                || !inside[index + 1]
                || !inside[index - width]
                || !inside[index + width];
        }
        if boundary {
            perimeter += 1;
        }
    }
    let bbox_perimeter = 2 * ((max_x - min_x + 1) + (max_y - min_y + 1));
    let straightness = if perimeter == 0 {
        1.0
    } else {
        (f64::from(u32::try_from(bbox_perimeter).unwrap_or(u32::MAX))
            / f64::from(u32::try_from(perimeter).unwrap_or(u32::MAX)))
        .clamp(0.0, 1.0)
    };
    (perimeter, straightness)
}

/// The smallest accepted period of a tile's pixels, and its
/// match fraction. `horizontal` shifts along x for a row
/// period; otherwise it shifts along y for a column period.
/// A period counts when at least [`PERIOD_MATCH_THRESHOLD`]
/// of the shifted pairs match. No period is accepted, and
/// the score is 0.0, when no candidate reaches the threshold.
///
/// Only informative pairs count: a pair counts when at
/// least one of its two pixels is not the dominant color.
/// A background-background pair matches at every shift, so
/// it carries no evidence that anything repeats, and
/// counting it lets the background outvote the pattern:
/// a tile of long vertical strokes on a flat background
/// "repeats" vertically at period 2 with 209 of 224 pairs
/// matching, because the background matches itself and the
/// stroke interiors match themselves, while only the
/// stroke caps disagree. Judged on the informative pairs
/// alone, that same shift scores 27 of 42: the strokes do
/// not repeat, their interiors merely run on.
fn find_period(
    pixels: &[[u8; 3]],
    width: usize,
    height: usize,
    horizontal: bool,
    dominant: [u8; 3],
) -> (Option<u32>, f64) {
    let limit = if horizontal { width } else { height };
    for period in MIN_PERIOD..=MAX_PERIOD {
        let step = period as usize;
        if step >= limit {
            continue;
        }
        let mut matches = 0_u32;
        let mut pairs = 0_u32;
        for ly in 0..height {
            for lx in 0..width {
                let (nx, ny) = if horizontal {
                    (lx + step, ly)
                } else {
                    (lx, ly + step)
                };
                if nx >= width || ny >= height {
                    continue;
                }
                let first = pixels[ly * width + lx];
                let second = pixels[ny * width + nx];
                if first == dominant && second == dominant {
                    continue;
                }
                pairs += 1;
                if first == second {
                    matches += 1;
                }
            }
        }
        if pairs > 0 {
            let score = f64::from(matches) / f64::from(pairs);
            if score >= PERIOD_MATCH_THRESHOLD {
                return (Some(period), score);
            }
        }
    }
    (None, 0.0)
}

/// Least-squares plane fit of luminance on (x, y): fit
/// quality in [0, 1], unit direction, and slope magnitude in
/// levels per pixel. Luminance is the standard Rec. 601
/// weighting. `pixels` is non-empty.
fn gradient_fit(pixels: &[[u8; 3]], width: usize) -> (f64, Option<(f64, f64)>, f64) {
    let mut n = 0.0_f64;
    let mut sum_x = 0.0_f64;
    let mut sum_y = 0.0_f64;
    let mut sum_l = 0.0_f64;
    let mut sum_xx = 0.0_f64;
    let mut sum_yy = 0.0_f64;
    let mut cross_moment_x = 0.0_f64;
    let mut cross_moment_y = 0.0_f64;
    let mut sum_ll = 0.0_f64;
    for (index, pixel) in pixels.iter().enumerate() {
        let x = f64::from(u32::try_from(index % width).unwrap_or(u32::MAX));
        let y = f64::from(u32::try_from(index / width).unwrap_or(u32::MAX));
        let luminance =
            0.299 * f64::from(pixel[0]) + 0.587 * f64::from(pixel[1]) + 0.114 * f64::from(pixel[2]);
        n += 1.0;
        sum_x += x;
        sum_y += y;
        sum_l += luminance;
        sum_xx += x * x;
        sum_yy += y * y;
        cross_moment_x += x * luminance;
        cross_moment_y += y * luminance;
        sum_ll += luminance * luminance;
    }
    let mean_x = sum_x / n;
    let mean_y = sum_y / n;
    let mean_l = sum_l / n;
    let var_x = (sum_xx / n - mean_x * mean_x).max(0.0);
    let var_y = (sum_yy / n - mean_y * mean_y).max(0.0);
    let var_l = (sum_ll / n - mean_l * mean_l).max(0.0);
    let slope_x = if var_x > 0.0 {
        (cross_moment_x / n - mean_x * mean_l) / var_x
    } else {
        0.0
    };
    let slope_y = if var_y > 0.0 {
        (cross_moment_y / n - mean_y * mean_l) / var_y
    } else {
        0.0
    };
    let fit = if var_l > 0.0 {
        ((slope_x * slope_x * var_x + slope_y * slope_y * var_y) / var_l).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let magnitude = (slope_x * slope_x + slope_y * slope_y).sqrt();
    let direction = if magnitude > 1e-9 {
        Some((slope_x / magnitude, slope_y / magnitude))
    } else {
        None
    };
    (fit, direction, magnitude)
}

/// All features of one tile of an RGB8 frame.
fn extract(frame: &Frame, grid: TileGrid, tx: u32, ty: u32) -> TileFeatures {
    let (x0, y0, x1, y1) = grid.tile_rect(tx, ty);
    let width = (x1 - x0) as usize;
    let height = (y1 - y0) as usize;
    let pixels = tile_pixels(frame, x0, y0, x1, y1);
    if pixels.is_empty() {
        return TileFeatures::default();
    }
    let palette = palette_stats(&pixels);
    let edges = edge_stats(&pixels, width, height);
    let components = components(&pixels, width, height);
    let (contour_length, contour_straightness) =
        contour_geometry(&components.largest_pixels, width, height);
    let (row_period, row_period_score) = if edges.columns_vary {
        find_period(&pixels, width, height, true, palette.dominant)
    } else {
        (None, 0.0)
    };
    let (col_period, col_period_score) = if edges.rows_vary {
        find_period(&pixels, width, height, false, palette.dominant)
    } else {
        (None, 0.0)
    };
    let (gradient_fit_value, gradient_direction, gradient_magnitude) = gradient_fit(&pixels, width);
    let foreground_count = pixels.len() - palette.dominant_count;
    let mut foreground_edge_count = 0_usize;
    for (index, pixel) in pixels.iter().enumerate() {
        if *pixel != palette.dominant && edges.edge[index] {
            foreground_edge_count += 1;
        }
    }
    let foreground_edge_ratio = if foreground_count == 0 {
        0.0
    } else {
        f64::from(u32::try_from(foreground_edge_count).unwrap_or(u32::MAX))
            / f64::from(u32::try_from(foreground_count).unwrap_or(u32::MAX))
    };
    TileFeatures {
        palette_size: palette.size,
        channel_variance: palette.channel_variance,
        channel_range: palette.channel_range,
        edge_density: edges.edge_density,
        component_count: components.count,
        largest_component_fraction: components.largest_fraction,
        mean_component_size: components.mean_size,
        dominant_contour_length: contour_length,
        dominant_contour_straightness: contour_straightness,
        row_period,
        col_period,
        row_period_score,
        col_period_score,
        texture_complexity: edges.texture_complexity,
        gradient_fit: gradient_fit_value,
        gradient_direction,
        gradient_magnitude,
        foreground_edge_ratio,
    }
}
