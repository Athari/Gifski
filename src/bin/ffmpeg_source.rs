use crate::source::{Fps, Source, MAX_DEFAULT_VIDEO_FPS};
use crate::{BinResult, SrcPath};
use gifski::{Collector, Settings};
use imgref::*;
use rgb::*;

fn stream_framerate(stream: &ffmpeg::format::stream::Stream) -> Option<f32> {
    let avg = stream.avg_frame_rate();
    if avg.numerator() > 0 && avg.denominator() > 0 {
        return Some(avg.numerator() as f32 / avg.denominator() as f32);
    }
    let r = stream.rate();
    if r.numerator() > 0 && r.denominator() > 0 {
        return Some(r.numerator() as f32 / r.denominator() as f32);
    }
    None
}

pub struct FfmpegDecoder {
    input_context: ffmpeg::format::context::Input,
    frames: u64,
    effective_fps: f32,
    speed: f32,
    settings: Settings,
}

impl Source for FfmpegDecoder {
    fn total_frames(&self) -> Option<u64> {
        Some(self.frames)
    }

    fn collect(&mut self, dest: &mut Collector) -> BinResult<()> {
        self.collect_frames(dest)
    }
}

impl FfmpegDecoder {
    pub fn new(src: SrcPath, rate: Fps, settings: Settings) -> BinResult<Self> {
        ffmpeg::init().map_err(|e| format!("Unable to initialize ffmpeg: {e}"))?;
        let input_context = match src {
            SrcPath::Path(path) => ffmpeg::format::input(&path)
                .map_err(|e| format!("Unable to open video file {}: {}", path.display(), e))?,
            SrcPath::Stdin(_) => return Err("Video files must be specified as a path on disk. Input via stdin is not supported".into()),
        };

        let stream = input_context.streams().best(ffmpeg::media::Type::Video).ok_or("The file has no video tracks")?;
        let effective_fps = match rate.fps {
            Some(fps) => fps,
            None => {
                let source_fps = stream_framerate(&stream).ok_or("Video has no framerate metadata; specify --fps explicitly")?;
                source_fps.min(MAX_DEFAULT_VIDEO_FPS)
            }
        };

        let filter_fps = effective_fps / rate.speed;
        let time_base = f64::from(stream.time_base().numerator()) / f64::from(stream.time_base().denominator());
        let frames = (stream.duration() as f64 * time_base * f64::from(filter_fps)).ceil().max(0.0) as u64;
        Ok(Self { input_context, frames, effective_fps, speed: rate.speed, settings })
    }

    #[inline(never)]
    pub fn collect_frames(&mut self, dest: &mut Collector) -> BinResult<()> {
        let (stream_index, mut decoder, mut filter) = {
            let filter_fps = self.effective_fps / self.speed;
            let stream = self.input_context.streams().best(ffmpeg::media::Type::Video).ok_or("The file has no video tracks")?;

            let mut codec_context = ffmpeg::codec::context::Context::new();
            codec_context.set_parameters(stream.parameters())?;
            let decoder = codec_context.decoder().video().map_err(|e| format!("Unable to decode the codec used in the video: {e}"))?;

            let (dest_width, dest_height) = self.settings.dimensions_for_image(decoder.width() as _, decoder.height() as _);

            let buffer_args = format!("width={}:height={}:video_size={}x{}:pix_fmt={}:time_base={}:sar={}",
                dest_width,
                dest_height,
                decoder.width(),
                decoder.height(),
                decoder.format().descriptor().ok_or("ffmpeg format error")?.name(),
                stream.time_base(),
                (|sar: ffmpeg::util::rational::Rational| match sar.numerator() {
                    0 => "1".to_string(),
                    _ => format!("{}/{}", sar.numerator(), sar.denominator()),
                })(decoder.aspect_ratio()),
            );
            let mut filter = ffmpeg::filter::Graph::new();
            filter.add(&ffmpeg::filter::find("buffer").ok_or("ffmpeg format error")?, "in", &buffer_args)?;
            filter.add(&ffmpeg::filter::find("buffersink").ok_or("ffmpeg format error")?, "out", "")?;
            filter.output("in", 0)?.input("out", 0)?.parse(&format!("fps=fps={filter_fps},format=rgba"))?;
            filter.validate()?;
            (stream.index(), decoder, filter)
        };

        let add_frame = |rgba_frame: &ffmpeg::util::frame::Video, pts: f64, pos: i64| -> BinResult<()> {
            let stride = rgba_frame.stride(0);
            if stride % 4 != 0 {
                Err("incompatible video")?;
            }
            let rgba_frame = ImgVec::new_stride(
                rgba_frame.data(0).as_rgba().to_owned(),
                rgba_frame.width() as usize,
                rgba_frame.height() as usize,
                stride / 4,
            );
            Ok(dest.add_frame_rgba(pos as usize, rgba_frame, pts)?)
        };

        let mut vid_frame = ffmpeg::util::frame::Video::empty();
        let mut filt_frame = ffmpeg::util::frame::Video::empty();
        let mut i = 0;
        let mut pts_last_packet = 0;
        let pts_frame_step = 1.0 / f64::from(self.effective_fps);

        let packets = self.input_context.packets().filter_map(|(s, packet)| {
            if s.index() != stream_index {
                // ignore irrelevant streams
                None
            } else {
                pts_last_packet = packet.pts()? + packet.duration();
                Some(packet)
            }
        })
        // extra packet to flush remaining frames
        .chain(std::iter::once(ffmpeg::Packet::empty()));

        for packet in packets {
            decoder.send_packet(&packet)?;
            loop {
                match decoder.receive_frame(&mut vid_frame) {
                    Ok(()) => (),
                    Err(ffmpeg::Error::Other { errno: ffmpeg::error::EAGAIN } | ffmpeg::Error::Eof) => break,
                    Err(e) => return Err(Box::new(e)),
                }
                filter.get("in").ok_or("ffmpeg format error")?.source().add(&vid_frame)?;
                let mut out = filter.get("out").ok_or("ffmpeg format error")?;
                let mut out = out.sink();
                while out.frame(&mut filt_frame).is_ok() {
                    add_frame(&filt_frame, pts_frame_step * i as f64, i)?;
                    i += 1;
                }
            }
        }

        // now flush filter's buffer
        filter.get("in").ok_or("ffmpeg format error")?.source().close(pts_last_packet)?;
        let mut out = filter.get("out").ok_or("ffmpeg format error")?;
        let mut out = out.sink();
        while out.frame(&mut filt_frame).is_ok() {
            add_frame(&filt_frame, pts_frame_step * i as f64, i)?;
            i += 1;
        }
        Ok(())
    }
}
