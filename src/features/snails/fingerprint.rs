//! Picture fingerprints and the detail check. Pure functions, no Discord or database.
//!
//! How two pictures are compared (picked by the benchmark in the project files,
//! `image-match-bench/`):
//!
//! 1. **Fingerprint**: cut away flat borders, then take a 256-bit difference hash (dHash
//!    16×16) of the whole picture and of 80 shifted crops that keep 84–100% of each side.
//!    Two pictures match when one's whole picture is within [`MATCH_DISTANCE`] bits of the
//!    other's whole picture or one of its crops. That absorbs crops, which a single hash
//!    can't: cutting 10% off one side looks to a hash like a different picture.
//! 2. **Detail check**: many crops give look-alikes (a meme template with a new caption)
//!    many chances to match. So a match is confirmed by lining both pictures up at 64×64 and
//!    comparing them block by block: recompression changes every block a little, a new
//!    caption changes a few blocks a lot.

use std::sync::LazyLock;

use image::imageops::{self, FilterType};
use image::{DynamicImage, GrayImage};
use image_hasher::{HashAlg, Hasher, HasherConfig};

/// Bytes in one hash (16×16 bits).
pub const HASH_BYTES: usize = 32;
/// Hashes per picture: the whole picture, then the crops.
pub const HASHES: usize = 81;
/// Two pictures are a candidate match at this many differing bits or fewer (out of 256).
pub const MATCH_DISTANCE: u32 = 41;
/// The detail check fails when any block differs by more than this.
pub const DETAIL_LIMIT: f32 = 0.5;
/// Only regions of about the same shape are compared: width/height may differ by about 6%.
const SHAPE_TOLERANCE: f32 = 0.06;
/// Pictures are shrunk to this longest side first; the hash only needs 16×17 pixels.
const WORK_SIZE: u32 = 256;

/// Crop positions along one side: (kept fraction, start). Crossing the two sides gives the
/// 81 regions, with the whole picture (1.0, 0.0) × (1.0, 0.0) first.
const AXIS: [(f32, f32); 9] = [
    (1.0, 0.0),
    (0.92, 0.0),
    (0.92, 0.04),
    (0.92, 0.08),
    (0.84, 0.0),
    (0.84, 0.04),
    (0.84, 0.08),
    (0.84, 0.12),
    (0.84, 0.16),
];

/// A region as fractions of the picture: (x, y, width, height).
type Region = (f32, f32, f32, f32);

static REGIONS: LazyLock<Vec<Region>> = LazyLock::new(|| {
    let mut regions = Vec::with_capacity(HASHES);
    for &(kx, sx) in &AXIS {
        for &(ky, sy) in &AXIS {
            regions.push((sx, sy, kx, ky));
        }
    }
    regions
});

static HASHER: LazyLock<Hasher> = LazyLock::new(|| {
    HasherConfig::new()
        .hash_alg(HashAlg::Gradient)
        .hash_size(16, 16)
        .resize_filter(FilterType::Triangle)
        .to_hasher()
});

/// A picture's fingerprint, as stored in the database.
#[derive(Debug, Clone, PartialEq)]
pub struct Fingerprint {
    /// Width / height of the whole picture after trimming.
    pub aspect: f32,
    /// [`HASHES`] hashes of [`HASH_BYTES`] each, the whole picture first.
    pub hashes: Vec<u8>,
}

impl Fingerprint {
    fn hash(&self, i: usize) -> &[u8] {
        &self.hashes[i * HASH_BYTES..(i + 1) * HASH_BYTES]
    }

    /// The shape of region `i`.
    fn aspect_of(&self, i: usize) -> f32 {
        let (_, _, w, h) = REGIONS[i];
        self.aspect * w / h
    }
}

/// Fingerprints a picture.
pub fn fingerprint(img: &DynamicImage) -> Fingerprint {
    let gray = prepare(img);
    let (w, h) = gray.dimensions();
    let mut hashes = Vec::with_capacity(HASHES * HASH_BYTES);
    for &region in REGIONS.iter() {
        let crop = DynamicImage::ImageLuma8(crop(&gray, region));
        hashes.extend_from_slice(HASHER.hash_image(&crop).as_bytes());
    }
    Fingerprint {
        aspect: w as f32 / h as f32,
        hashes,
    }
}

/// The closest pair of regions: the whole of one picture against the whole or a crop of the
/// other, in both directions. Lower is more alike; 0 is the same hash.
pub fn distance(a: &Fingerprint, b: &Fingerprint) -> u32 {
    let mut best = u32::MAX;
    let mut try_pair = |a: &Fingerprint, i: usize, b: &Fingerprint, j: usize| {
        if (a.aspect_of(i) / b.aspect_of(j)).ln().abs() <= SHAPE_TOLERANCE {
            best = best.min(hamming(a.hash(i), b.hash(j)));
        }
    };
    try_pair(a, 0, b, 0);
    for i in 1..HASHES {
        try_pair(a, 0, b, i);
        try_pair(a, i, b, 0);
    }
    best
}

fn hamming(a: &[u8], b: &[u8]) -> u32 {
    a.iter().zip(b).map(|(x, y)| (x ^ y).count_ones()).sum()
}

/// Grayscale, borders trimmed, shrunk to [`WORK_SIZE`].
fn prepare(img: &DynamicImage) -> GrayImage {
    let gray = trim(img.to_luma8());
    let (w, h) = gray.dimensions();
    if w.max(h) <= WORK_SIZE {
        return gray;
    }
    let scale = WORK_SIZE as f32 / w.max(h) as f32;
    let (nw, nh) = (
        ((w as f32 * scale).round() as u32).max(1),
        ((h as f32 * scale).round() as u32).max(1),
    );
    imageops::resize(&gray, nw, nh, FilterType::Triangle)
}

/// Cuts away flat borders (letterboxing, bars around a screenshot): rows and columns whose
/// pixels all sit close to the top-left pixel's shade.
fn trim(gray: GrayImage) -> GrayImage {
    let (w, h) = gray.dimensions();
    if w == 0 || h == 0 {
        return gray;
    }
    let bg = gray.get_pixel(0, 0)[0] as i32;
    let flat = |x: u32, y: u32| (gray.get_pixel(x, y)[0] as i32 - bg).abs() < 12;
    let flat_row = |y: u32| (0..w).all(|x| flat(x, y));
    let flat_col = |x: u32, top: u32, bottom: u32| (top..bottom).all(|y| flat(x, y));
    let (mut top, mut bottom) = (0, h);
    while top < bottom && flat_row(top) {
        top += 1;
    }
    while bottom > top && flat_row(bottom - 1) {
        bottom -= 1;
    }
    let (mut left, mut right) = (0, w);
    while left < right && flat_col(left, top, bottom) {
        left += 1;
    }
    while right > left && flat_col(right - 1, top, bottom) {
        right -= 1;
    }
    // A mostly flat picture would be cut down to nearly nothing: leave it as it is.
    if bottom - top < h / 3 || right - left < w / 3 {
        return gray;
    }
    imageops::crop_imm(&gray, left, top, right - left, bottom - top).to_image()
}

/// The part of `gray` that `region` covers, at least 1×1.
fn crop(gray: &GrayImage, (x, y, w, h): Region) -> GrayImage {
    let (gw, gh) = (gray.width() as f32, gray.height() as f32);
    let left = ((gw * x) as u32).min(gray.width() - 1);
    let top = ((gh * y) as u32).min(gray.height() - 1);
    let width = ((gw * w) as u32).clamp(1, gray.width() - left);
    let height = ((gh * h) as u32).clamp(1, gray.height() - top);
    imageops::crop_imm(gray, left, top, width, height).to_image()
}

// ---- Detail check ----

/// Side of the squares compared in the detail check.
const DETAIL_SIZE: u32 = 64;
/// The squares are compared in this many blocks per side.
const BLOCKS: usize = 8;

#[cfg(test)]
/// Whether two pictures that matched by hash also match in their details.
pub fn same_details(a: &DynamicImage, b: &DynamicImage) -> bool {
    detail_difference(a, b) <= DETAIL_LIMIT
}

/// How much the most different block of two lined-up pictures differs, in standard
/// deviations of brightness. About 0.0–0.3 for a recompressed or resized copy, 0.5 and up
/// for a new caption or different text.
pub fn detail_difference(a: &DynamicImage, b: &DynamicImage) -> f32 {
    let (a, b) = (prepare(a), prepare(b));
    let whole = (0.0, 0.0, 1.0, 1.0);
    let a_whole = square(&a, whole);
    let b_whole = square(&b, whole);

    // Find the region of one picture that looks most like the whole of the other.
    let mut best: Option<(f32, bool, Region)> = None;
    for &region in REGIONS.iter() {
        for (a_side, diff) in [
            (false, mean_diff(&a_whole, &square(&b, region))),
            (true, mean_diff(&b_whole, &square(&a, region))),
        ] {
            if best.is_none_or(|(d, _, _)| diff < d) {
                best = Some((diff, a_side, region));
            }
        }
    }
    let (_, a_side, region) = best.expect("there are regions");

    // Nudge that region's edges for a closer fit, then compare block by block.
    let (cropped, target) = if a_side {
        (&a, &b_whole)
    } else {
        (&b, &a_whole)
    };
    let fitted = refine(cropped, region, target);
    worst_block(&fitted, target)
}

/// A region scaled to a 64×64 square, normalized to mean 0 and standard deviation 1 so
/// brightness and contrast changes don't count.
fn square(gray: &GrayImage, region: Region) -> Vec<f32> {
    let small = imageops::resize(
        &crop(gray, region),
        DETAIL_SIZE,
        DETAIL_SIZE,
        FilterType::Triangle,
    );
    let values: Vec<f32> = small.pixels().map(|p| p[0] as f32).collect();
    let n = values.len() as f32;
    let mean = values.iter().sum::<f32>() / n;
    let std = (values.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / n).sqrt();
    values.iter().map(|v| (v - mean) / (std + 1e-3)).collect()
}

fn mean_diff(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum::<f32>() / a.len() as f32
}

/// Moves the region's edges one at a time while the fit keeps improving.
fn refine(gray: &GrayImage, region: Region, target: &[f32]) -> Vec<f32> {
    // Work with edges (left, top, right, bottom) so each can move on its own.
    let (x, y, w, h) = region;
    let mut edges = [x, y, x + w, y + h];
    let to_region = |e: [f32; 4]| (e[0], e[1], e[2] - e[0], e[3] - e[1]);
    let mut best = mean_diff(&square(gray, to_region(edges)), target);
    for _ in 0..3 {
        let mut moved = false;
        for edge in 0..4 {
            for step in [-0.02, -0.01, -0.004, 0.004, 0.01, 0.02] {
                let mut e = edges;
                e[edge] = (e[edge] + step).clamp(0.0, 1.0);
                if e[2] - e[0] < 0.5 || e[3] - e[1] < 0.5 {
                    continue;
                }
                let diff = mean_diff(&square(gray, to_region(e)), target);
                if diff < best {
                    (best, edges, moved) = (diff, e, true);
                }
            }
        }
        if !moved {
            break;
        }
    }
    square(gray, to_region(edges))
}

/// The largest average difference in any of the 8×8 blocks.
fn worst_block(a: &[f32], b: &[f32]) -> f32 {
    let size = DETAIL_SIZE as usize;
    let block = size / BLOCKS;
    let mut sums = [0.0f32; BLOCKS * BLOCKS];
    for y in 0..size {
        for x in 0..size {
            sums[(y / block) * BLOCKS + x / block] += (a[y * size + x] - b[y * size + x]).abs();
        }
    }
    sums.iter()
        .fold(0.0, |m, s| m.max(s / (block * block) as f32))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};

    /// A test picture with photo-like structure: broad waves plus finer detail, different
    /// everywhere. (Stripes only a few pixels wide would be smeared by JPEG and shrinking,
    /// which real pictures aren't made of.)
    fn picture(seed: u32, w: u32, h: u32) -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::from_fn(w, h, |x, y| {
            let v = |k: u32| {
                let (x, y, s) = (x as f32, y as f32, (seed + k) as f32);
                let broad = ((x * (0.9 + 0.3 * s) + y * (1.3 + 0.2 * s)) / 40.0).sin();
                let fine = ((x * (1.7 + 0.1 * s) - y * (0.8 + 0.4 * s)) / 12.0).cos();
                (128.0 + 80.0 * broad + 40.0 * fine) as u8
            };
            Rgb([v(0), v(1), v(2)])
        }))
    }

    fn jpeg(img: &DynamicImage, quality: u8) -> DynamicImage {
        let mut out = Vec::new();
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
        img.to_rgb8().write_with_encoder(encoder).unwrap();
        image::load_from_memory(&out).unwrap()
    }

    #[test]
    fn fingerprint_shape() {
        let fp = fingerprint(&picture(1, 400, 300));
        assert_eq!(fp.hashes.len(), HASHES * HASH_BYTES);
        assert!((fp.aspect - 4.0 / 3.0).abs() < 0.02);
    }

    #[test]
    fn copies_match_and_others_dont() {
        let original = picture(1, 400, 300);
        let fp = fingerprint(&original);
        assert_eq!(distance(&fp, &fp), 0);

        let copies = [
            ("jpeg", jpeg(&original, 50)),
            ("half size", original.resize(200, 150, FilterType::Triangle)),
            ("10% off the left", original.crop_imm(40, 0, 360, 300)),
            ("8% off right and bottom", original.crop_imm(0, 0, 368, 276)),
        ];
        for (name, copy) in copies {
            let d = distance(&fp, &fingerprint(&copy));
            assert!(d <= MATCH_DISTANCE, "{name}: distance {d}");
            // The direction doesn't matter.
            assert_eq!(d, distance(&fingerprint(&copy), &fp), "{name}");
            assert!(
                same_details(&original, &copy),
                "{name}: {}",
                detail_difference(&original, &copy)
            );
        }

        let other = fingerprint(&picture(9, 400, 300));
        assert!(distance(&fp, &other) > MATCH_DISTANCE);
    }

    #[test]
    fn a_new_caption_fails_the_detail_check() {
        let original = picture(1, 400, 300);
        let mut captioned = original.to_rgb8();
        // A white bar with black "text" across the bottom fifth.
        for y in 240..290 {
            for x in 20..380 {
                let ink = (x / 6 + y / 9) % 3 == 0;
                captioned.put_pixel(
                    x,
                    y,
                    if ink {
                        Rgb([0, 0, 0])
                    } else {
                        Rgb([255, 255, 255])
                    },
                );
            }
        }
        let captioned = DynamicImage::ImageRgb8(captioned);
        assert!(
            !same_details(&original, &captioned),
            "{}",
            detail_difference(&original, &captioned)
        );
    }

    #[test]
    fn trim_removes_flat_borders() {
        let inner = picture(2, 200, 100).to_luma8();
        let mut boxed = GrayImage::from_pixel(260, 160, image::Luma([0]));
        imageops::replace(&mut boxed, &inner, 30, 30);
        let trimmed = trim(boxed);
        // The test picture's own edge pixels may be dark enough to trim a row or two.
        assert!(
            trimmed.width() >= 196 && trimmed.width() <= 200,
            "{}",
            trimmed.width()
        );
        assert!(
            trimmed.height() >= 96 && trimmed.height() <= 100,
            "{}",
            trimmed.height()
        );
        // A flat picture stays as it is.
        let flat = GrayImage::from_pixel(50, 50, image::Luma([7]));
        assert_eq!(trim(flat).dimensions(), (50, 50));
    }
}
