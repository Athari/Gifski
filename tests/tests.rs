use gifski::{Settings, new, progress};
use imgref::{ImgRef, ImgRefMut, ImgVec};
use rgb::{ComponentMap, RGBA8};
use std::path::{Path, PathBuf};

#[test]
fn n_frames() {
    for num_frames in 1..=11 {
        assert_anim_eq(num_frames, frame_filename, None, 0.8);
    }
}

fn assert_anim_eq(num_frames: usize, frame_filename: fn(usize) -> PathBuf, frame_edit: Option<fn(usize, ImgRefMut<RGBA8>)>, max_diff: f64) {
    assert_anim_eq_with_settings(Settings::default(), num_frames, frame_filename, frame_edit, max_diff);
}

fn assert_global_anim_eq(num_frames: usize, frame_filename: fn(usize) -> PathBuf, frame_edit: Option<fn(usize, ImgRefMut<RGBA8>)>, max_diff: f64) {
    assert_anim_eq_with_settings(
        Settings { global_palette: true, ..Settings::default() },
        num_frames, frame_filename, frame_edit, max_diff,
    );
}

fn assert_anim_eq_with_settings(
    settings: Settings,
    num_frames: usize,
    frame_filename: fn(usize) -> PathBuf,
    frame_edit: Option<fn(usize, ImgRefMut<RGBA8>)>,
    max_diff: f64,
) {
    let (c, w) = new(settings.clone()).unwrap();

    let t = std::thread::spawn(move || {
        for n in 0..num_frames {
            let pts = if num_frames == 1 { 0.1 } else { n as f64 / 10. };
            let name = frame_filename(n);
            if let Some(frame_edit) = frame_edit {
                let mut frame = load_frame(&name);
                frame_edit(n, frame.as_mut());
                c.add_frame_rgba(n, frame, pts).unwrap();
            } else {
                c.add_frame_png_file(n, name, pts).unwrap();
            }
        }
    });

    let mut out = Vec::new();
    w.write(&mut out, &mut progress::NoProgress {}).unwrap();
    t.join().unwrap();

    if settings.global_palette {
        let mut opts = gif::DecodeOptions::new();
        opts.set_color_output(gif::ColorOutput::Indexed);
        let decoder = opts.read_info(&out[..]).unwrap();
        let pal = decoder.global_palette().expect("Global palette should be present in header");
        assert!(!pal.is_empty(), "Global palette should not be empty");
        assert_eq!(pal.len() % 3, 0);
        if let Some(max_colors) = settings.max_colors {
            assert!(pal.len() / 3 <= usize::from(max_colors).next_power_of_two());
        }
    }

    let mut n = 0;
    let mut frames_seen = 0;
    for_each_frame(&out, |delay, frame, actual| {
        if settings.global_palette {
            assert!(frame.palette.is_none(), "Frame {frames_seen} must not have a local palette");
        } else if let Some(max_colors) = settings.max_colors {
            let pal = frame.palette.as_ref().expect("Local palette should be present");
            assert!(pal.len() / 3 <= usize::from(max_colors).next_power_of_two());
        }
        frames_seen += 1;
        let next_n = delay as usize / 10;
        while n < next_n {
            let name = frame_filename(n);
            let mut expected = load_frame(&name);
            if let Some(frame_edit) = frame_edit {
                frame_edit(n, expected.as_mut());
            }
            assert_images_eq(expected.as_ref(), actual, max_diff, format_args!("n={n}/{num_frames}, {delay}/{next_n}, {}", name.display()));
            n += 1;
        }
    });
    assert!(n == num_frames, "{frames_seen} : {num_frames}");
}

fn load_frame(name: &Path) -> ImgVec<RGBA8> {
    let img = lodepng::decode32_file(name).unwrap();
    ImgVec::new(img.buffer, img.width, img.height)
}

#[test]
fn all_dupe_frames() {
    let (c, w) = new(Settings::default()).unwrap();

    let t = std::thread::spawn(move || {
        c.add_frame_png_file(0, frame_filename(1), 0.1).unwrap();
        c.add_frame_png_file(1, frame_filename(1), 1.2).unwrap();
        c.add_frame_png_file(2, frame_filename(1), 1.3).unwrap();
    });

    let mut out = Vec::new();
    w.write(&mut out, &mut progress::NoProgress {}).unwrap();
    t.join().unwrap();

    let mut n = 0;
    let mut delays = vec![];
    for_each_frame(&out, |delay, frame, actual| {
        let expected = lodepng::decode32_file(frame_filename(1)).unwrap();
        let expected = ImgVec::new(expected.buffer, expected.width, expected.height);
        assert_images_eq(expected.as_ref(), actual, 0., format_args!("n={n}, {delay} "));
        delays.push(frame.delay);
        n += 1;
    });
    assert_eq!(delays, [130]);
}

#[test]
fn errors() {
    let (c, w) = new(Settings::default()).unwrap();

    #[derive(Debug)]
    struct Errors(Vec<String>);

    impl progress::ProgressReporter for Errors {
        fn increase(&mut self) -> bool { true }
        fn error(&mut self, msg: String) {
            self.0.push(msg);
        }
    }

    let t = std::thread::spawn(move || {
        c.add_frame_png_file(1, frame_filename(1), 0.0).unwrap();
        c.add_frame_png_file(1, frame_filename(1), 2.0).unwrap();
        c.add_frame_png_file(2, frame_filename(1), 1.1).unwrap();
    });

    let mut out = Vec::new();
    let mut errs = Errors(vec![]);
    w.write(&mut out, &mut errs).unwrap();
    t.join().unwrap();

    // Due to concurrent processing, the exact errors and their count may vary
    // depending on thread scheduling. We verify that the error detection works
    // by checking for expected error patterns rather than exact messages.
    assert!(!errs.0.is_empty(), "expected errors to be reported for invalid frame sequence");

    // Check that we detect skipped frame index (expected 0, got 1)
    let has_skipped_frame_error = errs.0.iter().any(|e| e.contains("expected frame_number 0, got 1"));
    assert!(has_skipped_frame_error, "should detect skipped frame index: {errs:#?}");

    // Check that we detect duplicate frame index (expected N, got 1)
    let has_duplicate_index_error = errs.0.iter().any(|e|
        e.contains("got 1") && e.contains("expected frame_number") && !e.contains("expected frame_number 0")
    );
    assert!(has_duplicate_index_error, "should detect duplicate frame index: {errs:#?}");

    // Check that we detect non-monotonic pts (pts should increase)
    let has_pts_error = errs.0.iter().any(|e| e.contains("to have pts >"));
    assert!(has_pts_error, "should detect non-monotonic presentation timestamp: {errs:#?}");
}

#[test]
fn all_but_one_dupe_frames() {
    let (c, w) = new(Settings::default()).unwrap();

    let t = std::thread::spawn(move || {
        c.add_frame_png_file(0, frame_filename(0), 0.0).unwrap();
        c.add_frame_png_file(1, frame_filename(1), 1.2).unwrap();
        c.add_frame_png_file(2, frame_filename(1), 1.3).unwrap();
    });

    let mut out = Vec::new();
    w.write(&mut out, &mut progress::NoProgress {}).unwrap();
    t.join().unwrap();

    let mut delays = vec![];
    let mut n = 0;
    for_each_frame(&out, |delay, frame, actual| {
        let name = frame_filename(if n == 0 { 0 } else { 1 });
        let expected = lodepng::decode32_file(&name).unwrap();
        let expected = ImgVec::new(expected.buffer, expected.width, expected.height);
        assert_images_eq(expected.as_ref(), actual, 1.7, format_args!("n={n}, {delay} {}", name.display()));
        delays.push(frame.delay);
        n += 1;
    });
    assert_eq!(delays, [120, 20]);
}

fn frame_filename(n: usize) -> PathBuf {
    format!("tests/{}.png", (n % 3) + 1).into()
}

fn for_each_frame(mut gif_data: &[u8], mut cb: impl FnMut(u32, &gif::Frame, ImgRef<RGBA8>)) {
    let mut gif_opts = gif::DecodeOptions::new();
    gif_opts.set_color_output(gif::ColorOutput::Indexed);
    let mut decoder = gif_opts.read_info(&mut gif_data).unwrap();
    let mut screen = gif_dispose::Screen::new_decoder(&decoder);

    let mut delay = 0;
    while let Some(frame) = decoder.read_next_frame().unwrap() {
        screen.blit_frame(frame).unwrap();
        delay += u32::from(frame.delay);
        cb(delay, frame, screen.pixels_rgba());
    }
}

#[test]
fn anim3() {
    assert_anim_eq(6 * 3, |n| format!("tests/a3/{}{}.png", ["x", "y", "z"][n / 6], n % 6).into(), None, 0.8);
}

#[test]
fn anim3_transparent1() {
    assert_anim_eq(6*3, |n| format!("tests/a3/{}{}.png", ["x","y","z"][n/6], n%6).into(), Some(|_,mut fr| {
        fr.pixels_mut().for_each(|px| if px.r == 0 && px.g == 0 { px.a = 0; });
    }), 0.8);
}

#[test]
fn anim3_transparent2() {
    assert_anim_eq(6*3, |n| format!("tests/a3/{}{}.png", ["x","y","z"][n/6], n%6).into(), Some(|_,mut fr| {
        fr.pixels_mut().for_each(|px| if px.r != 0 { px.a = 0; });
    }), 0.8);
}

#[test]
fn anim3_twitch() {
    assert_anim_eq(6*3*3, |x| {
        let n = (x/3) ^ (x&1);
        format!("tests/a3/{}{}.png", ["x","y","z"][n/6], n%6).into()
    }, None, 0.8);
}

#[test]
fn anim3_mix() {
    assert_anim_eq(6*3*3, |x| {
        let n = (x/3) ^ (x&3);
        format!("tests/a3/{}{}.png", ["x","y","z"][(n/6)%3], n%6).into()
    }, Some(|n, mut fr| {
        fr.pixels_mut().take(12).for_each(|px| {
            px.g = px.g.wrapping_add(n as _);
        });
    }), 2.);
}

#[test]
fn anim2_fwd() {
    assert_anim_eq(43, |n| format!("tests/a2/{:02}.png", 1 + n).into(), None, 0.8);
}

#[test]
fn anim2_rev() {
    assert_anim_eq(43, |n| format!("tests/a2/{:02}.png", 43 - n).into(), None, 0.8);
}

#[test]
fn anim2_dupes() {
    assert_anim_eq(43 * 2, |n| format!("tests/a2/{:02}.png", 1 + n / 2).into(), None, 0.8);
}

#[test]
fn anim2_flips() {
    assert_anim_eq(43 * 2, |n| format!("tests/a2/{:02}.png", if n & 1 == 0 { 10 } else { 1 + n / 2 }).into(), None, 0.8);
}

#[test]
fn anim2_transparent() {
    assert_anim_eq(43, |n| format!("tests/a2/{:02}.png", 1+n).into(), Some(|_, mut fr| {
        fr.pixels_mut().for_each(|px| if px.r > 128 { px.a = 0; });
    }), 0.8);
}

#[test]
fn anim2_transparent2() {
    assert_anim_eq(43, |n| format!("tests/a2/{:02}.png", 43-n).into(), Some(|_, mut fr| {
        fr.pixels_mut().for_each(|px| if px.g > 200 { px.a = 0; });
    }), 0.8);
}

#[test]
fn anim2_transparent_half() {
    assert_anim_eq(43, |n| format!("tests/a2/{:02}.png", 43-n).into(), Some(|_, mut fr| {
        let n = fr.width()*(fr.height()/2);
        fr.pixels_mut().skip(n).for_each(|px| if px.g > 200 { px.a = 0; });
    }), 0.8);
}

#[track_caller]
fn assert_images_eq(a: ImgRef<RGBA8>, b: ImgRef<RGBA8>, max_diff: f64, msg: impl std::fmt::Display) {
    let diff = a.pixels().zip(b.pixels()).map(|(a,b)| {
        if a.a != b.a {
            return 300000;
        }
        if a.a == 0 {
            return 0;
        }
        let a = a.map(i32::from);
        let b = b.map(i32::from);
        let d = a - b;
        (d.r * d.r * 2 +
         d.g * d.g * 3 +
         d.b * d.b) as u64
    }).sum::<u64>() as f64 / (a.width() * a.height() * 3) as f64;
    if diff > max_diff {
        dump("expected", a);
        dump("actual", b);
    }
    assert!(diff <= max_diff, "{diff} diff > {max_diff} {msg}");
}

fn dump(filename: &str, px: ImgRef<RGBA8>) {
    let (buf, w, h) = px.to_contiguous_buf();
    lodepng::encode32_file(format!("/tmp/gifski-test-{filename}.png"), &buf, w, h).unwrap();
}

#[test]
fn global_palette_anim2() {
    assert_global_anim_eq(10, |n| format!("tests/a2/{:02}.png", 1 + n).into(), None, 1.5);
}

#[test]
fn global_palette_transparent() {
    assert_global_anim_eq(10, |n| format!("tests/a2/{:02}.png", 1 + n).into(), Some(|_, mut fr| {
        fr.pixels_mut().next().unwrap().a = 0;
    }), 1.5);
}

#[test]
fn global_palette_single_frame() {
    assert_global_anim_eq(1, |_| "tests/1.png".into(), None, 0.8);
}

#[test]
fn max_colors_validation() {
    use rgb::RGB8;
    assert!(new(Settings { max_colors: Some(1), ..Settings::default() }).is_err());
    assert!(new(Settings { max_colors: Some(257), ..Settings::default() }).is_err());
    assert!(new(Settings { max_colors: Some(2), ..Settings::default() }).is_ok());
    assert!(new(Settings { max_colors: Some(256), ..Settings::default() }).is_ok());
    // 1 slot is reserved for transparency, so 4 fixed colors cannot fit in max_colors = 4
    assert!(new(Settings {
        max_colors: Some(4),
        fixed_colors: vec![RGB8::new(1, 0, 0), RGB8::new(2, 0, 0), RGB8::new(3, 0, 0), RGB8::new(4, 0, 0)],
        ..Settings::default()
    }).is_err());
    assert!(new(Settings {
        max_colors: Some(4),
        fixed_colors: vec![RGB8::new(1, 0, 0), RGB8::new(2, 0, 0), RGB8::new(3, 0, 0)],
        ..Settings::default()
    }).is_ok());
}

#[test]
fn max_colors_encoding() {
    assert_anim_eq_with_settings(
        Settings { max_colors: Some(16), ..Settings::default() },
        5, |n| format!("tests/a2/{:02}.png", 1 + n).into(), None, 15.0,
    );
    assert_anim_eq_with_settings(
        Settings { max_colors: Some(16), global_palette: true, speed: gifski::Speed::SLOW, ..Settings::default() },
        5, |n| format!("tests/a2/{:02}.png", 1 + n).into(), None, 15.0,
    );

    // Explicit max_colors (128) should override the quality < 50 heuristic (which would cap quality=10 at 32 colors)
    let (c, w) = new(Settings {
        quality: 10,
        max_colors: Some(128),
        ..Settings::default()
    }).unwrap();
    let t = std::thread::spawn(move || {
        c.add_frame_png_file(0, "tests/1.png".into(), 0.1).unwrap();
    });
    let mut out = Vec::new();
    w.write(&mut out, &mut progress::NoProgress {}).unwrap();
    t.join().unwrap();

    let mut opts = gif::DecodeOptions::new();
    opts.set_color_output(gif::ColorOutput::Indexed);
    let mut decoder = opts.read_info(&out[..]).unwrap();
    let frame = decoder.read_next_frame().unwrap().unwrap();
    let pal = frame.palette.as_ref().expect("Local palette should be present");
    assert_eq!(pal.len() / 3, 128);
}

#[test]
fn dithering_level_validation() {
    assert!(new(Settings { dithering_level: Some(-0.1), ..Settings::default() }).is_err());
    assert!(new(Settings { dithering_level: Some(1.1), ..Settings::default() }).is_err());
    assert!(new(Settings { dithering_level: Some(f32::NAN), ..Settings::default() }).is_err());
    assert!(new(Settings { dithering_level: Some(0.0), ..Settings::default() }).is_ok());
    assert!(new(Settings { dithering_level: Some(0.5), ..Settings::default() }).is_ok());
    assert!(new(Settings { dithering_level: Some(1.0), ..Settings::default() }).is_ok());
}

#[test]
fn dithering_level_encoding() {
    assert_anim_eq_with_settings(
        Settings { dithering_level: Some(0.0), ..Settings::default() },
        5, |n| format!("tests/a2/{:02}.png", 1 + n).into(), None, 5.0,
    );
    assert_anim_eq_with_settings(
        Settings { dithering_level: Some(1.0), ..Settings::default() },
        5, |n| format!("tests/a2/{:02}.png", 1 + n).into(), None, 1.0,
    );
}

#[test]
fn speed_validation() {
    use gifski::Speed;
    assert_eq!(Speed::SLOW.0, 1);
    assert_eq!(Speed::MEDIUM.0, 4);
    assert_eq!(Speed::FAST.0, 10);
    assert_eq!(Speed(5).0, 5);

    assert!(new(Settings { speed: Speed(0), ..Settings::default() }).is_err());
    assert!(new(Settings { speed: Speed(11), ..Settings::default() }).is_err());
    assert!(new(Settings { speed: Speed::SLOW, ..Settings::default() }).is_ok());
    assert!(new(Settings { speed: Speed::MEDIUM, ..Settings::default() }).is_ok());
    assert!(new(Settings { speed: Speed::FAST, ..Settings::default() }).is_ok());
    assert!(new(Settings { speed: Speed(5), ..Settings::default() }).is_ok());
}

#[test]
fn speed_encoding() {
    assert_anim_eq_with_settings(
        Settings { speed: gifski::Speed::SLOW, ..Settings::default() },
        5, |n| format!("tests/a2/{:02}.png", 1 + n).into(), None, 15.0,
    );
    assert_anim_eq_with_settings(
        Settings { speed: gifski::Speed::FAST, ..Settings::default() },
        5, |n| format!("tests/a2/{:02}.png", 1 + n).into(), None, 15.0,
    );
    assert_anim_eq_with_settings(
        Settings { speed: gifski::Speed(5), ..Settings::default() },
        5, |n| format!("tests/a2/{:02}.png", 1 + n).into(), None, 15.0,
    );
}

#[test]
fn max_pixels_validation() {
    assert!(new(Settings { max_pixels: Some(0), ..Settings::default() }).is_err());
    assert!(new(Settings { max_pixels: Some(480_000), width: Some(800), ..Settings::default() }).is_err());
    assert!(new(Settings { max_pixels: Some(480_000), height: Some(600), ..Settings::default() }).is_err());
    assert!(new(Settings { max_pixels: Some(480_000), ..Settings::default() }).is_ok());
}

#[test]
fn max_pixels_dimensions() {
    // Default: original resolution is kept (no downscaling by default)
    assert_eq!(Settings::default().dimensions_for_image(1920, 1080), (1920, 1080));
    assert_eq!(Settings::default().dimensions_for_image(800, 600), (800, 600));

    // With max_pixels set:
    let s = Settings { max_pixels: Some(800 * 600), ..Settings::default() };
    // 800x600 = 480_000 px, not exceeded
    assert_eq!(s.dimensions_for_image(800, 600), (800, 600));
    // 1000x700 = 700_000 px (< 720_000 threshold), factor rounds to 1
    assert_eq!(s.dimensions_for_image(1000, 700), (1000, 700));
    // 1200x800 = 960_000 px (>= 720_000 threshold), factor rounds to 2
    assert_eq!(s.dimensions_for_image(1200, 800), (600, 400));
    // 1920x1080 = 2_073_600 px, factor rounds to 2
    assert_eq!(s.dimensions_for_image(1920, 1080), (960, 540));
    // 3840x2160 = 8_294_400 px, factor rounds to 4
    assert_eq!(s.dimensions_for_image(3840, 2160), (960, 540));
}

#[test]
fn alpha_dithering_validation() {
    use gifski::OrderedDithering;

    // Default effective_alpha_mid: low=8, high=134 => 8 + (134 - 8) / 3 * 2 = 92
    assert_eq!(Settings::default().effective_alpha_mid(), 92);

    // Overridden alpha_mid
    let s = Settings { alpha_mid: Some(89), ..Settings::default() };
    assert_eq!(s.effective_alpha_mid(), 89);

    // Dynamic calculation with custom low/high
    let s = Settings { alpha_low: 0, alpha_high: 255, alpha_mid: None, ..Settings::default() };
    assert_eq!(s.effective_alpha_mid(), 170);

    // Validation: alpha_low <= alpha_high
    assert!(new(Settings { alpha_low: 100, alpha_high: 50, ..Settings::default() }).is_err());
    assert!(new(Settings { alpha_low: 100, alpha_high: 100, ..Settings::default() }).is_ok());

    // Validation: alpha_mid must be within [alpha_low, alpha_high]
    assert!(new(Settings { alpha_low: 50, alpha_high: 100, alpha_mid: Some(40), ..Settings::default() }).is_err());
    assert!(new(Settings { alpha_low: 50, alpha_high: 100, alpha_mid: Some(110), ..Settings::default() }).is_err());
    assert!(new(Settings { alpha_low: 50, alpha_high: 100, alpha_mid: Some(50), ..Settings::default() }).is_ok());
    assert!(new(Settings { alpha_low: 50, alpha_high: 100, alpha_mid: Some(100), ..Settings::default() }).is_ok());
    assert!(new(Settings { alpha_low: 50, alpha_high: 100, alpha_mid: Some(75), ..Settings::default() }).is_ok());

    // All enum variants succeed
    for algo in [
        OrderedDithering::None,
        OrderedDithering::Bayer2,
        OrderedDithering::Bayer4,
        OrderedDithering::Bayer8,
        OrderedDithering::Blue16,
        OrderedDithering::Blue32,
        OrderedDithering::Blue64,
    ] {
        assert!(new(Settings { alpha_dithering: algo, ..Settings::default() }).is_ok());
    }
}

#[test]
fn alpha_dithering_encoding() {
    use gifski::OrderedDithering;

    for algo in [
        OrderedDithering::None,
        OrderedDithering::Bayer2,
        OrderedDithering::Bayer4,
        OrderedDithering::Bayer8,
        OrderedDithering::Blue16,
        OrderedDithering::Blue32,
        OrderedDithering::Blue64,
    ] {
        assert_anim_eq_with_settings(
            Settings { alpha_dithering: algo, ..Settings::default() },
            3, |n| format!("tests/a2/{:02}.png", 1 + n).into(), None, 15.0,
        );
    }

    // Also test with custom alpha thresholds
    assert_anim_eq_with_settings(
        Settings {
            alpha_dithering: OrderedDithering::Blue32,
            alpha_low: 0,
            alpha_high: 255,
            alpha_mid: Some(128),
            ..Settings::default()
        },
        3, |n| format!("tests/a2/{:02}.png", 1 + n).into(), None, 15.0,
    );
}
