/*
 gifski pngquant-based GIF encoder
 © 2017 Kornel Lesiński

 This program is free software: you can redistribute it and/or modify
 it under the terms of the GNU Affero General Public License as
 published by the Free Software Foundation, either version 3 of the
 License, or (at your option) any later version.

 This program is distributed in the hope that it will be useful,
 but WITHOUT ANY WARRANTY; without even the implied warranty of
 MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 GNU Affero General Public License for more details.

 You should have received a copy of the GNU Affero General Public License
 along with this program.  If not, see <https://www.gnu.org/licenses/>.
*/
//! gif.ski library allows creation of GIF animations from [arbitrary pixels][ImgVec],
//! or [PNG files][Collector::add_frame_png_file].
//!
//! See the [`new`] function to get started.
#![doc(html_logo_url = "https://gif.ski/icon.png")]
#![allow(clippy::bool_to_int_with_if)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::enum_glob_use)]
#![allow(clippy::if_not_else)]
#![allow(clippy::inline_always)]
#![allow(clippy::manual_range_contains)]
#![allow(clippy::match_same_arms)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::needless_pass_by_value)]
#![allow(clippy::redundant_closure_for_method_calls)]
#![allow(clippy::wildcard_imports)]

use encoderust::RustEncoder;
use gif::DisposalMethod;
use imagequant::{Attributes, Histogram, Image, QuantizationResult};
use imgref::*;
use rgb::*;

mod error;
pub use crate::error::*;
use ordered_channel::bounded as ordqueue_new;
use ordered_channel::Receiver as OrdQueueIter;
use ordered_channel::Sender as OrdQueue;
pub mod progress;
use crate::progress::*;
pub mod c_api;
mod denoise;
use crate::denoise::*;
pub mod collector;
mod encoderust;
#[doc(inline)]
pub use crate::collector::Collector;
use crate::collector::{FrameSource, InputFrame, InputFrameResized};

#[cfg(feature = "gifsicle")]
mod gifsicle;

mod minipool;

use crossbeam_channel::{Receiver, Sender};
use std::cell::Cell;
use std::collections::VecDeque;
use std::io::prelude::*;
use std::num::NonZeroU8;
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::atomic::Ordering::Relaxed;
use std::thread;

#[cfg(feature = "wasm")]
pub mod wasm;

#[cfg(feature = "wasm")]
pub use wasm::GifskiWasm;

mod dither;
use dither::*;

use smart_default::SmartDefault;

/// Number of repetitions
pub type Repeat = gif::Repeat;

/// Encoding speed level (1-10)
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Speed(pub u8);

impl Speed {
    pub const SLOW: Self = Self(1);
    pub const MEDIUM: Self = Self(4);
    pub const FAST: Self = Self(10);
}

impl std::ops::Deref for Speed {
    type Target = u8;
    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<u8> for Speed {
    #[inline]
    fn from(val: u8) -> Self {
        Self(val)
    }
}

/// Dithering algorithm or matrix.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, strum::EnumIter, strum::EnumString, strum::VariantNames, strum::Display, strum::IntoStaticStr)]
#[strum(ascii_case_insensitive)]
pub enum Dithering {
    #[strum(serialize = "none")]
    None,
    #[strum(serialize = "floyd-steinberg")]
    FloydSteinberg,
    #[strum(serialize = "bayer2")]
    Bayer2,
    #[strum(serialize = "bayer4")]
    Bayer4,
    #[strum(serialize = "bayer8")]
    Bayer8,
    #[strum(serialize = "blue16")]
    Blue16,
    #[default]
    #[strum(serialize = "blue32")]
    Blue32,
    #[strum(serialize = "blue64")]
    Blue64,
    #[strum(serialize = "checkerboard")]
    Checkerboard,
}

impl Dithering {
    #[must_use]
    #[inline]
    pub fn is_ordered(&self) -> bool {
        !matches!(self, Self::None | Self::FloydSteinberg)
    }

    #[must_use]
    #[inline]
    pub fn is_error_diffusion(&self) -> bool {
        matches!(self, Self::FloydSteinberg)
    }
}

/// Encoding settings for the `new()` function
#[derive(Clone, Debug, SmartDefault)]
pub struct Settings {
    /// Resize to max this width if non-0.
    pub width: Option<u32>,
    /// Resize to max this height if width is non-0. Note that aspect ratio is not preserved.
    pub height: Option<u32>,
    /// 1-100, but useful range is 50-100. Recommended to set to 100.
    #[default = 100]
    pub quality: u8,
    /// Quality 1-100 of temporal denoising. Lower values reduce motion. `None` defaults to `quality`.
    pub motion_quality: Option<u8>,
    /// Quality 1-100 of gifsicle compression. Lower values add noise. `None` defaults to `quality`.
    pub lossy_quality: Option<u8>,
    /// Encoding speed: Speed::SLOW, Speed::MEDIUM, Speed::FAST, or Speed(1-10). Defaults to Speed::MEDIUM.
    #[default(Speed::MEDIUM)]
    pub speed: Speed,
    /// Sets the looping method for the image sequence.
    #[default(Repeat::Infinite)]
    pub repeat: Repeat,
    /// Use a single global palette for all frames instead of per-frame palettes.
    pub global_palette: bool,
    /// Background color for semitransparent pixels, or `None` to dither alpha.
    pub matte: Option<RGB8>,
    /// Colors to always keep in the palette (max 255, because one more is reserved for transparency).
    pub fixed_colors: Vec<RGB8>,
    /// Maximum number of colors in the palette (2-256). `None` defaults to 256.
    pub max_colors: Option<u16>,
    /// Error diffusion dithering level (0.0 = none, 1.0 = full). `None` defaults to automatic based on `quality`.
    pub dithering_level: Option<f32>,
    /// Maximum number of pixels before auto-downscaling triggers. `None` disables downscaling (keeps original resolution).
    pub max_pixels: Option<u32>,
    /// Dithering matrix used for semi-transparent alpha. Defaults to `Dithering::Blue32`.
    #[default(Dithering::Blue32)]
    pub alpha_dithering: Dithering,
    /// Dithering algorithm or matrix used for colors. Defaults to `Dithering::FloydSteinberg`.
    #[default(Dithering::FloydSteinberg)]
    pub color_dithering: Dithering,
    /// Minimum alpha threshold below which pixels become transparent (0-255). Defaults to 8.
    #[default = 8]
    pub alpha_low: u8,
    /// Maximum alpha threshold at or above which pixels become opaque (0-255). Defaults to 134.
    #[default = 134]
    pub alpha_high: u8,
    /// Threshold used for anti-aliased edges and non-dithered alpha (0-255). `None` defaults to `low + (high - low) / 3 * 2` (92 with defaults).
    pub alpha_mid: Option<u8>,
    /// Temporal radius for post-dither denoising of stray oscillating pixels (0=off, 1=3 frames, 2=5 frames). Defaults to 0.
    pub dither_denoise_radius: u8,
    /// Color distance threshold (0-255) for dither denoising. Differences above this are considered content and preserved. Defaults to 16.
    #[default = 16]
    pub dither_denoise_threshold: u8,
    /// Color distance threshold (0-255) for edge-aware dithering suppression. 0 disables suppression. Defaults to 0.
    pub dither_edge_threshold: u8,
}

impl Settings {
    /// Effective threshold used for anti-aliased edges and non-dithered alpha.
    #[must_use]
    #[inline]
    pub fn effective_alpha_mid(&self) -> u8 {
        self.alpha_mid.unwrap_or_else(|| {
            self.alpha_low.saturating_add((self.alpha_high.saturating_sub(self.alpha_low)) / 3 * 2)
        })
    }
    /// quality is used in other places, like gifsicle or frame differences,
    /// and it's better to lower quality there before ruining quantization
    pub(crate) fn color_quality(&self) -> u8 {
        (u16::from(self.quality) * 4 / 3).min(100) as u8
    }

    pub(crate) fn effective_motion_quality(&self) -> u8 {
        self.motion_quality.unwrap_or(self.quality)
    }

    pub(crate) fn effective_lossy_quality(&self) -> u8 {
        self.lossy_quality.unwrap_or(self.quality)
    }

    pub(crate) fn gifsicle_loss(&self) -> u32 {
        let lossy_quality = self.effective_lossy_quality();
        if cfg!(feature = "gifsicle") && lossy_quality < 100 {
            ((100. / 5. - f32::from(lossy_quality) / 5.).powf(1.8).ceil() as u32 + 10) * 10
        } else {
            0
        }
    }

    pub(crate) fn effective_dithering_level(&self) -> f32 {
        if let Some(dithering_level) = self.dithering_level {
            return dithering_level;
        }
        let gifsicle_quality = if cfg!(feature = "gifsicle") { self.effective_lossy_quality() } else { 100 };
        debug_assert!(gifsicle_quality <= 100);
        // lossy LZW adds its own dithering, so the input could be less nosiy to compensate
        // but don't change dithering unless gifsicle quality < 90, and don't completely disable it
        let gifsicle_factor = 0.25 + f32::from(gifsicle_quality) * (1. / 100. * 1. / 0.9 * 0.75);

        f32::from(self.quality).mul_add(1. / 50. * gifsicle_factor, -1.).clamp(0.2, 1.)
    }

    /// Returns true if encoding speed is fast (speed >= 10).
    #[must_use]
    #[inline]
    pub fn fast(&self) -> bool {
        *self.speed >= 10
    }

    /// Returns true if encoding speed is slow (speed <= 1).
    #[must_use]
    #[inline]
    pub fn slow(&self) -> bool {
        *self.speed <= 1
    }

    /// `add_frame` is going to resize the images to this size.
    #[must_use]
    #[inline]
    pub fn dimensions_for_image(&self, width: usize, height: usize) -> (usize, usize) {
        dimensions_for_image((width, height), (self.width, self.height), self.max_pixels)
    }
}

/// Perform GIF writing
pub struct Writer {
    /// Input frame decoder results
    queue_iter: Option<Receiver<InputFrame>>,
    settings: Settings,
    max_threads: NonZeroU8,
}

struct GIFFrame {
    left: u16,
    top: u16,
    image: ImgVec<u8>,
    pal: Vec<RGB8>,
    dispose: DisposalMethod,
    transparent_index: Option<u8>,
}

/// Frame before quantization
struct DiffMessage {
    /// 1..
    ordinal_frame_number: usize,
    pts: f64, frame_duration: f64,
    image: ImgVec<RGBA8>,
    importance_map: Vec<u8>,
}

struct QuantizeMessage {
    /// 1.. with holes
    ordinal_frame_number: usize,
    /// 0.. no holes
    frame_index: u32,
    first_frame_has_transparency: bool,
    image: ImgVec<RGBA8>,
    importance_map: Vec<u8>,
    prev_frame_keeps: bool,
    dispose: gif::DisposalMethod,
    end_pts: f64,
    has_next_frame: bool,
}

/// Frame post quantization, before remap
struct RemapMessage {
    /// 1..
    ordinal_frame_number: usize,
    end_pts: f64,
    dispose: DisposalMethod,
    liq: Attributes,
    remap: QuantizationResult,
    liq_image: Image<'static>,
    out_buf: Vec<u8>,
    has_next_frame: bool,
    source_image: Option<ImgVec<RGBA8>>,
}

/// Frame post quantization and remap
struct FrameMessage {
    /// 0..
    frame_index: usize,
    /// 1..
    ordinal_frame_number: usize,
    end_pts: f64,
    frame: GIFFrame,
    screen_width: u16,
    screen_height: u16,
}

struct BufferedRemap {
    ordinal_frame_number: usize,
    frame_index: usize,
    end_pts: f64,
    dispose: DisposalMethod,
    has_next_frame: bool,
    source_image: Option<ImgVec<RGBA8>>,
    raw_image8: ImgVec<u8>,
    image8_pal: Vec<RGBA8>,
}

/// Start new encoding on two threads.
///
/// Encoding is always multi-threaded, and the `Collector` and `Writer`
/// must be used on sepate threads.
///
/// You feed input frames to the [`Collector`], and ask the [`Writer`] to
/// start writing the GIF.
///
/// If you don't start writing, then adding frames will block forever.
///
///
/// ```rust,no_run
/// use gifski::*;
///
/// let (collector, writer) = gifski::new(Settings::default())?;
/// std::thread::scope(|t| -> Result<(), Error> {
///     let frames_thread = t.spawn(move || {
///         for i in 0..10 {
///             collector.add_frame_png_file(i, format!("frame{i:04}.png").into(), i as f64 * 0.1)?;
///         }
///         drop(collector);
///         Ok(())
///     });
///
///     writer.write(std::fs::File::create("demo.gif")?, &mut progress::NoProgress {})?;
///     frames_thread.join().unwrap()
/// })?;
/// Ok::<_, Error>(())
/// ```
#[inline]
pub fn new(settings: Settings) -> GifResult<(Collector, Writer)> {
    if settings.quality == 0 || settings.quality > 100 {
        return Err(Error::WrongSize("quality must be 1-100".into())); // I forgot to add a better error variant
    }
    if settings.motion_quality.is_some_and(|q| q == 0 || q > 100) {
        return Err(Error::WrongSize("motion_quality must be 1-100".into()));
    }
    if settings.lossy_quality.is_some_and(|q| q == 0 || q > 100) {
        return Err(Error::WrongSize("lossy_quality must be 1-100".into()));
    }
    if !(1..=10).contains(&settings.speed.0) {
        return Err(Error::WrongSize("speed must be 1-10".into()));
    }
    if settings.dithering_level.is_some_and(|d| !(0.0..=1.0).contains(&d)) {
        return Err(Error::WrongSize("dithering_level must be 0.0-1.0".into()));
    }
    if settings.alpha_dithering.is_error_diffusion() {
        return Err(Error::WrongSize("floyd-steinberg is not supported for alpha dithering".into()));
    }
    if settings.max_colors.is_some_and(|c| c < 2 || c > 256) {
        return Err(Error::WrongSize("max_colors must be 2-256".into()));
    }
    if settings.fixed_colors.len() >= usize::from(settings.max_colors.unwrap_or(256)) {
        return Err(Error::WrongSize("too many fixed_colors for max_colors (1 is reserved for transparency)".into()));
    }
    if settings.width.unwrap_or(0) > 1 << 16 || settings.height.unwrap_or(0) > 1 << 16 {
        return Err(Error::WrongSize("image size too large".into()));
    }
    if settings.max_pixels.is_some_and(|p| p == 0) {
        return Err(Error::WrongSize("max_pixels must be greater than 0".into()));
    }
    if settings.max_pixels.is_some() && (settings.width.is_some() || settings.height.is_some()) {
        return Err(Error::WrongSize("max_pixels cannot be used together with width or height".into()));
    }
    if settings.alpha_low > settings.alpha_high {
        return Err(Error::WrongSize("alpha_low must be <= alpha_high".into()));
    }
    if settings.alpha_mid.is_some_and(|mid| mid < settings.alpha_low || mid > settings.alpha_high) {
        return Err(Error::WrongSize("alpha_mid must be between alpha_low and alpha_high".into()));
    }
    if settings.dither_denoise_radius > 2 {
        return Err(Error::WrongSize("dither_denoise_radius must be 0, 1, or 2".into()));
    }

    let max_threads = thread::available_parallelism().map(|t| t.get().min(255) as u8).unwrap_or(8);
    let (queue, queue_iter) = crossbeam_channel::bounded(5.min(max_threads.into())); // should be sufficient for denoiser lookahead
    Ok((
        Collector {
            queue,
        },
        Writer {
            queue_iter: Some(queue_iter),
            settings,
            max_threads: max_threads.try_into()?,
        },
    ))
}

#[inline(never)]
#[cfg_attr(debug_assertions, track_caller)]
fn resized_binary_alpha(image: ImgVec<RGBA8>, settings: &Settings) -> CatResult<ImgVec<RGBA8>> {
    let (width, height) = dimensions_for_image((image.width(), image.height()), (settings.width, settings.height), settings.max_pixels);

    let mut image = if width != image.width() || height != image.height() {
        let tmp = image.as_ref();
        let (buf, img_width, img_height) = tmp.to_contiguous_buf();
        assert_eq!(buf.len(), img_width * img_height);

        let mut r = resize::new(img_width, img_height, width, height, resize::Pixel::RGBA8P, resize::Type::Lanczos3)?;
        let mut dst = vec![RGBA8::new(0, 0, 0, 0); width * height];
        r.resize(&buf, &mut dst)?;
        ImgVec::new(dst, width, height)
    } else {
        image
    };

    if let Some(matte) = settings.matte {
        image.pixels_mut().filter(|px| px.a < 255 && px.a > 0).for_each(move |px| {
            let alpha = u16::from(px.a);
            let inv_alpha = 255 - alpha;

            *px = RGBA8 {
                r: ((u16::from(px.r) * alpha + u16::from(matte.r) * inv_alpha) / 255) as u8,
                g: ((u16::from(px.g) * alpha + u16::from(matte.g) * inv_alpha) / 255) as u8,
                b: ((u16::from(px.b) * alpha + u16::from(matte.b) * inv_alpha) / 255) as u8,
                a: 255,
            };
        });
    } else {
        dither_image(image.as_mut(), settings.alpha_dithering, settings.alpha_low, settings.alpha_high, settings.effective_alpha_mid());
    }

    Ok(image)
}

#[allow(clippy::identity_op)]
#[allow(clippy::erasing_op)]
#[inline(never)]
fn dither_image(
    mut image: ImgRefMut<RGBA8>,
    alpha_dithering: Dithering,
    alpha_low: u8,
    alpha_high: u8,
    alpha_mid: u8,
) {
    let width = image.width();
    let height = image.height();

    let Some(matrix) = DitherMatrix::for_algorithm(alpha_dithering) else {
        for row in image.rows_mut() {
            for px in row.iter_mut() {
                if px.a < 255 {
                    px.a = if px.a < alpha_mid { 0 } else { 255 };
                }
            }
        }
        return;
    };

    // dithering of anti-aliased edges can look very fuzzy, so disable it near the edges
    let mut anti_aliasing = vec![false; width * height];
    loop9::loop9(image.as_ref(), 0, 0, width, height, |x, y, top, mid, bot| {
        if mid.curr.a != 255 && mid.curr.a != 0 {
            fn is_edge(a: u8, b: u8) -> bool {
                a < 12 && b >= 240 ||
                b < 12 && a >= 240
            }
            if is_edge(top.curr.a, bot.curr.a) ||
            is_edge(mid.prev.a, mid.next.a) ||
            is_edge(top.prev.a, bot.next.a) ||
            is_edge(top.next.a, bot.prev.a) {
                anti_aliasing[x + y * width] = true;
            }
        }
    });

    let x_mask = matrix.width - 1;
    let y_mask = matrix.height - 1;
    let m_width = matrix.width;
    let max_val = matrix.max_val;
    let range = (alpha_high - alpha_low) as u16;

    let mut lut = [0u8; 256];
    for (val, slot) in lut[..=max_val as usize].iter_mut().enumerate() {
        *slot = (alpha_low as u16 + (val as u16 * range + max_val / 2) / max_val) as u8;
    }

    // Make transparency binary
    for (y, (row, aa)) in image.rows_mut().zip(anti_aliasing.chunks_exact(width)).enumerate() {
        for (x, (px, aa)) in row.iter_mut().zip(aa.iter().copied()).enumerate() {
            if px.a < 255 {
                if aa {
                    px.a = if px.a < alpha_mid { 0 } else { 255 };
                } else {
                    let dither_val = matrix.data[(y & y_mask) * m_width + (x & x_mask)];
                    px.a = if px.a < lut[dither_val as usize] { 0 } else { 255 };
                }
            }
        }
    }
}

/// `add_frame` is going to resize the image to this size.
/// The `Option` args are user-specified max width, max height, and max pixels
#[inline(never)]
fn dimensions_for_image((img_w, img_h): (usize, usize), resize_to: (Option<u32>, Option<u32>), max_pixels: Option<u32>) -> (usize, usize) {
    match resize_to {
        (None, None) => {
            if let Some(max_pixels) = max_pixels {
                let max_px = max_pixels as usize;
                let factor = ((img_w * img_h + max_px / 2) as f64 / max_px as f64).sqrt().round() as usize;
                if factor > 1 {
                    (img_w / factor, img_h / factor)
                } else {
                    (img_w, img_h)
                }
            } else {
                (img_w, img_h)
            }
        },
        (Some(w), Some(h)) => {
            ((w as usize).min(img_w), (h as usize).min(img_h))
        },
        (Some(w), None) => {
            let w = (w as usize).min(img_w);
            (w, img_h * w / img_w)
        },
        (None, Some(h)) => {
            let h = (h as usize).min(img_h);
            (img_w * h / img_h, h)
        },
    }
}

#[derive(Copy, Clone)]
enum LastFrameDuration {
    FixedOffset(f64),
    FrameRate(f64),
}

impl LastFrameDuration {
    #[inline]
    pub fn value(&self) -> f64 {
        match self {
            Self::FixedOffset(val) | Self::FrameRate(val) => *val,
        }
    }

    #[inline]
    pub fn shift_every_pts_by(&self) -> f64 {
        match self {
            Self::FixedOffset(offset) => *offset,
            Self::FrameRate(_) => 0.,
        }
    }
}

/// Encode collected frames
impl Writer {
    /// Create and configure `imagequant::Attributes` with speed, quality, and palette limits.
    fn new_liq_attr(&self, first_frame: bool) -> CatResult<Attributes> {
        let mut liq = Attributes::new();
        let speed = if first_frame {
            self.settings.speed.0.min(4)
        } else {
            self.settings.speed.0
        };
        liq.set_speed(i32::from(speed))?;
        let quality = if !first_frame {
            self.settings.color_quality()
        } else {
            100 // the first frame is too important to ruin it
        };
        liq.set_quality(0, quality)?;
        let max_colors = if let Some(max_colors) = self.settings.max_colors {
            u32::from(max_colors)
        } else if self.settings.quality < 50 {
            let min_colors = 5 + self.settings.fixed_colors.len() as u32;
            u32::from(self.settings.quality * 2).max(min_colors).next_power_of_two().min(256)
        } else {
            256
        };
        if max_colors < 256 {
            liq.set_max_colors(max_colors)?;
        }
        Ok(liq)
    }

    /// Create an `imagequant::Image` and attach importance map, transparency, and fixed colors.
    fn new_liq_image(&self, liq: &Attributes, image: ImgVec<RGBA8>, importance_map: &[u8], set_importance_map: bool, needs_transparency: bool) -> CatResult<Image<'static>> {
        let (buf, width, height) = image.into_contiguous_buf();
        let mut img = liq.new_image(buf, width, height, 0.)?;
        // only later remapping tracks which area has been damaged by transparency
        // so for previous-transparent background frame the importance map may be invalid
        // because there's a transparent hole in the background not taken into account,
        // and palette may lack colors to fill that hole
        if set_importance_map {
            img.set_importance_map(importance_map)?;
        }
        // first frame may be transparent too, so it's not just for diffs
        if needs_transparency {
            img.add_fixed_color(RGBA8::new(0, 0, 0, 0))?;
        }
        // user may have colors which need to be preserved and left undithered
        for color in &self.settings.fixed_colors {
            img.add_fixed_color(RGBA8::new(color.r, color.g, color.b, 255))?;
        }
        Ok(img)
    }

    /// Requantize if needed to ensure power-of-two palette sizes when slow speed is enabled.
    fn tune_quantization_result(&self, liq: &mut Attributes, mut res: QuantizationResult, requantize: impl FnOnce(&mut Attributes) -> CatResult<QuantizationResult>) -> CatResult<QuantizationResult> {
        // GIF only stores power-of-two palette sizes
        if self.settings.slow() {
            let max_colors = usize::from(self.settings.max_colors.unwrap_or(256));
            let len = res.palette_len();
            let target_len = len.next_power_of_two().min(max_colors);
            // it has little impact on compression (128c -> 64c is only 7% smaller)
            if (len < 128 || len > 220) && len < target_len {
                liq.set_max_colors(target_len as _)?;
                liq.set_quality(0, 100)?;
                res = requantize(liq)?;
            }
        }
        if self.settings.color_dithering.is_error_diffusion() {
            res.set_dithering_level(self.settings.effective_dithering_level())?;
        } else {
            res.set_dithering_level(0.0)?;
        }
        Ok(res)
    }

    /// Pre-reserve and prepare dithering buffer.
    fn prepare_dither_out(&self, res: &mut QuantizationResult, img: &mut Image<'_>) -> CatResult<Vec<u8>> {
        let (width, height) = (img.width(), img.height());
        let mut out = Vec::new();
        out.try_reserve_exact(width * height).map_err(imagequant::liq_error::from)?;
        if self.settings.color_dithering.is_error_diffusion() {
            res.optionally_prepare_for_dithering_with_background_set(img, &mut out.spare_capacity_mut()[..width * height])?;
        }
        Ok(out)
    }

    /// `importance_map` is computed from previous and next frame.
    /// Improves quality of pixels visible for longer.
    /// Avoids wasting palette on pixels identical to the background.
    ///
    /// `background` is the previous frame.
    fn quantize(&self, image: ImgVec<RGBA8>, importance_map: &[u8], first_frame: bool, needs_transparency: bool, prev_frame_keeps: bool) -> CatResult<(Attributes, QuantizationResult, Image<'static>, Vec<u8>)> {
        let mut liq = self.new_liq_attr(first_frame)?;
        let mut img = self.new_liq_image(&liq, image, importance_map, first_frame || prev_frame_keeps, needs_transparency)?;

        let res = liq.quantize(&mut img)?;
        let mut res = self.tune_quantization_result(&mut liq, res, |liq| liq.quantize(&mut img).map_err(From::from))?;
        let out = self.prepare_dither_out(&mut res, &mut img)?;

        Ok((liq, res, img, out))
    }

    fn remap<'a>(&self, liq: Attributes, mut res: QuantizationResult, mut img: Image<'a>, background: Option<ImgRef<'a, RGBA8>>, mut pal_img: Vec<u8>, source_image: Option<ImgVec<RGBA8>>, dither_context: &mut DitherColorContext) -> CatResult<(ImgVec<u8>, Vec<RGBA8>)> {
        let (width, height) = (img.width(), img.height());
        if let Some(matrix) = DitherMatrix::for_algorithm(self.settings.color_dithering) {
            let pal = res.palette_vec();
            pal_img.resize(width * height, 0);
            let src = source_image.as_ref().expect("source_image required for ordered color dithering");
            let exact_match = self.settings.global_palette && self.settings.effective_motion_quality() == 100;
            dither_color(
                src.as_ref(),
                &pal,
                matrix,
                self.settings.effective_dithering_level(),
                exact_match,
                background,
                &mut pal_img,
                dither_context,
                self.settings.dither_edge_threshold,
            );
            Ok((Img::new(pal_img, width, height), pal))
        } else {
            if let Some(bg) = background {
                img.set_background(Image::new_stride_borrowed(&liq, bg.buf(), bg.width(), bg.height(), bg.stride(), 0.)?)?;
            }

            let pal = res.remap_into_vec(&mut img, &mut pal_img)?;
            debug_assert_eq!(img.width() * img.height(), pal_img.len());

            Ok((Img::new(pal_img, width, height), pal))
        }
    }

    #[inline(never)]
    fn write_frames(&self, write_queue: Receiver<FrameMessage>, writer: &mut dyn Write, reporter: &Mutex<Option<&mut dyn ProgressReporter>>) -> CatResult<()> {
        let (lzw_queue, lzw_recv) = ordqueue_new(2);
        minipool::new_scope((if self.settings.fast() || self.settings.gifsicle_loss() > 0 { 3 } else { 1 }).try_into().unwrap(), "lzw", move || {
            let mut pts_in_delay_units = 0_u64;

            let written = Rc::new(Cell::new(0));
            let mut enc = RustEncoder::new(writer, written.clone());

            let mut n_done = 0;
            for tmp in lzw_recv {
                let (end_pts, ordinal_frame_number, frame, screen_width, screen_height): (f64, _, _, _, _) = tmp;
                // delay=1 doesn't work, and it's too late to drop frames now
                let delay = ((end_pts * 100_f64).round() as u64)
                    .saturating_sub(pts_in_delay_units)
                    .clamp(2, 30000) as u16;
                pts_in_delay_units += u64::from(delay);

                enc.write_frame(frame, delay, screen_width, screen_height, &self.settings)?;

                let mut reporter_lock = reporter.lock().map_err(|_| Error::ThreadSend)?;
                let reporter = reporter_lock.as_deref_mut().ok_or(Error::Aborted)?;
                reporter.written_bytes(written.get());

                // loop to report skipped frames too
                while n_done < ordinal_frame_number {
                    n_done += 1;
                    if !reporter.increase() {
                        *reporter_lock = None; // prevent further abort-caused errors from being logged
                        return Err(Error::Aborted);
                    }
                }
            }
            if n_done == 0 {
                Err(Error::NoFrames)
            } else {
                Ok(())
            }
        }, move |failed| {
            for FrameMessage {frame, frame_index, ordinal_frame_number, end_pts, screen_width, screen_height } in write_queue {
                if failed.load(Relaxed) {
                    return Err(Error::Aborted);
                }

                let frame = RustEncoder::<&mut dyn std::io::Write>::compress_frame(frame, &self.settings)?;
                lzw_queue.send(frame_index, (end_pts, ordinal_frame_number, frame, screen_width, screen_height))?;
            }
            Ok(())
        })
    }

    /// Start writing frames. This function will not return until the [`Collector`] is dropped.
    ///
    /// `outfile` can be any writer, such as `File` or `&mut Vec`.
    ///
    /// `ProgressReporter.increase()` is called each time a new frame is being written.
    #[inline]
    pub fn write<W: Write>(mut self, mut writer: W, reporter: &mut dyn ProgressReporter) -> GifResult<()> {
        let decode_queue_recv = self.queue_iter.take().ok_or(Error::Aborted)?;
        self.write_inner(decode_queue_recv, &mut writer, reporter)
    }

    #[inline(never)]
    fn write_inner(&self, decode_queue_recv: Receiver<InputFrame>, writer: &mut dyn Write, reporter: &mut dyn ProgressReporter) -> CatResult<()> {
        let reporter = &Mutex::new(Some(reporter));

        thread::scope(|s| {
            let (diff_queue, diff_queue_recv) = ordqueue_new(0);
            let resize_thread = thread::Builder::new().name("resize".into()).spawn_scoped(s, move || {
                self.make_resize(decode_queue_recv, diff_queue)
            })?;
            let (quant_queue, quant_queue_recv) = crossbeam_channel::bounded(0);
            let diff_thread = thread::Builder::new().name("diff".into()).spawn_scoped(s, move || {
                self.make_diffs(diff_queue_recv, quant_queue, reporter)
            })?;
            let (remap_queue, remap_queue_recv) = ordqueue_new(0);
            let quant_thread = thread::Builder::new().name("quant".into()).spawn_scoped(s, move || {
                self.quantize_frames(quant_queue_recv, remap_queue)
            })?;
            let (write_queue, write_queue_recv) = crossbeam_channel::bounded(0);
            let remap_thread = thread::Builder::new().name("remap".into()).spawn_scoped(s, move || {
                self.remap_frames(remap_queue_recv, write_queue)
            })?;
            let res0 = self.write_frames(write_queue_recv, writer, reporter);
            let res1 = resize_thread.join().map_err(handle_join_error)?;
            let res2 = diff_thread.join().map_err(handle_join_error)?;
            let res3 = quant_thread.join().map_err(handle_join_error)?;
            let res4 = remap_thread.join().map_err(handle_join_error)?;
            combine_res(combine_res(combine_res(res0, res1), combine_res(res2, res3)), res4)
        })
    }

    /// Apply resizing and crate a blurred version for the diff/denoise phase
    fn make_resize(&self, inputs: Receiver<InputFrame>, diff_queue: OrdQueue<InputFrameResized>) -> CatResult<()> {
        minipool::new_scope(self.max_threads.min(if self.settings.speed != Speed::MEDIUM { 6 } else { 4 }.try_into()?), "resize", move || {
            Ok(())
        }, move |abort| {
            for frame in inputs {
                if abort.load(Relaxed) {
                    return Err(Error::Aborted);
                }
                let image = match frame.frame {
                    FrameSource::Pixels(image) => image,
                    #[cfg(feature = "png")]
                    FrameSource::PngData(data) => {
                        let image = lodepng::decode32(&data)
                            .map_err(|err| Error::PNG(format!("Can't load PNG: {err}")))?;
                        Img::new(image.buffer, image.width, image.height)
                    },
                    #[cfg(feature = "png")]
                    FrameSource::Path(path) => {
                        let image = lodepng::decode32_file(&path)
                            .map_err(|err| Error::PNG(format!("Can't load {}: {err}", path.display())))?;
                        Img::new(image.buffer, image.width, image.height)
                    },
                };
                let resized = resized_binary_alpha(image, &self.settings)?;
                let frame_blurred = if self.settings.slow() { smart_blur(resized.as_ref()) } else { less_smart_blur(resized.as_ref()) };
                diff_queue.send(frame.frame_index, InputFrameResized {
                    original_index: frame.frame_index,
                    frame: resized,
                    frame_blurred,
                    presentation_timestamp: frame.presentation_timestamp,
                })?;
            }
            Ok(())
        })
    }

    /// Find differences between frames, and compute importance maps
    fn make_diffs(&self, mut inputs: OrdQueueIter<InputFrameResized>, diffs: Sender<DiffMessage>, reporter: &Mutex<Option<&mut dyn ProgressReporter>>) -> CatResult<()> {
        let first_frame = inputs.next().ok_or(Error::NoFrames)?;

        let mut last_frame_duration = if first_frame.presentation_timestamp > 1. / 100. {
            // this is gifski's weird rule that a non-zero first-frame pts
            // shifts the whole anim and is the delay of the last frame
            LastFrameDuration::FixedOffset(first_frame.presentation_timestamp)
        } else {
            LastFrameDuration::FrameRate(0.)
        };

        let bypass_temporal_filter = self.settings.effective_motion_quality() == 100 && self.settings.color_dithering.is_ordered();
        let mut denoiser = Denoiser::new(first_frame.frame.width(), first_frame.frame.height(), self.settings.effective_motion_quality(), bypass_temporal_filter)?;

        let mut ordinal_frame_number = 0;
        let mut next_original_index_expected = 0;
        let mut last_frame_pts = 0.;
        let mut next_frame = Some(first_frame);
        loop {
            // NB! There are two interleaved loops here:
            //  - one to feed the denoiser
            //  - the other to process denoised frames
            //
            // The denoiser buffers five frames, so these two loops process different frames!
            // But need to be interleaved in one `loop{}` to get frames falling out of denoiser's buffer.

            ////////////////////// Feed denoiser: /////////////////////

            if let Some(InputFrameResized { frame, frame_blurred, original_index, presentation_timestamp: raw_pts }) = next_frame {
                if original_index != next_original_index_expected {
                    if let Some(r) = &mut *reporter.lock().map_err(|_| Error::ThreadSend)? {
                        r.error(format!("expected frame_number {next_original_index_expected}, got {original_index}"));
                    }
                }
                next_original_index_expected = original_index + 1;
                ordinal_frame_number += 1;

                let mut pts = raw_pts - last_frame_duration.shift_every_pts_by();
                if pts < last_frame_pts {
                    if let Some(r) = &mut *reporter.lock().map_err(|_| Error::ThreadSend)? {
                        r.error(format!("expected frame_number {original_index} to have pts > {last_frame_pts:0.3}, got {raw_pts:0.3}"));
                    }
                    pts = last_frame_pts;
                }
                else if let LastFrameDuration::FrameRate(duration) = &mut last_frame_duration {
                    *duration = pts - last_frame_pts;
                }
                last_frame_pts = pts;

                let (width, height) = (frame.width(), frame.height());
                denoiser.push_frame(frame, frame_blurred.as_ref(), (ordinal_frame_number, pts, last_frame_duration)).map_err(|_| {
                    Error::WrongSize(format!("Frame {ordinal_frame_number} has wrong size ({width}×{height})"))
                })?;
            } else {
                denoiser.flush();
            }

            ////////////////////// Consume denoised frames /////////////////////

            match denoiser.pop() {
                Denoised::Done => {
                    debug_assert!(inputs.next().is_none());
                    break;
                },
                Denoised::NotYet => {},
                Denoised::Frame { importance_map, frame: image, meta: (ordinal_frame_number, pts, last_frame_duration) } => {
                    let (importance_map, ..) = importance_map.into_contiguous_buf();
                    diffs.send(DiffMessage {
                        importance_map,
                        ordinal_frame_number,
                        image,
                        pts, frame_duration: last_frame_duration.value().max(1. / 100.),
                    })?;
                },
            }
            next_frame = inputs.next();
        }

        Ok(())
    }

    fn quantize_frames(&self, inputs: Receiver<DiffMessage>, remap_queue: OrdQueue<RemapMessage>) -> CatResult<()> {
        if self.settings.global_palette {
            self.quantize_frames_global(inputs, remap_queue)
        } else {
            self.quantize_frames_local(inputs, remap_queue)
        }
    }

    /// Collect and filter frames from the diff queue for quantization.
    ///
    /// Determines disposal method (`Keep` vs `Background`) depending on whether the next frame
    /// becomes transparent or if this is the last frame of a transparent looped animation.
    /// Deduplicates identical frames and drops unchanged frames when `prev_frame_keeps`.
    fn collect_quantize_messages(&self, inputs: Receiver<DiffMessage>, mut send: impl FnMut(QuantizeMessage) -> CatResult<()>) -> CatResult<()> {
        let mut inputs = inputs.into_iter();
        let next_frame = inputs.next().ok_or(Error::NoFrames)?;

        let DiffMessage { image: first_frame, .. } = &next_frame;
        let first_frame_has_transparency = first_frame.pixels().any(|px| px.a < 128);

        let mut prev_frame_keeps = false;
        let mut frame_index = 0;
        let mut importance_map = None;
        let mut next_frame = Some(next_frame);
        while let Some(DiffMessage { image, pts, frame_duration, ordinal_frame_number, importance_map: new_importance_map }) = next_frame {
            next_frame = inputs.next();

            if importance_map.is_none() {
                importance_map = Some(new_importance_map);
            }

            let dispose = if let Some(DiffMessage { image: next_image, .. }) = &next_frame {
                // Skip identical frames
                if next_image.as_ref() == image.as_ref() {
                    // this keeps importance_map of the previous frame in the identical-frame series
                    // (important, because subsequent identical frames have all-zero importance_map and would be dropped too)
                    continue;
                }

                // If the next frame becomes transparent, this frame has to clear to bg for it
                if next_image.pixels().zip(image.pixels()).any(|(next, curr)| next.a < curr.a) {
                    DisposalMethod::Background
                } else {
                    DisposalMethod::Keep
                }
            } else if first_frame_has_transparency {
                // Last frame should reset to background to avoid breaking transparent looped anims
                DisposalMethod::Background
            } else {
                // macOS preview gets Background wrong
                DisposalMethod::Keep
            };

            let importance_map = importance_map.take().ok_or(Error::ThreadSend)?; // always set at the beginning

            let bypass_drop = self.settings.effective_motion_quality() == 100 && self.settings.color_dithering.is_ordered();
            if !prev_frame_keeps || bypass_drop || importance_map.iter().any(|&px| px > 0) {
                let end_pts = if let Some(&DiffMessage { pts: next_pts, .. }) = next_frame.as_ref() {
                    next_pts
                } else {
                    pts + frame_duration
                };
                debug_assert!(end_pts > 0.);

                send(QuantizeMessage {
                    image,
                    ordinal_frame_number, frame_index,
                    first_frame_has_transparency,
                    importance_map, prev_frame_keeps, dispose, end_pts,
                    has_next_frame: next_frame.is_some(),
                })?;

                frame_index += 1;
                prev_frame_keeps = dispose == DisposalMethod::Keep;
            }
        }
        Ok(())
    }

    /// Clear pixels that didn't change compared to the background when the previous frame keeps its buffer.
    fn clear_unchanged_background(&self, image: &mut ImgVec<RGBA8>, importance_map: &[u8], frame_index: u32, prev_frame_keeps: bool) {
        if prev_frame_keeps && !self.settings.color_dithering.is_ordered() {
            // if denoiser says the background didn't change, then believe it
            // (except higher quality settings, which try to improve it every time)
            let bg_keep_likelihood = u32::from(self.settings.quality.saturating_sub(80) / 4);
            if self.settings.fast() || (self.settings.quality < 100 && (frame_index % 5) >= bg_keep_likelihood) {
                image.pixels_mut().zip(importance_map).filter(|&(_, &m)| m == 0).for_each(|(px, _)| *px = RGBA8::new(0, 0, 0, 0));
            }
        }
    }

    fn quantize_frames_local(&self, inputs: Receiver<DiffMessage>, remap_queue: OrdQueue<RemapMessage>) -> CatResult<()> {
        minipool::new_channel(self.max_threads.min(4.try_into()?), "quant", move |quant_queue| {
            self.collect_quantize_messages(inputs, |msg| quant_queue.send(msg).map_err(From::from))
        }, move |QuantizeMessage { end_pts, mut image, importance_map, ordinal_frame_number, frame_index, dispose, first_frame_has_transparency, prev_frame_keeps, has_next_frame }| {
            self.clear_unchanged_background(&mut image, &importance_map, frame_index, prev_frame_keeps);

            let source_image = if self.settings.color_dithering.is_ordered() {
                Some(image.clone())
            } else {
                None
            };
            let needs_transparency = frame_index > 0 || (frame_index == 0 && first_frame_has_transparency);
            let (liq, remap, liq_image, out_buf) = self.quantize(image, &importance_map, frame_index == 0, needs_transparency, prev_frame_keeps)?;

            Ok(remap_queue.send(frame_index as usize, RemapMessage {
                ordinal_frame_number,
                end_pts,
                dispose,
                liq, remap,
                liq_image,
                out_buf,
                has_next_frame,
                source_image,
            })?)
        })
    }

    fn quantize_frames_global(&self, inputs: Receiver<DiffMessage>, remap_queue: OrdQueue<RemapMessage>) -> CatResult<()> {
        let mut collected = Vec::new();
        self.collect_quantize_messages(inputs, |msg| {
            collected.push(msg);
            Ok(())
        })?;

        if collected.is_empty() {
            return Err(Error::NoFrames);
        }

        let mut liq = self.new_liq_attr(false)?;
        let mut hist = Histogram::new(&liq);
        let total_frames = collected.len();
        let needs_transparency = total_frames > 1 || collected[0].first_frame_has_transparency;
        if needs_transparency {
            // first frame may be transparent too, so it's not just for diffs
            hist.add_fixed_color(RGBA8::new(0, 0, 0, 0), 0.)?;
        }
        // user may have colors which need to be preserved and left undithered
        for color in &self.settings.fixed_colors {
            hist.add_fixed_color(RGBA8::new(color.r, color.g, color.b, 255), 0.)?;
        }

        let mut prepared = Vec::with_capacity(total_frames);
        for mut msg in collected {
            self.clear_unchanged_background(&mut msg.image, &msg.importance_map, msg.frame_index, msg.prev_frame_keeps);
            // only later remapping tracks which area has been damaged by transparency
            // so for previous-transparent background frame the importance map may be invalid
            // because there's a transparent hole in the background not taken into account,
            // and palette may lack colors to fill that hole
            let set_importance_map = msg.frame_index == 0 || msg.prev_frame_keeps;
            let source_image = if self.settings.color_dithering.is_ordered() {
                Some(msg.image.clone())
            } else {
                None
            };
            let mut img = self.new_liq_image(&liq, msg.image, &msg.importance_map, set_importance_map, needs_transparency)?;
            hist.add_image(&liq, &mut img)?;
            prepared.push((msg.ordinal_frame_number, msg.frame_index, msg.end_pts, msg.dispose, msg.has_next_frame, img, source_image));
        }

        let res = hist.quantize(&liq)?;
        let res = self.tune_quantization_result(&mut liq, res, |liq| hist.quantize(liq).map_err(From::from))?;

        minipool::new_channel(self.max_threads.min(4.try_into()?), "quant", move |quant_queue| {
            for (ordinal_frame_number, frame_index, end_pts, dispose, has_next_frame, img, source_image) in prepared {
                quant_queue.send((ordinal_frame_number, frame_index, end_pts, dispose, has_next_frame, img, source_image, res.clone(), liq.clone()))?;
            }
            Ok(())
        }, move |(ordinal_frame_number, frame_index, end_pts, dispose, has_next_frame, mut img, source_image, mut remap, liq)| {
            let out = self.prepare_dither_out(&mut remap, &mut img)?;
            Ok(remap_queue.send(frame_index as usize, RemapMessage {
                ordinal_frame_number,
                end_pts,
                dispose,
                liq,
                remap,
                liq_image: img,
                out_buf: out,
                has_next_frame,
                source_image,
            })?)
        })
    }

    fn remap_frames(&self, mut inputs: OrdQueueIter<RemapMessage>, write_queue: Sender<FrameMessage>) -> CatResult<()> {
        if self.settings.dither_denoise_radius > 0 && self.settings.color_dithering.is_ordered() {
            return self.remap_frames_with_denoise(inputs, write_queue);
        }
        let mut frame_index = 0;
        let first_frame = inputs.next().ok_or(Error::NoFrames)?;
        let mut screen = gif_dispose::Screen::new(first_frame.liq_image.width(), first_frame.liq_image.height(), None);

        #[cfg(debug_assertions)]
        let mut debug_screen = gif_dispose::Screen::new(first_frame.liq_image.width(), first_frame.liq_image.height(), None);

        let mut dither_context = DitherColorContext::new();
        let mut next_frame = Some(first_frame);
        while let Some(RemapMessage {ordinal_frame_number, end_pts, dispose, liq, remap, liq_image, out_buf, has_next_frame, source_image}) = next_frame {
            let pixels = screen.pixels_rgba();
            let screen_width = pixels.width() as u16;
            let screen_height = pixels.height() as u16;
            let mut screen_after_dispose = screen.dispose_only();

            let (mut image8, image8_pal) = {
                let bg = if frame_index != 0 { Some(screen_after_dispose.pixels_rgba()) } else { None };
                self.remap(liq, remap, liq_image, bg, out_buf, source_image, &mut dither_context)?
            };

            let (image8_pal, transparent_index) = transparent_index_from_palette(image8_pal, image8.as_mut());

            #[cfg(debug_assertions)]
            debug_screen.blit(Some(&image8_pal), dispose, 0, 0, image8.as_ref(), transparent_index)?;

            let (left, top) = if frame_index != 0 && has_next_frame {
                let (left, top, new_width, new_height) = trim_image(image8.as_ref(), &image8_pal, transparent_index, dispose, screen_after_dispose.pixels_rgba())
                    .unwrap_or((0, 0, 1, 1));
                if new_width != image8.width() || new_height != image8.height() {
                    let new_buf = image8.sub_image(left.into(), top.into(), new_width, new_height).to_contiguous_buf().0.into_owned();
                    image8 = ImgVec::new(new_buf, new_width, new_height);
                }
                (left, top)
            } else {
                // must keep first and last frame
                (0, 0)
            };

            screen_after_dispose.then_blit(Some(&image8_pal), dispose, left, top, image8.as_ref(), transparent_index)?;

            #[cfg(debug_assertions)]
            debug_assert!(debug_screen.pixels_rgba() == screen.pixels_rgba(), "fr {ordinal_frame_number} {left}/{top} {}x{}", image8.width(), image8.height());

            write_queue.send(FrameMessage {
                frame_index,
                ordinal_frame_number,
                end_pts,
                screen_width,
                screen_height,
                frame: GIFFrame {
                    left,
                    top,
                    image: image8,
                    pal: image8_pal,
                    transparent_index,
                    dispose,
                },
            })?;
            frame_index += 1;
            next_frame = inputs.next();
        }
        Ok(())
    }

    fn remap_frames_with_denoise(&self, mut inputs: OrdQueueIter<RemapMessage>, write_queue: Sender<FrameMessage>) -> CatResult<()> {
        let first_frame = inputs.next().ok_or(Error::NoFrames)?;
        let mut screen = gif_dispose::Screen::new(first_frame.liq_image.width(), first_frame.liq_image.height(), None);

        #[cfg(debug_assertions)]
        let mut debug_screen = gif_dispose::Screen::new(first_frame.liq_image.width(), first_frame.liq_image.height(), None);

        let mut dither_context = DitherColorContext::new();

        let radius = self.settings.dither_denoise_radius;
        let threshold = self.settings.dither_denoise_threshold;
        let exact_match = self.settings.global_palette && self.settings.effective_motion_quality() == 100;

        let remap_one = |msg: RemapMessage, frame_index: usize, dither_context: &mut DitherColorContext| -> CatResult<BufferedRemap> {
            let RemapMessage { ordinal_frame_number, end_pts, dispose, liq, remap, liq_image, out_buf, has_next_frame, source_image } = msg;
            let (raw_image8, image8_pal) = self.remap(liq, remap, liq_image, None, out_buf, source_image.clone(), dither_context)?;
            Ok(BufferedRemap {
                ordinal_frame_number,
                frame_index,
                end_pts,
                dispose,
                has_next_frame,
                source_image,
                raw_image8,
                image8_pal,
            })
        };

        let mut next_input_idx = 0;
        let mut curr = Some(remap_one(first_frame, next_input_idx, &mut dither_context)?);
        next_input_idx += 1;

        let mut future: VecDeque<BufferedRemap> = VecDeque::with_capacity(radius as usize);
        while future.len() < radius as usize {
            if let Some(msg) = inputs.next() {
                future.push_back(remap_one(msg, next_input_idx, &mut dither_context)?);
                next_input_idx += 1;
            } else {
                break;
            }
        }

        let mut history: VecDeque<ImgVec<u8>> = VecDeque::with_capacity(radius as usize);

        while let Some(mut current) = curr {
            let width = current.raw_image8.width();
            let height = current.raw_image8.height();

            let effective_radius = if history.is_empty() || future.is_empty() {
                0
            } else if radius == 2 && history.len() >= 2 && future.len() >= 2 {
                2
            } else {
                1
            };

            if effective_radius > 0 {
                let trans_idx = current.image8_pal.iter().position(|p| p.a <= 128).map(|i| i as u8);
                let (prev_slices, next_slices): (Vec<&[u8]>, Vec<&[u8]>) = if effective_radius == 1 {
                    (vec![history.back().unwrap().buf().as_slice()], vec![future.front().unwrap().raw_image8.buf().as_slice()])
                } else {
                    (
                        vec![history[1].buf().as_slice(), history[0].buf().as_slice()],
                        vec![future[0].raw_image8.buf().as_slice(), future[1].raw_image8.buf().as_slice()],
                    )
                };

                denoise_dithered_frame(
                    current.raw_image8.buf_mut(),
                    width,
                    height,
                    &prev_slices,
                    &next_slices,
                    &current.image8_pal,
                    effective_radius,
                    threshold,
                    trans_idx,
                );
            }

            // Save cleaned raw frame to history before background transparency is applied
            history.push_back(current.raw_image8.clone());
            if history.len() > radius as usize {
                history.pop_front();
            }

            let pixels = screen.pixels_rgba();
            let screen_width = pixels.width() as u16;
            let screen_height = pixels.height() as u16;
            let mut screen_after_dispose = screen.dispose_only();

            // Transparency matching against background screen
            let trans_idx = current.image8_pal.iter().position(|p| p.a <= 128).map(|i| i as u8);
            if current.frame_index != 0 {
                if let Some(trans_idx) = trans_idx {
                    let bg = screen_after_dispose.pixels_rgba();
                    let src_img = current.source_image.as_ref().expect("source_image required for ordered color dithering");
                    for y in 0..height {
                        let out_row = &mut current.raw_image8.buf_mut()[y * width..(y + 1) * width];
                        let src_row = &src_img.buf()[y * src_img.stride()..y * src_img.stride() + width];
                        let bg_row = &bg.buf()[y * bg.stride()..y * bg.stride() + width];
                        apply_background_transparency(out_row, src_row, bg_row, &current.image8_pal, trans_idx, exact_match);
                    }
                }
            }

            let (image8_pal, transparent_index) = transparent_index_from_palette(current.image8_pal, current.raw_image8.as_mut());

            #[cfg(debug_assertions)]
            debug_screen.blit(Some(&image8_pal), current.dispose, 0, 0, current.raw_image8.as_ref(), transparent_index)?;

            let mut image8 = current.raw_image8;
            let (left, top) = if current.frame_index != 0 && current.has_next_frame {
                let (left, top, new_width, new_height) = trim_image(image8.as_ref(), &image8_pal, transparent_index, current.dispose, screen_after_dispose.pixels_rgba())
                    .unwrap_or((0, 0, 1, 1));
                if new_width != image8.width() || new_height != image8.height() {
                    let new_buf = image8.sub_image(left.into(), top.into(), new_width, new_height).to_contiguous_buf().0.into_owned();
                    image8 = ImgVec::new(new_buf, new_width, new_height);
                }
                (left, top)
            } else {
                (0, 0)
            };

            screen_after_dispose.then_blit(Some(&image8_pal), current.dispose, left, top, image8.as_ref(), transparent_index)?;

            #[cfg(debug_assertions)]
            debug_assert!(debug_screen.pixels_rgba() == screen.pixels_rgba(), "fr {} {}/{} {}x{}", current.ordinal_frame_number, left, top, image8.width(), image8.height());

            write_queue.send(FrameMessage {
                frame_index: current.frame_index,
                ordinal_frame_number: current.ordinal_frame_number,
                end_pts: current.end_pts,
                screen_width,
                screen_height,
                frame: GIFFrame {
                    left,
                    top,
                    image: image8,
                    pal: image8_pal,
                    transparent_index,
                    dispose: current.dispose,
                },
            })?;

            // Advance curr from future
            curr = future.pop_front();
            if let Some(msg) = inputs.next() {
                future.push_back(remap_one(msg, next_input_idx, &mut dither_context)?);
                next_input_idx += 1;
            }
        }

        Ok(())
    }
}

fn transparent_index_from_palette(mut image8_pal: Vec<RGBA8>, mut image8: ImgRefMut<u8>) -> (Vec<RGB8>, Option<u8>) {
    // Palette may have multiple transparent indices :(
    let mut transparent_index = None;
    for (i, p) in image8_pal.iter_mut().enumerate() {
        if p.a <= 128 {
            *p = RGBA8::new(71, 80, 76, 0);
            let new_index = i as u8;
            if let Some(old_index) = transparent_index {
                image8.pixels_mut().filter(|px| **px == new_index).for_each(|px| *px = old_index);
            } else {
                transparent_index = Some(new_index);
            }
        }
    }

    // Check that palette is fine and has no duplicate transparent indices
    debug_assert!(image8_pal.iter().enumerate().all(|(idx, color)| {
        Some(idx as u8) == transparent_index || color.a > 128 || !image8.pixels().any(|px| px == idx as u8)
    }));

    (image8_pal.into_iter().map(|r| r.rgb()).collect(), transparent_index)
}

/// When one thread unexpectedly fails, all other threads fail with Aborted, but that Aborted isn't the relevant cause
#[inline]
fn combine_res(res1: Result<(), Error>, res2: Result<(), Error>) -> Result<(), Error> {
    use Error::*;
    match (res1, res2) {
        (Err(e), Ok(())) | (Ok(()), Err(e)) => Err(e),
        (Err(ThreadSend), res) | (res, Err(ThreadSend)) => res,
        (Err(Aborted), res) | (res, Err(Aborted)) => res,
        (Err(NoFrames), res) | (res, Err(NoFrames)) => res,
        (_, res2) => res2,
    }
}

fn trim_image(mut image_trimmed: ImgRef<u8>, image8_pal: &[RGB8], transparent_index: Option<u8>, dispose: DisposalMethod, mut screen: ImgRef<RGBA8>) -> Option<(u16, u16, usize, usize)> {
    debug_assert_eq!(image_trimmed.width(), screen.width());
    debug_assert_eq!(image_trimmed.height(), screen.height());

    let is_matching_pixel = move |px: u8, bg: RGBA8| -> bool {
        if Some(px) == transparent_index {
            if dispose == DisposalMethod::Keep {
                // if dispose == keep, then transparent pixels do nothing, so they can be cropped out
                true
            } else {
                debug_assert_eq!(dispose, DisposalMethod::Background);
                // if disposing to background, then transparent pixels paint transparency, so bg has to actually be transparent to match
                bg.a == 0
            }
        } else {
            let Some(pal_px) = image8_pal.get(px as usize) else {
                debug_assert!(false, "{px} > {}", image8_pal.len());
                return false;
            };
            pal_px.with_alpha(255) == bg
        }
    };

    let bottom = image_trimmed.rows().zip(screen.rows()).rev()
        .take_while(|(img_row, screen_row)| {
            img_row.iter().copied().zip(screen_row.iter().copied())
                .all(|(px, bg)| is_matching_pixel(px, bg))
        })
        .count();

    if bottom > 0 {
        if bottom == image_trimmed.height() {
            return None;
        }
        image_trimmed = image_trimmed.sub_image(0, 0, image_trimmed.width(), image_trimmed.height() - bottom);
        screen = screen.sub_image(0, 0, screen.width(), screen.height() - bottom);
    }

    let top = image_trimmed.rows().zip(screen.rows())
        .take_while(|(img_row, screen_row)| {
            img_row.iter().copied().zip(screen_row.iter().copied())
                .all(|(px, bg)| is_matching_pixel(px, bg))
        })
        .count();

    if top > 0 {
        debug_assert_ne!(image_trimmed.height(), top);
        image_trimmed = image_trimmed.sub_image(0, top, image_trimmed.width(), image_trimmed.height() - top);
        screen = screen.sub_image(0, top, screen.width(), screen.height() - top);
    }

    let left = (0..image_trimmed.width() - 1)
        .take_while(|&x| {
            (0..image_trimmed.height()).all(|y| {
                let px = image_trimmed[(x, y)];
                is_matching_pixel(px, screen[(x, y)])
            })
        }).count();
    if left > 0 {
        debug_assert!(image_trimmed.width() > left);
        image_trimmed = image_trimmed.sub_image(left, 0, image_trimmed.width() - left, image_trimmed.height());
        screen = screen.sub_image(left, 0, screen.width() - left, screen.height());
    }
    debug_assert_eq!(image_trimmed.width(), screen.width());
    debug_assert_eq!(image_trimmed.height(), screen.height());

    let right = (1..image_trimmed.width()).rev()
        .take_while(|&x| {
            (0..image_trimmed.height()).all(|y| {
                let px = image_trimmed[(x, y)];
                is_matching_pixel(px, screen[(x, y)])
            })
        }).count();
    if right > 0 {
        debug_assert!(image_trimmed.width() > right);
        image_trimmed = image_trimmed.sub_image(0, 0, image_trimmed.width() - right, image_trimmed.height());
    }

    Some((left as _, top as _, image_trimmed.width(), image_trimmed.height()))
}

trait PushInCapacity<T> {
    fn push_in_cap(&mut self, val: T);
}

impl<T> PushInCapacity<T> for Vec<T> {
    #[inline(always)]
    #[cfg_attr(debug_assertions, track_caller)]
    fn push_in_cap(&mut self, val: T) {
        debug_assert_ne!(self.capacity(), self.len());
        if self.capacity() != self.len() {
            self.push(val);
        }
    }
}

#[cold]
fn handle_join_error(err: Box<dyn std::any::Any + Send>) -> Error {
    let msg = err.downcast_ref::<String>().map(|s| s.as_str())
    .or_else(|| err.downcast_ref::<&str>().copied()).unwrap_or("unknown panic");
    eprintln!("thread crashed (this is a bug): {msg}");
    Error::ThreadSend
}

#[test]
fn sendable() {
    fn is_send<T: Send>() {}
    is_send::<Collector>();
    is_send::<Writer>();
}
