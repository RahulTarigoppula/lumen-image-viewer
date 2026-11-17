//! Objective measurements of every capture in a project: no-reference
//! metrics (sharpness, noise, clipping, color cast and, when available, the
//! camera metrics such as visual noise) on each capture scaled to the same
//! size, so devices with different resolutions are compared fairly.

use image::RgbaImage;
use lumen_analysis::job::{run_image, AnalysisPool};
use lumen_analysis::metrics::{self, Direction};
use lumen_analysis::{FrameView, ImageSpec, JobControl, RgbView};
use std::collections::BTreeMap;

/// Long side every capture is scaled to before measuring (pixels).
pub const MEASURE_SIDE: u32 = 2000;

/// The metrics measured, in display order: (metric id, short name). Ids
/// the analysis library does not know are skipped.
pub const MEASURES: [(&str, &str); 6] = [
    ("sharpness.laplacian", "Sharpness"),
    ("noise", "Noise"),
    ("visual_noise", "Visual noise"),
    ("exposure", "Clipping"),
    ("color_cast", "White balance"),
    ("banding", "Banding"),
];

/// The measured metrics that this build knows.
pub fn available() -> Vec<(&'static str, &'static str)> {
    MEASURES.iter().copied().filter(|(id, _)| metrics::get(id).is_some()).collect()
}

/// True when a higher value of metric `id` is better (`None`: neutral or unknown).
pub fn higher_is_better(id: &str) -> Option<bool> {
    match metrics::get(id)?.info().direction {
        Direction::HigherIsBetter => Some(true),
        Direction::LowerIsBetter => Some(false),
        Direction::Neutral => None,
    }
}

/// Short display name of a measured metric.
pub fn name_of(id: &str) -> &str {
    MEASURES.iter().find(|(m, _)| *m == id).map(|(_, n)| *n).unwrap_or(id)
}

/// Unit label of a metric's headline value ("%", "levels", ...).
pub fn unit_of(id: &str) -> &'static str {
    match id {
        "exposure" => "%",
        "noise" => "levels",
        "color_cast" => "dE",
        _ => "",
    }
}

/// Headline value per metric id for one image.
pub fn measure(img: &RgbaImage, pool: &AnalysisPool) -> BTreeMap<String, f64> {
    let mut out = BTreeMap::new();
    let ids: Vec<String> = available().into_iter().map(|(id, _)| id.to_string()).collect();
    if ids.is_empty() || img.width() < 16 || img.height() < 16 {
        return out;
    }
    let scaled;
    let src = if img.width().max(img.height()) > MEASURE_SIDE {
        scaled = crate::imaging::resize_to_fit(img, MEASURE_SIDE, MEASURE_SIDE);
        &scaled
    } else {
        img
    };
    let Ok(view) = RgbView::rgba8(src.as_raw(), src.width() as usize, src.height() as usize) else { return out };
    let view = FrameView::Rgb(view);
    let mut spec = ImageSpec { metrics: ids, preview_first: false, ..Default::default() };
    spec.options.want_map = false;
    let ctl = JobControl::default();
    let Ok(report) = run_image(pool, &spec, &view, None, &ctl, &|_| {}) else { return out };
    for e in &report.metrics {
        let keys = e.values.iter().map(|v| v.key.as_str());
        let Some(k) = metrics::headline_key(&e.id, keys) else { continue };
        if let Some(v) = e.values.iter().find(|v| v.key == k).and_then(|v| v.value) {
            if v.is_finite() {
                out.insert(e.id.clone(), v);
            }
        }
    }
    out
}

/// Format a measured value for tables.
pub fn fmt_value(id: &str, v: f64) -> String {
    let a = v.abs();
    let s = if a >= 100.0 {
        format!("{v:.0}")
    } else if a >= 10.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.2}")
    };
    match unit_of(id) {
        "" => s,
        "%" => format!("{s}%"),
        u => format!("{s} {u}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_a_textured_image() {
        let img = RgbaImage::from_fn(640, 480, |x, y| {
            let v = (((x / 8) + (y / 8)) % 2 * 160 + 40) as u8;
            image::Rgba([v, v, v, 255])
        });
        let pool = AnalysisPool::new(Some(2));
        let m = measure(&img, &pool);
        assert!(m.contains_key("sharpness.laplacian"), "{m:?}");
        assert!(m.contains_key("noise"), "{m:?}");
        assert!(m.values().all(|v| v.is_finite()));
        // A sharper copy measures sharper than a blurred one.
        let blurred = image::imageops::blur(&img, 3.0);
        let mb = measure(&blurred, &pool);
        assert!(m["sharpness.laplacian"] > mb["sharpness.laplacian"]);
        assert_eq!(higher_is_better("sharpness.laplacian"), Some(true));
    }

    #[test]
    fn tiny_images_are_skipped() {
        let pool = AnalysisPool::new(Some(1));
        assert!(measure(&RgbaImage::new(8, 8), &pool).is_empty());
    }

    #[test]
    fn formatting() {
        assert_eq!(fmt_value("exposure", 1.234), "1.23%");
        assert_eq!(fmt_value("sharpness.laplacian", 412.6), "413");
        assert_eq!(fmt_value("noise", 12.34), "12.3 levels");
        assert_eq!(name_of("noise"), "Noise");
        assert_eq!(name_of("unknown"), "unknown");
    }
}
