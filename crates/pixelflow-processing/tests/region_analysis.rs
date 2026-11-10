//! Synthetic-tile tests for region analysis. Every input
//! image is generated in code: no PNG files, no fixture
//! directory, nothing copyrighted. These tests run on a clean
//! checkout with no fixtures present.
//!
//! The classification cases follow section 13 of the
//! region-aware specification: one synthetic tile per class
//! (flat color, linear gradient, repeating pattern, sprite
//! silhouette, thin text strokes), a mixed tile that must
//! classify as `Unknown`, plus the override, stability, and
//! routing behavior. Expected classes come from the
//! documented decision procedure in `region_analysis.rs`,
//! with each tile's features reasoned through in a comment;
//! nothing here is a value copied from a run of the code
//! under test.

use pixelflow_core::{Frame, PixelFormat};
use pixelflow_processing::region_analysis::{
    ContentClass, RegionAnalyzer, RegionMap, Route, RoutingProfile, TileClassification,
    TileFeatures, TileOverride,
};

/// Build an RGB8 frame from a per-pixel function. `pixel`
/// takes the column and row as `usize`. The function may
/// hold mutable state, as the noise frame does.
fn rgb_frame(
    width: usize,
    height: usize,
    mut pixel: impl FnMut(usize, usize) -> [u8; 3],
) -> Result<Frame, String> {
    let mut data = Vec::with_capacity(width * height * 3);
    for row in 0..height {
        for col in 0..width {
            data.extend_from_slice(&pixel(col, row));
        }
    }
    let width_u = u32::try_from(width).map_err(|err| err.to_string())?;
    let height_u = u32::try_from(height).map_err(|err| err.to_string())?;
    Frame::new(width_u, height_u, PixelFormat::Rgb8, data).map_err(|err| err.to_string())
}

/// Build an RGB8 frame filled with one color.
fn flat_frame(width: usize, height: usize, color: [u8; 3]) -> Result<Frame, String> {
    rgb_frame(width, height, |_, _| color)
}

/// Analyze a frame with no overrides and no previous map.
fn analyze_frame(frame: &Frame) -> RegionMap {
    RegionAnalyzer::default().analyze(frame, &[], None)
}

/// The class of one tile, or an error when the tile is
/// outside the map.
fn tile_class(map: &RegionMap, tx: u32, ty: u32) -> Result<ContentClass, String> {
    map.tile(tx, ty)
        .map(|tile| tile.class)
        .ok_or_else(|| format!("tile ({tx}, {ty}) is outside the map"))
}

/// A frame whose only content is thin 1-pixel strokes: four
/// vertical bars at irregular columns and heights, dark ink
/// on a light background. The bars are thin, so nearly every
/// ink pixel is an edge pixel; there are 5 components; and
/// no period is accepted: judged on informative pairs, the
/// closest is the column period 2 at 27/42, then the row
/// period 4 at 18/32, both below the 0.9 acceptance
/// threshold. The bars sit 4 apart, but their heights
/// differ, so the ink itself does not repeat; a vertical
/// shift matches only the stroke interiors, while the caps
/// disagree. Counting every shifted pair instead would
/// report periods 4 and 2 at 178/192 and 209/224, because
/// background-background pairs match at any shift: that
/// reading is the mismeasurement `find_period` avoids by
/// counting informative pairs only. Text is the only class
/// that scores primary.
fn text_frame() -> Result<Frame, String> {
    rgb_frame(16, 16, |x, y| {
        let ink = (x == 1 && (2..11).contains(&y))
            || (x == 5 && (2..8).contains(&y))
            || (x == 9 && (5..14).contains(&y))
            || (x == 13 && (4..15).contains(&y));
        if ink { [20, 20, 20] } else { [240, 240, 240] }
    })
}

/// A frame holding one sprite: an 8x10 solid body with a
/// 2x2 detail inside it, on a flat background. Three
/// components, a dominant background, a solid body whose
/// interior pixels are not edges. Sprite is the only class
/// that scores primary.
fn sprite_frame() -> Result<Frame, String> {
    rgb_frame(16, 16, |x, y| {
        let in_body = (4..12).contains(&x) && (3..13).contains(&y);
        let in_detail = (7..9).contains(&x) && (6..8).contains(&y);
        if in_detail {
            [240, 220, 60]
        } else if in_body {
            [200, 40, 40]
        } else {
            [30, 30, 120]
        }
    })
}

/// A frame holding the same sprite at horizontal offset
/// `x0`, on a 32x32 flat background. The sprite stays
/// inside tile (0, 0) for offsets 4 through 7.
fn moving_sprite_frame(x0: usize) -> Result<Frame, String> {
    rgb_frame(32, 32, |x, y| {
        let in_body = x >= x0 && x < x0 + 8 && (3..13).contains(&y);
        let in_detail = x >= x0 + 3 && x < x0 + 5 && (6..8).contains(&y);
        if in_detail {
            [240, 220, 60]
        } else if in_body {
            [200, 40, 40]
        } else {
            [30, 30, 120]
        }
    })
}

/// Deterministic unstructured noise: a linear congruential
/// generator filling every channel. Palette near 256, edge
/// density near 1, hundreds of components, no period, no
/// gradient fit. No class scores primary, so the tile is
/// `Unknown`.
fn noise_frame() -> Result<Frame, String> {
    let mut state: u32 = 0x1234_5678;
    let mut next_byte = move || {
        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        u8::try_from(state >> 24).unwrap_or(0)
    };
    rgb_frame(16, 16, |_, _| [next_byte(), next_byte(), next_byte()])
}

#[test]
fn flat_tile_is_flat() -> Result<(), String> {
    let frame = flat_frame(16, 16, [200, 30, 90])?;
    let map = analyze_frame(&frame);
    assert_eq!(tile_class(&map, 0, 0)?, ContentClass::Flat);
    let tile = map.tile(0, 0).ok_or_else(|| "tile missing".to_string())?;
    assert!(tile.confidence >= 0.6);
    assert!(!tile.overridden);
    assert_eq!(map.feature_version(), 2);
    Ok(())
}

#[test]
fn horizontal_gradient_is_gradient() -> Result<(), String> {
    // 17 levels per pixel: below the edge threshold, so no
    // edges; luminance is exactly linear in x, so the plane
    // fit is 1.0 and the range is 255. All rows are identical,
    // but a tile constant along an axis has no period on
    // that axis, so no pattern score fires.
    let frame = rgb_frame(16, 16, |x, _| {
        let value = u8::try_from(x * 17).unwrap_or(255);
        [value, value, value]
    })?;
    let map = analyze_frame(&frame);
    assert_eq!(tile_class(&map, 0, 0)?, ContentClass::Gradient);
    Ok(())
}

#[test]
fn vertical_gradient_is_gradient() -> Result<(), String> {
    let frame = rgb_frame(16, 16, |_, y| {
        let value = u8::try_from(y * 17).unwrap_or(255);
        [value, value, value]
    })?;
    let map = analyze_frame(&frame);
    assert_eq!(tile_class(&map, 0, 0)?, ContentClass::Gradient);
    Ok(())
}

#[test]
fn checkerboard_is_patterned() -> Result<(), String> {
    // 2-pixel cells repeat every 4 pixels in both axes, so
    // both periods score 1.0. The cells are thin enough
    // that text scores 0.85, but the pattern scores 0.9.
    let frame = rgb_frame(16, 16, |x, y| {
        if (x / 2 + y / 2) % 2 == 0 {
            [20, 20, 20]
        } else {
            [235, 235, 235]
        }
    })?;
    let map = analyze_frame(&frame);
    assert_eq!(tile_class(&map, 0, 0)?, ContentClass::Patterned);
    let tile = map.tile(0, 0).ok_or_else(|| "tile missing".to_string())?;
    assert_eq!(tile.features.row_period, Some(4));
    assert_eq!(tile.features.col_period, Some(4));
    Ok(())
}

#[test]
fn sprite_silhouette_is_sprite() -> Result<(), String> {
    let frame = sprite_frame()?;
    let map = analyze_frame(&frame);
    assert_eq!(tile_class(&map, 0, 0)?, ContentClass::Sprite);
    Ok(())
}

#[test]
fn thin_strokes_are_text() -> Result<(), String> {
    let frame = text_frame()?;
    let map = analyze_frame(&frame);
    assert_eq!(tile_class(&map, 0, 0)?, ContentClass::Text);
    Ok(())
}

#[test]
fn noise_is_unknown() -> Result<(), String> {
    let frame = noise_frame()?;
    let map = analyze_frame(&frame);
    assert_eq!(tile_class(&map, 0, 0)?, ContentClass::Unknown);
    Ok(())
}

#[test]
fn override_wins_over_detector() -> Result<(), String> {
    let frame = flat_frame(16, 16, [200, 30, 90])?;
    let analyzer = RegionAnalyzer::default();
    let overrides = [TileOverride::new(0, 0, 1, 1, ContentClass::Text)];
    let map = analyzer.analyze(&frame, &overrides, None);
    assert_eq!(tile_class(&map, 0, 0)?, ContentClass::Text);
    let tile = map.tile(0, 0).ok_or_else(|| "tile missing".to_string())?;
    assert!(tile.overridden);
    assert!((tile.confidence - 1.0).abs() < 1e-9);
    assert_eq!(map.overridden_count(), 1);
    Ok(())
}

#[test]
fn override_can_force_unknown() -> Result<(), String> {
    let frame = sprite_frame()?;
    let analyzer = RegionAnalyzer::default();
    let overrides = [TileOverride::new(0, 0, 1, 1, ContentClass::Unknown)];
    let map = analyzer.analyze(&frame, &overrides, None);
    assert_eq!(tile_class(&map, 0, 0)?, ContentClass::Unknown);
    assert_eq!(map.overridden_count(), 1);
    Ok(())
}

#[test]
fn override_rect_covers_tiles_and_later_wins() -> Result<(), String> {
    let frame = flat_frame(32, 32, [200, 30, 90])?;
    let analyzer = RegionAnalyzer::default();
    let overrides = [
        TileOverride::new(0, 0, 2, 2, ContentClass::Gradient),
        TileOverride::new(1, 1, 2, 2, ContentClass::Sprite),
    ];
    let map = analyzer.analyze(&frame, &overrides, None);
    assert_eq!(tile_class(&map, 0, 0)?, ContentClass::Gradient);
    assert_eq!(tile_class(&map, 1, 0)?, ContentClass::Gradient);
    assert_eq!(tile_class(&map, 0, 1)?, ContentClass::Gradient);
    assert_eq!(tile_class(&map, 1, 1)?, ContentClass::Sprite);
    assert_eq!(map.overridden_count(), 4);
    Ok(())
}

#[test]
fn override_outside_grid_changes_nothing() -> Result<(), String> {
    let frame = flat_frame(32, 32, [200, 30, 90])?;
    let analyzer = RegionAnalyzer::default();
    let overrides = [TileOverride::new(5, 5, 9, 9, ContentClass::Text)];
    let map = analyzer.analyze(&frame, &overrides, None);
    assert_eq!(map.overridden_count(), 0);
    assert_eq!(tile_class(&map, 0, 0)?, ContentClass::Flat);
    Ok(())
}

#[test]
fn sprite_moving_one_pixel_per_frame_keeps_class() -> Result<(), String> {
    // The specification's stability case: a sprite moving
    // one pixel per frame across a static background must
    // not flicker, and the per-frame reclassified tile count
    // stays small. The sprite's features barely move within
    // its tile, so gating holds the class.
    let analyzer = RegionAnalyzer::default();
    let mut previous: Option<RegionMap> = None;
    for x0 in 4..8 {
        let frame = moving_sprite_frame(x0)?;
        let map = analyzer.analyze(&frame, &[], previous.as_ref());
        assert_eq!(tile_class(&map, 0, 0)?, ContentClass::Sprite);
        assert_eq!(tile_class(&map, 1, 0)?, ContentClass::Flat);
        if let Some(prev) = &previous {
            assert_eq!(map.class_changes_since(prev), 0);
        }
        previous = Some(map);
    }
    Ok(())
}

#[test]
fn wholesale_change_reclassifies() -> Result<(), String> {
    let analyzer = RegionAnalyzer::default();
    let flat = flat_frame(16, 16, [200, 30, 90])?;
    let first = analyzer.analyze(&flat, &[], None);
    assert_eq!(tile_class(&first, 0, 0)?, ContentClass::Flat);
    let gradient = rgb_frame(16, 16, |x, _| {
        let value = u8::try_from(x * 17).unwrap_or(255);
        [value, value, value]
    })?;
    let second = analyzer.analyze(&gradient, &[], Some(&first));
    assert_eq!(tile_class(&second, 0, 0)?, ContentClass::Gradient);
    assert_eq!(second.class_changes_since(&first), 1);
    assert_eq!(second.changed_tiles_since(&first), vec![(0, 0)]);
    Ok(())
}

/// Features of a checkerboard tile: pattern scores 0.9,
/// text scores 0.85. Built by hand so the hysteresis test
/// controls both scores exactly.
fn checkerboard_features() -> TileFeatures {
    TileFeatures {
        palette_size: 2,
        channel_variance: [11556.0, 11556.0, 11556.0],
        channel_range: [215, 215, 215],
        edge_density: 0.75,
        component_count: 64,
        largest_component_fraction: 4.0 / 256.0,
        mean_component_size: 4.0,
        dominant_contour_length: 8,
        dominant_contour_straightness: 1.0,
        row_period: Some(4),
        col_period: Some(4),
        row_period_score: 1.0,
        col_period_score: 1.0,
        texture_complexity: 0.8,
        gradient_fit: 0.0,
        gradient_direction: None,
        gradient_magnitude: 0.0,
        foreground_edge_ratio: 1.0,
    }
}

/// Features of a thin-stroke text tile: text scores 0.85,
/// nothing else scores primary.
fn text_features() -> TileFeatures {
    TileFeatures {
        palette_size: 2,
        channel_variance: [11000.0, 11000.0, 11000.0],
        channel_range: [220, 220, 220],
        edge_density: 0.3,
        component_count: 5,
        largest_component_fraction: 221.0 / 256.0,
        mean_component_size: 256.0 / 5.0,
        dominant_contour_length: 60,
        dominant_contour_straightness: 0.8,
        row_period: None,
        col_period: None,
        row_period_score: 0.0,
        col_period_score: 0.0,
        texture_complexity: 0.3,
        gradient_fit: 0.0,
        gradient_direction: None,
        gradient_magnitude: 0.0,
        foreground_edge_ratio: 1.0,
    }
}

#[test]
fn hysteresis_holds_previous_class_on_close_scores() {
    // Patterned 0.9 beats Text 0.85 by only 0.05, less than
    // the 0.10 hysteresis margin, so a tile previously
    // classified Text stays Text. The previous features are
    // far from the current ones, so gating does not hold it:
    // this test isolates hysteresis.
    let analyzer = RegionAnalyzer::default();
    let features = checkerboard_features();
    let previous = TileClassification {
        class: ContentClass::Text,
        confidence: 0.85,
        features: text_features(),
        overridden: false,
    };
    let result = analyzer.classify(&features, Some(&previous));
    assert_eq!(result.class, ContentClass::Text);
    assert!((result.confidence - 0.85).abs() < 1e-9);
    assert!(!result.overridden);
    // With no previous class, the same features classify as
    // the winning class.
    let fresh = analyzer.classify(&features, None);
    assert_eq!(fresh.class, ContentClass::Patterned);
}

#[test]
fn hysteresis_does_not_hold_evaporated_class() {
    // The previous class scores 0.0 on the current features,
    // below the confidence threshold, so it cannot hold the
    // tile: the fresh winner takes it.
    let analyzer = RegionAnalyzer::default();
    let features = checkerboard_features();
    let previous = TileClassification {
        class: ContentClass::Flat,
        confidence: 1.0,
        features: TileFeatures {
            palette_size: 1,
            ..TileFeatures::default()
        },
        overridden: false,
    };
    let result = analyzer.classify(&features, Some(&previous));
    assert_eq!(result.class, ContentClass::Patterned);
}

#[test]
fn gating_holds_class_when_features_barely_move() {
    // The current features would classify as Patterned
    // (periods accepted, text second), but they are
    // identical to the previous features except for the
    // period fields, which the feature distance does not
    // count. Distance 0.0 is below the gate, so the
    // previous Text classification stands, with its stored
    // features: the stored row period is still `None`.
    let analyzer = RegionAnalyzer::default();
    let mut current = text_features();
    current.row_period = Some(4);
    current.col_period = Some(4);
    current.row_period_score = 1.0;
    current.col_period_score = 1.0;
    let previous = TileClassification {
        class: ContentClass::Text,
        confidence: 0.85,
        features: text_features(),
        overridden: false,
    };
    let result = analyzer.classify(&current, Some(&previous));
    assert_eq!(result.class, ContentClass::Text);
    assert_eq!(result.features.row_period, None);
}

#[test]
fn weak_evidence_is_unknown() {
    // A hand-built feature set where the best score is a
    // 0.55 secondary: below the 0.6 threshold, so the tile
    // is `Unknown` with confidence 1 minus the best score.
    let analyzer = RegionAnalyzer::default();
    let features = TileFeatures {
        palette_size: 3,
        channel_variance: [5000.0, 5000.0, 5000.0],
        channel_range: [150, 150, 150],
        edge_density: 0.1,
        component_count: 3,
        largest_component_fraction: 0.5,
        mean_component_size: 256.0 / 3.0,
        dominant_contour_length: 40,
        dominant_contour_straightness: 0.7,
        row_period: None,
        col_period: None,
        row_period_score: 0.0,
        col_period_score: 0.0,
        texture_complexity: 0.1,
        gradient_fit: 0.0,
        gradient_direction: None,
        gradient_magnitude: 0.0,
        // 0.8 blocks the sprite primary (solid bodies score
        // low) and the edge density blocks the text primary,
        // so the best score is a 0.55 secondary.
        foreground_edge_ratio: 0.8,
    };
    let result = analyzer.classify(&features, None);
    assert_eq!(result.class, ContentClass::Unknown);
    assert!((result.confidence - 0.45).abs() < 1e-9);
}

#[test]
fn default_routing_sends_text_and_sprite_to_branches() -> Result<(), String> {
    // Left tile text, right tile flat, in one 32x16 frame.
    let frame = rgb_frame(32, 16, |x, y| {
        if x < 16 {
            let ink = (x == 1 && (2..11).contains(&y))
                || (x == 5 && (2..8).contains(&y))
                || (x == 9 && (5..14).contains(&y))
                || (x == 13 && (4..15).contains(&y));
            if ink { [20, 20, 20] } else { [240, 240, 240] }
        } else {
            [200, 30, 90]
        }
    })?;
    let map = analyze_frame(&frame);
    let routes = map.routes(&RoutingProfile::default());
    assert_eq!(routes.len(), 2);
    assert_eq!(routes[0].class, ContentClass::Text);
    assert_eq!(routes[0].route, Route::Branch("text"));
    assert_eq!(routes[1].class, ContentClass::Flat);
    assert_eq!(routes[1].route, Route::General);
    Ok(())
}

#[test]
fn halo_covers_neighbor_tiles() -> Result<(), String> {
    // Text tile at (0, 0) of a 32x32 frame. A 1-pixel halo
    // reaches the three neighboring tiles and no further.
    let frame = rgb_frame(32, 32, |x, y| {
        if x < 16 && y < 16 {
            let ink = (x == 1 && (2..11).contains(&y))
                || (x == 5 && (2..8).contains(&y))
                || (x == 9 && (5..14).contains(&y))
                || (x == 13 && (4..15).contains(&y));
            if ink { [20, 20, 20] } else { [240, 240, 240] }
        } else {
            [200, 30, 90]
        }
    })?;
    let map = analyze_frame(&frame);
    let halo = map.halo_for(ContentClass::Text, 1);
    assert_eq!(halo.count(), 4);
    assert!(halo.is_dirty(0, 0));
    assert!(halo.is_dirty(1, 0));
    assert!(halo.is_dirty(0, 1));
    assert!(halo.is_dirty(1, 1));
    assert!(!halo.is_dirty(2, 0));
    assert!(!halo.is_dirty(0, 2));
    let profile_halo = map.halos(&RoutingProfile::default());
    assert_eq!(profile_halo.count(), 4);
    Ok(())
}

#[test]
fn empty_frame_has_no_tiles() -> Result<(), String> {
    let frame = Frame::new(0, 0, PixelFormat::Rgb8, Vec::new()).map_err(|err| err.to_string())?;
    let map = analyze_frame(&frame);
    assert!(map.tiles().is_empty());
    assert_eq!(map.grid().tile_count(), 0);
    Ok(())
}

#[test]
fn strided_flat_frame_is_flat() -> Result<(), String> {
    // Row padding must not leak into the analysis: the rows
    // carry 8 padding bytes after 16 flat pixels each.
    let mut data = Vec::with_capacity(16 * 56);
    for _ in 0..16 {
        for _ in 0..16 {
            data.extend_from_slice(&[200, 30, 90]);
        }
        data.extend_from_slice(&[0; 8]);
    }
    let frame = Frame::new_with_stride(16, 16, PixelFormat::Rgb8, 56, data)
        .map_err(|err| err.to_string())?;
    let map = analyze_frame(&frame);
    assert_eq!(tile_class(&map, 0, 0)?, ContentClass::Flat);
    Ok(())
}

#[test]
fn rgba_and_gray_frames_analyze() -> Result<(), String> {
    let mut data = Vec::with_capacity(16 * 16 * 4);
    for _ in 0..16 * 16 {
        data.extend_from_slice(&[200, 30, 90, 255]);
    }
    let rgba = Frame::new(16, 16, PixelFormat::Rgba8, data).map_err(|err| err.to_string())?;
    assert_eq!(tile_class(&analyze_frame(&rgba), 0, 0)?, ContentClass::Flat);
    let mut gray = Vec::with_capacity(16 * 16);
    for y in 0..16 {
        for _ in 0..16 {
            gray.push(u8::try_from(y * 17).unwrap_or(255));
        }
    }
    let gray_frame = Frame::new(16, 16, PixelFormat::Gray8, gray).map_err(|err| err.to_string())?;
    assert_eq!(
        tile_class(&analyze_frame(&gray_frame), 0, 0)?,
        ContentClass::Gradient
    );
    Ok(())
}

#[test]
fn tile_outside_grid_is_none() -> Result<(), String> {
    let frame = flat_frame(16, 16, [200, 30, 90])?;
    let map = analyze_frame(&frame);
    assert!(map.tile(1, 0).is_none());
    assert!(map.tile(0, 1).is_none());
    Ok(())
}
