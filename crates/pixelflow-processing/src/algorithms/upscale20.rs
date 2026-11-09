//! Algorithm 20 from the retro upscaler experiments, as a library.
//!
//! 20 = 12 (combined) + edge_follow2, with no dither source anywhere.
//! The code below is the standalone Rust port from
//! `upscale20/src/main.rs`, moved here verbatim: the mathematical
//! operations, float widths, gates, and the hardcoded Savitzky-Golay
//! kernel tables (numpy pinv values) are unchanged, so its output is
//! identical to the standalone program. Do not simplify, redesign, or
//! silently alter it during integration. The only I/O dependency in
//! the original program was the `png` crate, and that lives in the
//! CLI now; this module is pure std plus pixelflow-core types.
//!
//! Testing switches, kept from the port: `UPSCALE20_NO_FOLLOW=1`
//! skips the edge follower (12 only); `UPSCALE20_DEBUG_RUNS=1`
//! prints each accepted run to stderr.

use std::env;

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
        [p[0] as f32, p[1] as f32, p[2] as f32]
    }
}

impl FImg {
    fn new(w: usize, h: usize) -> Self {
        FImg { w, h, d: vec![0.0; w * h * 3] }
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

fn clamp_i(v: i64, lo: i64, hi: i64) -> i64 {
    if v < lo {
        lo
    } else if v > hi {
        hi
    } else {
        v
    }
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

/// scipy 'reflect' index: (d c b a | a b c d | d c b a)
fn reflect_idx(mut i: i64, n: i64) -> i64 {
    if n <= 1 {
        return 0;
    }
    loop {
        if i < 0 {
            i = -i - 1;
        } else if i >= n {
            i = 2 * n - 1 - i;
        } else {
            return i;
        }
    }
}

/// shifted(a, dy, dx): a[y+dy][x+dx] with edge clamping, like algos1.shifted.
fn shifted_u(a: &UImg, dy: i64, dx: i64) -> UImg {
    let mut out = UImg { w: a.w, h: a.h, d: vec![0; a.d.len()] };
    for y in 0..a.h {
        let sy = clamp_i(y as i64 + dy, 0, a.h as i64 - 1) as usize;
        for x in 0..a.w {
            let sx = clamp_i(x as i64 + dx, 0, a.w as i64 - 1) as usize;
            let p = a.get(sy, sx);
            let i = (y * a.w + x) * 3;
            out.d[i] = p[0];
            out.d[i + 1] = p[1];
            out.d[i + 2] = p[2];
        }
    }
    out
}

fn shifted_b(a: &[bool], w: usize, h: usize, dy: i64, dx: i64) -> Vec<bool> {
    let mut out = vec![false; w * h];
    for y in 0..h {
        let sy = clamp_i(y as i64 + dy, 0, h as i64 - 1) as usize;
        for x in 0..w {
            let sx = clamp_i(x as i64 + dx, 0, w as i64 - 1) as usize;
            out[y * w + x] = a[sy * w + sx];
        }
    }
    out
}

fn color_ids(a: &UImg) -> Vec<u32> {
    let mut out = vec![0u32; a.w * a.h];
    for i in 0..a.w * a.h {
        out[i] = ((a.d[i * 3] as u32) << 16) | ((a.d[i * 3 + 1] as u32) << 8) | a.d[i * 3 + 2] as u32;
    }
    out
}

fn to_u8_img(x: &FImg) -> Vec<u8> {
    x.d.iter().map(|v| clamp_f(v.round(), 0.0, 255.0) as u8).collect()
}

// ---------------------------------------------------------------- erf / taps
/// Abramowitz-Stegun 7.1.26, |error| < 1.5e-7. std has no erf.
fn erf(x: f64) -> f64 {
    let (s, ax) = if x < 0.0 { (-1.0, -x) } else { (1.0, x) };
    let t = 1.0 / (1.0 + 0.3275911 * ax);
    let poly = (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t
        + 0.254829592)
        * t;
    s * (1.0 - poly * (-ax * ax).exp())
}

fn phi(x: f64) -> f64 {
    0.5 * (1.0 + erf(x / std::f64::consts::SQRT_2))
}

/// Gaussian coverage taps over ks = [-1, 0, 1], like algos1._taps.
fn taps(u: f32, sig: f32) -> [f32; 3] {
    let ks = [-1.0f64, 0.0, 1.0];
    let mut w = [0.0f64; 3];
    let mut sum = 0.0;
    for (i, k) in ks.iter().enumerate() {
        w[i] = phi((k + 0.5 - u as f64) / sig as f64) - phi((k - 0.5 - u as f64) / sig as f64);
        sum += w[i];
    }
    [ (w[0] / sum) as f32, (w[1] / sum) as f32, (w[2] / sum) as f32 ]
}

// ---------------------------------------------------------------- blur
/// Separable gaussian blur, scipy gaussian_filter semantics:
/// sigma, truncate 4.0 (radius ceil(4*sigma)), mode 'reflect', per axis.
fn gaussian_blur(a: &FImg, sigma: f32) -> FImg {
    if sigma <= 0.0 {
        return a.clone();
    }
    let r = (4.0 * sigma).ceil() as i64;
    let mut kern = Vec::new();
    let mut sum = 0.0f64;
    for k in -r..=r {
        let v = (-0.5 * (k as f64 / sigma as f64).powi(2)).exp();
        kern.push(v);
        sum += v;
    }
    for v in kern.iter_mut() {
        *v /= sum;
    }
    let mut tmp = FImg::new(a.w, a.h);
    for y in 0..a.h {
        for x in 0..a.w {
            let mut acc = [0.0f64; 3];
            for (ki, k) in (-r..=r).enumerate() {
                let sx = reflect_idx(x as i64 + k, a.w as i64) as usize;
                let p = a.get(y, sx);
                let wgt = kern[ki];
                for c in 0..3 {
                    acc[c] += wgt * p[c] as f64;
                }
            }
            tmp.set(y, x, [acc[0] as f32, acc[1] as f32, acc[2] as f32]);
        }
    }
    let mut out = FImg::new(a.w, a.h);
    for y in 0..a.h {
        for x in 0..a.w {
            let mut acc = [0.0f64; 3];
            for (ki, k) in (-r..=r).enumerate() {
                let sy = reflect_idx(y as i64 + k, a.h as i64) as usize;
                let p = tmp.get(sy, x);
                let wgt = kern[ki];
                for c in 0..3 {
                    acc[c] += wgt * p[c] as f64;
                }
            }
            out.set(y, x, [acc[0] as f32, acc[1] as f32, acc[2] as f32]);
        }
    }
    out
}

/// 3x3 box blur, scipy uniform_filter size (3,3,1), mode 'reflect'.
fn uniform3(a: &FImg) -> FImg {
    let mut tmp = FImg::new(a.w, a.h);
    for y in 0..a.h {
        for x in 0..a.w {
            let mut acc = [0.0f64; 3];
            for k in -1..=1i64 {
                let sx = reflect_idx(x as i64 + k, a.w as i64) as usize;
                let p = a.get(y, sx);
                for c in 0..3 {
                    acc[c] += p[c] as f64 / 3.0;
                }
            }
            tmp.set(y, x, [acc[0] as f32, acc[1] as f32, acc[2] as f32]);
        }
    }
    let mut out = FImg::new(a.w, a.h);
    for y in 0..a.h {
        for x in 0..a.w {
            let mut acc = [0.0f64; 3];
            for k in -1..=1i64 {
                let sy = reflect_idx(y as i64 + k, a.h as i64) as usize;
                let p = tmp.get(sy, x);
                for c in 0..3 {
                    acc[c] += p[c] as f64 / 3.0;
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
    let n = v.len() as i64;
    let mut out = vec![0.0f32; v.len()];
    for i in 0..v.len() {
        let mut s = 0.0f32;
        for k in -2..=2i64 {
            s += v[clamp_i(i as i64 + k, 0, n - 1) as usize];
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

/// Scale2x / EPX, one doubling, like algos1.scale2x.
fn scale2x(a: &UImg) -> UImg {
    let ids = color_ids(a);
    let id_at = |y: i64, x: i64| -> u32 {
        let sy = clamp_i(y, 0, a.h as i64 - 1) as usize;
        let sx = clamp_i(x, 0, a.w as i64 - 1) as usize;
        ids[sy * a.w + sx]
    };
    let mut out = UImg { w: a.w * 2, h: a.h * 2, d: vec![0; a.w * 2 * a.h * 2 * 3] };
    for y in 0..a.h {
        for x in 0..a.w {
            let (yy, xx) = (y as i64, x as i64);
            let b = id_at(yy - 1, xx);
            let h_ = id_at(yy + 1, xx);
            let d = id_at(yy, xx - 1);
            let f = id_at(yy, xx + 1);
            let e = ids[y * a.w + x];
            let ad = a.getf(y, (xx - 1).max(0) as usize);
            let af = a.getf(y, (xx + 1).min(a.w as i64 - 1) as usize);
            let centre = a.getf(y, x);
            let c0 = d == b && b != f && d != h_;
            let c1 = b == f && b != d && f != h_;
            let c2 = d == h_ && d != b && h_ != f;
            let c3 = h_ == f && d != h_ && b != f;
            let _ = e;
            let mut q = |dy: usize, dx: usize, p: [f32; 3]| {
                let i = ((y * 2 + dy) * out.w + (x * 2 + dx)) * 3;
                out.d[i] = clamp_f(p[0].round(), 0.0, 255.0) as u8;
                out.d[i + 1] = clamp_f(p[1].round(), 0.0, 255.0) as u8;
                out.d[i + 2] = clamp_f(p[2].round(), 0.0, 255.0) as u8;
            };
            q(0, 0, if c0 { ad } else { centre });
            q(0, 1, if c1 { af } else { centre });
            q(1, 0, if c2 { ad } else { centre });
            q(1, 1, if c3 { af } else { centre });
        }
    }
    out
}

fn scalenx(a: &UImg, s: usize) -> UImg {
    let n = (s as f64).log2() as usize;
    assert_eq!(1usize << n, s, "scale must be a power of two");
    let mut cur = a.clone();
    for _ in 0..n {
        cur = scale2x(&cur);
    }
    cur
}

// ---------------------------------------------------------------- 07 dither
/// Pair-consistent 2x2 checker mask, algos2.dither_mask. Fires only
/// where a checker cell's colour pair repeats 2 px away.
fn dither_mask(a: &UImg) -> Vec<bool> {
    let ids = color_ids(a);
    let (w, h) = (a.w, a.h);
    let id_at = |y: i64, x: i64| -> u32 {
        let sy = clamp_i(y, 0, h as i64 - 1) as usize;
        let sx = clamp_i(x, 0, w as i64 - 1) as usize;
        ids[sy * w + sx]
    };
    let mut checker = vec![false; w * h];
    let mut key = vec![-1i64; w * h];
    for y in 0..h {
        for x in 0..w {
            let tl = id_at(y as i64, x as i64);
            let tr = id_at(y as i64, x as i64 + 1);
            let bl = id_at(y as i64 + 1, x as i64);
            let br = id_at(y as i64 + 1, x as i64 + 1);
            let c = tl == br && tr == bl && tl != tr;
            checker[y * w + x] = c;
            if c {
                let lo = tl.min(tr) as i64;
                let hi = tl.max(tr) as i64;
                key[y * w + x] = lo * (1i64 << 24) + hi;
            }
        }
    }
    let mut same = vec![false; w * h];
    for (dy, dx) in [(0i64, 2i64), (0, -2), (2, 0), (-2, 0)] {
        let k2 = shifted_i64(&key, w, h, dy, dx);
        for i in 0..w * h {
            if checker[i] && key[i] == k2[i] && k2[i] >= 0 {
                same[i] = true;
            }
        }
    }
    let mut mask = vec![false; w * h];
    for dy in 0..2i64 {
        for dx in 0..2i64 {
            let s = shifted_b(&same, w, h, -dy, -dx);
            for i in 0..w * h {
                mask[i] |= s[i];
            }
        }
    }
    mask
}

fn shifted_i64(a: &[i64], w: usize, h: usize, dy: i64, dx: i64) -> Vec<i64> {
    let mut out = vec![0i64; w * h];
    for y in 0..h {
        let sy = clamp_i(y as i64 + dy, 0, h as i64 - 1) as usize;
        for x in 0..w {
            let sx = clamp_i(x as i64 + dx, 0, w as i64 - 1) as usize;
            out[y * w + x] = a[sy * w + sx];
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
const OFFS: [(i64, i64); 9] = [
    (-1, -1), (-1, 0), (-1, 1),
    (0, -1), (0, 0), (0, 1),
    (1, -1), (1, 0), (1, 1),
];

/// 08: contrast-adaptive sharp labelset, algos2.labelset_adaptive.
fn labelset_adaptive(a: &UImg, s: usize) -> FImg {
    let sig = 0.35f32;
    let (w, h) = (a.w, a.h);
    // 9 shifted colour/id planes
    let mut planes_c: Vec<FImg> = Vec::with_capacity(9);
    let mut planes_id: Vec<Vec<u32>> = Vec::with_capacity(9);
    for (dy, dx) in OFFS {
        planes_c.push({
            let sh = shifted_u(a, dy, dx);
            let mut f = FImg::new(w, h);
            for i in 0..w * h {
                f.d[i * 3] = sh.d[i * 3] as f32;
                f.d[i * 3 + 1] = sh.d[i * 3 + 1] as f32;
                f.d[i * 3 + 2] = sh.d[i * 3 + 2] as f32;
            }
            f
        });
        planes_id.push(color_ids(&shifted_u(a, dy, dx)));
    }
    let mut out = FImg::new(w * s, h * s);
    let kbase = 1.04 * sig * s as f32;
    for iy in 0..s {
        let wy = taps((iy as f32 + 0.5) / s as f32 - 0.5, sig);
        for ix in 0..s {
            let wx = taps((ix as f32 + 0.5) / s as f32 - 0.5, sig);
            // tap weight w[k] = wy[dy+1] * wx[dx+1]
            let mut tw = [0.0f32; 9];
            for (k, (dy, dx)) in OFFS.iter().enumerate() {
                tw[k] = wy[(dy + 1) as usize] * wx[(dx + 1) as usize];
            }
            for y in 0..h {
                for x in 0..w {
                    // L[i] = sum_j tw[j] where ids[j] == ids[i]
                    let mut l = [0.0f32; 9];
                    for i in 0..9 {
                        let mut acc = 0.0f32;
                        for j in 0..9 {
                            if planes_id[j][y * w + x] == planes_id[i][y * w + x] {
                                acc += tw[j];
                            }
                        }
                        l[i] = acc;
                    }
                    let mut i1 = 0;
                    for i in 1..9 {
                        if l[i] > l[i1] {
                            i1 = i;
                        }
                    }
                    let l1 = l[i1];
                    let c1 = planes_c[i1].get(y, x);
                    let id1 = planes_id[i1][y * w + x];
                    // runner-up: best L among taps whose id != id1
                    let mut i2 = 0;
                    let mut best = f32::NEG_INFINITY;
                    for i in 0..9 {
                        let lv = if planes_id[i][y * w + x] == id1 { -1.0 } else { l[i] };
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
                    let p = [
                        c1[0] + p2 * (c2[0] - c1[0]),
                        c1[1] + p2 * (c2[1] - c1[1]),
                        c1[2] + p2 * (c2[2] - c1[2]),
                    ];
                    out.set(y * s + iy, x * s + ix, p);
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------- 09 / 12
/// 09's low-contrast merge, algos2.merge_low_contrast: average each
/// source pixel with 3x3 neighbours within T=40 RGB, self weight 1.5.
fn merge_low_contrast(a: &UImg) -> FImg {
    let (w, h) = (a.w, a.h);
    let mut num = FImg::new(w, h);
    let mut den = vec![0.0f32; w * h];
    for (dy, dx) in OFFS {
        let nb = shifted_u(a, dy, dx);
        for y in 0..h {
            for x in 0..w {
                let centre = a.getf(y, x);
                let n = nb.getf(y, x);
                let dist = ((n[0] - centre[0]).powi(2)
                    + (n[1] - centre[1]).powi(2)
                    + (n[2] - centre[2]).powi(2))
                .sqrt();
                let mut wgt = if dist < 40.0 { 1.0 } else { 0.0 };
                if dy == 0 && dx == 0 {
                    wgt *= 1.5;
                }
                let i = y * w + x;
                den[i] += wgt;
                let cur = num.get(y, x);
                num.set(y, x, [
                    cur[0] + wgt * n[0],
                    cur[1] + wgt * n[1],
                    cur[2] + wgt * n[2],
                ]);
            }
        }
    }
    let mut out = FImg::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let d = den[y * w + x].max(1e-6);
            let n = num.get(y, x);
            out.set(y, x, [n[0] / d, n[1] / d, n[2] / d]);
        }
    }
    out
}

/// Smooth neighbourhoods: at most 2 distinct colours in 3x3 and
/// local std < 12, algos3.smooth_mask.
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
                let sy = clamp_i(y as i64 + dy, 0, h as i64 - 1) as usize;
                for x in 0..w {
                    let sx = clamp_i(x as i64 + dx, 0, w as i64 - 1) as usize;
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
            let v = a.d[i * 3 + c] as f32;
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

/// 09's merge, gated to smooth neighbourhoods, algos3.gated_merge.
fn gated_merge(a: &UImg) -> FImg {
    let merged = merge_low_contrast(a);
    let gate = smooth_mask(a);
    let mut out = FImg::new(a.w, a.h);
    for y in 0..a.h {
        for x in 0..a.w {
            out.set(y, x, if gate[y * a.w + x] { merged.get(y, x) } else { a.getf(y, x) });
        }
    }
    out
}

/// 11's grain at low strength, algos3.grain_layer.
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
            out.set(y, x, [strength * g[0] * gate, strength * g[1] * gate, strength * g[2] * gate]);
        }
    }
    out
}

/// Hard-light blend, algos2.hardlight.
fn hardlight(base: &FImg, top: &FImg) -> FImg {
    let mut out = FImg::new(base.w, base.h);
    for i in 0..base.w * base.h {
        for c in 0..3 {
            let b = base.d[i * 3 + c] / 255.0;
            let t = top.d[i * 3 + c] / 255.0;
            let v = if t <= 0.5 { 2.0 * b * t } else { 1.0 - 2.0 * (1.0 - b) * (1.0 - t) };
            out.d[i * 3 + c] = v * 255.0;
        }
    }
    out
}

/// 12: the combined recipe, algos3.combined.
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
            detail.d[i * 3 + c] = detail_u.d[i * 3 + c] as f32;
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
/// Savitzky-Golay kernels for an odd window, quadratic fit, evaluated
/// at centre: (value, derivative, curvature) weights. These are the
/// exact f64 values numpy's pinv(Vandermonde) returns in algos4
/// (_sg_kernel), hardcoded: the follower's apex gate reads the *sign*
/// of the fitted slope, and at the flat points of a staircase the
/// slope is exactly 0 in exact arithmetic, so its sign is whatever
/// the pinv's ~1e-15 rounding noise says. Recomputing the kernels
/// from the normal equations gives a different noise sign and flips
/// the gate on ~14 samples of a long run. The tables below are the
/// only windows fit_run can ask for (7, 9, 11).
fn sg_kernels(win: usize) -> (Vec<f64>, Vec<f64>, Vec<f64>) {
    let (kv, kd, k2): (&[f64], &[f64], &[f64]) = match win {
        7 => (
            &[-0.09523809523809519, 0.1428571428571429, 0.2857142857142856, 0.33333333333333326, 0.2857142857142856, 0.14285714285714282, -0.0952380952380952],
            &[-0.10714285714285719, -0.07142857142857144, -0.035714285714285705, 1.3068619591195664e-17, 0.03571428571428574, 0.07142857142857144, 0.10714285714285715],
            &[0.059523809523809514, -1.3877787807814457e-17, -0.0357142857142857, -0.0476190476190476, -0.0357142857142857, 3.469446951953614e-18, 0.05952380952380951],
        ),
        9 => (
            &[-0.09090909090909094, 0.06060606060606061, 0.1688311688311688, 0.23376623376623373, 0.2554112554112554, 0.23376623376623373, 0.1688311688311688, 0.06060606060606059, -0.09090909090909093],
            &[-0.06666666666666665, -0.05, -0.033333333333333326, -0.016666666666666663, 3.5832294041243814e-18, 0.01666666666666667, 0.03333333333333334, 0.05, 0.06666666666666665],
            &[0.030303030303030297, 0.0075757575757575725, -0.008658008658008658, -0.018398268398268393, -0.02164502164502164, -0.018398268398268393, -0.008658008658008658, 0.007575757575757574, 0.030303030303030297],
        ),
        11 => (
            &[-0.08391608391608382, 0.020979020979021032, 0.10256410256410252, 0.16083916083916086, 0.1958041958041958, 0.20745920745920743, 0.1958041958041958, 0.1608391608391608, 0.10256410256410255, 0.020979020979020956, -0.08391608391608395],
            &[-0.04545454545454544, -0.03636363636363638, -0.027272727272727282, -0.0181818181818182, -0.009090909090909101, -5.9045068634225964e-18, 0.00909090909090909, 0.018181818181818188, 0.02727272727272728, 0.03636363636363638, 0.045454545454545484],
            &[0.01748251748251747, 0.006993006993006988, -0.0011655011655011607, -0.0069930069930069965, -0.01048951048951049, -0.011655011655011656, -0.010489510489510492, -0.006993006993006992, -0.001165501165501165, 0.006993006993006996, 0.017482517482517484],
        ),
        _ => panic!("sg_kernels: unsupported window {win}"),
    };
    (kv.to_vec(), kd.to_vec(), k2.to_vec())
}

/// Local quadratic fit of samples v(t), algos4._fit_run.
/// Returns (fitted, slope, curvature, rms residual).
fn fit_run(t: &[f64], v: &[f64]) -> Option<(Vec<f64>, Vec<f64>, Vec<f64>, f64)> {
    let n = t.len();
    let w = std::cmp::min(11, if n % 2 == 1 { n } else { n - 1 });
    if w < 7 {
        return None;
    }
    let (kv, kd, k2) = sg_kernels(w);
    let h = w / 2;
    // pad v by edge replication
    let mut vp = Vec::with_capacity(n + 2 * h);
    for _ in 0..h {
        vp.push(v[0]);
    }
    vp.extend_from_slice(v);
    for _ in 0..h {
        vp.push(v[n - 1]);
    }
    let conv = |k: &[f64]| -> Vec<f64> {
        let mut out = vec![0.0; n];
        for i in 0..n {
            let mut s = 0.0;
            for j in 0..w {
                s += k[j] * vp[i + j];
            }
            out[i] = s;
        }
        out
    };
    let f = conv(&kv);
    let d = conv(&kd);
    let c2 = conv(&k2);
    let rms = (v.iter().zip(f.iter()).map(|(a, b)| (a - b).powi(2)).sum::<f64>() / n as f64).sqrt();
    let _ = t;
    Some((f, d, c2, rms))
}

// ---------------------------------------------------------------- runs
struct Run {
    t: Vec<f64>,
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

/// Coherent boundary runs, algos5.find_runs2 (apex + scaled min_len).
fn find_runs2(a: &UImg) -> (Vec<Run>, Vec<Run>) {
    let min_len = if std::cmp::min(a.h, a.w) >= 200 { 24usize } else { 12usize };
    let min_contrast = 50.0f32;
    let max_slope = 0.75f64;
    let max_rms = 0.45f64;
    let max_curv = 0.02f64;
    let max_curv_apex = 0.06f64;

    let extract = |axis: usize| -> Vec<RawRun> {
        let (n_along, n_across) = if axis == 0 { (a.w, a.h - 1) } else { (a.h, a.w - 1) };
        // diff, Aarr, Barr as closures over (along index j, across index i)
        let diff_at = |j: usize, i: usize| -> f32 {
            if axis == 0 {
                let p0 = a.getf(i, j);
                let p1 = a.getf(i + 1, j);
                ((p1[0] - p0[0]).powi(2) + (p1[1] - p0[1]).powi(2) + (p1[2] - p0[2]).powi(2)).sqrt()
            } else {
                let p0 = a.getf(j, i);
                let p1 = a.getf(j, i + 1);
                ((p1[0] - p0[0]).powi(2) + (p1[1] - p0[1]).powi(2) + (p1[2] - p0[2]).powi(2)).sqrt()
            }
        };
        let a_at = |j: usize, i: usize| -> [u8; 3] {
            if axis == 0 { a.get(i, j) } else { a.get(j, i) }
        };
        let mut active: Vec<RawRun> = Vec::new();
        let mut done: Vec<RawRun> = Vec::new();
        for j in 0..n_along {
            let mut used: Vec<bool> = vec![false; active.len()];
            for i in 0..n_across {
                let d = diff_at(j, i);
                if d <= 0.0 {
                    continue;
                }
                let y = i as f64 + 1.0;
                let col_a = a_at(j, i);
                if (d as f64) < min_contrast as f64 {
                    continue;
                }
                let mut best: Option<usize> = None;
                let mut bestd = 1.5f64;
                for (k, r) in active.iter().enumerate() {
                    if used[k] || r.a != col_a {
                        continue;
                    }
                    let dd = (y - r.last_y).abs();
                    if dd < bestd {
                        best = Some(k);
                        bestd = dd;
                    }
                }
                match best {
                    None => {
                        active.push(RawRun { t: vec![j as i64], v: vec![y], a: col_a, last_y: y });
                        used.push(false);
                    }
                    Some(k) => {
                        active[k].t.push(j as i64);
                        active[k].v.push(y);
                        active[k].last_y = y;
                        used[k] = true;
                    }
                }
            }
            let mut still: Vec<RawRun> = Vec::new();
            for r in active.into_iter() {
                if *r.t.last().unwrap() == j as i64 {
                    still.push(r);
                } else {
                    done.push(r);
                }
            }
            active = still;
        }
        done.extend(active);
        done
    };

    let finish = |raw: Vec<RawRun>| -> Vec<Run> {
        let mut out = Vec::new();
        for r in raw {
            if r.t.len() < min_len {
                continue;
            }
            let t: Vec<f64> = r.t.iter().map(|v| *v as f64).collect();
            let v = r.v.clone();
            let Some((f, slope, c2, rms)) = fit_run(&t, &v) else { continue };
            if rms > max_rms {
                continue;
            }
            let n = v.len();
            let mut ok: Vec<bool> = slope.iter().map(|s| s.abs() <= max_slope).collect();
            // apex: slope sign changes mark a peak/valley
            let sgn: Vec<f64> = slope.iter().map(|s| if *s > 0.0 { 1.0 } else if *s < 0.0 { -1.0 } else { 0.0 }).collect();
            let mut apex = vec![false; n];
            for i in 0..n - 1 {
                if sgn[i + 1] != sgn[i] {
                    let lo = i.saturating_sub(1);
                    let hi = std::cmp::min(n, i + 3);
                    for k in lo..hi {
                        apex[k] = true;
                    }
                }
            }
            for i in 0..n {
                let curv_ok = c2[i].abs() <= max_curv || (apex[i] && c2[i].abs() <= max_curv_apex);
                ok[i] = ok[i] && curv_ok;
            }
            // monotonicity with one sign change allowed per 7-sample window
            for i in 0..n {
                let lo = i.saturating_sub(3);
                let hi = std::cmp::min(n, i + 4);
                let mut changes = 0i32;
                let mut prev: Option<f64> = None;
                for k in lo..hi - 1 {
                    let d = v[k + 1] - v[k];
                    if d == 0.0 {
                        continue;
                    }
                    let s = if d > 0.0 { 1.0 } else { -1.0 };
                    if let Some(p) = prev {
                        if s != p {
                            changes += 1;
                        }
                    }
                    prev = Some(s);
                }
                if changes > 1 {
                    ok[i] = false;
                }
            }
            if ok.iter().filter(|b| **b).count() < min_len {
                continue;
            }
            if std::env::var("UPSCALE20_DEBUG_RUNS").is_ok() {
                eprintln!(
                    "run t {}..{} len {} v {:.2}..{:.2} rms {:.3} ok {}",
                    t[0],
                    t[t.len() - 1],
                    t.len(),
                    v.iter().cloned().fold(f64::INFINITY, f64::min),
                    v.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
                    rms,
                    ok.iter().filter(|b| **b).count()
                );
                if t[0] == 0.0 && t[t.len() - 1] == 242.0 {
                    eprintln!("rs ok: {}", ok.iter().map(|b| if *b { '1' } else { '0' }).collect::<String>());
                    eprintln!("rs c2: {:?}", c2.iter().take(20).map(|v| (v * 10000.0).round() / 10000.0).collect::<Vec<_>>());
                }
            }
            out.push(Run { t, v, f, ok });
        }
        out
    };

    (finish(extract(0)), finish(extract(1)))
}

/// Sample the source profile along `axis` at line `idx` and continuous
/// coordinates `c`, algos4._profile_sample_vec. Transition width
/// 0.25 source px, centred on the row/column boundary, smoothstep.
fn profile_sample_vec(a: &UImg, axis: usize, idx: usize, c: &[f64]) -> Vec<[f32; 3]> {
    let n = if axis == 0 { a.h } else { a.w } as i64;
    let col = |i: i64| -> [f32; 3] {
        let ii = clamp_i(i, 0, n - 1) as usize;
        if axis == 0 { a.getf(ii, idx) } else { a.getf(idx, ii) }
    };
    c.iter()
        .map(|&cc| {
            let i = (cc - 0.5).floor() as i64;
            let frac = cc - 0.5 - i as f64;
            let i0 = clamp_i(i, 0, n - 2);
            let mut f = clamp_f(((frac - 0.375) / 0.25) as f32, 0.0, 1.0);
            f = f * f * (3.0 - 2.0 * f);
            let p0 = col(i0);
            let p1 = col(i0 + 1);
            [
                (1.0 - f) * p0[0] + f * p1[0],
                (1.0 - f) * p0[1] + f * p1[1],
                (1.0 - f) * p0[2] + f * p1[2],
            ]
        })
        .collect()
}

fn interp(x: f64, xs: &[f64], ys: &[f64]) -> f64 {
    // np.interp: linear, clamped at the ends
    if x <= xs[0] {
        return ys[0];
    }
    if x >= *xs.last().unwrap() {
        return *ys.last().unwrap();
    }
    let mut lo = 0usize;
    let mut hi = xs.len() - 1;
    while hi - lo > 1 {
        let mid = (lo + hi) / 2;
        if xs[mid] <= x {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let t = (x - xs[lo]) / (xs[hi] - xs[lo]);
    ys[lo] + t * (ys[hi] - ys[lo])
}

struct Stats {
    h_runs: usize,
    v_runs: usize,
    h_len: usize,
    v_len: usize,
    touched_frac: f64,
}

/// edge_follow2, algos5.edge_follow2: re-render coherent runs at
/// their fitted subpixel positions by shifting the source profile.
fn edge_follow2(base: &FImg, a: &UImg, s: usize) -> (FImg, Stats) {
    let (h, w) = (a.h, a.w);
    let mut out = base.clone();
    let (h_runs, v_runs) = find_runs2(a);
    let stats_h_runs = h_runs.len();
    let stats_v_runs = v_runs.len();
    let stats_h_len: usize = h_runs.iter().map(|r| r.t.len()).sum();
    let stats_v_len: usize = v_runs.iter().map(|r| r.t.len()).sum();
    let mut best = vec![f32::INFINITY; w * s * h * s];
    let mut touched = vec![false; w * s * h * s];
    let band = 2.5f64;

    let apply = |run: &Run, axis: usize, out: &mut FImg, best: &mut [f32], touched: &mut [bool]| {
        let t = &run.t;
        let v_arr = &run.v;
        let w_arr = uniform1d_nearest(&run.ok.iter().map(|b| if *b { 1.0f32 } else { 0.0 }).collect::<Vec<_>>());
        let t0 = t[0] as i64;
        let t1 = *t.last().unwrap() as i64;
        for u in t0..=t1 {
            // searchsorted left, clamped
            let mut k = t.partition_point(|&tv| tv < u as f64);
            if k >= t.len() {
                k = t.len() - 1;
            }
            if !run.ok[k] && w_arr[k] <= 0.01 {
                continue;
            }
            let v_u = v_arr[k];
            if axis == 0 {
                // output columns inside source column u
                let ous: Vec<usize> = (u as usize * s..(u as usize + 1) * s).collect();
                let us: Vec<f64> = ous.iter().map(|o| (*o as f64 + 0.5) / s as f64).collect();
                let f: Vec<f64> = us.iter().map(|&uu| interp(uu, t, &run.f)).collect();
                let wgt: Vec<f64> = us.iter().map(|&uu| interp(uu, t, &w_arr.iter().map(|v| *v as f64).collect::<Vec<_>>())).collect();
                let fmin = f.iter().cloned().fold(f64::INFINITY, f64::min);
                let fmax = f.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                let oy0 = ((((fmin - band) * s as f64).floor() as i64).max(0)) as usize;
                let oy1 = ((((fmax + band) * s as f64).ceil() as i64) + 1).min((h * s) as i64) as usize;
                if oy0 >= oy1 {
                    continue;
                }
                let ys: Vec<f64> = (oy0..oy1).map(|y| (y as f64 + 0.5) / s as f64).collect();
                for (j, &ox) in ous.iter().enumerate() {
                    if wgt[j] <= 0.01 {
                        continue;
                    }
                    let sj = (f[j] - v_u) * wgt[j];
                    let mut keep_idx: Vec<usize> = Vec::new();
                    for (yi, &yy) in ys.iter().enumerate() {
                        let d = (yy - f[j]).abs();
                        if d <= band && (d as f32) < best[(oy0 + yi) * out.w + ox] {
                            keep_idx.push(yi);
                        }
                    }
                    if keep_idx.is_empty() {
                        continue;
                    }
                    let coords: Vec<f64> = keep_idx.iter().map(|&yi| ys[yi] - sj).collect();
                    let vals = profile_sample_vec(a, 0, u as usize, &coords);
                    for (vi, &yi) in keep_idx.iter().enumerate() {
                        let oy = oy0 + yi;
                        let cur = out.get(oy, ox);
                        let wj = wgt[j] as f32;
                        out.set(oy, ox, [
                            (1.0 - wj) * cur[0] + wj * vals[vi][0],
                            (1.0 - wj) * cur[1] + wj * vals[vi][1],
                            (1.0 - wj) * cur[2] + wj * vals[vi][2],
                        ]);
                        best[oy * out.w + ox] = (ys[yi] - f[j]).abs() as f32;
                        touched[oy * out.w + ox] = true;
                    }
                }
            } else {
                let ous: Vec<usize> = (u as usize * s..(u as usize + 1) * s).collect();
                let us: Vec<f64> = ous.iter().map(|o| (*o as f64 + 0.5) / s as f64).collect();
                let f: Vec<f64> = us.iter().map(|&uu| interp(uu, t, &run.f)).collect();
                let wgt: Vec<f64> = us.iter().map(|&uu| interp(uu, t, &w_arr.iter().map(|v| *v as f64).collect::<Vec<_>>())).collect();
                let fmin = f.iter().cloned().fold(f64::INFINITY, f64::min);
                let fmax = f.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                let ox0 = ((((fmin - band) * s as f64).floor() as i64).max(0)) as usize;
                let ox1 = ((((fmax + band) * s as f64).ceil() as i64) + 1).min((w * s) as i64) as usize;
                if ox0 >= ox1 {
                    continue;
                }
                let xs: Vec<f64> = (ox0..ox1).map(|x| (x as f64 + 0.5) / s as f64).collect();
                for (j, &oy) in ous.iter().enumerate() {
                    if wgt[j] <= 0.01 {
                        continue;
                    }
                    let sj = (f[j] - v_u) * wgt[j];
                    let mut keep_idx: Vec<usize> = Vec::new();
                    for (xi, &xx) in xs.iter().enumerate() {
                        let d = (xx - f[j]).abs();
                        if d <= band && (d as f32) < best[oy * out.w + (ox0 + xi)] {
                            keep_idx.push(xi);
                        }
                    }
                    if keep_idx.is_empty() {
                        continue;
                    }
                    let coords: Vec<f64> = keep_idx.iter().map(|&xi| xs[xi] - sj).collect();
                    let vals = profile_sample_vec(a, 1, u as usize, &coords);
                    for (vi, &xi) in keep_idx.iter().enumerate() {
                        let ox = ox0 + xi;
                        let cur = out.get(oy, ox);
                        let wj = wgt[j] as f32;
                        out.set(oy, ox, [
                            (1.0 - wj) * cur[0] + wj * vals[vi][0],
                            (1.0 - wj) * cur[1] + wj * vals[vi][1],
                            (1.0 - wj) * cur[2] + wj * vals[vi][2],
                        ]);
                        best[oy * out.w + ox] = (xs[xi] - f[j]).abs() as f32;
                        touched[oy * out.w + ox] = true;
                    }
                }
            }
        }
    };

    for r in &h_runs {
        apply(r, 0, &mut out, &mut best, &mut touched);
    }
    for r in &v_runs {
        apply(r, 1, &mut out, &mut best, &mut touched);
    }
    let touched_frac = touched.iter().filter(|b| **b).count() as f64 / touched.len() as f64;
    (
        out,
        Stats { h_runs: stats_h_runs, v_runs: stats_v_runs, h_len: stats_h_len, v_len: stats_v_len, touched_frac },
    )
}

/// 20: 12 combined, then edge_follow2. No dither source anywhere.
/// UPSCALE20_NO_FOLLOW=1 skips the follower (12 only), for testing.
fn clean_base_edge(a: &UImg, s: usize) -> (FImg, Stats) {
    let base = combined(a, s);
    if std::env::var("UPSCALE20_NO_FOLLOW").is_ok() {
        return (base, Stats { h_runs: 0, v_runs: 0, h_len: 0, v_len: 0, touched_frac: 0.0 });
    }
    edge_follow2(&base, a, s)
}

// ---------------------------------------------------------------- adapter
/// Algorithm 20 as a [`Processor`]: 12 (combined) + edge_follow2, no
/// dither source anywhere. Input must be RGB8; output is RGB8 at
/// `width * scale` by `height * scale`.
pub struct Upscale20;

impl Processor for Upscale20 {
    fn name(&self) -> &'static str {
        "upscale20"
    }

    fn supported_scales(&self) -> &[u32] {
        &[2, 4]
    }

    fn process(&self, frame: &Frame, scale: u32) -> Result<Frame, ProcessError> {
        if !self.supported_scales().contains(&scale) {
            return Err(ProcessError::UnsupportedScale { algorithm: "upscale20", scale });
        }
        if frame.format != PixelFormat::Rgb8 {
            return Err(ProcessError::UnsupportedFormat {
                algorithm: "upscale20",
                format: frame.format.to_string(),
            });
        }
        let src = UImg { w: frame.width as usize, h: frame.height as usize, d: frame.data.clone() };
        let (out, _stats) = clean_base_edge(&src, scale as usize);
        Ok(Frame {
            width: out.w as u32,
            height: out.h as u32,
            format: PixelFormat::Rgb8,
            data: to_u8_img(&out),
        })
    }
}
