use crate::source::{Fps, Source, MAX_DEFAULT_VIDEO_FPS};
use crate::{BinResult, SrcPath};
use gifski::Collector;
use imgref::ImgVec;
use std::io::{BufReader, Read};
use y4m::{Colorspace, Decoder, ParseError};
use yuv::YUV;
use yuv::color::{MatrixCoefficients, Range};
use yuv::convert::RGBConvert;

pub struct Y4MDecoder {
    fps: Fps,
    in_color_space: Option<MatrixCoefficients>,
    decoder: Decoder<Box<BufReader<dyn Read>>>,
    file_size: Option<u64>,
}

impl Y4MDecoder {
    pub fn new(src: SrcPath, fps: Fps, in_color_space: Option<MatrixCoefficients>) -> BinResult<Self> {
        let mut file_size = None;
        let reader = match src {
            SrcPath::Path(path) => {
                let f = std::fs::File::open(path)?;
                let m = f.metadata()?;
                #[cfg(unix)] {
                    use std::os::unix::fs::MetadataExt;
                    file_size = Some(m.size());
                }
                #[cfg(windows)] {
                    use std::os::windows::fs::MetadataExt;
                    file_size = Some(m.file_size());
                }
                Box::new(BufReader::new(f)) as Box<BufReader<dyn Read>>
            },
            SrcPath::Stdin(buf) => Box::new(buf) as Box<BufReader<dyn Read>>,
        };

        let decoder = Decoder::new(reader).map_err(|e| match e {
            y4m::Error::EOF => "The y4m file is truncated or invalid",
            y4m::Error::BadInput => "The y4m file contains invalid metadata",
            y4m::Error::UnknownColorspace => "y4m uses an unusual color format that is not supported",
            y4m::Error::OutOfMemory => "Out of memory, or the y4m file has bogus dimensions",
            y4m::Error::ParseError(ParseError::InvalidY4M) => "The input is not a y4m file",
            y4m::Error::ParseError(error) => return format!("y4m contains invalid data: {error}"),
            y4m::Error::IoError(error) => return format!("I/O error when reading a y4m file: {error}"),
        }.to_string())?;

        if fps.fps.is_none() {
            let framerate = decoder.get_framerate();
            if framerate.num == 0 || framerate.den == 0 {
                return Err("Video has no framerate metadata; specify --fps explicitly".into());
            }
        }

        Ok(Self {
            file_size,
            fps,
            in_color_space,
            decoder,
        })
    }
}

enum Samp {
    Mono,
    S1x1,
    S2x1,
    S2x2,
}

impl Source for Y4MDecoder {
    fn total_frames(&self) -> Option<u64> {
        self.file_size.map(|file_size| {
            let w = self.decoder.get_width();
            let h = self.decoder.get_height();
            let d = self.decoder.get_bytes_per_sample();
            let s = match self.decoder.get_colorspace() {
                Colorspace::Cmono => 4,
                Colorspace::Cmono12 => 4,
                Colorspace::C420 => 6,
                Colorspace::C420p10 => 6,
                Colorspace::C420p12 => 6,
                Colorspace::C420jpeg => 6,
                Colorspace::C420paldv => 6,
                Colorspace::C420mpeg2 => 6,
                Colorspace::C422 => 8,
                Colorspace::C422p10 => 8,
                Colorspace::C422p12 => 8,
                Colorspace::C444 => 12,
                Colorspace::C444p10 => 12,
                Colorspace::C444p12 => 12,
                _ => 12,
            };
            let raw_frames = file_size.saturating_sub(self.decoder.get_raw_params().len() as _) / (w * h * d * s / 4 + 6) as u64;
            let fps = self.decoder.get_framerate();
            if fps.num > 0 && fps.den > 0 {
                let source_fps = fps.num as f64 / fps.den as f64;
                let wanted_fps = match self.fps.fps {
                    Some(wanted) => f64::from(wanted),
                    None => source_fps.min(MAX_DEFAULT_VIDEO_FPS as f64),
                };
                let ratio = (wanted_fps / source_fps).min(1.0);
                (raw_frames as f64 * ratio).ceil() as u64
            } else {
                raw_frames
            }
        })
    }

    fn collect(&mut self, c: &mut Collector) -> BinResult<()> {
        let fps = self.decoder.get_framerate();
        let source_fps = if fps.num > 0 && fps.den > 0 {
            fps.num as f64 / fps.den as f64
        } else if let Some(wanted) = self.fps.fps {
            f64::from(wanted)
        } else {
            return Err("Video has no framerate metadata; specify --fps explicitly".into());
        };
        let frame_time = 1. / source_fps;
        let wanted_fps = match self.fps.fps {
            Some(wanted) => f64::from(wanted),
            None => source_fps.min(MAX_DEFAULT_VIDEO_FPS as f64),
        };
        let wanted_frame_time = 1. / wanted_fps;
        let skip_frames = match self.fps.fps {
            Some(wanted) => (f64::from(wanted) - source_fps).abs() > 1e-4,
            None => source_fps > MAX_DEFAULT_VIDEO_FPS as f64,
        };
        let width = self.decoder.get_width();
        let height = self.decoder.get_height();
        let raw_params_str = &*String::from_utf8_lossy(self.decoder.get_raw_params()).into_owned();
        let range = raw_params_str.split_once("COLORRANGE=").map(|(_, r)| {
            if r.starts_with("FULL") { Range::Full } else { Range::Limited }
        });

        let matrix = self.in_color_space.unwrap_or({
            if height <= 480 && width <= 720 { MatrixCoefficients::BT601 } else { MatrixCoefficients::BT709 }
        });

        let (samp, conv) = match self.decoder.get_colorspace() {
            Colorspace::Cmono => (Samp::Mono, RGBConvert::<u8>::new(range.unwrap_or(Range::Limited), MatrixCoefficients::Identity)),
            Colorspace::Cmono12 => return Err("Y4M with Cmono12 is not supported yet".into()),
            Colorspace::C420 => (Samp::S2x2, RGBConvert::<u8>::new(range.unwrap_or(Range::Limited), matrix)),
            Colorspace::C420p10 => return Err("Y4M with C420p10 is not supported yet".into()),
            Colorspace::C420p12 => return Err("Y4M with C420p12 is not supported yet".into()),
            Colorspace::C420jpeg => (Samp::S2x2, RGBConvert::<u8>::new(range.unwrap_or(Range::Limited), matrix)),
            Colorspace::C420paldv => (Samp::S2x2, RGBConvert::<u8>::new(range.unwrap_or(Range::Limited), matrix)),
            Colorspace::C420mpeg2 => (Samp::S2x2, RGBConvert::<u8>::new(range.unwrap_or(Range::Limited), matrix)),
            Colorspace::C422 => (Samp::S2x1, RGBConvert::<u8>::new(range.unwrap_or(Range::Limited), matrix)),
            Colorspace::C422p10 => return Err("Y4M with C422p10 is not supported yet".into()),
            Colorspace::C422p12 => return Err("Y4M with C422p12 is not supported yet".into()),
            Colorspace::C444 => (Samp::S1x1, RGBConvert::<u8>::new(range.unwrap_or(Range::Limited), matrix)),
            Colorspace::C444p10 => return Err("Y4M with C444p10 is not supported yet".into()),
            Colorspace::C444p12 => return Err("Y4M with C444p12 is not supported yet".into()),
            _ => return Err(format!("Y4M uses unsupported color mode {raw_params_str}").into()),
        };
        let conv = conv?;
        if width == 0 || width > u16::MAX as _ || height == 0 || height > u16::MAX as _ {
            return Err("Video too large".into());
        }

        #[cold]
        fn bad_frame(mode: &str) -> BinResult<()> {
            Err(format!("Bad Y4M frame (using {mode})").into())
        }

        let mut idx = 0;
        let mut presentation_timestamp = 0.0;
        let mut wanted_pts = 0.0;
        loop {
            match self.decoder.read_frame() {
                Ok(frame) => {
                    let this_frame_pts = presentation_timestamp / f64::from(self.fps.speed);
                    presentation_timestamp += frame_time;
                    if skip_frames && presentation_timestamp < wanted_pts {
                        continue; // skip a frame
                    }
                    wanted_pts += wanted_frame_time;

                    let y = frame.get_y_plane();
                    if y.is_empty() {
                        return bad_frame(raw_params_str);
                    }
                    let u = frame.get_u_plane();
                    let v = frame.get_v_plane();
                    if v.len() != u.len() {
                        return bad_frame(raw_params_str);
                    }

                    let mut out = Vec::new();
                    out.try_reserve(width * height)?;
                    match samp {
                        Samp::Mono => todo!(),
                        Samp::S1x1 => {
                            if v.len() != y.len() {
                                return bad_frame(raw_params_str);
                            }

                            let y = y.chunks_exact(width);
                            let u = u.chunks_exact(width);
                            let v = v.chunks_exact(width);
                            if y.len() != v.len() {
                                return bad_frame(raw_params_str);
                            }
                            for (y, (u, v)) in y.zip(u.zip(v)) {
                                out.extend(
                                    y.iter().copied().zip(u.iter().copied().zip(v.iter().copied()))
                                    .map(|(y, (u, v))| {
                                        conv.to_rgb(YUV {y, u, v}).with_alpha(255)
                                    }));
                            }
                        },
                        Samp::S2x1 => {
                            let y = y.chunks_exact(width);
                            let u = u.chunks_exact(width.div_ceil(2));
                            let v = v.chunks_exact(width.div_ceil(2));
                            if y.len() != v.len() {
                                return bad_frame(raw_params_str);
                            }
                            for (y, (u, v)) in y.zip(u.zip(v)) {
                                let u = u.iter().copied().flat_map(|x| [x, x]);
                                let v = v.iter().copied().flat_map(|x| [x, x]);
                                out.extend(
                                    y.iter().copied().zip(u.zip(v))
                                    .map(|(y, (u, v))| {
                                        conv.to_rgb(YUV {y, u, v}).with_alpha(255)
                                    }));
                            }
                        },
                        Samp::S2x2 => {
                            let y = y.chunks_exact(width);
                            let u = u.chunks_exact(width.div_ceil(2)).flat_map(|r| [r, r]);
                            let v = v.chunks_exact(width.div_ceil(2)).flat_map(|r| [r, r]);
                            for (y, (u, v)) in y.zip(u.zip(v)) {
                                let u = u.iter().copied().flat_map(|x| [x, x]);
                                let v = v.iter().copied().flat_map(|x| [x, x]);
                                out.extend(
                                    y.iter().copied().zip(u.zip(v))
                                    .map(|(y, (u, v))| {
                                        conv.to_rgb(YUV {y, u, v}).with_alpha(255)
                                    }));
                            }
                        },
                    }
                    if out.len() != width * height {
                        return bad_frame(raw_params_str);
                    }
                    let pixels = ImgVec::new(out, width, height);

                    c.add_frame_rgba(idx, pixels, this_frame_pts)?;
                    idx += 1;
                },
                Err(y4m::Error::EOF) => break,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TempY4m(PathBuf);
    impl Drop for TempY4m {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn create_test_y4m(fps_num: u32, fps_den: u32) -> TempY4m {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("gifski_test_{}_{count}.y4m", std::process::id()));
        let mut file = std::fs::File::create(&path).expect("failed to create temp file");
        let header = format!("YUV4MPEG2 W2 H2 F{fps_num}:{fps_den} Ip A0:0 C420\nFRAME\n\0\0\0\0\0\0");
        file.write_all(header.as_bytes()).expect("failed to write y4m header");
        file.flush().expect("failed to flush");
        TempY4m(path)
    }

    #[test]
    fn test_y4m_missing_framerate_fails_without_fps() {
        let temp = create_test_y4m(0, 0);
        let res = Y4MDecoder::new(SrcPath::Path(temp.0.clone()), Fps { fps: None, speed: 1.0 }, None);
        assert!(res.is_err());
        let err_msg = res.err().unwrap().to_string();
        assert!(err_msg.contains("Video has no framerate metadata; specify --fps explicitly"), "unexpected error: {err_msg}");
    }

    #[test]
    fn test_y4m_missing_framerate_succeeds_with_explicit_fps() {
        let temp = create_test_y4m(0, 0);
        let res = Y4MDecoder::new(SrcPath::Path(temp.0.clone()), Fps { fps: Some(25.0), speed: 1.0 }, None);
        assert!(res.is_ok());
    }

    #[test]
    fn test_y4m_valid_framerate_succeeds_without_fps() {
        let temp = create_test_y4m(24, 1);
        let res = Y4MDecoder::new(SrcPath::Path(temp.0.clone()), Fps { fps: None, speed: 1.0 }, None);
        assert!(res.is_ok());
    }
}
