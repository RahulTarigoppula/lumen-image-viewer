//! ICC color profile header and description.

use super::{be32, Field, Group};

fn sig(b: &[u8], o: usize) -> String {
    b.get(o..o + 4).map(|s| String::from_utf8_lossy(s).trim().to_string()).unwrap_or_default()
}

/// "Color profile (ICC)" group and the profile description (for Capture).
pub(crate) fn group(icc: &[u8], warn: &mut Vec<String>) -> (Group, Option<String>) {
    let mut f = Vec::new();
    if icc.len() < 128 {
        warn.push(format!("The ICC profile is too short ({} bytes).", icc.len()));
        f.push(Field::new("Size", format!("{} bytes", icc.len()), ""));
        return (Group { name: "Color profile (ICC)".into(), fields: f }, None);
    }
    if icc.get(36..40) != Some(&b"acsp"[..]) {
        warn.push("The ICC profile has no 'acsp' signature (it may be damaged).".into());
    }
    let declared = be32(icc, 0).unwrap_or(0) as usize;
    if declared != icc.len() {
        warn.push(format!("The ICC profile declares {declared} bytes but {} are stored.", icc.len()));
    }
    let desc = crate::decode::color::profile_description(icc).filter(|d| !d.is_empty());
    if let Some(d) = &desc {
        f.push(Field::new("Description", d.clone(), gamut_note(d)));
    }
    let space = sig(icc, 16);
    let space_name = match space.as_str() {
        "RGB" => "RGB".to_string(),
        "GRAY" => "Grayscale".to_string(),
        "CMYK" => "CMYK".to_string(),
        "Lab" => "CIE Lab".to_string(),
        "YCbr" => "YCbCr".to_string(),
        s => s.to_string(),
    };
    f.push(Field::new("Color space", space_name, "The color model of the pixel data this profile describes."));
    let pcs = match sig(icc, 20).as_str() {
        "XYZ" => "CIE XYZ".to_string(),
        "Lab" => "CIE Lab".to_string(),
        s => s.to_string(),
    };
    f.push(Field::new("Connection space (PCS)", pcs, "The device-independent space used to convert between profiles."));
    let (major, minor) = (icc[8], icc[9] >> 4);
    f.push(Field::new("Version", format!("{major}.{minor}"), "ICC v2 profiles are the most compatible; v4 profiles are newer and more precise."));
    let class = match sig(icc, 12).as_str() {
        "mntr" => "Display (monitor)".to_string(),
        "scnr" => "Input (camera or scanner)".to_string(),
        "prtr" => "Output (printer)".to_string(),
        "link" => "Device link".to_string(),
        "spac" => "Color space".to_string(),
        "abst" => "Abstract".to_string(),
        "nmcl" => "Named color".to_string(),
        s => s.to_string(),
    };
    f.push(Field::new("Device class", class, ""));
    let intent = match be32(icc, 64).unwrap_or(0) {
        0 => "Perceptual",
        1 => "Relative colorimetric",
        2 => "Saturation",
        3 => "Absolute colorimetric",
        _ => "Unknown",
    };
    f.push(Field::new("Rendering intent", intent, "How out-of-gamut colors are handled when converting (the profile's default)."));
    let creator = sig(icc, 80);
    if !creator.is_empty() && creator.chars().all(|c| c.is_ascii_graphic() || c == ' ') {
        f.push(Field::new("Creator", creator, "Signature of the software or vendor that made the profile."));
    }
    f.push(Field::new("Size", format!("{} bytes", super::thousands(icc.len() as u64)), ""));
    (Group { name: "Color profile (ICC)".into(), fields: f }, desc)
}

/// Explanation for well-known profile names.
pub(crate) fn gamut_note(desc: &str) -> &'static str {
    let l = desc.to_ascii_lowercase();
    if l.contains("p3") {
        "Display P3: the wide color gamut of iPhones, many Android phones and modern screens; about 25% more colors than sRGB (deeper reds and greens)."
    } else if l.contains("adobe rgb") || l.contains("adobergb") {
        "Adobe RGB: a wide gamut used by cameras and print workflows; looks dull if a viewer ignores the profile."
    } else if l.contains("2020") || l.contains("2100") || l.contains("pq") || l.contains("hlg") {
        "BT.2020 / BT.2100: the very wide gamut used for HDR video and HDR photos."
    } else if l.contains("srgb") {
        "sRGB: the standard color space of the web and most screens."
    } else if l.contains("prophoto") {
        "ProPhoto RGB: a very wide editing gamut; needs color management to look right."
    } else {
        ""
    }
}
