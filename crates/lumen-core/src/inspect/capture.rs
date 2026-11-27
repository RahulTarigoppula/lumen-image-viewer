//! The "Capture" section: what the camera did and which features fired,
//! interpreted in plain language from EXIF, XMP, the Samsung trailer, MPF,
//! HEIF auxiliary images and the Apple maker note. Only present facts are
//! listed; guesses say so in their note.

use super::exifview::{apple_capture_type, Ex};
use super::xmp::GOOGLE_ENCRYPTED;
use super::{Ctx, Field};
use crate::catalog::human_size;
use exif::Tag;

/// "1/50 s", "0.6 s", "2 s".
pub(crate) fn fmt_exposure(t: f64) -> String {
    if t >= 1.0 {
        if (t - t.round()).abs() < 0.05 { format!("{} s", t.round()) } else { format!("{t:.1} s") }
    } else if t > 0.0 {
        let d = 1.0 / t;
        if d < 10.0 && (d - d.round()).abs() > 0.05 { format!("{t:.2} s") } else { format!("1/{} s", d.round()) }
    } else {
        "0 s".into()
    }
}

/// "1.8", "2", "11".
pub(crate) fn fmt_num(v: f64) -> String {
    let s = format!("{v:.1}");
    s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
}

/// EV100 from aperture, shutter time and ISO.
pub(crate) fn ev100(n: f64, t: f64, iso: f64) -> Option<f64> {
    if n > 0.0 && t > 0.0 && iso > 0.0 {
        let ev = (n * n / t).log2() - (iso / 100.0).log2();
        ev.is_finite().then_some(ev)
    } else {
        None
    }
}

/// Typical scene for an EV100 value.
pub(crate) fn ev_scene(ev: f64) -> &'static str {
    if ev >= 14.0 {
        "bright sunlight"
    } else if ev >= 11.0 {
        "daylight (overcast or shade)"
    } else if ev >= 8.0 {
        "bright indoor or dusk"
    } else if ev >= 5.0 {
        "normal indoor lighting"
    } else if ev >= 2.0 {
        "dim indoor / night street"
    } else if ev >= -1.0 {
        "night scene or candlelight"
    } else {
        "very dark (moonlight, night sky)"
    }
}

fn flash_text(v: u32) -> String {
    if v & 0x20 != 0 {
        return "No flash function".into();
    }
    let fired = v & 1 != 0;
    let mode = match (v >> 3) & 3 {
        1 => Some("forced on"),
        2 => Some("off"),
        3 => Some("auto"),
        _ => None,
    };
    let mut s = if fired { "Fired".to_string() } else { "Did not fire".to_string() };
    if let Some(m) = mode {
        s.push_str(&format!(" ({m})"));
    }
    if v & 0x40 != 0 {
        s.push_str(", red-eye reduction");
    }
    if fired && (v >> 1) & 3 == 2 {
        s.push_str(", return light not detected");
    }
    s
}

/// Build the Capture fields.
pub(crate) fn build(ctx: &Ctx) -> Vec<Field> {
    let mut f: Vec<Field> = Vec::new();
    let mut add = |name: &str, value: String, note: &str| f.push(Field::new(name, value, note));
    let ex = ctx.exif.map(Ex);
    let ex = ex.as_ref();
    let text = |t: Tag| ex.and_then(|e| e.text(t));
    let num = |t: Tag| ex.and_then(|e| e.f64(t));
    let uint = |t: Tag| ex.and_then(|e| e.uint(t));
    let sef_names = ctx.sef_names();
    let sef_has = |needle: &str| {
        let n = needle.to_ascii_lowercase();
        sef_names.iter().copied().find(|s| s.to_ascii_lowercase().contains(&n))
    };
    let semantics = ctx.item_semantics();
    let sam = ctx.samsung;
    let sam_int = |t: u16| sam.and_then(|s| s.int(t));
    // Google Camera mode ("...SpecialType-PORTRAIT", "...-NIGHT"...), upper case.
    let special = ctx.xget("GCamera:SpecialTypeID").map(|v| v.to_ascii_uppercase());
    let special_has = |w: &str| special.as_deref().is_some_and(|s| s.contains(w));

    // Camera, software, lens, module.
    let make = text(Tag::Make);
    let model = text(Tag::Model);
    let camera = match (make, model) {
        (Some(mk), Some(md)) if md.to_lowercase().starts_with(&mk.to_lowercase()) => Some(md),
        (Some(mk), Some(md)) => Some(format!("{mk} {md}")),
        (a, b) => a.or(b),
    };
    if let Some(c) = camera {
        add("Camera", c, "From the EXIF Make and Model tags.");
    }
    if let Some(s) = text(Tag::Software) {
        add("Firmware / software", s, "Camera firmware or the app that last saved the file.");
    }
    let lens_model = text(Tag::LensModel);
    let lens = match (text(Tag::LensMake), lens_model.clone()) {
        (Some(mk), Some(md)) if md.to_lowercase().starts_with(&mk.to_lowercase()) => Some(md),
        (Some(mk), Some(md)) => Some(format!("{mk} {md}")),
        (a, b) => b.or(a),
    };
    if let Some(l) = lens {
        add("Lens", l, "On phones the lens name usually tells which camera module took the photo.");
    }
    let f35 = uint(Tag::FocalLengthIn35mmFilm).filter(|&v| v > 0);
    let is_front = lens_model.as_deref().is_some_and(|l| l.to_ascii_lowercase().contains("front"));
    if is_front {
        add("Camera module", "Front (selfie)".into(), "From the lens name.");
    } else if let Some(eq) = f35 {
        let m = match eq {
            0..=19 => "Ultra-wide",
            20..=35 => "Main (wide)",
            36..=60 => "2x-ish telephoto or crop",
            _ => "Telephoto",
        };
        add(
            "Camera module",
            format!("{m}, guessed from the focal length"),
            &format!("Guess based on the 35 mm equivalent focal length ({eq} mm): under 20 mm ultra-wide, 20-35 mm main camera, 35-60 mm about 2x, above 60 mm telephoto. Digital zoom and cropping shift it."),
        );
    }

    // Zoom and focal length.
    if let Some(z) = num(Tag::DigitalZoomRatio).filter(|&z| z > 1.0) {
        add("Zoom", format!("{z:.1}x digital zoom"), "The image was cropped and upscaled from the sensor (less real detail).");
    }
    if let Some(eq) = f35 {
        let v = match num(Tag::FocalLength).filter(|&v| v > 0.0) {
            Some(real) => format!("{eq} mm (actual {} mm)", fmt_num(real)),
            None => format!("{eq} mm"),
        };
        add("Equivalent focal length", v, "Field of view expressed as a full-frame (35 mm) focal length.");
    } else if let Some(real) = num(Tag::FocalLength).filter(|&v| v > 0.0) {
        add("Focal length", format!("{} mm", fmt_num(real)), "");
    }

    // Exposure: EXIF, or the Samsung maker note where EXIF has no value.
    use super::makernote::tag as st;
    let t_exif = num(Tag::ExposureTime).filter(|&v| v > 0.0);
    let n_exif = num(Tag::FNumber).filter(|&v| v > 0.0);
    let iso_exif = uint(Tag::PhotographicSensitivity).filter(|&v| v > 0).or_else(|| uint(Tag::ISOSpeed).filter(|&v| v > 0));
    let t = t_exif.or_else(|| sam.and_then(|s| s.real(st::EXPOSURE_TIME)).filter(|&v| v > 0.0 && v < 3600.0));
    let n = n_exif.or_else(|| sam.and_then(|s| s.real(st::F_NUMBER)).filter(|&v| v > 0.0 && v < 100.0));
    let iso = iso_exif.or_else(|| sam_int(st::ISO).filter(|&v| v > 0 && v < 10_000_000).map(|v| v as u32));
    let from_maker_note = (t.is_some() && t_exif.is_none()) || (n.is_some() && n_exif.is_none()) || (iso.is_some() && iso_exif.is_none());
    let mut parts = Vec::new();
    if let Some(t) = t {
        parts.push(fmt_exposure(t));
    }
    if let Some(n) = n {
        parts.push(format!("f/{}", fmt_num(n)));
    }
    if let Some(i) = iso {
        parts.push(format!("ISO {i}"));
    }
    if !parts.is_empty() {
        let note = if from_maker_note {
            "Shutter time, aperture and ISO sensitivity. Values missing from EXIF were read from the Samsung maker note."
        } else {
            "Shutter time, aperture and ISO sensitivity."
        };
        add("Exposure", parts.join(" \u{00B7} "), note);
    }
    if let (Some(t), Some(n), Some(i)) = (t, n, iso) {
        if let Some(ev) = ev100(n, t, i as f64) {
            add(
                "Exposure value",
                format!("EV {ev:.1}"),
                &format!(
                    "About EV {:.0}: {}. EV100 = log2(N^2/t) - log2(ISO/100): the scene brightness the exposure settings imply (higher is brighter).",
                    ev,
                    ev_scene(ev)
                ),
            );
        }
    }
    if let Some(v) = uint(Tag::ExposureProgram).filter(|&v| v > 0) {
        let s = match v {
            1 => "Manual",
            2 => "Normal program (auto)",
            3 => "Aperture priority",
            4 => "Shutter priority",
            5 => "Creative program (depth of field)",
            6 => "Action program (fast shutter)",
            7 => "Portrait mode",
            8 => "Landscape mode",
            _ => "Other",
        };
        add("Exposure program", s.into(), "");
    }
    if let Some(v) = uint(Tag::ExposureMode) {
        let s = match v {
            0 => "Auto exposure",
            1 => "Manual exposure",
            2 => "Auto bracket",
            _ => "Other",
        };
        add("Exposure mode", s.into(), "Auto bracket means several frames at different exposures were taken.");
    }
    if let Some(b) = num(Tag::ExposureBiasValue).filter(|b| b.abs() > 0.001) {
        add("Exposure bias", format!("{b:+.1} EV"), "Exposure compensation: positive brightens, negative darkens.");
    } else if num(Tag::ExposureBiasValue).is_none() {
        if let Some(b) = sam.and_then(|s| s.real(st::EXPOSURE_COMP)).filter(|b| b.abs() > 0.001 && b.abs() < 20.0) {
            add("Exposure bias", format!("{b:+.1} EV"), "Exposure compensation: positive brightens, negative darkens. From the Samsung maker note.");
        }
    }
    if let Some(v) = uint(Tag::MeteringMode).filter(|&v| v > 0) {
        let s = match v {
            1 => "Average",
            2 => "Center-weighted average",
            3 => "Spot",
            4 => "Multi-spot",
            5 => "Pattern (matrix / evaluative)",
            6 => "Partial",
            _ => "Other",
        };
        add("Metering", s.into(), "How the camera measured the light to set the exposure.");
    }
    if let Some(v) = uint(Tag::WhiteBalance) {
        add("White balance", if v == 1 { "Manual" } else { "Auto" }.into(), "");
    } else if let Some(v) = sam_int(st::WHITE_BALANCE).filter(|v| *v == 0 || *v == 1) {
        add("White balance", if v == 1 { "Manual" } else { "Auto" }.into(), "From the Samsung maker note.");
    }
    if let Some(v) = uint(Tag::LightSource).filter(|&v| v > 0) {
        let s = match v {
            1 => "Daylight",
            2 => "Fluorescent",
            3 => "Tungsten (incandescent)",
            4 => "Flash",
            9 => "Fine weather",
            10 => "Cloudy",
            11 => "Shade",
            12 => "Daylight fluorescent",
            13 => "Day white fluorescent",
            14 => "Cool white fluorescent",
            15 => "White fluorescent",
            16 => "Warm white fluorescent",
            17 => "Standard light A",
            18 => "Standard light B",
            19 => "Standard light C",
            20 => "D55",
            21 => "D65",
            22 => "D75",
            23 => "D50",
            24 => "ISO studio tungsten",
            _ => "Other",
        };
        add("Light source", s.into(), "Light source the white balance was set for.");
    }
    if let Some(v) = uint(Tag::Flash) {
        add("Flash", flash_text(v), "");
    }
    if sam_int(st::FACE_DETECT) == Some(1) {
        let v = if sam_int(st::FACE_RECOGNITION) == Some(1) { "On, with face recognition" } else { "On" };
        add("Face detection", v.into(), "Samsung maker note FaceDetect = On: the camera looked for faces to set focus and exposure.");
    }
    if let Some(v) = uint(Tag::SceneCaptureType) {
        let s = match v {
            0 => "Standard",
            1 => "Landscape",
            2 => "Portrait",
            3 => "Night scene",
            _ => "Other",
        };
        add("Scene type", s.into(), "Scene mode the camera reported.");
    }
    if let Some(v) = uint(Tag::SubjectDistanceRange).filter(|&v| v > 0) {
        let s = match v {
            1 => "Macro",
            2 => "Close view",
            3 => "Distant view",
            _ => "Other",
        };
        add("Subject distance range", s.into(), "");
    }
    for (tag, name, low, high) in [
        (Tag::Contrast, "Contrast setting", "Soft", "Hard"),
        (Tag::Saturation, "Saturation setting", "Low", "High"),
        (Tag::Sharpness, "Sharpness setting", "Soft", "Hard"),
    ] {
        if let Some(v) = uint(tag).filter(|&v| v > 0) {
            add(name, if v == 1 { low } else if v == 2 { high } else { "Other" }.into(), "In-camera processing setting (not Normal).");
        }
    }

    // Multi-frame / HDR.
    let mut mf: Vec<(String, String)> = Vec::new();
    let composite = uint(Tag::CompositeImage);
    match composite {
        Some(2) => mf.push(("General composite image".into(), "EXIF CompositeImage = 2: merged from several images.".into())),
        Some(3) => mf.push(("Composite captured while shooting (multi-frame)".into(), "EXIF CompositeImage = 3: the camera merged several frames at capture time.".into())),
        _ => {}
    }
    let counts = ex.map(|e| e.uints(Tag::SourceImageNumberOfCompositeImage)).unwrap_or_default();
    if let Some(&total) = counts.first() {
        let used = counts.get(1).copied().filter(|&u| u > 0).unwrap_or(total);
        if used > 1 {
            let mut s = format!("{used} frames merged");
            if total > used {
                s.push_str(&format!(" (of {total} captured)"));
            }
            mf.push((s, "EXIF SourceImageNumberOfCompositeImage.".into()));
        }
    }
    let custom = uint(Tag::CustomRendered);
    match custom {
        Some(1) => mf.push(("Custom image processing".into(), "EXIF CustomRendered = 1.".into())),
        Some(2) => mf.push(("HDR (original not saved)".into(), "EXIF CustomRendered = 2 (Apple).".into())),
        Some(3) => mf.push(("HDR (original saved too)".into(), "EXIF CustomRendered = 3 (Apple).".into())),
        Some(4) => mf.push(("Original frame of an HDR photo".into(), "EXIF CustomRendered = 4 (Apple).".into())),
        Some(7) => mf.push(("Portrait HDR".into(), "EXIF CustomRendered = 7 (Apple).".into())),
        _ => {}
    }
    match ctx.apple.hdr_type {
        Some(3) => mf.push(("HDR image (Apple Smart HDR)".into(), "Apple maker note HDRImageType = 3.".into())),
        Some(4) => mf.push(("Original (non-HDR) frame".into(), "Apple maker note HDRImageType = 4.".into())),
        _ => {}
    }
    if let Some(name) = sef_has("hdr") {
        mf.push(("Samsung HDR processing".into(), format!("Samsung trailer entry {name}.")));
    }
    if sam_int(st::SMART_RANGE) == Some(1) {
        mf.push(("Samsung Smart Range (wider dynamic range)".into(), "Samsung maker note SmartRange = On.".into()));
    }
    // Google HDR+: one entry with every clue as its source.
    let mut hdrp: Vec<String> = Vec::new();
    if let Some((k, _)) = ctx.xfind(GOOGLE_ENCRYPTED[0].0) {
        hdrp.push(format!("XMP {k} is present."));
    }
    if ctx.google_hdrp {
        hdrp.push("The EXIF maker note is Google's HDR+ note (HDRP).".into());
    }
    if let Some(s) = text(Tag::Software).filter(|s| s.starts_with("HDR+")) {
        hdrp.push(format!("EXIF Software is {s}."));
    }
    if !hdrp.is_empty() {
        mf.push(("HDR+ multi-frame processing (Google)".into(), hdrp.join(" ")));
    }
    if mf.is_empty() && composite == Some(1) {
        mf.push(("Single frame (not a composite)".into(), "EXIF CompositeImage = 1.".into()));
    }
    if !mf.is_empty() {
        let value = mf.iter().map(|(v, _)| v.as_str()).collect::<Vec<_>>().join("; ");
        let note = format!(
            "Phones often merge several frames for less noise and more dynamic range. Sources: {}",
            mf.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>().join(" ")
        );
        add("Multi-frame / HDR", value, &note);
    }

    // HDR gain map.
    let mut gm: Vec<String> = Vec::new();
    if ctx.xhas_ns("hdrgm:") {
        gm.push("XMP hdrgm (Ultra HDR / Adobe gain map)".into());
    }
    if ctx.xhas_ns("HDRGainMap:") {
        gm.push("XMP HDRGainMap (Apple)".into());
    }
    if semantics.iter().any(|s| s.eq_ignore_ascii_case("GainMap")) {
        gm.push("GContainer GainMap item".into());
    }
    let heif_map = ctx.aux.iter().any(|a| a.to_ascii_lowercase().contains("hdrgainmap"));
    if heif_map {
        gm.push("HEIF auxiliary HDR gain map image".into());
    }
    if ctx.iso_gain_map {
        gm.push("ISO 21496-1 gain map segment".into());
    }
    if let Some(g) = ctx.gain {
        gm.push(format!("{} metadata in {}", g.meta.kind.label(), g.source));
    }
    if !gm.is_empty() {
        let value = match ctx.gain {
            Some(g) => format!("Yes, highlights up to {}x brighter", fmt_num(g.meta.max_boost() as f64)),
            None => "Yes (Ultra HDR / Apple HDR)".into(),
        };
        let mut note = format!("HDR gain map (Ultra HDR / Apple HDR): the photo can show brighter highlights on HDR screens; other screens show the normal image. Found: {}.", gm.join(", "));
        if heif_map {
            note.push_str(" The viewer's gain map views read JPEG files only, not HEIC.");
        }
        add("HDR gain map", value, &note);
    }

    // Night mode / long exposure.
    let mut night: Option<(String, String)> = None;
    if uint(Tag::SceneCaptureType) == Some(3) {
        night = Some(("Night scene mode".into(), "EXIF SceneCaptureType = Night scene.".into()));
    }
    if night.is_none() && (special_has("ASTRO") || special_has("NIGHT")) {
        let v = if special_has("ASTRO") { "Astrophotography (Google Night Sight)" } else { "Night Sight (Google)" };
        night = Some((v.into(), format!("XMP GCamera:SpecialTypeID = {}.", ctx.xget("GCamera:SpecialTypeID").unwrap_or_default())));
    }
    if night.is_none() {
        let xmp_night = ctx.xmp.iter().find(|(k, v)| k.to_ascii_lowercase().contains("night") || (k.ends_with("SpecialTypeID") && v.to_ascii_lowercase().contains("night")));
        if let Some(name) = sef_has("night") {
            night = Some(("Probably night mode".into(), format!("Guess: the Samsung trailer entry {name} mentions night.")));
        } else if let Some((k, _)) = xmp_night {
            night = Some(("Probably night mode".into(), format!("Guess: the XMP property {k} mentions night.")));
        }
    }
    if night.is_none() {
        if let Some(t) = t.filter(|&t| t >= 0.25) {
            night = Some((
                "Probably night mode or a long exposure".into(),
                format!("Guess: the shutter was open for {}; handheld phones only do that in night mode (or on a tripod).", fmt_exposure(t)),
            ));
        }
    }
    if let Some((v, note)) = night {
        add("Night mode / long exposure", v, &note);
    }

    // Portrait / depth.
    let mut depth: Vec<String> = Vec::new();
    if ctx.mpf.iter().any(|m| m.type_code() == 0x020002) {
        depth.push("MPF disparity (depth) image".into());
    }
    if ctx.xhas_ns("GDepth:") {
        depth.push("XMP GDepth depth map".into());
    }
    if ctx.xhas_ns("GImage:") {
        depth.push("XMP GImage (original before blur)".into());
    }
    if semantics.iter().any(|s| s.eq_ignore_ascii_case("Depth")) {
        depth.push("GContainer Depth item".into());
    }
    for key in ["dual_shot", "live_focus", "dualshot"] {
        if let Some(name) = sef_has(key) {
            depth.push(format!("Samsung trailer entry {name}"));
            break;
        }
    }
    if ctx.apple.capture_type == Some(2) {
        depth.push("Apple maker note capture type Portrait".into());
    }
    if matches!(custom, Some(7) | Some(8)) {
        depth.push("EXIF CustomRendered (Apple portrait)".into());
    }
    if ctx.aux.iter().any(|a| {
        let l = a.to_ascii_lowercase();
        l.contains("depth") || l.contains("disparity") || l.contains("matte")
    }) {
        depth.push("HEIF auxiliary depth / matte image".into());
    }
    // Google Camera portrait clues say the mode, not that a depth map is stored.
    let with_map = !depth.is_empty();
    if special_has("PORTRAIT") {
        depth.push("XMP GCamera:SpecialTypeID (portrait)".into());
    }
    if let Some((k, _)) = ctx.xfind(&["GCamera:PortraitNote", "GCamera:PortraitRequest", "GCamera:PortraitVersion"]) {
        depth.push(format!("XMP {k}"));
    }
    if !depth.is_empty() {
        let v = if with_map { "Portrait (background blur) with a depth map" } else { "Portrait (background blur)" };
        add("Portrait / depth", v.into(), &format!("Found: {}.", depth.join(", ")));
    }

    // Motion photo.
    let mut motion: Vec<String> = Vec::new();
    let flag_on = |k: &str| ctx.xget(k).is_some_and(|v| v.trim() == "1" || v.trim().eq_ignore_ascii_case("true"));
    if flag_on("GCamera:MotionPhoto") || flag_on("Camera:MotionPhoto") {
        motion.push("XMP MotionPhoto flag".into());
    }
    if flag_on("GCamera:MicroVideo") {
        motion.push("XMP MicroVideo flag".into());
    }
    let item_video = ctx.items.iter().find(|it| it.iter().any(|(k, v)| k.ends_with(":Semantic") && v == "MotionPhoto"));
    if item_video.is_some() {
        motion.push("GContainer MotionPhoto item".into());
    }
    if let Some(name) = sef_has("motion") {
        motion.push(format!("Samsung trailer entry {name}"));
    }
    if ctx.video.is_some() {
        motion.push("video data after the JPEG".into());
    }
    if ctx.apple.content_id.is_some() {
        add(
            "Live Photo",
            "Yes (the video is a separate .MOV file)".into(),
            "The Apple maker note carries a Live Photo content identifier that pairs this photo with its video.",
        );
    }
    if !motion.is_empty() {
        let size = ctx
            .video
            .or_else(|| item_video.and_then(|it| it.iter().find(|(k, _)| k.ends_with(":Length")).and_then(|(_, v)| v.trim().parse().ok())))
            .or_else(|| ctx.xget("GCamera:MicroVideoOffset").and_then(|v| v.trim().parse().ok()))
            .filter(|&s: &u64| s > 0);
        let value = match size {
            Some(s) => format!("Yes, embedded video of {}", human_size(s)),
            None => "Yes".into(),
        };
        add("Motion photo", value, &format!("A short video clip is stored inside the file. Found: {}.", motion.join(", ")));
    }

    // Panorama and burst.
    let mut pano: Vec<String> = Vec::new();
    if ctx.mpf.iter().any(|m| m.type_code() == 0x020001) {
        pano.push("MPF panorama image".into());
    }
    if custom == Some(6) {
        pano.push("EXIF CustomRendered = 6 (Apple)".into());
    }
    if let Some(name) = sef_has("panorama") {
        pano.push(format!("Samsung trailer entry {name}"));
    }
    if ctx.xhas_ns("GPano:") {
        pano.push("XMP GPano (photo sphere / panorama)".into());
    }
    if special_has("PANO") || special_has("SPHERE") {
        pano.push("XMP GCamera:SpecialTypeID (panorama)".into());
    }
    if !pano.is_empty() {
        add("Panorama", "Yes".into(), &format!("Found: {}.", pano.join(", ")));
    }
    let mut burst: Vec<String> = Vec::new();
    if let Some(name) = sef_has("burst") {
        burst.push(format!("Samsung trailer entry {name}"));
    }
    if ctx.apple.burst.is_some() {
        burst.push("Apple maker note burst ID".into());
    }
    if let Some((k, _)) = ctx.xmp.iter().find(|(k, _)| k.contains("BurstID") || k.contains("BurstUUID")) {
        burst.push(format!("XMP {k}"));
    }
    if !burst.is_empty() {
        let best = ctx.xfind(&["GCamera:BurstPrimary"]).is_some_and(|(_, v)| v.trim() == "1" || v.trim().eq_ignore_ascii_case("true"));
        if best {
            burst.push("XMP GCamera:BurstPrimary = 1 (the camera picked this photo)".into());
        }
        let v = if best { "Part of a burst sequence (the best shot)" } else { "Part of a burst sequence" };
        add("Burst", v.into(), &format!("Found: {}.", burst.join(", ")));
    }
    if let Some(name) = sef_has("single_take").or_else(|| sef_has("singletake")) {
        add("Single Take", "Yes".into(), &format!("Samsung trailer entry {name}."));
    }
    // Encrypted Google Camera blocks: only their presence can be shown.
    let mut blocks: Vec<&str> = Vec::new();
    let mut found: Vec<String> = Vec::new();
    if ctx.google_hdrp {
        blocks.push(GOOGLE_ENCRYPTED[0].1);
        found.push("the EXIF maker note (HDRP)".into());
    }
    for (names, what) in GOOGLE_ENCRYPTED.iter() {
        if let Some((k, _)) = ctx.xfind(names) {
            if !blocks.contains(what) {
                blocks.push(*what);
            }
            found.push(format!("XMP {k}"));
        }
    }
    if !blocks.is_empty() {
        add(
            "Google Camera data",
            format!("{} (encrypted)", blocks.join(", ")),
            &format!("Blocks the Google Camera app stores encrypted; only Google's own tools read them. They show the photo came from a Pixel's camera app. Found: {}.", found.join(", ")),
        );
    }
    if let Some(v) = ctx.apple.capture_type.and_then(apple_capture_type) {
        add("Apple capture type", v.into(), "From the Apple maker note.");
    }

    // Color space.
    let mut cs: Vec<String> = Vec::new();
    if let Some(v) = uint(Tag::ColorSpace) {
        cs.push(match v {
            1 => "sRGB (EXIF)".to_string(),
            2 => "Adobe RGB (EXIF)".to_string(),
            0xFFFF => "Uncalibrated (EXIF)".to_string(),
            v => format!("{v} (EXIF)"),
        });
    }
    if let Some(d) = ctx.icc_desc {
        cs.push(format!("{d} (ICC profile)"));
    }
    if let Some(c) = ctx.cicp {
        cs.push(c.to_string());
    }
    if sef_has("color_display_p3").is_some() && !cs.iter().any(|c| c.contains("P3")) {
        cs.push("Display P3 (Samsung trailer)".into());
    }
    if !cs.is_empty() {
        let mut note = "Which colors the pixel values mean. ".to_string();
        if uint(Tag::ColorSpace) == Some(0xFFFF) {
            note.push_str("Uncalibrated in EXIF usually means a wide-gamut profile is embedded; viewers use the ICC profile. ");
        }
        if let Some(d) = ctx.icc_desc {
            note.push_str(super::icc::gamut_note(d));
        }
        add("Color space", cs.join(", "), note.trim());
    }

    // Time, place, identity.
    if let Some(dt) = text(Tag::DateTimeOriginal).or_else(|| text(Tag::DateTimeDigitized)) {
        let mut v = dt.replacen(':', "-", 2);
        if let Some(sub) = text(Tag::SubSecTimeOriginal).filter(|s| s.chars().all(|c| c.is_ascii_digit())) {
            v.push_str(&format!(".{sub}"));
        }
        if let Some(off) = text(Tag::OffsetTimeOriginal) {
            v.push_str(&format!(" {off}"));
        }
        add("Captured", v, "Local time on the camera when the photo was taken, with the time zone offset when stored.");
    }
    if let Some(e) = ctx.exif {
        let gps: Vec<&exif::Field> = e.fields().filter(|f| f.tag.context() == exif::Context::Gps).collect();
        let has_pos = gps.iter().any(|f| f.tag == Tag::GPSLatitude) && gps.iter().any(|f| f.tag == Tag::GPSLongitude);
        let v = if has_pos {
            "Yes (coordinates are in the EXIF tab)"
        } else if !gps.is_empty() {
            "GPS tags without a position"
        } else {
            "No GPS data"
        };
        add("Location", v.into(), "Whether the photo records where it was taken.");
    }
    if let Some(id) = text(Tag::ImageUniqueID) {
        add("Image ID", id, "Unique identifier the camera assigned to this capture.");
    }
    f
}
