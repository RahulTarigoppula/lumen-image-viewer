//! Company branding of reports: a company or team name, a logo, an accent
//! color and a footer line ("Confidential"). It is saved in the settings (the
//! defaults for every report) and in a project (its own, when it has one).
//! Empty fields keep Lumen's own look.

use super::{contain, Box2, Picture, ACCENT};
use image::RgbaImage;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Branding as the user set it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Branding {
    /// Company or team name, e.g. "Acme Camera Lab".
    pub company: String,
    /// Logo file (PNG or JPEG): absolute, or relative to the project folder
    /// (a project keeps its own copy in `.lumen`). Empty: no logo.
    pub logo: String,
    /// Accent color of rules, labels and table headers; `None` is Lumen blue.
    pub accent: Option<[u8; 3]>,
    /// One line at the foot of every page, e.g. "Confidential".
    pub footer: String,
}

impl Branding {
    /// Nothing set: reports look as they always did.
    pub fn is_empty(&self) -> bool {
        self.company.trim().is_empty() && self.logo.trim().is_empty() && self.accent.is_none() && self.footer.trim().is_empty()
    }

    pub fn accent(&self) -> [u8; 3] {
        self.accent.unwrap_or(ACCENT)
    }

    /// The logo file; a relative path starts at `root` (the project folder).
    pub fn logo_path(&self, root: Option<&Path>) -> Option<PathBuf> {
        let l = self.logo.trim();
        if l.is_empty() {
            return None;
        }
        let p = Path::new(l);
        if p.is_absolute() {
            return Some(p.to_path_buf());
        }
        let mut out = root?.to_path_buf();
        for part in l.split(['/', '\\']).filter(|s| !s.is_empty()) {
            out.push(part);
        }
        Some(out)
    }

    /// The same branding with an absolute logo path (for a worker thread).
    pub fn resolved(&self, root: Option<&Path>) -> Branding {
        Branding { logo: self.logo_path(root).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(), ..self.clone() }
    }

    /// One line for dialogs: "Acme Camera Lab, logo, accent color, footer".
    pub fn summary(&self) -> String {
        if self.is_empty() {
            return "None: Lumen's own look".into();
        }
        let mut parts: Vec<String> = Vec::new();
        if !self.company.trim().is_empty() {
            parts.push(self.company.trim().to_string());
        }
        if !self.logo.trim().is_empty() {
            parts.push("logo".into());
        }
        if self.accent.is_some() {
            parts.push("accent color".into());
        }
        if !self.footer.trim().is_empty() {
            parts.push(format!("footer \"{}\"", self.footer.trim()));
        }
        parts.join(", ")
    }
}

/// Long side of the embedded logo in pixels: sharp on a printed cover.
const LOGO_SIDE: u32 = 900;

/// Branding ready for the page layout: the logo decoded, scaled and encoded.
#[derive(Clone, Debug)]
pub struct Brand {
    pub company: String,
    /// The logo flattened on white (it sits on white wherever it is shown).
    pub logo: Option<Picture>,
    pub accent: [u8; 3],
    pub footer: String,
}

impl Default for Brand {
    fn default() -> Self {
        Brand { company: String::new(), logo: None, accent: ACCENT, footer: String::new() }
    }
}

impl Brand {
    /// `b` with the logo image `logo` (already decoded). Transparent margins
    /// are cut off, so the logo lines up with the page's edges and rules.
    pub fn new(b: &Branding, logo: Option<&RgbaImage>) -> Brand {
        Brand {
            company: b.company.trim().to_string(),
            logo: logo.and_then(|img| Picture::from_rgba_quality(&trim_transparent(img), LOGO_SIDE, 95).ok()),
            accent: b.accent(),
            footer: b.footer.trim().to_string(),
        }
    }

    /// `b` with its logo read from disk (`root`: the project folder, for a
    /// relative logo path). A logo that cannot be read is left out; the
    /// second value says why.
    pub fn load(b: &Branding, root: Option<&Path>) -> (Brand, Option<String>) {
        let Some(path) = b.logo_path(root) else { return (Brand::new(b, None), None) };
        match read_logo(&path) {
            Ok(img) => (Brand::new(b, Some(&img)), None),
            Err(e) => (Brand::new(b, None), Some(format!("the logo {} could not be read: {e:#}", path.display()))),
        }
    }

    /// Company branding is set: the cover uses the white, logo-friendly design.
    pub fn branded(&self) -> bool {
        !self.company.is_empty() || self.logo.is_some()
    }
}

/// `img` without its fully transparent margins (a faint edge, alpha up to
/// 8, counts as transparent). An image with nothing visible stays as it is.
pub fn trim_transparent(img: &RgbaImage) -> RgbaImage {
    let (w, h) = img.dimensions();
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    for (x, y, p) in img.enumerate_pixels() {
        if p[3] > 8 {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }
    if x0 == u32::MAX || (x0 == 0 && y0 == 0 && x1 + 1 == w && y1 + 1 == h) {
        return img.clone();
    }
    image::imageops::crop_imm(img, x0, y0, x1 - x0 + 1, y1 - y0 + 1).to_image()
}

/// Decode a logo file (PNG or JPEG; anything Lumen opens works).
pub fn read_logo(path: &Path) -> anyhow::Result<RgbaImage> {
    let d = crate::decode::decode_file(path, crate::decode::DecodeOptions { color_manage: true, animations: false })?;
    anyhow::ensure!(d.image.width() > 0 && d.image.height() > 0, "empty image");
    Ok((*d.image).clone())
}

/// Where a logo of aspect ratio `a` (width / height) goes in the slot `bx`:
/// as large as fits without leaving the slot at any aspect ratio, against
/// the slot's left (or right) edge and centred vertically.
pub fn logo_box(bx: Box2, a: f32, right: bool) -> Box2 {
    let a = if a.is_finite() && a > 0.0 { a.clamp(0.02, 50.0) } else { 1.0 };
    let c = contain(bx, a);
    let x = if right { bx.x + bx.w - c.w } else { bx.x };
    Box2::new(x, c.y, c.w, c.h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branding_round_trips_and_old_files_load() {
        let b = Branding { company: "Acme Camera Lab".into(), logo: ".lumen/report-logo.png".into(), accent: Some([200, 30, 60]), footer: "Confidential".into() };
        let json = serde_json::to_string(&b).unwrap();
        assert_eq!(serde_json::from_str::<Branding>(&json).unwrap(), b);
        // Missing fields are empty; unknown ones are ignored.
        let old: Branding = serde_json::from_str(r#"{"company":"X","later":1}"#).unwrap();
        assert_eq!(old, Branding { company: "X".into(), ..Default::default() });
        assert!(Branding::default().is_empty());
        assert!(!old.is_empty());
        assert_eq!(Branding::default().accent(), ACCENT);
        assert_eq!(b.summary(), "Acme Camera Lab, logo, accent color, footer \"Confidential\"");
    }

    #[test]
    fn logo_paths_resolve_against_the_project() {
        let root = Path::new(r"C:\Projects\Round3");
        let b = Branding { logo: ".lumen/report-logo.png".into(), ..Default::default() };
        assert_eq!(b.logo_path(Some(root)), Some(root.join(".lumen").join("report-logo.png")));
        assert_eq!(b.logo_path(None), None);
        let abs = Branding { logo: r"D:\Brand\logo.jpg".into(), ..Default::default() };
        assert_eq!(abs.logo_path(Some(root)), Some(PathBuf::from(r"D:\Brand\logo.jpg")));
        assert_eq!(b.resolved(Some(root)).logo, root.join(".lumen").join("report-logo.png").to_string_lossy());
        assert_eq!(Branding::default().logo_path(Some(root)), None);
    }

    #[test]
    fn logo_box_stays_in_its_slot_at_any_shape() {
        let slot = Box2::new(72.0, 52.0, 260.0, 64.0);
        for a in [0.0, 0.01, 0.2, 1.0, 4.06, 10.0, 80.0, f32::NAN, f32::INFINITY] {
            for right in [false, true] {
                let b = logo_box(slot, a, right);
                assert!(b.x >= slot.x - 0.01 && b.y >= slot.y - 0.01 && b.x + b.w <= slot.x + slot.w + 0.01 && b.y + b.h <= slot.y + slot.h + 0.01, "{a} {b:?}");
                assert!(b.w > 0.0 && b.h > 0.0, "{a} {b:?}");
                // Against the chosen edge.
                if right {
                    assert!((b.x + b.w - (slot.x + slot.w)).abs() < 0.01);
                } else {
                    assert!((b.x - slot.x).abs() < 0.01);
                }
            }
        }
        // A wide logo fills the width, a tall one the height.
        assert!((logo_box(slot, 10.0, false).w - 260.0).abs() < 0.01);
        assert!((logo_box(slot, 0.5, false).h - 64.0).abs() < 0.01);
    }

    #[test]
    fn transparent_margins_are_trimmed() {
        // A 40 x 20 mark inside a 100 x 60 transparent canvas, with a faint halo.
        let img = RgbaImage::from_fn(100, 60, |x, y| {
            let inside = (30..70).contains(&x) && (20..40).contains(&y);
            image::Rgba([10, 20, 30, if inside { 255 } else if x == 10 { 5 } else { 0 }])
        });
        let t = trim_transparent(&img);
        assert_eq!(t.dimensions(), (40, 20));
        assert_eq!(t.get_pixel(0, 0)[3], 255);
        // Opaque images and fully transparent ones are left alone.
        let opaque = RgbaImage::from_pixel(8, 4, image::Rgba([1, 2, 3, 255]));
        assert_eq!(trim_transparent(&opaque), opaque);
        let empty = RgbaImage::new(8, 4);
        assert_eq!(trim_transparent(&empty), empty);
    }

    #[test]
    fn brand_reads_a_png_logo_and_survives_a_missing_one() {
        let dir = std::env::temp_dir().join(format!("lumen-brand-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("logo.png");
        RgbaImage::from_fn(300, 100, |x, _| image::Rgba([(x % 255) as u8, 40, 90, if x < 20 { 0 } else { 255 }])).save(&png).unwrap();
        let b = Branding { company: "Acme".into(), logo: png.to_string_lossy().into_owned(), accent: Some([1, 2, 3]), footer: " Confidential ".into() };
        let (brand, warn) = Brand::load(&b, None);
        assert!(warn.is_none(), "{warn:?}");
        let logo = brand.logo.as_ref().unwrap();
        // The transparent strip at the left is cut off.
        assert_eq!((logo.w, logo.h), (280, 100));
        assert_eq!(brand.accent, [1, 2, 3]);
        assert_eq!(brand.footer, "Confidential");
        assert!(brand.branded());
        let missing = Branding { logo: dir.join("nope.png").to_string_lossy().into_owned(), ..Default::default() };
        let (brand, warn) = Brand::load(&missing, None);
        assert!(brand.logo.is_none() && warn.is_some() && !brand.branded());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
