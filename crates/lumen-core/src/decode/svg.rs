//! SVG / SVGZ rendering with resvg (pure Rust).

use super::Decoded;
use anyhow::{anyhow, Result};
use image::RgbaImage;
use resvg::{tiny_skia, usvg};
use std::path::Path;
use std::sync::{Arc, OnceLock};

/// Long side used when the document has no usable intrinsic size, and the
/// cap for very large intrinsic sizes.
pub const MAX_SIDE: u32 = 2048;
/// Small icons are rendered at least this large so zooming stays crisp.
pub const MIN_SIDE: u32 = 512;

fn fontdb() -> Arc<usvg::fontdb::Database> {
    static DB: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = usvg::fontdb::Database::new();
        db.load_system_fonts();
        Arc::new(db)
    })
    .clone()
}

fn parse(bytes: &[u8], path: Option<&Path>) -> Result<usvg::Tree> {
    let dir = path.and_then(|p| p.parent()).map(|p| p.to_path_buf());
    let opt = usvg::Options {
        // Image links go through `linked_image` only (usvg's own resolver opens
        // any path, a network share's too).
        resources_dir: None,
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_data: usvg::ImageHrefResolver::default_data_resolver(),
            resolve_string: Box::new(move |href: &str, opts: &usvg::Options| linked_image(href, dir.as_deref(), opts)),
        },
        fontdb: fontdb(),
        default_size: usvg::Size::from_wh(1024.0, 1024.0).unwrap(),
        ..Default::default()
    };
    usvg::Tree::from_data(bytes, &opt).map_err(|e| anyhow!("Invalid SVG: {e}"))
}

/// Largest image file an SVG may link to.
const MAX_LINKED_IMAGE: u64 = 64 << 20;

/// The file `href` names, for an SVG in `dir`: only a relative path to a file
/// in that folder or below it (no drive, root, UNC path, URL or `..`), so an
/// SVG cannot make Lumen read other files or contact another computer. Data
/// URLs are resolved by usvg itself.
fn linked_file(href: &str, dir: &Path) -> Option<std::path::PathBuf> {
    use std::path::Component;
    let rel = Path::new(href);
    if href.contains(':') || href.starts_with(['/', '\\']) || !rel.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir)) {
        return None;
    }
    let file = dir.join(rel);
    // A link or junction inside the folder must not lead out of it.
    let (real_dir, real_file) = (std::fs::canonicalize(dir).ok()?, std::fs::canonicalize(&file).ok()?);
    real_file.starts_with(&real_dir).then_some(file)
}

/// An `<image>` linked by an SVG in `dir` (None: no folder, the SVG came from memory).
fn linked_image(href: &str, dir: Option<&Path>, opts: &usvg::Options) -> Option<usvg::ImageKind> {
    use std::io::Read;
    let path = linked_file(href, dir?)?;
    let file = std::fs::File::open(&path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut data = Vec::new();
    file.take(MAX_LINKED_IMAGE + 1).read_to_end(&mut data).ok()?;
    if data.len() as u64 > MAX_LINKED_IMAGE {
        return None;
    }
    let (png, jpeg, gif) = (data.starts_with(b"\x89PNG"), data.starts_with(&[0xFF, 0xD8, 0xFF]), data.starts_with(b"GIF8"));
    let webp = data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP";
    if png {
        Some(usvg::ImageKind::PNG(Arc::new(data)))
    } else if jpeg {
        Some(usvg::ImageKind::JPEG(Arc::new(data)))
    } else if gif {
        Some(usvg::ImageKind::GIF(Arc::new(data)))
    } else if webp {
        Some(usvg::ImageKind::WEBP(Arc::new(data)))
    } else if matches!(crate::formats::ext_of(&path).as_str(), "svg" | "svgz") {
        // Another SVG: its own links are ignored (as usvg does).
        usvg::Tree::from_data_nested(&data, opts).ok().map(usvg::ImageKind::SVG)
    } else {
        None
    }
}

/// Pixel size to render at: intrinsic size clamped to [MIN_SIDE, MAX_SIDE]
/// on the long side (aspect preserved), further limited by `max_side`.
fn target_size(w: f32, h: f32, max_side: Option<u32>) -> (u32, u32) {
    let long = w.max(h).max(1.0);
    let mut side = long.clamp(MIN_SIDE as f32, MAX_SIDE as f32);
    if let Some(m) = max_side {
        side = side.min(m as f32);
    }
    let s = side / long;
    (((w * s).round() as u32).max(1), ((h * s).round() as u32).max(1))
}

pub fn render(bytes: &[u8], path: Option<&Path>, max_side: Option<u32>) -> Result<RgbaImage> {
    let tree = parse(bytes, path)?;
    let size = tree.size();
    let (tw, th) = target_size(size.width(), size.height(), max_side);
    let mut pixmap = tiny_skia::Pixmap::new(tw, th).ok_or_else(|| anyhow!("SVG size is invalid"))?;
    let tf = tiny_skia::Transform::from_scale(tw as f32 / size.width(), th as f32 / size.height());
    resvg::render(&tree, tf, &mut pixmap.as_mut());
    // tiny-skia stores premultiplied RGBA.
    let mut data = pixmap.take();
    for p in data.chunks_exact_mut(4) {
        let a = p[3] as u32;
        if a != 0 && a != 255 {
            for c in &mut p[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    RgbaImage::from_raw(tw, th, data).ok_or_else(|| anyhow!("SVG render failed"))
}

pub fn decode(bytes: &[u8], path: Option<&Path>) -> Result<Decoded> {
    let tree_size = parse(bytes, path)?.size();
    let img = render(bytes, path, None)?;
    let mut d = Decoded::from_rgba(img, "SVG");
    d.color_info = format!("Vector · {:.0}x{:.0} intrinsic", tree_size.width(), tree_size.height());
    d.source_size = (tree_size.width().round() as u32, tree_size.height().round() as u32);
    Ok(d)
}

pub fn probe(bytes: &[u8]) -> Option<(u32, u32)> {
    let s = parse(bytes, None).ok()?.size();
    Some((s.width().round() as u32, s.height().round() as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20" viewBox="0 0 40 20">
        <rect x="0" y="0" width="20" height="20" fill="#ff0000"/></svg>"##;

    #[test]
    fn renders_with_transparency_and_min_size() {
        let d = decode(SVG.as_bytes(), None).unwrap();
        assert_eq!(d.image.dimensions(), (512, 256));
        assert_eq!(d.image.get_pixel(10, 10).0, [255, 0, 0, 255]);
        assert_eq!(d.image.get_pixel(500, 10).0[3], 0);
        assert!(d.has_alpha);
        let t = render(SVG.as_bytes(), None, Some(64)).unwrap();
        assert_eq!(t.dimensions(), (64, 32));
    }

    #[test]
    fn huge_intrinsic_is_capped_and_garbage_fails() {
        let big = r#"<svg xmlns="http://www.w3.org/2000/svg" width="90000" height="45000"/>"#;
        assert_eq!(render(big.as_bytes(), None, None).unwrap().dimensions(), (2048, 1024));
        assert!(decode(b"not an svg", None).is_err());
        assert!(decode(b"", None).is_err());
    }

    #[test]
    fn svgz_is_supported() {
        // resvg detects gzip input itself; build a stored (uncompressed) gzip member.
        let raw = SVG.as_bytes();
        let mut gz = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
        // Deflate stored block.
        gz.push(1);
        gz.extend_from_slice(&(raw.len() as u16).to_le_bytes());
        gz.extend_from_slice(&(!(raw.len() as u16)).to_le_bytes());
        gz.extend_from_slice(raw);
        gz.extend_from_slice(&crc32(raw).to_le_bytes());
        gz.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        assert!(decode(&gz, None).is_ok());
    }

    #[test]
    fn linked_images_stay_in_the_svg_folder() {
        let d = std::env::temp_dir().join(format!("lumen-svg-links-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("img")).unwrap();
        let mut png = Vec::new();
        image::RgbaImage::from_pixel(1, 1, image::Rgba([255, 0, 0, 255])).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        std::fs::write(d.join("img").join("red.png"), &png).unwrap();
        let outside = std::env::temp_dir().join(format!("lumen-svg-outside-{}.png", std::process::id()));
        std::fs::write(&outside, &png).unwrap();
        assert!(linked_file("img/red.png", &d).is_some());
        assert!(linked_file("./img/red.png", &d).is_some());
        let abs = outside.display().to_string();
        for bad in [abs.as_str(), "../x.png", "img/../../x.png", r"\\server\share\x.png", "//server/share/x.png", "file:///C:/x.png", "http://example.com/x.png", "C:x.png", "/x.png", "img/missing.png"] {
            assert!(linked_file(bad, &d).is_none(), "{bad}");
        }
        // Rendered: the image in the folder shows, the one outside it does not,
        // and an SVG from memory links to no file at all.
        let svg = |href: &str| format!(r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="20" height="10"><image x="0" y="0" width="10" height="10" xlink:href="{href}"/></svg>"#);
        let path = d.join("a.svg");
        let [r, g, _, a] = render(svg("img/red.png").as_bytes(), Some(path.as_path()), Some(20)).unwrap().get_pixel(4, 5).0;
        assert!(r > 200 && g < 50 && a > 200, "{:?}", [r, g, a]);
        assert_eq!(render(svg(&abs).as_bytes(), Some(path.as_path()), Some(20)).unwrap().get_pixel(4, 5).0[3], 0);
        assert_eq!(render(svg("img/red.png").as_bytes(), None, Some(20)).unwrap().get_pixel(4, 5).0[3], 0);
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_file(&outside);
    }

    fn crc32(b: &[u8]) -> u32 {
        let mut c = 0xFFFF_FFFFu32;
        for &x in b {
            c ^= x as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
            }
        }
        !c
    }
}
