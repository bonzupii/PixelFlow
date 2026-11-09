//! Algorithm 20 from the retro upscaler experiments, as a library.
//!
//! 20 = 12 (combined) + `edge_follow2`, with no dither source anywhere.
//
// This file is a transliteration of the Python reference
// (upscale-experiments rounds 1-6, algos1-6.py). Its numeric output is
// pinned by the pixel regression test. The notation follows the
// reference where the lints permit it; where a lint fires, the code is
// restructured instead: lossless casts use From, index and sign
// arithmetic carry the right type by construction, constants carry
// digit separators, and long functions are split into named helpers.
// No allow attribute exists in this file. The narrowing float casts
// that remain are the ones that cannot be removed without changing
// the math: f64 accumulators and weights written into f32 buffers,
// f32 values written into u8 output, and the f32 best-distance
// comparison in the edge follower, whose rounding decides near-tie
// winners and is pinned by the regression test (mario 34/6).
//! The code below is the standalone Rust port from
//! `contour/src/main.rs`, moved here verbatim: the mathematical
//! operations, float widths, gates, and the hardcoded Savitzky-Golay
//! kernel tables (numpy pinv values) are unchanged, so its output is
//! identical to the standalone program. Do not simplify, redesign, or
//! silently alter it during integration. The only I/O dependency in
//! the original program was the `png` crate, and that lives in the
//! CLI now; this module is pure std plus pixelflow-core types.
//!
//! Testing switches, kept from the port: `CONTOUR_NO_FOLLOW=1`
//! skips the edge follower (12 only); `CONTOUR_DEBUG_RUNS=1`
//! prints each accepted run to stderr.

use pixelflow_core::{Frame, PixelFormat, ProcessError, Processor};

// ---------------------------------------------------------------- images
#[derive(Clone)]
struct UImg {
    w: usize,
    h: usize,
    d: Vec<u8>, // row-major RGB
}

#[derive(Clone)]
struct FImg {
    w: usize,
    h: usize,
    d: Vec<f32>, // row-major RGB
}

impl UImg {
    fn get(&self, y: usize, x: usize) -> [u8; 3] {
        let i = (y * self.w + x) * 3;
        [self.d[i], self.d[i + 1], self.d[i + 2]]
    }
    fn getf(&self, y: usize, x: usize) -> [f32; 3] {
        let p = self.get(y, x);
        [f32::from(p[0]), f32::from(p[1]), f32::from(p[2])]
    }
}

impl FImg {
    fn new(w: usize, h: usize) -> Self {
        FImg {
            w,
            h,
            d: vec![0.0; w * h * 3],
        }
    }
    fn get(&self, y: usize, x: usize) -> [f32; 3] {
        let i = (y * self.w + x) * 3;
        [self.d[i], self.d[i + 1], self.d[i + 2]]
    }
    fn set(&mut self, y: usize, x: usize, p: [f32; 3]) {
        let i = (y * self.w + x) * 3;
        self.d[i] = p[0];
        self.d[i + 1] = p[1];
        self.d[i + 2] = p[2];
    }
}

/// Clamp `pos + delta` into `[0, hi]` as a usize index. `hi` is the
/// last valid index (length - 1); callers only call this when the
/// length is nonzero, so `hi` is a real index. The offset stays in
/// usize via `saturating_add_signed`, so no narrowing cast is needed.
fn clamp_idx(pos: usize, delta: isize, hi: usize) -> usize {
    pos.saturating_add_signed(delta).min(hi)
}

fn clamp_f(v: f32, lo: f32, hi: f32) -> f32 {
    if v < lo {
        lo
    } else if v > hi {
        hi
    } else {
        v
    }
}

/// Fold a non-negative position into `[0, n)` by scipy 'reflect':
/// (d c b a | a b c d | d c b a). The fold is periodic with period
/// `2n`: reduce mod `2n`, then mirror past `n - 1`.
fn reflect_fold(pos: usize, n: usize) -> usize {
    if n <= 1 {
        return 0;
    }
    let folded = pos % (2 * n);
    if folded < n {
        folded
    } else {
        2 * n - 1 - folded
    }
}

/// scipy 'reflect' index for position `x + k` in a length-`n` axis.
/// A negative position `-m` folds to `m - 1` first, then as above.
fn reflect_idx(x: usize, k: isize, n: usize) -> usize {
    match x.checked_add_signed(k) {
        Some(pos) => reflect_fold(pos, n),
        None => reflect_fold(k.unsigned_abs() - x - 1, n),
    }
}

/// Fold `x + d` into `[0, n)` by scipy 'reflect'.
fn reflect_add(x: usize, d: usize, n: usize) -> usize {
    reflect_fold(x + d, n)
}

/// Fold `x - d` into `[0, n)` by scipy 'reflect'.
fn reflect_sub(x: usize, d: usize, n: usize) -> usize {
    if d <= x {
        reflect_fold(x - d, n)
    } else {
        reflect_fold(d - x - 1, n)
    }
}

/// Floor of `v`, clamped to `[0, n]`, as a u32. Binary search over
/// integer midpoints, whose f64 values are exact, so this is the
/// floor exactly, with no float-to-integer cast. Callers pass `n`
/// as a u32 so the midpoints need no conversion either.
fn clamp_floor_u32(v: f64, n: u32) -> u32 {
    if v <= 0.0 {
        return 0;
    }
    if v >= f64::from(n) {
        return n;
    }
    let mut lo = 0u32;
    let mut hi = n;
    while hi - lo > 1 {
        let mid = lo.midpoint(hi);
        if f64::from(mid) <= v {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lo
}

/// shifted(a, dy, dx): a[y+dy][x+dx] with edge clamping, like `algos1.shifted`.
fn shifted_u(src: &UImg, dy: isize, dx: isize) -> UImg {
    let mut out = UImg {
        w: src.w,
        h: src.h,
        d: vec![0; src.d.len()],
    };
    for y in 0..src.h {
        let sy = clamp_idx(y, dy, src.h - 1);
        for x in 0..src.w {
            let sx = clamp_idx(x, dx, src.w - 1);
            let p = src.get(sy, sx);
            let i = (y * src.w + x) * 3;
            out.d[i] = p[0];
            out.d[i + 1] = p[1];
            out.d[i + 2] = p[2];
        }
    }
    out
}

fn shifted_b(mask: &[bool], w: usize, h: usize, dy: isize, dx: isize) -> Vec<bool> {
    let mut out = vec![false; w * h];
    for y in 0..h {
        let sy = clamp_idx(y, dy, h - 1);
        for x in 0..w {
            let sx = clamp_idx(x, dx, w - 1);
            out[y * w + x] = mask[sy * w + sx];
        }
    }
    out
}

fn color_ids(src: &UImg) -> Vec<u32> {
    let mut out = vec![0u32; src.w * src.h];
    for (i, px) in src.d.as_chunks::<3>().0.iter().enumerate() {
        out[i] = (u32::from(px[0]) << 16) | (u32::from(px[1]) << 8) | u32::from(px[2]);
    }
    out
}

fn to_u8_img(x: &FImg) -> Vec<u8> {
    x.d.iter()
        .map(|v| clamp_f(v.round(), 0.0, 255.0) as u8)
        .collect()
}

// ---------------------------------------------------------------- erf / taps
/// Abramowitz-Stegun 7.1.26, |error| < 1.5e-7. std has no erf.
fn erf(x: f64) -> f64 {
    let (s, ax) = if x < 0.0 { (-1.0, -x) } else { (1.0, x) };
    let t = 1.0 / (1.0 + 0.327_591_1 * ax);
    let poly = (((((1.061_405_429 * t - 1.453_152_027) * t) + 1.421_413_741) * t - 0.284_496_736)
        * t
        + 0.254_829_592)
        * t;
    s * (1.0 - poly * (-ax * ax).exp())
}

// Normal CDF shape, the Abramowitz-Stegun form written as a
// midpoint: 0.5 + erf(x / sqrt(2)) * 0.5.
fn phi(x: f64) -> f64 {
    0.5 + erf(x / std::f64::consts::SQRT_2) * 0.5
}

/// Gaussian coverage taps over ks = [-1, 0, 1], like `algos1._taps`.
fn taps(u: f32, sig: f32) -> [f32; 3] {
    let ks = [-1.0f64, 0.0, 1.0];
    let mut w = [0.0f64; 3];
    let mut sum = 0.0;
    for (i, k) in ks.iter().enumerate() {
        w[i] = phi((k + 0.5 - f64::from(u)) / f64::from(sig))
            - phi((k - 0.5 - f64::from(u)) / f64::from(sig));
        sum += w[i];
    }
    [
        (w[0] / sum) as f32,
        (w[1] / sum) as f32,
        (w[2] / sum) as f32,
    ]
}

// ---------------------------------------------------------------- blur
/// Separable gaussian blur, scipy `gaussian_filter` semantics:
/// sigma, truncate 4.0 (radius ceil(4*sigma)), mode 'reflect', per axis.
fn gaussian_blur(src: &FImg, sigma: f32) -> FImg {
    if sigma <= 0.0 {
        return src.clone();
    }
    // Kernel radius as an f32: the taps are exact integers, so the
    // f32 counter below visits exactly -r..=r with no float-to-int
    // cast, and the tap offsets come from the kernel index.
    let radius = (4.0 * sigma).ceil();
    let mut kern = Vec::new();
    let mut sum = 0.0f64;
    let mut offset = -radius;
    while offset <= radius {
        let v = (-0.5 * (f64::from(offset) / f64::from(sigma)).powi(2)).exp();
        kern.push(v);
        sum += v;
        offset += 1.0;
    }
    for weight in &mut kern {
        *weight /= sum;
    }
    let centre = kern.len() / 2;
    let mut tmp = FImg::new(src.w, src.h);
    for y in 0..src.h {
        for x in 0..src.w {
            let mut acc = [0.0f64; 3];
            for (ki, &wgt) in kern.iter().enumerate() {
                let sx = if ki < centre {
                    reflect_sub(x, centre - ki, src.w)
                } else {
                    reflect_add(x, ki - centre, src.w)
                };
                let px = src.get(y, sx);
                for c in 0..3 {
                    acc[c] += wgt * f64::from(px[c]);
                }
            }
            tmp.set(y, x, [acc[0] as f32, acc[1] as f32, acc[2] as f32]);
        }
    }
    let mut out = FImg::new(src.w, src.h);
    for y in 0..src.h {
        for x in 0..src.w {
            let mut acc = [0.0f64; 3];
            for (ki, &wgt) in kern.iter().enumerate() {
                let sy = if ki < centre {
                    reflect_sub(y, centre - ki, src.h)
                } else {
                    reflect_add(y, ki - centre, src.h)
                };
                let px = tmp.get(sy, x);
                for c in 0..3 {
                    acc[c] += wgt * f64::from(px[c]);
                }
            }
            out.set(y, x, [acc[0] as f32, acc[1] as f32, acc[2] as f32]);
        }
    }
    out
}

/// 3x3 box blur, scipy `uniform_filter` size (3,3,1), mode 'reflect'.
fn uniform3(src: &FImg) -> FImg {
    let mut tmp = FImg::new(src.w, src.h);
    for y in 0..src.h {
        for x in 0..src.w {
            let mut acc = [0.0f64; 3];
            for k in [-1isize, 0, 1] {
                let sx = reflect_idx(x, k, src.w);
                let px = src.get(y, sx);
                for c in 0..3 {
                    acc[c] += f64::from(px[c]) / 3.0;
                }
            }
            tmp.set(y, x, [acc[0] as f32, acc[1] as f32, acc[2] as f32]);
        }
    }
    let mut out = FImg::new(src.w, src.h);
    for y in 0..src.h {
        for x in 0..src.w {
            let mut acc = [0.0f64; 3];
            for k in [-1isize, 0, 1] {
                let sy = reflect_idx(y, k, src.h);
                let px = tmp.get(sy, x);
                for c in 0..3 {
                    acc[c] += f64::from(px[c]) / 3.0;
                }
            }
            out.set(y, x, [acc[0] as f32, acc[1] as f32, acc[2] as f32]);
        }
    }
    out
}

/// 1D uniform filter, size 5, mode 'nearest' (edge clamp), for the
/// follower's gate smoothing. Input/output are plain f32 slices.
fn uniform1d_nearest(v: &[f32]) -> Vec<f32> {
    let n = v.len();
    let mut out = vec![0.0f32; v.len()];
    for i in 0..v.len() {
        let mut s = 0.0f32;
        for k in -2isize..=2 {
            s += v[clamp_idx(i, k, n - 1)];
        }
        out[i] = s / 5.0;
    }
    out
}

// ---------------------------------------------------------------- 00 / 02
fn nearest(a: &UImg, s: usize) -> FImg {
    let mut out = FImg::new(a.w * s, a.h * s);
    for y in 0..a.h {
        for x in 0..a.w {
            let p = a.getf(y, x);
            for dy in 0..s {
                for dx in 0..s {
                    out.set(y * s + dy, x * s + dx, p);
                }
            }
        }
    }
    out
}

/// Scale2x / EPX, one doubling, like `algos1.scale2x`.
/// Reference neighbour names: `b` is up, `h_` is down, `d` is left,
/// `f` is right.
fn scale2x(src: &UImg) -> UImg {
    let ids = color_ids(src);
    let id_at = |y: usize, dy: isize, x: usize, dx: isize| -> u32 {
        let sy = clamp_idx(y, dy, src.h - 1);
        let sx = clamp_idx(x, dx, src.w - 1);
        ids[sy * src.w + sx]
    };
    let mut out = UImg {
        w: src.w * 2,
        h: src.h * 2,
        d: vec![0; src.w * 2 * src.h * 2 * 3],
    };
    for y in 0..src.h {
        for x in 0..src.w {
            let up = id_at(y, -1, x, 0);
            let down = id_at(y, 1, x, 0);
            let left = id_at(y, 0, x, -1);
            let right = id_at(y, 0, x, 1);
            let left_px = src.getf(y, x.saturating_sub(1));
            let right_px = src.getf(y, (x + 1).min(src.w - 1));
            let centre = src.getf(y, x);
            let c0 = left == up && up != right && left != down;
            let c1 = up == right && up != left && right != down;
            let c2 = left == down && left != up && down != right;
            let c3 = down == right && left != down && up != right;
            let mut put = |dy: usize, dx: usize, px: [f32; 3]| {
                let idx = ((y * 2 + dy) * out.w + (x * 2 + dx)) * 3;
                out.d[idx] = clamp_f(px[0].round(), 0.0, 255.0) as u8;
                out.d[idx + 1] = clamp_f(px[1].round(), 0.0, 255.0) as u8;
                out.d[idx + 2] = clamp_f(px[2].round(), 0.0, 255.0) as u8;
            };
            put(0, 0, if c0 { left_px } else { centre });
            put(0, 1, if c1 { right_px } else { centre });
            put(1, 0, if c2 { left_px } else { centre });
            put(1, 1, if c3 { right_px } else { centre });
        }
    }
    out
}

fn scalenx(a: &UImg, s: usize) -> UImg {
    let n = s.trailing_zeros();
    assert_eq!(1usize << n, s, "scale must be a power of two");
    let mut cur = a.clone();
    for _ in 0..n {
        cur = scale2x(&cur);
    }
    cur
}

// ---------------------------------------------------------------- 07 dither
/// Pair-consistent 2x2 checker mask, `algos2.dither_mask`. Fires only
/// where a checker cell's colour pair repeats 2 px away.
fn dither_mask(src: &UImg) -> Vec<bool> {
    let ids = color_ids(src);
    let (w, h) = (src.w, src.h);
    let id_at = |y: usize, dy: isize, x: usize, dx: isize| -> u32 {
        let sy = clamp_idx(y, dy, h - 1);
        let sx = clamp_idx(x, dx, w - 1);
        ids[sy * w + sx]
    };
    let mut checker = vec![false; w * h];
    let mut key = vec![-1i64; w * h];
    for y in 0..h {
        for x in 0..w {
            let tl = id_at(y, 0, x, 0);
            let tr = id_at(y, 0, x, 1);
            let bl = id_at(y, 1, x, 0);
            let br = id_at(y, 1, x, 1);
            let c = tl == br && tr == bl && tl != tr;
            checker[y * w + x] = c;
            if c {
                let lo = i64::from(tl.min(tr));
                let hi = i64::from(tl.max(tr));
                key[y * w + x] = lo * (1i64 << 24) + hi;
            }
        }
    }
    let mut same = vec![false; w * h];
    for (dy, dx) in [(0isize, 2isize), (0, -2), (2, 0), (-2, 0)] {
        let k2 = shifted_i64(&key, w, h, dy, dx);
        for i in 0..w * h {
            if checker[i] && key[i] == k2[i] && k2[i] >= 0 {
                same[i] = true;
            }
        }
    }
    let mut mask = vec![false; w * h];
    for dy in 0..2isize {
        for dx in 0..2isize {
            let s = shifted_b(&same, w, h, -dy, -dx);
            for i in 0..w * h {
                mask[i] |= s[i];
            }
        }
    }
    mask
}

fn shifted_i64(vals: &[i64], w: usize, h: usize, dy: isize, dx: isize) -> Vec<i64> {
    let mut out = vec![0i64; w * h];
    for y in 0..h {
        let sy = clamp_idx(y, dy, h - 1);
        for x in 0..w {
            let sx = clamp_idx(x, dx, w - 1);
            out[y * w + x] = vals[sy * w + sx];
        }
    }
    out
}

/// Per-pixel 50% blend of the aligned 2x2 block the pixel sits in.
fn dither_blend(a: &UImg) -> FImg {
    let (w, h) = (a.w, a.h);
    let h2 = h / 2;
    let w2 = w / 2;
    let mut out = FImg::new(w, h);
    for by in 0..h2 {
        for bx in 0..w2 {
            let mut acc = [0.0f32; 3];
            for dy in 0..2 {
                for dx in 0..2 {
                    let p = a.getf(by * 2 + dy, bx * 2 + dx);
                    for c in 0..3 {
                        acc[c] += p[c] / 4.0;
                    }
                }
            }
            for dy in 0..2 {
                for dx in 0..2 {
                    out.set(by * 2 + dy, bx * 2 + dx, acc);
                }
            }
        }
    }
    if h % 2 == 1 {
        for x in 0..w2 * 2 {
            out.set(h - 1, x, a.getf(h - 1, x));
        }
        out.set(h - 1, w - 1, a.getf(h - 1, w - 1));
    }
    if w % 2 == 1 {
        for y in 0..h {
            out.set(y, w - 1, a.getf(y, w - 1));
        }
    }
    out
}

// ---------------------------------------------------------------- 08
const OFFS: [(isize, isize); 9] = [
    (-1, -1),
    (-1, 0),
    (-1, 1),
    (0, -1),
    (0, 0),
    (0, 1),
    (1, -1),
    (1, 0),
    (1, 1),
];

/// 08: contrast-adaptive sharp labelset, `algos2.labelset_adaptive`.
/// Reference names: `w`/`h` are the width/height, `f` is a shifted
/// colour plane, `l` is the per-tap label weight, `p` is the blended
/// output pixel.
fn labelset_adaptive(src: &UImg, scale: usize) -> FImg {
    let sig = 0.35f32;
    let (width, height) = (src.w, src.h);
    // The scale factor as an f32 for the tap math. Scales are 2 or 4
    // (validated by the caller); anything past u16 is not a real
    // scale, and the empty image is the only FImg this can return.
    let Ok(scale_u16) = u16::try_from(scale) else {
        return FImg::new(0, 0);
    };
    let scale_f = f32::from(scale_u16);
    // 9 shifted colour/id planes
    let mut planes_c: Vec<FImg> = Vec::with_capacity(9);
    let mut planes_id: Vec<Vec<u32>> = Vec::with_capacity(9);
    for (dy, dx) in OFFS {
        planes_c.push({
            let sh = shifted_u(src, dy, dx);
            let mut plane = FImg::new(width, height);
            for i in 0..width * height {
                plane.d[i * 3] = f32::from(sh.d[i * 3]);
                plane.d[i * 3 + 1] = f32::from(sh.d[i * 3 + 1]);
                plane.d[i * 3 + 2] = f32::from(sh.d[i * 3 + 2]);
            }
            plane
        });
        planes_id.push(color_ids(&shifted_u(src, dy, dx)));
    }
    let mut out = FImg::new(width * scale, height * scale);
    let kbase = 1.04 * sig * scale_f;
    // row_f/col_f count the subpixel offsets in f32, exactly equal to
    // the usize offsets as f32, so the tap math needs no cast.
    let mut row_f = 0.0f32;
    for iy in 0..scale {
        let wy = taps((row_f + 0.5) / scale_f - 0.5, sig);
        let mut col_f = 0.0f32;
        for ix in 0..scale {
            let wx = taps((col_f + 0.5) / scale_f - 0.5, sig);
            // tap weight w[k] = wy[dy+1] * wx[dx+1]; OFFS is row-major,
            // so dy+1 is k/3 and dx+1 is k%3.
            let mut tw = [0.0f32; 9];
            for (k, _) in OFFS.iter().enumerate() {
                tw[k] = wy[k / 3] * wx[k % 3];
            }
            for y in 0..height {
                for x in 0..width {
                    // L[i] = sum_j tw[j] where ids[j] == ids[i]
                    let mut label_w = [0.0f32; 9];
                    for i in 0..9 {
                        let mut acc = 0.0f32;
                        for j in 0..9 {
                            if planes_id[j][y * width + x] == planes_id[i][y * width + x] {
                                acc += tw[j];
                            }
                        }
                        label_w[i] = acc;
                    }
                    let mut i1 = 0;
                    for i in 1..9 {
                        if label_w[i] > label_w[i1] {
                            i1 = i;
                        }
                    }
                    let l1 = label_w[i1];
                    let c1 = planes_c[i1].get(y, x);
                    let id1 = planes_id[i1][y * width + x];
                    // runner-up: best L among taps whose id != id1
                    let mut i2 = 0;
                    let mut best = f32::NEG_INFINITY;
                    for i in 0..9 {
                        let lv = if planes_id[i][y * width + x] == id1 {
                            -1.0
                        } else {
                            label_w[i]
                        };
                        if lv > best {
                            best = lv;
                            i2 = i;
                        }
                    }
                    let l2 = best.max(0.0);
                    let c2 = planes_c[i2].get(y, x);
                    let den = ((c2[0] - c1[0]).powi(2)
                        + (c2[1] - c1[1]).powi(2)
                        + (c2[2] - c1[2]).powi(2))
                    .sqrt();
                    let gate = clamp_f((den - 60.0) / 60.0, 0.0, 1.0);
                    let k2 = kbase * (1.0 + 0.9 * gate);
                    let p2 = clamp_f(0.5 - (l1 - l2) * k2, 0.0, 0.5);
                    let pixel = [
                        c1[0] + p2 * (c2[0] - c1[0]),
                        c1[1] + p2 * (c2[1] - c1[1]),
                        c1[2] + p2 * (c2[2] - c1[2]),
                    ];
                    out.set(y * scale + iy, x * scale + ix, pixel);
                }
            }
            col_f += 1.0;
        }
        row_f += 1.0;
    }
    out
}

// ---------------------------------------------------------------- 09 / 12
/// 09's low-contrast merge, `algos2.merge_low_contrast`: average each
/// source pixel with 3x3 neighbours within T=40 RGB, self weight 1.5.
fn merge_low_contrast(src: &UImg) -> FImg {
    let (width, height) = (src.w, src.h);
    let mut num = FImg::new(width, height);
    let mut den = vec![0.0f32; width * height];
    for (dy, dx) in OFFS {
        let nb = shifted_u(src, dy, dx);
        for y in 0..height {
            for x in 0..width {
                let centre = src.getf(y, x);
                let neigh = nb.getf(y, x);
                let dist = ((neigh[0] - centre[0]).powi(2)
                    + (neigh[1] - centre[1]).powi(2)
                    + (neigh[2] - centre[2]).powi(2))
                .sqrt();
                let mut wgt = if dist < 40.0 { 1.0 } else { 0.0 };
                if dy == 0 && dx == 0 {
                    wgt *= 1.5;
                }
                let idx = y * width + x;
                den[idx] += wgt;
                let cur = num.get(y, x);
                num.set(
                    y,
                    x,
                    [
                        cur[0] + wgt * neigh[0],
                        cur[1] + wgt * neigh[1],
                        cur[2] + wgt * neigh[2],
                    ],
                );
            }
        }
    }
    let mut out = FImg::new(width, height);
    for y in 0..height {
        for x in 0..width {
            let denom = den[y * width + x].max(1e-6);
            let num_px = num.get(y, x);
            out.set(
                y,
                x,
                [num_px[0] / denom, num_px[1] / denom, num_px[2] / denom],
            );
        }
    }
    out
}

/// Smooth neighbourhoods: at most 2 distinct colours in 3x3 and
/// local std < 12, `algos3.smooth_mask`.
fn smooth_mask(a: &UImg) -> Vec<bool> {
    let (w, h) = (a.w, a.h);
    let ids = color_ids(a);
    let mut distinct = vec![0usize; w * h];
    // shifted id planes
    let mut planes: Vec<Vec<u32>> = Vec::with_capacity(9);
    for (dy, dx) in OFFS {
        planes.push({
            let mut v = vec![0u32; w * h];
            for y in 0..h {
                let sy = clamp_idx(y, dy, h - 1);
                for x in 0..w {
                    let sx = clamp_idx(x, dx, w - 1);
                    v[y * w + x] = ids[sy * w + sx];
                }
            }
            v
        });
    }
    for i in 0..w * h {
        let mut vals = [0u32; 9];
        for k in 0..9 {
            vals[k] = planes[k][i];
        }
        vals.sort_unstable();
        let mut n = 1;
        for k in 1..9 {
            if vals[k] != vals[k - 1] {
                n += 1;
            }
        }
        distinct[i] = n;
    }
    // std3 via uniform 3x3 of af and af^2
    let mut af = FImg::new(w, h);
    let mut af2 = FImg::new(w, h);
    for i in 0..w * h {
        for c in 0..3 {
            let v = f32::from(a.d[i * 3 + c]);
            af.d[i * 3 + c] = v;
            af2.d[i * 3 + c] = v * v;
        }
    }
    let m = uniform3(&af);
    let m2 = uniform3(&af2);
    let mut out = vec![false; w * h];
    for i in 0..w * h {
        let mut std = 0.0f32;
        for c in 0..3 {
            let var = (m2.d[i * 3 + c] - m.d[i * 3 + c] * m.d[i * 3 + c]).max(0.0);
            std += var.sqrt();
        }
        std /= 3.0;
        out[i] = distinct[i] <= 2 && std < 12.0;
    }
    out
}

/// 09's merge, gated to smooth neighbourhoods, `algos3.gated_merge`.
fn gated_merge(a: &UImg) -> FImg {
    let merged = merge_low_contrast(a);
    let gate = smooth_mask(a);
    let mut out = FImg::new(a.w, a.h);
    for y in 0..a.h {
        for x in 0..a.w {
            out.set(
                y,
                x,
                if gate[y * a.w + x] {
                    merged.get(y, x)
                } else {
                    a.getf(y, x)
                },
            );
        }
    }
    out
}

/// 11's grain at low strength, `algos3.grain_layer`.
fn grain_layer(a: &UImg, s: usize, strength: f32) -> FImg {
    let near = nearest(a, s);
    let blur = gaussian_blur(&near, 1.0);
    // std from gaussian-filtered near and near^2, mean over channels
    let mut near2 = FImg::new(near.w, near.h);
    for i in 0..near.w * near.h {
        for c in 0..3 {
            let v = near.d[i * 3 + c];
            near2.d[i * 3 + c] = v * v;
        }
    }
    let sq = gaussian_blur(&near2, 1.0);
    let mean = gaussian_blur(&near, 1.0);
    let mut out = FImg::new(near.w, near.h);
    for y in 0..near.h {
        for x in 0..near.w {
            let g = [
                near.get(y, x)[0] - blur.get(y, x)[0],
                near.get(y, x)[1] - blur.get(y, x)[1],
                near.get(y, x)[2] - blur.get(y, x)[2],
            ];
            let mut std = 0.0f32;
            for c in 0..3 {
                let var = (sq.d[(y * near.w + x) * 3 + c]
                    - mean.d[(y * near.w + x) * 3 + c].powi(2))
                .max(0.0);
                std += var.sqrt();
            }
            std /= 3.0;
            let gate = clamp_f(std / 30.0, 0.0, 1.0);
            out.set(
                y,
                x,
                [
                    strength * g[0] * gate,
                    strength * g[1] * gate,
                    strength * g[2] * gate,
                ],
            );
        }
    }
    out
}

/// Hard-light blend, `algos2.hardlight`.
fn hardlight(base: &FImg, top: &FImg) -> FImg {
    let mut out = FImg::new(base.w, base.h);
    for i in 0..base.w * base.h {
        for c in 0..3 {
            let b = base.d[i * 3 + c] / 255.0;
            let t = top.d[i * 3 + c] / 255.0;
            let v = if t <= 0.5 {
                2.0 * b * t
            } else {
                1.0 - 2.0 * (1.0 - b) * (1.0 - t)
            };
            out.d[i * 3 + c] = v * 255.0;
        }
    }
    out
}

/// 12: the combined recipe, `algos3.combined`.
fn combined(a: &UImg, s: usize) -> FImg {
    let merged = gated_merge(a);
    let src = UImg {
        w: a.w,
        h: a.h,
        d: to_u8_img(&merged),
    };
    let mut out = labelset_adaptive(&src, s);
    // 07 dither blend on clean checkerboard, from the original source
    let dmask = dither_mask(a);
    let blend = dither_blend(a);
    for y in 0..a.h {
        for x in 0..a.w {
            if dmask[y * a.w + x] {
                let p = blend.get(y, x);
                for dy in 0..s {
                    for dx in 0..s {
                        out.set(y * s + dy, x * s + dx, p);
                    }
                }
            }
        }
    }
    // 11 grain at 0.3
    let grain = grain_layer(a, s, 0.3);
    for i in 0..out.w * out.h {
        for c in 0..3 {
            out.d[i * 3 + c] = clamp_f(out.d[i * 3 + c] + grain.d[i * 3 + c], 0.0, 255.0);
        }
    }
    // 10 hard-light at 0.5
    let detail_u = scalenx(a, s);
    let mut detail = FImg::new(detail_u.w, detail_u.h);
    for i in 0..detail_u.w * detail_u.h {
        for c in 0..3 {
            detail.d[i * 3 + c] = f32::from(detail_u.d[i * 3 + c]);
        }
    }
    let hl = hardlight(&out, &detail);
    for i in 0..out.w * out.h {
        for c in 0..3 {
            out.d[i * 3 + c] = clamp_f(0.5 * out.d[i * 3 + c] + 0.5 * hl.d[i * 3 + c], 0.0, 255.0);
        }
    }
    out
}

// ---------------------------------------------------------------- SG fit
const SG7_VALUE: &[f64] = &[
    -0.095_238_095_238_095_19,
    0.142_857_142_857_142_9,
    0.285_714_285_714_285_6,
    0.333_333_333_333_333_26,
    0.285_714_285_714_285_6,
    0.142_857_142_857_142_82,
    -0.095_238_095_238_095_2,
];
const SG7_DERIV: &[f64] = &[
    -0.107_142_857_142_857_19,
    -0.071_428_571_428_571_44,
    -0.035_714_285_714_285_705,
    1.306_861_959_119_566_4e-17,
    0.035_714_285_714_285_74,
    0.071_428_571_428_571_44,
    0.107_142_857_142_857_15,
];
const SG7_CURV: &[f64] = &[
    0.059_523_809_523_809_514,
    -1.387_778_780_781_445_7e-17,
    -0.035_714_285_714_285_7,
    -0.047_619_047_619_047_6,
    -0.035_714_285_714_285_7,
    3.469_446_951_953_614e-18,
    0.059_523_809_523_809_51,
];
const SG9_VALUE: &[f64] = &[
    -0.090_909_090_909_090_94,
    0.060_606_060_606_060_61,
    0.168_831_168_831_168_8,
    0.233_766_233_766_233_73,
    0.255_411_255_411_255_4,
    0.233_766_233_766_233_73,
    0.168_831_168_831_168_8,
    0.060_606_060_606_060_59,
    -0.090_909_090_909_090_93,
];
const SG9_DERIV: &[f64] = &[
    -0.066_666_666_666_666_65,
    -0.05,
    -0.033_333_333_333_333_326,
    -0.016_666_666_666_666_663,
    3.583_229_404_124_381_4e-18,
    0.016_666_666_666_666_67,
    0.033_333_333_333_333_34,
    0.05,
    0.066_666_666_666_666_65,
];
const SG9_CURV: &[f64] = &[
    0.030_303_030_303_030_297,
    0.007_575_757_575_757_572_5,
    -0.008_658_008_658_008_658,
    -0.018_398_268_398_268_393,
    -0.021_645_021_645_021_64,
    -0.018_398_268_398_268_393,
    -0.008_658_008_658_008_658,
    0.007_575_757_575_757_574,
    0.030_303_030_303_030_297,
];
const SG11_VALUE: &[f64] = &[
    -0.083_916_083_916_083_82,
    0.020_979_020_979_021_032,
    0.102_564_102_564_102_52,
    0.160_839_160_839_160_86,
    0.195_804_195_804_195_8,
    0.207_459_207_459_207_43,
    0.195_804_195_804_195_8,
    0.160_839_160_839_160_8,
    0.102_564_102_564_102_55,
    0.020_979_020_979_020_956,
    -0.083_916_083_916_083_95,
];
const SG11_DERIV: &[f64] = &[
    -0.045_454_545_454_545_44,
    -0.036_363_636_363_636_38,
    -0.027_272_727_272_727_282,
    -0.018_181_818_181_818_2,
    -0.009_090_909_090_909_101,
    -5.904_506_863_422_596_4e-18,
    0.009_090_909_090_909_09,
    0.018_181_818_181_818_188,
    0.027_272_727_272_727_28,
    0.036_363_636_363_636_38,
    0.045_454_545_454_545_484,
];
const SG11_CURV: &[f64] = &[
    0.017_482_517_482_517_47,
    0.006_993_006_993_006_988,
    -0.001_165_501_165_501_160_7,
    -0.006_993_006_993_006_996_5,
    -0.010_489_510_489_510_49,
    -0.011_655_011_655_011_656,
    -0.010_489_510_489_510_492,
    -0.006_993_006_993_006_992,
    -0.001_165_501_165_501_165,
    0.006_993_006_993_006_996,
    0.017_482_517_482_517_484,
];

/// Savitzky-Golay kernels for an odd window, quadratic fit, evaluated
/// at centre: (value, derivative, curvature) weights. These are the
/// exact f64 values numpy's pinv(Vandermonde) returns in algos4
/// (`_sg_kernel`), hardcoded: the follower's apex gate reads the *sign*
/// of the fitted slope, and at the flat points of a staircase the
/// slope is exactly 0 in exact arithmetic, so its sign is whatever
/// the pinv's ~1e-15 rounding noise says. Recomputing the kernels
/// from the normal equations gives a different noise sign and flips
/// the gate on ~14 samples of a long run. The tables below are the
/// only windows `fit_run` can ask for (7, 9, 11).
fn sg_kernels(win: usize) -> Option<(Vec<f64>, Vec<f64>, Vec<f64>)> {
    let (kv, kd, k2): (&[f64], &[f64], &[f64]) = match win {
        7 => (SG7_VALUE, SG7_DERIV, SG7_CURV),
        9 => (SG9_VALUE, SG9_DERIV, SG9_CURV),
        11 => (SG11_VALUE, SG11_DERIV, SG11_CURV),
        _ => return None,
    };
    Some((kv.to_vec(), kd.to_vec(), k2.to_vec()))
}

/// Local quadratic fit of samples `v(t)`, `algos4._fit_run`.
/// Returns (fitted, slope, curvature, rms residual).
/// Fitted curve, slope, curvature, and RMS residual from [`fit_run`].
type FitResult = (Vec<f64>, Vec<f64>, Vec<f64>, f64);

fn fit_run(samples: &[f64]) -> Option<FitResult> {
    let len = samples.len();
    let win = std::cmp::min(11, if len % 2 == 1 { len } else { len - 1 });
    if win < 7 {
        return None;
    }
    let (kv, kd, k2) = sg_kernels(win)?;
    let half = win / 2;
    // pad samples by edge replication
    let mut vp = Vec::with_capacity(len + 2 * half);
    for _ in 0..half {
        vp.push(samples[0]);
    }
    vp.extend_from_slice(samples);
    for _ in 0..half {
        vp.push(samples[len - 1]);
    }
    let conv = |kernel: &[f64]| -> Vec<f64> {
        let mut out = vec![0.0; len];
        for i in 0..len {
            let mut acc = 0.0;
            for j in 0..win {
                acc += kernel[j] * vp[i + j];
            }
            out[i] = acc;
        }
        out
    };
    let fitted = conv(&kv);
    let slope = conv(&kd);
    let c2 = conv(&k2);
    let len_f = f64::from(u32::try_from(len).ok()?);
    let rms = (samples
        .iter()
        .zip(fitted.iter())
        .map(|(a, b)| (a - b).powi(2))
        .sum::<f64>()
        / len_f)
        .sqrt();
    Some((fitted, slope, c2, rms))
}

// ---------------------------------------------------------------- runs
struct Run {
    t: Vec<i64>,
    v: Vec<f64>,
    f: Vec<f64>,
    ok: Vec<bool>,
}

struct RawRun {
    t: Vec<i64>,
    v: Vec<f64>,
    a: [u8; 3],
    last_y: f64,
}

/// Collect raw boundary runs along one axis: a run is a chain of
/// neighbouring high-contrast diffs of one colour, tracked across
/// columns (`axis` 0) or rows (`axis` 1). Part of `find_runs2`.
fn extract_runs(src: &UImg, axis: usize, min_contrast: f32) -> Vec<RawRun> {
    let (n_along, n_across) = if axis == 0 {
        (src.w, src.h - 1)
    } else {
        (src.h, src.w - 1)
    };
    // diff, Aarr, Barr as closures over (along index j, across index i)
    let diff_at = |j: usize, i: usize| -> f32 {
        if axis == 0 {
            let p0 = src.getf(i, j);
            let p1 = src.getf(i + 1, j);
            ((p1[0] - p0[0]).powi(2) + (p1[1] - p0[1]).powi(2) + (p1[2] - p0[2]).powi(2)).sqrt()
        } else {
            let p0 = src.getf(j, i);
            let p1 = src.getf(j, i + 1);
            ((p1[0] - p0[0]).powi(2) + (p1[1] - p0[1]).powi(2) + (p1[2] - p0[2]).powi(2)).sqrt()
        }
    };
    let col_at = |j: usize, i: usize| -> [u8; 3] {
        if axis == 0 {
            src.get(i, j)
        } else {
            src.get(j, i)
        }
    };
    let mut active: Vec<RawRun> = Vec::new();
    let mut done: Vec<RawRun> = Vec::new();
    for j in 0..n_along {
        let Ok(j64) = i64::try_from(j) else { break };
        let mut used: Vec<bool> = vec![false; active.len()];
        // Boundary position of across-index i, counted in f64 so it
        // stays exact: pos is i + 1 at the top of each iteration.
        let mut pos = 0.0f64;
        for i in 0..n_across {
            pos += 1.0;
            let diff = diff_at(j, i);
            if diff <= 0.0 {
                continue;
            }
            let col = col_at(j, i);
            if f64::from(diff) < f64::from(min_contrast) {
                continue;
            }
            let mut best: Option<usize> = None;
            let mut bestd = 1.5f64;
            for (k, r) in active.iter().enumerate() {
                if used[k] || r.a != col {
                    continue;
                }
                let dd = (pos - r.last_y).abs();
                if dd < bestd {
                    best = Some(k);
                    bestd = dd;
                }
            }
            match best {
                None => {
                    active.push(RawRun {
                        t: vec![j64],
                        v: vec![pos],
                        a: col,
                        last_y: pos,
                    });
                    used.push(false);
                }
                Some(k) => {
                    active[k].t.push(j64);
                    active[k].v.push(pos);
                    active[k].last_y = pos;
                    used[k] = true;
                }
            }
        }
        let mut still: Vec<RawRun> = Vec::new();
        for r in active {
            match r.t.last() {
                Some(&last) if last == j64 => still.push(r),
                _ => done.push(r),
            }
        }
        active = still;
    }
    done.extend(active);
    done
}

/// Fit raw runs and keep the coherent ones: long enough, low fit
/// residual, bounded slope and curvature (apex included). Part of
/// `find_runs2`.
/// Monotonicity gate: one slope sign change allowed per 7-sample window.
/// Returns, per sample, whether the window around that sample changes
/// direction at most once.
fn monotonicity_ok(values: &[f64]) -> Vec<bool> {
    let len = values.len();
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let lo = i.saturating_sub(3);
        let hi = std::cmp::min(len, i + 4);
        let mut changes = 0i32;
        let mut prev: Option<i8> = None;
        for k in lo..hi - 1 {
            let delta = values[k + 1] - values[k];
            if delta == 0.0 {
                continue;
            }
            let direction: i8 = if delta > 0.0 { 1 } else { -1 };
            if let Some(prev_sign) = prev
                && direction != prev_sign
            {
                changes += 1;
            }
            prev = Some(direction);
        }
        out.push(changes <= 1);
    }
    out
}

fn finish_runs(
    raw: Vec<RawRun>,
    min_len: usize,
    max_slope: f64,
    max_rms: f64,
    max_curv: f64,
    max_curv_apex: f64,
) -> Vec<Run> {
    let mut out = Vec::new();
    for r in raw {
        if r.t.len() < min_len {
            continue;
        }
        let times = r.t;
        let values = r.v;
        let Some((fitted, slope, curv2, rms)) = fit_run(&values) else {
            continue;
        };
        if rms > max_rms {
            continue;
        }
        let len = values.len();
        let mut ok: Vec<bool> = slope.iter().map(|s| s.abs() <= max_slope).collect();
        // apex: slope sign changes mark a peak/valley
        let sgn: Vec<i8> = slope
            .iter()
            .map(|s| {
                if *s > 0.0 {
                    1
                } else if *s < 0.0 {
                    -1
                } else {
                    0
                }
            })
            .collect();
        let mut apex = vec![false; len];
        for i in 0..len - 1 {
            if sgn[i + 1] != sgn[i] {
                let lo = i.saturating_sub(1);
                let hi = std::cmp::min(len, i + 3);
                apex[lo..hi].fill(true);
            }
        }
        for i in 0..len {
            let curv_ok =
                curv2[i].abs() <= max_curv || (apex[i] && curv2[i].abs() <= max_curv_apex);
            ok[i] = ok[i] && curv_ok;
        }
        // monotonicity with one sign change allowed per 7-sample window
        let mono = monotonicity_ok(&values);
        for i in 0..len {
            ok[i] = ok[i] && mono[i];
        }
        if ok.iter().filter(|b| **b).count() < min_len {
            continue;
        }
        if std::env::var("CONTOUR_DEBUG_RUNS").is_ok() {
            eprintln!(
                "run t {}..{} len {} v {:.2}..{:.2} rms {:.3} ok {}",
                times[0],
                times[times.len() - 1],
                times.len(),
                values.iter().copied().fold(f64::INFINITY, f64::min),
                values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
                rms,
                ok.iter().filter(|b| **b).count()
            );
            if times[0] == 0 && times[times.len() - 1] == 242 {
                eprintln!(
                    "rs ok: {}",
                    ok.iter()
                        .map(|b| if *b { '1' } else { '0' })
                        .collect::<String>()
                );
                eprintln!(
                    "rs c2: {:?}",
                    curv2
                        .iter()
                        .take(20)
                        .map(|cv| (cv * 10000.0).round() / 10000.0)
                        .collect::<Vec<_>>()
                );
            }
        }
        out.push(Run {
            t: times,
            v: values,
            f: fitted,
            ok,
        });
    }
    out
}

/// Coherent boundary runs, `algos5.find_runs2` (apex + scaled `min_len`).
fn find_runs2(src: &UImg) -> (Vec<Run>, Vec<Run>) {
    let min_len = if std::cmp::min(src.h, src.w) >= 200 {
        24usize
    } else {
        12usize
    };
    let min_contrast = 50.0f32;
    let max_slope = 0.75f64;
    let max_rms = 0.45f64;
    let max_curv = 0.02f64;
    let max_curv_apex = 0.06f64;
    (
        finish_runs(
            extract_runs(src, 0, min_contrast),
            min_len,
            max_slope,
            max_rms,
            max_curv,
            max_curv_apex,
        ),
        finish_runs(
            extract_runs(src, 1, min_contrast),
            min_len,
            max_slope,
            max_rms,
            max_curv,
            max_curv_apex,
        ),
    )
}

/// Sample the source profile along `axis` at line `idx` and continuous
/// coordinates `coords`, `algos4._profile_sample_vec`. Transition width
/// 0.25 source px, centred on the row/column boundary, smoothstep.
///
/// The sample index is the floor of `cc - 0.5`, clamped to `[0, n - 2]`,
/// found by [`clamp_floor_u32`] so it stays a usize by construction;
/// the fraction is `cc - 0.5` minus that floor as an f64, so the
/// fraction uses the unclamped floor, as in the reference.
fn profile_sample_vec(src: &UImg, axis: usize, idx: usize, coords: &[f64]) -> Vec<[f32; 3]> {
    let len = if axis == 0 { src.h } else { src.w };
    let Ok(len_u32) = u32::try_from(len) else {
        return Vec::new();
    };
    let col = |i: usize| -> [f32; 3] {
        let clamped = i.min(len.saturating_sub(1));
        if axis == 0 {
            src.getf(clamped, idx)
        } else {
            src.getf(idx, clamped)
        }
    };
    coords
        .iter()
        .map(|&cc| {
            let floor_f = (cc - 0.5).floor();
            let frac = cc - 0.5 - floor_f;
            let pos = clamp_floor_u32(floor_f, len_u32.saturating_sub(2)) as usize;
            let mut weight = clamp_f(((frac - 0.375) / 0.25) as f32, 0.0, 1.0);
            weight = weight * weight * (3.0 - 2.0 * weight);
            let p0 = col(pos);
            let p1 = col(pos + 1);
            [
                (1.0 - weight) * p0[0] + weight * p1[0],
                (1.0 - weight) * p0[1] + weight * p1[1],
                (1.0 - weight) * p0[2] + weight * p1[2],
            ]
        })
        .collect()
}

fn interp(x: f64, xs: &[i64], ys: &[f64]) -> Option<f64> {
    // np.interp: linear, clamped at the ends. The knots are integer
    // column indices; each converts to f64 exactly below 2^31, and
    // beyond that there is no interpolation to do.
    let (&x_first_i, &y_first) = (xs.first()?, ys.first()?);
    let x_first = f64::from(i32::try_from(x_first_i).ok()?);
    if x <= x_first {
        return Some(y_first);
    }
    let (&x_last_i, &y_last) = (xs.last()?, ys.last()?);
    let x_last = f64::from(i32::try_from(x_last_i).ok()?);
    if x >= x_last {
        return Some(y_last);
    }
    let mut lo = 0usize;
    let mut hi = xs.len() - 1;
    while hi - lo > 1 {
        let mid = lo.midpoint(hi);
        let x_mid = f64::from(i32::try_from(xs[mid]).ok()?);
        if x_mid <= x {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let x_lo = f64::from(i32::try_from(xs[lo]).ok()?);
    let x_hi = f64::from(i32::try_from(xs[hi]).ok()?);
    let t = (x - x_lo) / (x_hi - x_lo);
    Some(ys[lo] + t * (ys[hi] - ys[lo]))
}

struct Stats {
    h_runs: usize,
    v_runs: usize,
    h_len: usize,
    v_len: usize,
    touched_frac: f64,
}

/// One fitted run, sampled at the output columns of source column `u`:
/// the output columns, the fitted positions there, and the gate weights.
struct RunSlice {
    outs: Vec<usize>,
    fitted: Vec<f64>,
    weights: Vec<f64>,
}

/// Shared state for re-rendering runs: the source, the output being
/// written, and the per-pixel best-distance/touched maps. Grouped so
/// the per-run helpers stay under the argument limit.
struct FollowCtx<'a> {
    src: &'a UImg,
    out: &'a mut FImg,
    best: &'a mut [f32],
    touched: &'a mut [bool],
}

/// Sample `run` at the output columns inside source column `u`.
/// The sample coordinates are `(u + (j + 0.5) / scale)` for the
/// subpixel offsets `j`, computed in f64 from f64 parts, so no cast
/// is needed. Returns `None` when any interpolation falls outside
/// the run, which ends the run.
fn run_slice(
    run: &Run,
    u_idx: usize,
    u_f: f64,
    scale: usize,
    scale_f: f64,
    weights: &[f32],
) -> Option<RunSlice> {
    let times = &run.t;
    let outs: Vec<usize> = (u_idx * scale..(u_idx + 1) * scale).collect();
    let mut coords = Vec::with_capacity(scale);
    let mut jf = 0.0f64;
    for _ in 0..scale {
        coords.push((u_f * scale_f + jf + 0.5) / scale_f);
        jf += 1.0;
    }
    let fitted = coords
        .iter()
        .map(|&uu| interp(uu, times, &run.f))
        .collect::<Option<Vec<f64>>>()?;
    let weights_f64: Vec<f64> = weights.iter().map(|v| f64::from(*v)).collect();
    let wgt = coords
        .iter()
        .map(|&uu| interp(uu, times, &weights_f64))
        .collect::<Option<Vec<f64>>>()?;
    Some(RunSlice {
        outs,
        fitted,
        weights: wgt,
    })
}

/// Re-render one run slice across output rows (horizontal runs).
/// Part of `edge_follow2`.
fn apply_horizontal(
    ctx: &mut FollowCtx<'_>,
    slice: &RunSlice,
    u_idx: usize,
    v_u: f64,
    scale_f: f64,
    out_h: usize,
) {
    let band = 2.5f64;
    let Ok(out_h_u32) = u32::try_from(out_h) else {
        return;
    };
    let fitted = &slice.fitted;
    let weights = &slice.weights;
    let fmin = fitted.iter().copied().fold(f64::INFINITY, f64::min);
    let fmax = fitted.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let oy0_u32 = clamp_floor_u32((fmin - band) * scale_f, out_h_u32);
    let oy1_u32 = clamp_floor_u32((fmax + band) * scale_f + 1.0, out_h_u32);
    let oy0 = oy0_u32 as usize;
    let oy1 = oy1_u32 as usize;
    if oy0 >= oy1 {
        return;
    }
    let mut ys = Vec::with_capacity(oy1 - oy0);
    let mut yf = f64::from(oy0_u32);
    let oy1_f = f64::from(oy1_u32);
    while yf < oy1_f {
        ys.push((yf + 0.5) / scale_f);
        yf += 1.0;
    }
    for (j, &ox) in slice.outs.iter().enumerate() {
        if weights[j] <= 0.01 {
            continue;
        }
        let shift = (fitted[j] - v_u) * weights[j];
        let mut keep_idx: Vec<usize> = Vec::new();
        for (yi, &yy) in ys.iter().enumerate() {
            let dist = (yy - fitted[j]).abs();
            if dist <= band && (dist as f32) < ctx.best[(oy0 + yi) * ctx.out.w + ox] {
                keep_idx.push(yi);
            }
        }
        if keep_idx.is_empty() {
            continue;
        }
        let coords: Vec<f64> = keep_idx.iter().map(|&yi| ys[yi] - shift).collect();
        let vals = profile_sample_vec(ctx.src, 0, u_idx, &coords);
        for (vi, &yi) in keep_idx.iter().enumerate() {
            let oy = oy0 + yi;
            let cur = ctx.out.get(oy, ox);
            let wj = weights[j] as f32;
            ctx.out.set(
                oy,
                ox,
                [
                    (1.0 - wj) * cur[0] + wj * vals[vi][0],
                    (1.0 - wj) * cur[1] + wj * vals[vi][1],
                    (1.0 - wj) * cur[2] + wj * vals[vi][2],
                ],
            );
            ctx.best[oy * ctx.out.w + ox] = (ys[yi] - fitted[j]).abs() as f32;
            ctx.touched[oy * ctx.out.w + ox] = true;
        }
    }
}

/// Re-render one run slice across output columns (vertical runs).
/// Part of `edge_follow2`.
fn apply_vertical(
    ctx: &mut FollowCtx<'_>,
    slice: &RunSlice,
    u_idx: usize,
    v_u: f64,
    scale_f: f64,
    out_w: usize,
) {
    let band = 2.5f64;
    let Ok(out_w_u32) = u32::try_from(out_w) else {
        return;
    };
    let fitted = &slice.fitted;
    let weights = &slice.weights;
    let fmin = fitted.iter().copied().fold(f64::INFINITY, f64::min);
    let fmax = fitted.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let ox0_u32 = clamp_floor_u32((fmin - band) * scale_f, out_w_u32);
    let ox1_u32 = clamp_floor_u32((fmax + band) * scale_f + 1.0, out_w_u32);
    let ox0 = ox0_u32 as usize;
    let ox1 = ox1_u32 as usize;
    if ox0 >= ox1 {
        return;
    }
    let mut xs = Vec::with_capacity(ox1 - ox0);
    let mut xf = f64::from(ox0_u32);
    let ox1_f = f64::from(ox1_u32);
    while xf < ox1_f {
        xs.push((xf + 0.5) / scale_f);
        xf += 1.0;
    }
    for (j, &oy) in slice.outs.iter().enumerate() {
        if weights[j] <= 0.01 {
            continue;
        }
        let shift = (fitted[j] - v_u) * weights[j];
        let mut keep_idx: Vec<usize> = Vec::new();
        for (xi, &xx) in xs.iter().enumerate() {
            let dist = (xx - fitted[j]).abs();
            if dist <= band && (dist as f32) < ctx.best[oy * ctx.out.w + (ox0 + xi)] {
                keep_idx.push(xi);
            }
        }
        if keep_idx.is_empty() {
            continue;
        }
        let coords: Vec<f64> = keep_idx.iter().map(|&xi| xs[xi] - shift).collect();
        let vals = profile_sample_vec(ctx.src, 1, u_idx, &coords);
        for (vi, &xi) in keep_idx.iter().enumerate() {
            let ox = ox0 + xi;
            let cur = ctx.out.get(oy, ox);
            let wj = weights[j] as f32;
            ctx.out.set(
                oy,
                ox,
                [
                    (1.0 - wj) * cur[0] + wj * vals[vi][0],
                    (1.0 - wj) * cur[1] + wj * vals[vi][1],
                    (1.0 - wj) * cur[2] + wj * vals[vi][2],
                ],
            );
            ctx.best[oy * ctx.out.w + ox] = (xs[xi] - fitted[j]).abs() as f32;
            ctx.touched[oy * ctx.out.w + ox] = true;
        }
    }
}

/// Re-render one run at its fitted positions. Part of `edge_follow2`.
fn apply_run(
    run: &Run,
    axis: usize,
    scale: usize,
    out_h: usize,
    out_w: usize,
    ctx: &mut FollowCtx<'_>,
) {
    let Ok(scale_u32) = u32::try_from(scale) else {
        return;
    };
    let scale_f = f64::from(scale_u32);
    let times = &run.t;
    let Some(&t_last) = times.last() else { return };
    let weights = uniform1d_nearest(
        &run.ok
            .iter()
            .map(|b| if *b { 1.0f32 } else { 0.0 })
            .collect::<Vec<_>>(),
    );
    let t0 = times[0];
    let t1 = t_last;
    for u in t0..=t1 {
        let Ok(u_u32) = u32::try_from(u) else {
            continue;
        };
        let u_idx = u_u32 as usize;
        let u_f = f64::from(u_u32);
        // searchsorted left, clamped
        let mut idx = times.partition_point(|&tv| tv < u);
        if idx >= times.len() {
            idx = times.len() - 1;
        }
        if !run.ok[idx] && weights[idx] <= 0.01 {
            continue;
        }
        let v_u = run.v[idx];
        let Some(slice) = run_slice(run, u_idx, u_f, scale, scale_f, &weights) else {
            return;
        };
        if axis == 0 {
            apply_horizontal(ctx, &slice, u_idx, v_u, scale_f, out_h);
        } else {
            apply_vertical(ctx, &slice, u_idx, v_u, scale_f, out_w);
        }
    }
}

/// `edge_follow2`, `algos5.edge_follow2`: re-render coherent runs at
/// their fitted subpixel positions by shifting the source profile.
fn edge_follow2(base: &FImg, src: &UImg, scale: usize) -> (FImg, Stats) {
    let (height, width) = (src.h, src.w);
    let mut out = base.clone();
    let (h_runs, v_runs) = find_runs2(src);
    let horizontal_runs = h_runs.len();
    let vertical_runs = v_runs.len();
    let horizontal_len: usize = h_runs.iter().map(|r| r.t.len()).sum();
    let vertical_len: usize = v_runs.iter().map(|r| r.t.len()).sum();
    // Best (closest) distance per output pixel, in f32: the
    // comparison rounds the distance to f32 first, and that rounding
    // decides which run wins a near-tie. The regression test pins
    // this: mario's long hill-boundary runs compete at a near-tie,
    // and the f32 comparison is the reference port's behaviour.
    let mut best = vec![f32::INFINITY; width * scale * height * scale];
    let mut touched = vec![false; width * scale * height * scale];
    let mut ctx = FollowCtx {
        src,
        out: &mut out,
        best: &mut best,
        touched: &mut touched,
    };
    for run in &h_runs {
        apply_run(run, 0, scale, height * scale, width * scale, &mut ctx);
    }
    for run in &v_runs {
        apply_run(run, 1, scale, height * scale, width * scale, &mut ctx);
    }
    let (touched_count, total_count) = touched.iter().fold((0u32, 0u32), |(count, total), b| {
        (count + u32::from(*b), total + 1)
    });
    let touched_frac = f64::from(touched_count) / f64::from(total_count);
    (
        out,
        Stats {
            h_runs: horizontal_runs,
            v_runs: vertical_runs,
            h_len: horizontal_len,
            v_len: vertical_len,
            touched_frac,
        },
    )
}

/// 20: 12 combined, then `edge_follow2`. No dither source anywhere.
/// `CONTOUR_NO_FOLLOW`=1 skips the follower (12 only), for testing.
fn clean_base_edge(a: &UImg, s: usize) -> (FImg, Stats) {
    let base = combined(a, s);
    if std::env::var("CONTOUR_NO_FOLLOW").is_ok() {
        return (
            base,
            Stats {
                h_runs: 0,
                v_runs: 0,
                h_len: 0,
                v_len: 0,
                touched_frac: 0.0,
            },
        );
    }
    edge_follow2(&base, a, s)
}

// ---------------------------------------------------------------- adapter
/// Algorithm 20 as a [`Processor`]: 12 (combined) + `edge_follow2`, no
/// dither source anywhere. Input must be RGB8; output is RGB8 at
/// `width * scale` by `height * scale`.
pub struct Contour;

impl Processor for Contour {
    fn name(&self) -> &'static str {
        "contour"
    }

    fn supported_scales(&self) -> &[u32] {
        &[2, 4]
    }

    fn process(&self, frame: &Frame, scale: u32) -> Result<Frame, ProcessError> {
        if !self.supported_scales().contains(&scale) {
            return Err(ProcessError::UnsupportedScale {
                algorithm: "contour",
                scale,
            });
        }
        if frame.format != PixelFormat::Rgb8 {
            return Err(ProcessError::UnsupportedFormat {
                algorithm: "contour",
                format: frame.format.to_string(),
            });
        }
        // Output dimensions are frame dims times the scale; fail
        // early with an honest error instead of truncating.
        let out_width = frame.width.checked_mul(scale).ok_or_else(|| {
            ProcessError::Internal("contour output width exceeds u32 range".into())
        })?;
        let out_height = frame.height.checked_mul(scale).ok_or_else(|| {
            ProcessError::Internal("contour output height exceeds u32 range".into())
        })?;
        let src = UImg {
            w: frame.width as usize,
            h: frame.height as usize,
            d: frame.data.clone(),
        };
        let (out, stats) = clean_base_edge(&src, scale as usize);
        if std::env::var("CONTOUR_DEBUG_RUNS").is_ok() {
            eprintln!(
                "edge runs: {} horizontal ({} px), {} vertical ({} px), touched {:.1}% of output",
                stats.h_runs,
                stats.h_len,
                stats.v_runs,
                stats.v_len,
                stats.touched_frac * 100.0
            );
        }
        Ok(Frame {
            width: out_width,
            height: out_height,
            format: PixelFormat::Rgb8,
            data: to_u8_img(&out),
        })
    }
}
