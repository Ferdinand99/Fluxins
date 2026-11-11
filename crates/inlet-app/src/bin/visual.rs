// SPDX-License-Identifier: AGPL-3.0-or-later
//! `inlet-visual`: compares the screenshots of the visual checks with their baselines.
//!
//! ```text
//! inlet-visual compare --scenes tests/visual/scenes --actual <captures> \
//!     --baseline tests/visual/baseline/<os> --out <report folder>
//! ```
//!
//! For every scene file it reads `<captures>/<name>.png`, cuts a window title bar off
//! the top if the capture has one, and writes the result to `<out>/actual/<name>.png`
//! (RGB, compressed hard: these are the files a baseline update copies). It compares
//! that with `<baseline>/<name>.png`, leaving out the scene's `ignore` rectangles: a
//! pixel differs when one colour channel is off by more than `tolerance.channel`, and
//! a scene fails when more than `tolerance.pixels` of its pixels differ. Every scene
//! with a difference gets `<out>/diff/<name>.png` (the baseline in grey, differing
//! pixels in red, ignored areas in blue). `<out>/report.md` lists the results. The exit
//! code is 1 when a scene failed or had no baseline, 2 for wrong usage.

use std::fmt::Write as _;
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ImageEncoder, RgbImage};
use inlet::scene_file::SceneFile;

/// The tallest title bar cut off a capture (macOS captures the whole window).
const MAX_TITLE_BAR: u32 = 120;

struct Args {
    scenes: PathBuf,
    actual: PathBuf,
    baseline: PathBuf,
    out: PathBuf,
}

fn usage() -> ExitCode {
    eprintln!(
        "usage: inlet-visual compare --scenes <dir> --actual <dir> --baseline <dir> --out <dir>"
    );
    ExitCode::from(2)
}

fn parse_args() -> Option<Args> {
    let mut it = std::env::args().skip(1);
    if it.next().as_deref() != Some("compare") {
        return None;
    }
    let (mut scenes, mut actual, mut baseline, mut out) = (None, None, None, None);
    while let Some(flag) = it.next() {
        let value = PathBuf::from(it.next()?);
        match flag.as_str() {
            "--scenes" => scenes = Some(value),
            "--actual" => actual = Some(value),
            "--baseline" => baseline = Some(value),
            "--out" => out = Some(value),
            _ => return None,
        }
    }
    Some(Args {
        scenes: scenes?,
        actual: actual?,
        baseline: baseline?,
        out: out?,
    })
}

fn main() -> ExitCode {
    let Some(args) = parse_args() else {
        return usage();
    };
    match run(&args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(err) => {
            eprintln!("inlet-visual: {err}");
            ExitCode::from(2)
        }
    }
}

/// What came of one scene.
enum Outcome {
    Same,
    WithinTolerance(Diff),
    Different(Diff),
    NoCapture,
    NoBaseline,
    WrongSize {
        expected: (u32, u32),
        got: (u32, u32),
    },
}

struct Diff {
    pixels: u64,
    compared: u64,
    allowed: u64,
    max_delta: u8,
    /// Smallest rectangle around the differing pixels: x, y, w, h in device pixels.
    area: (u32, u32, u32, u32),
}

fn run(args: &Args) -> Result<bool, String> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(&args.scenes)
        .map_err(|e| format!("{}: {e}", args.scenes.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        .collect();
    files.sort();
    if files.is_empty() {
        return Err(format!("no scene files in {}", args.scenes.display()));
    }
    for dir in ["actual", "diff"] {
        std::fs::create_dir_all(args.out.join(dir)).map_err(|e| e.to_string())?;
    }
    let mut report = String::from(
        "# Visual check\n\n| Scene | Result | Differing pixels | Allowed | Max channel delta | Area (x, y, w, h) |\n|---|---|---|---|---|---|\n",
    );
    let mut ok = true;
    for path in &files {
        let scene = SceneFile::load(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let outcome = compare(&scene, args)?;
        let (result, diff) = match &outcome {
            Outcome::Same => ("same".to_string(), None),
            Outcome::WithinTolerance(d) => ("within tolerance".into(), Some(d)),
            Outcome::Different(d) => ("**different**".into(), Some(d)),
            Outcome::NoCapture => ("**no screenshot**".into(), None),
            Outcome::NoBaseline => ("**no baseline**".into(), None),
            Outcome::WrongSize { expected, got } => (
                format!(
                    "**size {}x{}, baseline {}x{}**",
                    got.0, got.1, expected.0, expected.1
                ),
                None,
            ),
        };
        if !matches!(outcome, Outcome::Same | Outcome::WithinTolerance(_)) {
            ok = false;
        }
        let _ = match diff {
            Some(d) => writeln!(
                report,
                "| {} | {result} | {} ({:.4} %) | {} | {} | {}, {}, {}, {} |",
                scene.name,
                d.pixels,
                100. * d.pixels as f64 / d.compared.max(1) as f64,
                d.allowed,
                d.max_delta,
                d.area.0,
                d.area.1,
                d.area.2,
                d.area.3
            ),
            None => writeln!(report, "| {} | {result} | | | | |", scene.name),
        };
    }
    let _ = writeln!(
        report,
        "\n{}\n\nScreenshots: `actual/`. Differences: `diff/` (differing pixels red, ignored areas blue). \
         To take these screenshots as the new baselines, see `scripts/update-visual-baselines.sh`.",
        if ok { "All scenes match their baselines." } else { "Some scenes do not match their baselines." }
    );
    std::fs::write(args.out.join("report.md"), &report).map_err(|e| e.to_string())?;
    print!("{report}");
    Ok(ok)
}

fn compare(scene: &SceneFile, args: &Args) -> Result<Outcome, String> {
    let file = format!("{}.png", scene.name);
    let capture = args.actual.join(&file);
    if !capture.exists() {
        return Ok(Outcome::NoCapture);
    }
    let expected = (
        (scene.window[0] as f32 * scene.scale).round() as u32,
        (scene.window[1] as f32 * scene.scale).round() as u32,
    );
    let actual = load_rgb(&capture)?;
    let actual = match without_title_bar(actual, expected) {
        Ok(img) => img,
        Err(got) => return Ok(Outcome::WrongSize { expected, got }),
    };
    save_png(&actual, &args.out.join("actual").join(&file))?;
    let baseline = args.baseline.join(&file);
    if !baseline.exists() {
        return Ok(Outcome::NoBaseline);
    }
    let baseline = load_rgb(&baseline)?;
    if baseline.dimensions() != expected {
        return Ok(Outcome::WrongSize {
            expected: baseline.dimensions(),
            got: expected,
        });
    }
    let ignored = ignore_mask(scene, expected);
    let (w, h) = expected;
    let mut diff_img = RgbImage::new(w, h);
    let (mut pixels, mut compared, mut max_delta) = (0u64, 0u64, 0u8);
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    for y in 0..h {
        for x in 0..w {
            let b = baseline.get_pixel(x, y).0;
            let grey = ((b[0] as u32 * 3 + b[1] as u32 * 6 + b[2] as u32) / 10 / 3) as u8;
            if ignored[(y * w + x) as usize] {
                diff_img.put_pixel(x, y, image::Rgb([grey / 2, grey / 2, 96 + grey / 2]));
                continue;
            }
            compared += 1;
            let a = actual.get_pixel(x, y).0;
            let delta = (0..3).map(|i| a[i].abs_diff(b[i])).max().unwrap_or(0);
            max_delta = max_delta.max(delta);
            if delta > scene.tolerance.channel {
                pixels += 1;
                (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
                diff_img.put_pixel(x, y, image::Rgb([255, 32, 64]));
            } else {
                diff_img.put_pixel(x, y, image::Rgb([grey, grey, grey]));
            }
        }
    }
    let allowed = (compared as f64 * scene.tolerance.pixels).floor() as u64;
    if max_delta == 0 {
        return Ok(Outcome::Same);
    }
    let area = if pixels > 0 {
        (x0, y0, x1 - x0 + 1, y1 - y0 + 1)
    } else {
        (0, 0, 0, 0)
    };
    let diff = Diff {
        pixels,
        compared,
        allowed,
        max_delta,
        area,
    };
    if pixels > 0 {
        save_png(&diff_img, &args.out.join("diff").join(&file))?;
    }
    Ok(if pixels > allowed {
        Outcome::Different(diff)
    } else {
        Outcome::WithinTolerance(diff)
    })
}

/// The capture at the expected size: as it is, or without the title bar a window
/// capture has at its top. `Err` holds the capture's size when neither fits.
fn without_title_bar(img: RgbImage, (w, h): (u32, u32)) -> Result<RgbImage, (u32, u32)> {
    let (iw, ih) = img.dimensions();
    if (iw, ih) == (w, h) {
        return Ok(img);
    }
    if iw == w && ih > h && ih - h <= MAX_TITLE_BAR {
        return Ok(image::imageops::crop_imm(&img, 0, ih - h, w, h).to_image());
    }
    Err((iw, ih))
}

/// The pixels the scene leaves out, in device pixels.
fn ignore_mask(scene: &SceneFile, (w, h): (u32, u32)) -> Vec<bool> {
    let mut mask = vec![false; (w * h) as usize];
    for [x, y, rw, rh] in &scene.ignore {
        let s = scene.scale;
        let x0 = ((*x as f32) * s).floor() as u32;
        let y0 = ((*y as f32) * s).floor() as u32;
        let x1 = (((x + rw) as f32) * s).ceil().min(w as f32) as u32;
        let y1 = (((y + rh) as f32) * s).ceil().min(h as f32) as u32;
        for yy in y0..y1 {
            for xx in x0..x1 {
                mask[(yy * w + xx) as usize] = true;
            }
        }
    }
    mask
}

fn load_rgb(path: &Path) -> Result<RgbImage, String> {
    image::open(path)
        .map(|i| i.to_rgb8())
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn save_png(img: &RgbImage, path: &Path) -> Result<(), String> {
    let file = File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    PngEncoder::new_with_quality(
        BufWriter::new(file),
        CompressionType::Best,
        FilterType::Adaptive,
    )
    .write_image(
        img,
        img.width(),
        img.height(),
        image::ExtendedColorType::Rgb8,
    )
    .map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_bar_is_cut_off_the_top() {
        let mut img = RgbImage::new(4, 7);
        img.put_pixel(0, 3, image::Rgb([9, 9, 9]));
        let cut = without_title_bar(img, (4, 4)).unwrap();
        assert_eq!(cut.dimensions(), (4, 4));
        assert_eq!(cut.get_pixel(0, 0).0, [9, 9, 9]);
        assert_eq!(
            without_title_bar(RgbImage::new(5, 4), (4, 4)).err(),
            Some((5, 4))
        );
    }
}
