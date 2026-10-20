use crate::BinResult;
use gifski::Collector;

/// Default frame rate for image sequences (e.g. PNG) which lack timing metadata.
pub const DEFAULT_FPS: f32 = 20.;
/// Maximum default frame rate for video sources when --fps is omitted, to prevent slow-motion due to GIF centisecond limits.
pub const MAX_DEFAULT_VIDEO_FPS: f32 = 50.;
pub trait Source {
    fn total_frames(&self) -> Option<u64>;
    fn collect(&mut self, dest: &mut Collector) -> BinResult<()>;
}

#[derive(Debug, Copy, Clone)]
pub struct Fps {
    /// output rate
    pub fps: Option<f32>,
    /// skip frames
    pub speed: f32,
}
