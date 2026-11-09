//! Letterbox layout: fit a source image inside a display area
//! without changing its aspect ratio.

/// A rectangle in physical pixels: top-left corner plus size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    /// Left edge, in physical pixels.
    pub x: u32,
    /// Top edge, in physical pixels.
    pub y: u32,
    /// Width, in physical pixels.
    pub width: u32,
    /// Height, in physical pixels.
    pub height: u32,
}

/// The largest rectangle of the source aspect ratio that fits inside
/// the destination, centered. Returns an empty rect at the origin
/// when either size is zero.
#[must_use]
pub fn fit_rect(src_width: u32, src_height: u32, dst_width: u32, dst_height: u32) -> Rect {
    if src_width == 0 || src_height == 0 || dst_width == 0 || dst_height == 0 {
        return Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
    }
    let src_w = u64::from(src_width);
    let src_h = u64::from(src_height);
    let dst_w = u64::from(dst_width);
    let dst_h = u64::from(dst_height);
    // Width binds when the destination is wider, relative to the
    // source, than the source is tall: dst_w / src_w <= dst_h / src_h.
    if dst_w * src_h <= dst_h * src_w {
        let width = dst_width;
        // The branch condition bounds this by dst_h, so the value
        // fits in u32 and the fallback is unreachable.
        let height = u32::try_from(dst_w * src_h / src_w).unwrap_or(dst_height);
        Rect {
            x: 0,
            y: (dst_height - height) / 2,
            width,
            height,
        }
    } else {
        let height = dst_height;
        // The branch condition bounds this by dst_w, so the value
        // fits in u32 and the fallback is unreachable.
        let width = u32::try_from(dst_h * src_w / src_h).unwrap_or(dst_width);
        Rect {
            x: (dst_width - width) / 2,
            y: 0,
            width,
            height,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upscaled_game_into_1080p_letterboxes_top_and_bottom() {
        // 1024x896 into 1920x1080: the height binds, bars left/right.
        let r = fit_rect(1024, 896, 1920, 1080);
        assert_eq!(
            r,
            Rect {
                x: 343,
                y: 0,
                width: 1234,
                height: 1080
            }
        );
    }

    #[test]
    fn exact_multiple_fills_the_destination() {
        assert_eq!(
            fit_rect(256, 224, 1024, 896),
            Rect {
                x: 0,
                y: 0,
                width: 1024,
                height: 896
            }
        );
    }

    #[test]
    fn portrait_source_letterboxes_sides() {
        let r = fit_rect(224, 256, 1920, 1080);
        assert_eq!(r.height, 1080);
        assert!(r.width < 1920);
        assert_eq!(r.x, (1920 - r.width) / 2);
        assert_eq!(r.y, 0);
    }

    #[test]
    fn zero_size_is_an_empty_rect() {
        assert_eq!(
            fit_rect(0, 224, 1920, 1080),
            Rect {
                x: 0,
                y: 0,
                width: 0,
                height: 0
            }
        );
        assert_eq!(
            fit_rect(256, 224, 0, 1080),
            Rect {
                x: 0,
                y: 0,
                width: 0,
                height: 0
            }
        );
    }
}
