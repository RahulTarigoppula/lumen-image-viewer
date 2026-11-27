//! Tests build tiny files byte by byte so every structure is known exactly.

use super::jpeg::{ijg_table, STD_CHROMA, STD_LUMA, ZIGZAG};
use super::*;

// ---- Builders ----

/// TIFF value for the test writer.
enum V {
    A(&'static str),
    S(Vec<u16>),
    L(Vec<u32>),
    R(Vec<(u32, u32)>),
    Sr(Vec<(i32, i32)>),
    U(Vec<u8>),
}

impl V {
    fn encode(&self) -> (u16, u32, Vec<u8>) {
        match self {
            V::A(s) => {
                let mut b = s.as_bytes().to_vec();
                b.push(0);
                (2, b.len() as u32, b)
            }
            V::S(v) => (3, v.len() as u32, v.iter().flat_map(|x| x.to_le_bytes()).collect()),
            V::L(v) => (4, v.len() as u32, v.iter().flat_map(|x| x.to_le_bytes()).collect()),
            V::R(v) => (5, v.len() as u32, v.iter().flat_map(|(n, d)| n.to_le_bytes().into_iter().chain(d.to_le_bytes())).collect()),
            V::Sr(v) => (10, v.len() as u32, v.iter().flat_map(|(n, d)| n.to_le_bytes().into_iter().chain(d.to_le_bytes())).collect()),
            V::U(v) => (7, v.len() as u32, v.clone()),
        }
    }
}

/// Write one little-endian IFD at the end of `out` (offsets relative to the
/// start of `out`). Returns the absolute positions of each entry's value field.
fn write_ifd(out: &mut Vec<u8>, entries: &[(u16, V)]) -> Vec<(u16, usize)> {
    let start = out.len();
    let n = entries.len();
    let data_start = start + 2 + 12 * n + 4;
    let mut data = Vec::new();
    let mut pos = Vec::new();
    out.extend_from_slice(&(n as u16).to_le_bytes());
    for (tag, v) in entries {
        let (typ, count, bytes) = v.encode();
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&typ.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        pos.push((*tag, out.len()));
        if bytes.len() <= 4 {
            let mut b = bytes.clone();
            b.resize(4, 0);
            out.extend_from_slice(&b);
        } else {
            out.extend_from_slice(&((data_start + data.len()) as u32).to_le_bytes());
            data.extend_from_slice(&bytes);
            if data.len() % 2 == 1 {
                data.push(0);
            }
        }
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&data);
    pos
}

/// Little-endian TIFF with IFD0 and optional Exif and GPS sub-IFDs.
fn tiff(mut ifd0: Vec<(u16, V)>, exif: Vec<(u16, V)>, gps: Vec<(u16, V)>) -> Vec<u8> {
    let mut out = b"II*\0".to_vec();
    out.extend_from_slice(&8u32.to_le_bytes());
    if !exif.is_empty() {
        ifd0.push((0x8769, V::L(vec![0])));
    }
    if !gps.is_empty() {
        ifd0.push((0x8825, V::L(vec![0])));
    }
    ifd0.sort_by_key(|e| e.0);
    let pos = write_ifd(&mut out, &ifd0);
    for (ptr, sub) in [(0x8769u16, exif), (0x8825, gps)] {
        if sub.is_empty() {
            continue;
        }
        let at = out.len() as u32;
        let mut sub = sub;
        sub.sort_by_key(|e| e.0);
        write_ifd(&mut out, &sub);
        let p = pos.iter().find(|(t, _)| *t == ptr).unwrap().1;
        out[p..p + 4].copy_from_slice(&at.to_le_bytes());
    }
    out
}

fn seg(marker: u8, data: &[u8]) -> Vec<u8> {
    let mut v = vec![0xFF, marker];
    v.extend_from_slice(&((data.len() + 2) as u16).to_be_bytes());
    v.extend_from_slice(data);
    v
}

/// DQT segment payload holding table `id` in zigzag order.
fn dqt(id: u8, natural: &[u16; 64]) -> Vec<u8> {
    let mut d = vec![id];
    for k in 0..64 {
        d.push(natural[ZIGZAG[k]] as u8);
    }
    d
}

fn sof(marker: u8, w: u16, h: u16, sampling: [u8; 3]) -> Vec<u8> {
    let mut d = vec![8];
    d.extend_from_slice(&h.to_be_bytes());
    d.extend_from_slice(&w.to_be_bytes());
    d.push(3);
    for (i, s) in sampling.iter().enumerate() {
        d.extend_from_slice(&[i as u8 + 1, *s, if i == 0 { 0 } else { 1 }]);
    }
    seg(marker, &d)
}

/// Minimal ICC v4 display profile with an 'mluc' description.
fn icc_profile(desc: &str) -> Vec<u8> {
    let utf16: Vec<u8> = desc.encode_utf16().flat_map(|c| c.to_be_bytes()).collect();
    let mut tag = b"mluc".to_vec();
    tag.extend_from_slice(&[0; 4]);
    tag.extend_from_slice(&1u32.to_be_bytes());
    tag.extend_from_slice(&12u32.to_be_bytes());
    tag.extend_from_slice(b"enUS");
    tag.extend_from_slice(&(utf16.len() as u32).to_be_bytes());
    tag.extend_from_slice(&28u32.to_be_bytes());
    tag.extend_from_slice(&utf16);
    let tag_off = 128 + 4 + 12;
    let total = tag_off + tag.len();
    let mut h = vec![0u8; 128];
    h[0..4].copy_from_slice(&(total as u32).to_be_bytes());
    h[4..8].copy_from_slice(b"appl");
    h[8] = 4;
    h[9] = 0x40;
    h[12..16].copy_from_slice(b"mntr");
    h[16..20].copy_from_slice(b"RGB ");
    h[20..24].copy_from_slice(b"XYZ ");
    h[36..40].copy_from_slice(b"acsp");
    h[80..84].copy_from_slice(b"appl");
    h.extend_from_slice(&1u32.to_be_bytes());
    h.extend_from_slice(b"desc");
    h.extend_from_slice(&(tag_off as u32).to_be_bytes());
    h.extend_from_slice(&(tag.len() as u32).to_be_bytes());
    h.extend_from_slice(&tag);
    h
}

const XMP: &str = r#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:GCamera="http://ns.google.com/photos/1.0/camera/"
    xmlns:hdrgm="http://ns.adobe.com/hdr-gain-map/1.0/" xmlns:dc="http://purl.org/dc/elements/1.1/"
    GCamera:MotionPhoto="1" hdrgm:Version="1.0" GCamera:Thumb="THUMB">
   <dc:creator><rdf:Seq><rdf:li>Alice</rdf:li><rdf:li>Bob &amp; Co</rdf:li></rdf:Seq></dc:creator>
   <dc:title><rdf:Alt><rdf:li xml:lang="x-default">Test &#x263A;</rdf:li></rdf:Alt></dc:title>
   <xmp:Label>Tom &lt;3</xmp:Label>
   <GContainer:Directory>
    <rdf:Seq>
     <rdf:li rdf:parseType="Resource"><Container:Item Item:Semantic="Primary" Item:Mime="image/jpeg"/></rdf:li>
     <rdf:li rdf:parseType="Resource"><Container:Item Item:Semantic="GainMap" Item:Mime="image/jpeg" Item:Length="1000"/></rdf:li>
    </rdf:Seq>
   </GContainer:Directory>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
<?xpacket end="w"?>"#;

fn xmp_text() -> String {
    XMP.replace("THUMB", &"QUJD".repeat(600))
}

fn mpf_payload(second_size: u32) -> Vec<u8> {
    let mut entries = Vec::new();
    // Primary: representative + baseline primary; second: undefined type (gain map).
    for (attr, size, off) in [(0x2003_0000u32, 5000u32, 0u32), (0, second_size, 4000)] {
        entries.extend_from_slice(&attr.to_le_bytes());
        entries.extend_from_slice(&size.to_le_bytes());
        entries.extend_from_slice(&off.to_le_bytes());
        entries.extend_from_slice(&[0; 4]);
    }
    let t = tiff(
        vec![(0xB000, V::U(b"0100".to_vec())), (0xB001, V::L(vec![2])), (0xB002, V::U(entries))],
        vec![],
        vec![],
    );
    let mut d = b"MPF\0".to_vec();
    d.extend_from_slice(&t);
    d
}

/// Samsung SEF trailer following the documented layout.
fn sef_trailer(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut data = Vec::new();
    let mut starts = Vec::new();
    for (name, payload) in entries {
        starts.push((data.len(), 8 + name.len() + payload.len()));
        data.extend_from_slice(&[0, 0, 0x01, 0x0A]);
        data.extend_from_slice(&(name.len() as u32).to_le_bytes());
        data.extend_from_slice(name.as_bytes());
        data.extend_from_slice(payload);
    }
    let dir_at = data.len();
    let mut dir = b"SEFH".to_vec();
    dir.extend_from_slice(&106u32.to_le_bytes());
    dir.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (start, size) in &starts {
        dir.extend_from_slice(&[0, 0, 0x01, 0x0A]);
        dir.extend_from_slice(&((dir_at - start) as u32).to_le_bytes());
        dir.extend_from_slice(&(*size as u32).to_le_bytes());
    }
    let mut out = data;
    out.extend_from_slice(&dir);
    out.extend_from_slice(&(dir.len() as u32).to_le_bytes());
    out.extend_from_slice(b"SEFT");
    out
}

fn ftyp_video() -> Vec<u8> {
    let mut v = 24u32.to_be_bytes().to_vec();
    v.extend_from_slice(b"ftypmp42");
    v.extend_from_slice(&0u32.to_be_bytes());
    v.extend_from_slice(b"isommp42");
    v.extend_from_slice(&[0u8; 3000]);
    v
}

fn phone_exif() -> Vec<u8> {
    tiff(
        vec![(0x010F, V::A("Google")), (0x0110, V::A("Pixel 8")), (0x0131, V::A("HDR+ 1.0.123")), (0x0112, V::S(vec![1]))],
        vec![
            (0x829A, V::R(vec![(1, 50)])),
            (0x829D, V::R(vec![(18, 10)])),
            (0x8827, V::S(vec![800])),
            (0x9003, V::A("2024:05:01 18:22:31")),
            (0x9011, V::A("+02:00")),
            (0x9204, V::Sr(vec![(-2, 3)])),
            (0x9209, V::S(vec![16])),
            (0x920A, V::R(vec![(69, 10)])),
            (0x927C, V::U(b"HDRP\x01\x02\x03\x04\x05\x06\x07\x08".to_vec())),
            (0xA001, V::S(vec![0xFFFF])),
            (0xA405, V::S(vec![26])),
            (0xA406, V::S(vec![0])),
            (0xA420, V::A("ABC123")),
            (0xA460, V::S(vec![3])),
            (0xA461, V::S(vec![9, 9])),
        ],
        vec![
            (0x0001, V::A("N")),
            (0x0002, V::R(vec![(48, 1), (51, 1), (0, 1)])),
            (0x0003, V::A("E")),
            (0x0004, V::R(vec![(2, 1), (21, 1), (0, 1)])),
        ],
    )
}

/// A phone-like JPEG with every structure the inspector reads.
fn full_jpeg(q: u32) -> Vec<u8> {
    let mut j = vec![0xFF, 0xD8];
    let mut app1 = b"Exif\0\0".to_vec();
    app1.extend_from_slice(&phone_exif());
    j.extend(seg(0xE1, &app1));
    let mut x = super::jpeg::XMP_PREFIX.to_vec();
    x.extend_from_slice(xmp_text().as_bytes());
    j.extend(seg(0xE1, &x));
    // ICC in two chunks, stored out of order.
    let icc = icc_profile("Display P3");
    let (a, b) = icc.split_at(100);
    for (seq, part) in [(2u8, b), (1u8, a)] {
        let mut d = b"ICC_PROFILE\0".to_vec();
        d.extend_from_slice(&[seq, 2]);
        d.extend_from_slice(part);
        j.extend(seg(0xE2, &d));
    }
    j.extend(seg(0xE2, &mpf_payload(1000)));
    j.extend(seg(0xFE, b"Hello comment"));
    let mut tables = dqt(0, &ijg_table(&STD_LUMA, q, true));
    tables.extend(dqt(1, &ijg_table(&STD_CHROMA, q, true)));
    j.extend(seg(0xDB, &tables));
    j.extend(sof(0xC0, 64, 48, [0x22, 0x11, 0x11]));
    j.extend(seg(0xDD, &[0, 4]));
    // SOS header, then entropy data with stuffed bytes and a restart marker.
    j.extend(seg(0xDA, &[3, 1, 0x00, 2, 0x11, 3, 0x11, 0, 63, 0]));
    j.extend_from_slice(&[0x12, 0xFF, 0x00, 0x34, 0xFF, 0xD0, 0x56, 0x78]);
    j.extend_from_slice(&[0xFF, 0xD9]);
    j
}

/// Gain map XMP as Ultra HDR writes it into the gain map image.
const GAIN_XMP: &str = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
 <rdf:Description rdf:about="" xmlns:hdrgm="http://ns.adobe.com/hdr-gain-map/1.0/" hdrgm:Version="1.0" hdrgm:GainMapMin="0"
  hdrgm:GainMapMax="MAX" hdrgm:Gamma="1" hdrgm:OffsetSDR="0.015625" hdrgm:OffsetHDR="0.015625" hdrgm:HDRCapacityMin="0"
  hdrgm:HDRCapacityMax="2" hdrgm:BaseRenditionIsHDR="False"/></rdf:RDF></x:xmpmeta>"#;

fn gain_xmp_segment(max: &str) -> Vec<u8> {
    let mut x = super::jpeg::XMP_PREFIX.to_vec();
    x.extend_from_slice(GAIN_XMP.replace("MAX", max).as_bytes());
    x
}

fn full_phone_file() -> Vec<u8> {
    let mut f = full_jpeg(75);
    // A second (gain map) JPEG, the motion photo video, then the Samsung trailer.
    f.extend_from_slice(&[0xFF, 0xD8]);
    f.extend(seg(0xE1, &gain_xmp_segment("2")));
    f.extend_from_slice(&[0xFF, 0xD9]);
    let video_at = f.len();
    f.extend(ftyp_video());
    let video_len = f.len() - video_at;
    assert_eq!(video_len, 3024);
    f.extend(sef_trailer(&[
        ("Image_UTC_Data", &b"1700000000000"[..]),
        ("MCC_Data", &b"450"[..]),
        ("Photo_HDR_Info", &[0u8, 1, 2, 3, 4][..]),
        ("Live_Focus_Info", &b"blur=5"[..]),
    ]));
    f
}

fn group<'a>(i: &'a Inspection, name: &str) -> &'a Group {
    i.other.iter().chain(i.exif.iter()).find(|g| g.name == name).unwrap_or_else(|| panic!("no group {name}: {:#?}", i.other))
}

fn value<'a>(fields: &'a [Field], name: &str) -> &'a str {
    fields.iter().find(|f| f.name == name).map(|f| f.value.as_str()).unwrap_or_else(|| panic!("no field {name}: {fields:#?}"))
}

fn quality_of(i: &Inspection) -> u32 {
    let v = value(&group(i, "Encoding").fields, "JPEG quality");
    v.trim_start_matches("about ").parse().unwrap()
}

// ---- Tests ----

#[test]
fn quality_estimate_matches_ijg_tables() {
    for q in [10u32, 50, 75, 85, 92, 98] {
        let (est, err) = super::jpeg::estimate_quality(&ijg_table(&STD_LUMA, q, true), Some(&ijg_table(&STD_CHROMA, q, true)));
        assert!(est.abs_diff(q) <= 2, "q={q} estimated {est}");
        assert!(err < 0.05, "q={q} err {err}");
    }
    for q in [75u32, 92] {
        let i = inspect_impl(&full_jpeg(q), "jpg", 0);
        assert!(quality_of(&i).abs_diff(q) <= 2, "q={q}: {}", quality_of(&i));
    }
}

#[test]
fn subsampling_names() {
    use super::jpeg::subsampling_name as s;
    assert_eq!(s((2, 2), (1, 1)), Some("4:2:0"));
    assert_eq!(s((2, 1), (1, 1)), Some("4:2:2"));
    assert_eq!(s((1, 1), (1, 1)), Some("4:4:4"));
    assert_eq!(s((1, 2), (1, 1)), Some("4:4:0"));
    assert_eq!(s((3, 1), (2, 1)), None);
}

#[test]
fn phone_jpeg_full_inspection() {
    let file = full_phone_file();
    let i = inspect_impl(&file, "jpg", file.len() as u64);
    let names: Vec<&str> = i.other.iter().map(|g| g.name.as_str()).collect();
    assert_eq!(
        names,
        ["File", "Encoding", "Color profile (ICC)", "XMP", "Samsung trailer", "Multi-picture (MPF)", "HDR gain map", "Maker note", "Comments"]
    );
    assert!(i.warnings.is_empty(), "{:?}", i.warnings);

    let file_g = &group(&i, "File").fields;
    assert_eq!(value(file_g, "Format"), "JPEG");
    assert_eq!(value(file_g, "Pixel dimensions"), "64 x 48");
    assert_eq!(value(file_g, "Aspect ratio"), "4:3");

    let enc = &group(&i, "Encoding").fields;
    assert!(value(enc, "Coding").starts_with("Baseline"));
    assert!(value(enc, "Chroma subsampling").starts_with("4:2:0"));
    assert_eq!(value(enc, "Restart interval"), "Every 4 MCUs");
    assert!(value(enc, "Data after the image").contains("after the JPEG end marker"));
    assert!(quality_of(&i).abs_diff(75) <= 2);

    let icc = &group(&i, "Color profile (ICC)").fields;
    assert_eq!(value(icc, "Description"), "Display P3");
    assert_eq!(value(icc, "Version"), "4.4");
    assert_eq!(value(icc, "Device class"), "Display (monitor)");

    let xmp = &group(&i, "XMP").fields;
    assert_eq!(value(xmp, "dc:creator"), "Alice, Bob & Co");
    assert_eq!(value(xmp, "dc:title"), "Test \u{263A}");
    assert_eq!(value(xmp, "xmp:Label"), "Tom <3");
    assert_eq!(value(xmp, "GCamera:MotionPhoto"), "1");
    assert!(value(xmp, "GCamera:Thumb").starts_with("(binary data, 2 KB)"), "{}", value(xmp, "GCamera:Thumb"));
    assert!(xmp.iter().any(|f| f.name.ends_with("Item:Semantic") && f.value == "GainMap"));

    let sef = &group(&i, "Samsung trailer").fields;
    assert!(value(sef, "Image_UTC_Data").starts_with("1700000000000 (2023-11-14 22:13:20 UTC)"));
    assert_eq!(value(sef, "MCC_Data"), "450");
    assert_eq!(value(sef, "Photo_HDR_Info"), "(binary, 5 bytes)");
    assert!(!sef.iter().find(|f| f.name == "MCC_Data").unwrap().note.is_empty());

    let mpf = &group(&i, "Multi-picture (MPF)").fields;
    assert_eq!(value(mpf, "Images"), "2");
    assert!(value(mpf, "Image 1").starts_with("Baseline primary image"));
    assert!(value(mpf, "Image 2").contains("probably a gain map"));
    assert!(value(mpf, "Container item 2").starts_with("GainMap"));

    let gm = &group(&i, "HDR gain map").fields;
    assert_eq!(value(gm, "Format"), "Ultra HDR (Adobe gain map XMP)");
    assert!(value(gm, "Gain map image").starts_with("The image the GContainer directory lists"), "{}", value(gm, "Gain map image"));
    assert_eq!(value(gm, "Boost"), "1x to 4x");

    let maker = &group(&i, "Maker note").fields;
    assert_eq!(value(maker, "Size"), "12 bytes");
    assert_eq!(value(maker, "Format"), "Google HDR+ maker note");
    assert_eq!(value(maker, "Contents"), "Encrypted (not readable)");

    assert_eq!(value(&group(&i, "Comments").fields, "Comment"), "Hello comment");

    // EXIF groups.
    let ex_names: Vec<&str> = i.exif.iter().map(|g| g.name.as_str()).collect();
    assert_eq!(ex_names, ["Image", "EXIF", "GPS"]);
    assert_eq!(value(&group(&i, "Image").fields, "Make"), "Google");

    // Capture.
    let c = &i.capture;
    assert_eq!(value(c, "Camera"), "Google Pixel 8");
    assert_eq!(value(c, "Firmware / software"), "HDR+ 1.0.123");
    assert_eq!(value(c, "Camera module"), "Main (wide), guessed from the focal length");
    assert_eq!(value(c, "Equivalent focal length"), "26 mm (actual 6.9 mm)");
    assert_eq!(value(c, "Exposure"), "1/50 s \u{00B7} f/1.8 \u{00B7} ISO 800");
    assert_eq!(value(c, "Exposure value"), "EV 4.3");
    assert_eq!(value(c, "Exposure bias"), "-0.7 EV");
    let mf = value(c, "Multi-frame / HDR");
    assert!(mf.contains("Composite captured while shooting") && mf.contains("9 frames merged"), "{mf}");
    assert!(mf.contains("Samsung HDR"), "{mf}");
    assert!(mf.contains("HDR+ multi-frame processing (Google)"), "{mf}");
    assert_eq!(value(c, "HDR gain map"), "Yes, highlights up to 4x brighter");
    assert!(value(c, "Portrait / depth").starts_with("Portrait"));
    assert_eq!(value(c, "Motion photo"), "Yes, embedded video of 3.0 KB");
    assert_eq!(value(c, "Color space"), "Uncalibrated (EXIF), Display P3 (ICC profile)");
    assert_eq!(value(c, "Captured"), "2024-05-01 18:22:31 +02:00");
    assert_eq!(value(c, "Location"), "Yes (coordinates are in the EXIF tab)");
    assert_eq!(value(c, "Image ID"), "ABC123");
    // Field order: camera first, location after captured.
    let pos = |n: &str| c.iter().position(|f| f.name == n).unwrap();
    assert!(pos("Camera") < pos("Exposure") && pos("Captured") < pos("Location"));
}

#[test]
fn progressive_444_quality_92() {
    let mut j = vec![0xFF, 0xD8];
    let mut tables = dqt(0, &ijg_table(&STD_LUMA, 92, true));
    tables.extend(dqt(1, &ijg_table(&STD_CHROMA, 92, true)));
    j.extend(seg(0xDB, &tables));
    j.extend(sof(0xC2, 4000, 3000, [0x11, 0x11, 0x11]));
    for _ in 0..3 {
        j.extend(seg(0xDA, &[1, 1, 0x00, 0, 0, 0]));
        j.extend_from_slice(&[1, 2, 3]);
    }
    j.extend_from_slice(&[0xFF, 0xD9]);
    let i = inspect_impl(&j, "jpg", j.len() as u64);
    let enc = &group(&i, "Encoding").fields;
    assert!(value(enc, "Coding").starts_with("Progressive"));
    assert!(value(enc, "Chroma subsampling").starts_with("4:4:4"));
    assert_eq!(value(enc, "Scans"), "3");
    assert!(quality_of(&i).abs_diff(92) <= 2);
    assert_eq!(value(&group(&i, "File").fields, "Megapixels"), "12.0 MP");
    assert!(i.capture.is_empty());
}

#[test]
fn png_text_and_xmp() {
    use std::io::Write;
    fn chunk(ty: &[u8], d: &[u8]) -> Vec<u8> {
        let mut v = (d.len() as u32).to_be_bytes().to_vec();
        v.extend_from_slice(ty);
        v.extend_from_slice(d);
        v.extend_from_slice(&[0; 4]);
        v
    }
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = 640u32.to_be_bytes().to_vec();
    ihdr.extend_from_slice(&480u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    png.extend(chunk(b"IHDR", &ihdr));
    png.extend(chunk(b"gAMA", &45455u32.to_be_bytes()));
    let long_prompt = format!("a cat, masterpiece\nSteps: 20, Sampler: Euler a, Seed: 42{}", "x".repeat(3000));
    let mut t = b"parameters\0".to_vec();
    t.extend_from_slice(long_prompt.as_bytes());
    png.extend(chunk(b"tEXt", &t));
    let mut z = b"Comment\0\0".to_vec();
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(b"compressed hello").unwrap();
    z.extend(enc.finish().unwrap());
    png.extend(chunk(b"zTXt", &z));
    let mut it = b"XML:com.adobe.xmp\0\0\0\0\0".to_vec();
    it.extend_from_slice(XMP.as_bytes());
    png.extend(chunk(b"iTXt", &it));
    png.extend(chunk(b"IDAT", &[0; 10]));
    png.extend(chunk(b"IEND", &[]));
    let i = inspect_impl(&png, "png", png.len() as u64);
    assert!(i.warnings.is_empty(), "{:?}", i.warnings);
    assert_eq!(value(&group(&i, "File").fields, "Pixel dimensions"), "640 x 480");
    let enc = &group(&i, "Encoding").fields;
    assert_eq!(value(enc, "Color type"), "RGBA (RGB + alpha)");
    assert!(value(enc, "Gamma (gAMA)").starts_with("0.45455"));
    assert_eq!(value(enc, "Chunks"), "IHDR, gAMA, tEXt, zTXt, iTXt, IDAT, IEND");
    let text = &group(&i, "PNG text").fields;
    let params = text.iter().find(|f| f.name == "parameters").unwrap();
    assert!(params.value.starts_with("a cat, masterpiece"));
    assert_eq!(params.value.chars().count(), 2003);
    assert!(params.note.contains("generator settings"));
    assert_eq!(value(text, "Comment"), "compressed hello");
    assert_eq!(value(&group(&i, "XMP").fields, "dc:creator"), "Alice, Bob & Co");
}

#[test]
fn gif_and_webp_basics() {
    let mut g = b"GIF89a".to_vec();
    g.extend_from_slice(&[2, 0, 2, 0, 0x80, 0, 0]);
    g.extend_from_slice(&[0; 6]);
    g.extend_from_slice(&[0x21, 0xFE, 5]);
    g.extend_from_slice(b"hello");
    g.push(0);
    g.extend_from_slice(&[0x2C, 0, 0, 0, 0, 2, 0, 2, 0, 0, 2, 2, 0x4C, 0x01, 0, 0x3B]);
    let i = inspect_impl(&g, "gif", g.len() as u64);
    assert!(i.warnings.is_empty(), "{:?}", i.warnings);
    assert_eq!(value(&group(&i, "Encoding").fields, "Frames"), "1");
    assert_eq!(value(&group(&i, "Comments").fields, "Comment"), "hello");

    let bits: u32 = 2 | (4 << 14);
    let mut vp8l = vec![0x2f];
    vp8l.extend_from_slice(&bits.to_le_bytes());
    let mut w = b"RIFF".to_vec();
    w.extend_from_slice(&((4 + 8 + vp8l.len() + 1) as u32).to_le_bytes());
    w.extend_from_slice(b"WEBPVP8L");
    w.extend_from_slice(&(vp8l.len() as u32).to_le_bytes());
    w.extend_from_slice(&vp8l);
    w.push(0);
    let i = inspect_impl(&w, "webp", w.len() as u64);
    assert!(i.warnings.is_empty(), "{:?}", i.warnings);
    assert_eq!(value(&group(&i, "File").fields, "Pixel dimensions"), "3 x 5");
    assert_eq!(value(&group(&i, "Encoding").fields, "Compression"), "Lossless (VP8L)");
}

#[test]
fn xmp_parser_forms() {
    let doc = xmp::Doc::parse(
        r#"<x:xmpmeta><rdf:RDF><rdf:Description a:B='single &quot;q&quot;'>
        <c:D rdf:parseType="Resource"><c:E>inner</c:E></c:D>
        <c:F><rdf:Bag><rdf:li>one</rdf:li><rdf:li>two</rdf:li></rdf:Bag></c:F>
        <!-- comment <c:G>no</c:G> -->
        <c:H><![CDATA[raw <text>]]></c:H>
        <c:Broken>unclosed
        </rdf:Description></rdf:RDF></x:xmpmeta>"#,
    );
    let p = doc.properties();
    let get = |k: &str| p.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    assert_eq!(get("a:B"), Some("single \"q\""));
    assert_eq!(get("c:D / c:E"), Some("inner"));
    assert_eq!(get("c:F"), Some("one, two"));
    assert_eq!(get("c:G"), None);
    assert_eq!(get("c:H"), Some("raw <text>"));
    assert_eq!(get("c:Broken"), Some("unclosed"));
    // Garbage never panics.
    for s in ["<", "<a", "<a b='", "</x>", "<<<>>>", "&#xFFFFFFFF;", "<a>&amp", "<a\u{e9}=\"\u{e9}\">\u{e9}&\u{e9};</a>"] {
        let _ = xmp::Doc::parse(s).properties();
    }
}

#[test]
fn samsung_trailer_inconsistent_is_a_warning() {
    let mut f = vec![0xFF, 0xD8, 0xFF, 0xD9];
    let mut t = sef_trailer(&[("MCC_Data", &b"310"[..])]);
    // Corrupt the entry offset so it points before the start of the file.
    let n = t.len();
    let dir_len = u32::from_le_bytes(t[n - 8..n - 4].try_into().unwrap()) as usize;
    let dir = n - 8 - dir_len;
    t[dir + 16..dir + 20].copy_from_slice(&0x7fff_ffffu32.to_le_bytes());
    f.extend(t);
    let i = inspect_impl(&f, "jpg", f.len() as u64);
    assert!(i.warnings.iter().any(|w| w.contains("Samsung trailer")), "{:?}", i.warnings);
    // A bogus length is a warning too.
    let mut g = vec![0xFF, 0xD8, 0xFF, 0xD9];
    g.extend_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
    g.extend_from_slice(b"SEFT");
    let i = inspect_impl(&g, "jpg", g.len() as u64);
    assert!(i.warnings.iter().any(|w| w.contains("Samsung trailer")), "{:?}", i.warnings);
}

#[test]
fn truncated_and_garbage_never_panic() {
    let full = full_phone_file();
    let jpeg_end = full_jpeg(75).len();
    for cut in (0..full.len()).step_by(7) {
        let i = inspect_impl(&full[..cut], "jpg", cut as u64);
        if cut > 2 && cut < jpeg_end {
            assert!(!i.warnings.is_empty(), "cut at {cut} gave no warning");
        }
    }
    // Pseudo-random garbage, also behind valid signatures.
    let mut seed = 0x1234_5678u32;
    let mut rnd = |n: usize| -> Vec<u8> {
        (0..n)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                (seed >> 24) as u8
            })
            .collect()
    };
    let sigs: [&[u8]; 9] = [
        b"",
        &[0xFF, 0xD8, 0xFF],
        b"\x89PNG\r\n\x1a\n",
        b"GIF89a",
        b"RIFF\0\0\0\0WEBP",
        b"\0\0\0\x18ftypheic",
        b"II*\0",
        b"BM",
        &[0xFF, 0x0A],
    ];
    for round in 0..40 {
        for sig in sigs {
            let mut b = sig.to_vec();
            b.extend(rnd(16 + round * 13));
            let _ = inspect_impl(&b, "jpg", b.len() as u64);
        }
    }
    assert!(!inspect_bytes(&[], "jpg").warnings.is_empty());
    assert!(!inspect_impl(&[0xFF, 0xD8, 0xFF, 0xE1, 0xFF, 0xFF, 1, 2], "jpg", 8).warnings.is_empty());
    // A missing file is a warning, not a panic.
    let i = inspect_file(std::path::Path::new("Z:/definitely/not/here.jpg"));
    assert!(!i.warnings.is_empty());
}

#[test]
fn wrong_extension_warns() {
    let j = full_jpeg(90);
    let i = inspect_impl(&j, "png", j.len() as u64);
    assert!(i.warnings.iter().any(|w| w.contains(".png") && w.contains("JPEG")), "{:?}", i.warnings);
}

// ---- Maker notes, Pixel XMP and gain maps ----

/// A small JPEG with the given segments (each complete) before the tables and frame.
fn jpeg_with(app: &[Vec<u8>]) -> Vec<u8> {
    let mut j = vec![0xFF, 0xD8];
    for s in app {
        j.extend_from_slice(s);
    }
    let mut tables = dqt(0, &ijg_table(&STD_LUMA, 90, true));
    tables.extend(dqt(1, &ijg_table(&STD_CHROMA, 90, true)));
    j.extend(seg(0xDB, &tables));
    j.extend(sof(0xC0, 64, 48, [0x22, 0x11, 0x11]));
    j.extend(seg(0xDA, &[3, 1, 0x00, 2, 0x11, 3, 0x11, 0, 63, 0]));
    j.extend_from_slice(&[0x12, 0x34, 0xFF, 0xD9]);
    j
}

fn exif_seg(tiff: Vec<u8>) -> Vec<u8> {
    let mut d = b"Exif\0\0".to_vec();
    d.extend(tiff);
    seg(0xE1, &d)
}

fn xmp_seg(xmp: &str) -> Vec<u8> {
    let mut x = super::jpeg::XMP_PREFIX.to_vec();
    x.extend_from_slice(xmp.as_bytes());
    seg(0xE1, &x)
}

/// Samsung Type2 maker note entries as a Galaxy phone writes them (plus one undocumented tag).
fn samsung_entries() -> Vec<(u16, V)> {
    vec![
        (0x0001, V::U(b"0100".to_vec())),
        (0x0002, V::L(vec![0x12000])),
        (0x0003, V::L(vec![0])),
        (0x000C, V::L(vec![7])),
        (0x0100, V::S(vec![1])),
        (0xA001, V::A("S918BXXU1AWBD")),
        (0xA010, V::L(vec![0, 0, 4080, 3060, 8, 6, 4072, 3054])),
        (0xA011, V::S(vec![0])),
        (0xA012, V::S(vec![1])),
        (0xA013, V::Sr(vec![(-1, 3)])),
        (0xA014, V::L(vec![50])),
        (0xA018, V::R(vec![(1, 120)])),
        (0xA019, V::R(vec![(17, 10)])),
        (0xA01A, V::L(vec![230])),
    ]
}

/// The Samsung maker note, its long values at offsets counted from `base` bytes before it.
fn samsung_note(base: usize) -> Vec<u8> {
    let mut out = vec![0u8; base];
    write_ifd(&mut out, &samsung_entries());
    out.split_off(base)
}

/// EXIF of a Galaxy phone: f-number in EXIF, shutter time and ISO only in the maker note.
fn samsung_tiff(note: Vec<u8>) -> Vec<u8> {
    tiff(vec![(0x010F, V::A("samsung")), (0x0110, V::A("SM-S918B"))], vec![(0x829D, V::R(vec![(17, 10)])), (0x927C, V::U(note)), (0xA405, V::S(vec![23]))], vec![])
}

#[test]
fn samsung_maker_note() {
    for absolute in [false, true] {
        let rel = samsung_note(0);
        let mut t = samsung_tiff(rel.clone());
        if absolute {
            // The same note with its offsets counted from the TIFF header.
            let pos = find(&t, &rel).unwrap();
            let abs = samsung_note(pos);
            assert_eq!(abs.len(), rel.len());
            t[pos..pos + abs.len()].copy_from_slice(&abs);
        }
        let mut file = jpeg_with(&[exif_seg(t)]);
        file.extend(sef_trailer(&[("Photo_HDR_Info", &[1u8, 2, 3][..]), ("Camera_Capture_Mode_Info", &b"1"[..])]));
        let i = inspect_impl(&file, "jpg", file.len() as u64);
        assert!(i.warnings.is_empty(), "{:?}", i.warnings);
        let m = &group(&i, "Maker note").fields;
        assert_eq!(value(m, "Format"), "Samsung maker note (Type2)");
        let note = &m.iter().find(|f| f.name == "Format").unwrap().note;
        assert_eq!(note.contains("start of the EXIF block"), absolute, "{note}");
        assert_eq!(value(m, "Maker note version"), "0100");
        assert_eq!(value(m, "Device type"), "Phone");
        assert_eq!(value(m, "Model ID"), "0x00000000");
        assert_eq!(value(m, "Tag 0x000C"), "7");
        assert_eq!(value(m, "Face detection"), "On");
        assert_eq!(value(m, "Firmware"), "S918BXXU1AWBD");
        assert_eq!(value(m, "Sensor area"), "Full 4080 x 3060, used 4064 x 3048 (from 8, 6)");
        assert_eq!(value(m, "Color space"), "sRGB");
        assert_eq!(value(m, "Smart Range"), "On");
        assert_eq!(value(m, "Exposure compensation"), "-0.3 EV");
        assert_eq!(value(m, "ISO"), "ISO 50");
        assert_eq!(value(m, "Exposure time"), "1/120 s");
        assert_eq!(value(m, "F-number"), "f/1.7");
        assert_eq!(value(m, "Focal length (35 mm equivalent)"), "23 mm");
        assert!(m.iter().all(|f| f.name == "Maker" || !f.note.is_empty()), "every row explains itself: {m:#?}");
        // Capture: what EXIF lacks comes from the maker note; trailer and maker note are both sources.
        let c = &i.capture;
        assert_eq!(value(c, "Camera"), "samsung SM-S918B");
        assert_eq!(value(c, "Exposure"), "1/120 s \u{00B7} f/1.7 \u{00B7} ISO 50");
        assert!(c.iter().find(|f| f.name == "Exposure").unwrap().note.contains("Samsung maker note"));
        assert_eq!(value(c, "Exposure bias"), "-0.3 EV");
        assert_eq!(value(c, "Face detection"), "On");
        let mf = c.iter().find(|f| f.name == "Multi-frame / HDR").unwrap();
        assert_eq!(mf.value, "Samsung HDR processing; Samsung Smart Range (wider dynamic range)");
        assert!(mf.note.contains("Samsung trailer entry Photo_HDR_Info.") && mf.note.contains("Samsung maker note SmartRange = On."), "{}", mf.note);
        assert_eq!(value(&group(&i, "Samsung trailer").fields, "Camera_Capture_Mode_Info"), "1");
    }
}

#[test]
fn damaged_samsung_note_is_a_warning() {
    let full = samsung_note(0);
    // The directory is complete, the long values are cut off.
    let cut = full[..2 + 12 * 14 + 10].to_vec();
    let file = jpeg_with(&[exif_seg(samsung_tiff(cut))]);
    let i = inspect_impl(&file, "jpg", file.len() as u64);
    assert!(i.warnings.iter().any(|w| w.contains("Samsung maker note")), "{:?}", i.warnings);
    let m = &group(&i, "Maker note").fields;
    assert_eq!(value(m, "Device type"), "Phone");
    assert!(m.iter().all(|f| f.name != "Firmware"));
    // Cut inside the directory, and garbage after the signature: no panic.
    for n in [14, 16, 40, 100] {
        let file = jpeg_with(&[exif_seg(samsung_tiff(full[..n].to_vec()))]);
        let i = inspect_impl(&file, "jpg", file.len() as u64);
        assert!(n == 14 || i.warnings.iter().any(|w| w.contains("cut short")), "{n}: {:?}", i.warnings);
    }
    let mut junk = full.clone();
    for (k, b) in junk.iter_mut().enumerate().skip(14) {
        *b = (k * 97 % 251) as u8;
    }
    let file = jpeg_with(&[exif_seg(samsung_tiff(junk))]);
    let _ = inspect_impl(&file, "jpg", file.len() as u64);
    // Another maker's note that starts the same way is not read as Samsung's.
    let t = tiff(vec![(0x010F, V::A("Canon"))], vec![(0x927C, V::U(full))], vec![]);
    let file = jpeg_with(&[exif_seg(t)]);
    let i = inspect_impl(&file, "jpg", file.len() as u64);
    assert!(group(&i, "Maker note").fields.iter().all(|f| f.name != "Device type"));
}

const PIXEL_XMP: &str = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
 <rdf:Description rdf:about="" xmlns:GCamera="http://ns.google.com/photos/1.0/camera/"
  GCamera:HdrPlusMakernote="SERSUAMxMjM0NTY3ODk=" GCamera:shot_log_data="c2hvdGxvZw==" GCamera:SpecialTypeID="SPECIAL"
  GCamera:BurstID="7c2a1e1b-0d6f-4f3c-9e0a-1f2b3c4d5e6f" GCamera:BurstPrimary="1"/></rdf:RDF></x:xmpmeta>"#;

fn pixel_file(special: &str) -> Vec<u8> {
    let t = tiff(
        vec![(0x010F, V::A("Google")), (0x0110, V::A("Pixel 8 Pro")), (0x0131, V::A("HDR+ 1.0.612345678zd"))],
        vec![
            (0x829A, V::R(vec![(1, 4)])),
            (0x927C, V::U(b"HDRP\x03\x9a\x17\xc4\x88\x01\x02\x03\x04\x05\x06".to_vec())),
            (0xA434, V::A("Pixel 8 Pro back camera 6.9mm f/1.68")),
        ],
        vec![],
    );
    jpeg_with(&[exif_seg(t), xmp_seg(&PIXEL_XMP.replace("SPECIAL", special))])
}

#[test]
fn pixel_hdr_plus_and_camera_modes() {
    let f = pixel_file("com.google.android.apps.camera.gallery.specialtype.SpecialType-NIGHT");
    let i = inspect_impl(&f, "jpg", f.len() as u64);
    assert!(i.warnings.is_empty(), "{:?}", i.warnings);
    let m = &group(&i, "Maker note").fields;
    assert_eq!(value(m, "Format"), "Google HDR+ maker note");
    assert_eq!(value(m, "Contents"), "Encrypted (not readable)");
    assert_eq!(value(m, "HDR+ data version"), "3");
    assert_eq!(value(m, "Also stored (XMP)"), "HDR+ notes, shot log (encrypted)");
    let x = &group(&i, "XMP").fields;
    assert_eq!(value(x, "GCamera:HdrPlusMakernote"), "(encrypted Google data, 15 bytes)");
    assert!(value(x, "GCamera:shot_log_data").starts_with("(encrypted Google data"));
    assert!(!x.iter().find(|f| f.name == "GCamera:BurstPrimary").unwrap().note.is_empty());
    let c = &i.capture;
    assert_eq!(value(c, "Camera"), "Google Pixel 8 Pro");
    let mf = c.iter().find(|f| f.name == "Multi-frame / HDR").unwrap();
    assert_eq!(mf.value, "HDR+ multi-frame processing (Google)");
    for clue in ["XMP GCamera:HdrPlusMakernote", "HDR+ note (HDRP)", "EXIF Software is HDR+ 1.0.612345678zd"] {
        assert!(mf.note.contains(clue), "{clue}: {}", mf.note);
    }
    assert_eq!(value(c, "Night mode / long exposure"), "Night Sight (Google)");
    assert_eq!(value(c, "Burst"), "Part of a burst sequence (the best shot)");
    assert_eq!(value(c, "Google Camera data"), "HDR+ notes, shot log (encrypted)");
    assert!(c.iter().all(|f| f.name != "Portrait / depth"));

    let f = pixel_file("com.google.android.apps.camera.gallery.specialtype.SpecialType-ASTRO");
    let c = inspect_impl(&f, "jpg", f.len() as u64).capture;
    assert_eq!(value(&c, "Night mode / long exposure"), "Astrophotography (Google Night Sight)");
    let f = pixel_file("com.google.android.apps.camera.gallery.specialtype.SpecialType-PORTRAIT");
    let c = inspect_impl(&f, "jpg", f.len() as u64).capture;
    assert_eq!(value(&c, "Portrait / depth"), "Portrait (background blur)");
    // A quarter second without a night clue stays a guess.
    assert!(value(&c, "Night mode / long exposure").starts_with("Probably"));
}

/// MPF payload ("MPF\0" and the index): the main image and one gain map `offset` bytes after the MPF header.
fn mpf_for(offset: u32, size: u32) -> Vec<u8> {
    let mut entries = Vec::new();
    for (attr, sz, off) in [(0x2003_0000u32, 1000u32, 0u32), (0, size, offset)] {
        entries.extend_from_slice(&attr.to_le_bytes());
        entries.extend_from_slice(&sz.to_le_bytes());
        entries.extend_from_slice(&off.to_le_bytes());
        entries.extend_from_slice(&[0; 4]);
    }
    let t = tiff(vec![(0xB000, V::U(b"0100".to_vec())), (0xB001, V::L(vec![2])), (0xB002, V::U(entries))], vec![], vec![]);
    let mut d = b"MPF\0".to_vec();
    d.extend_from_slice(&t);
    d
}

/// An Ultra HDR style file: a 32 x 16 main JPEG (EXIF orientation 6; with
/// `with_index`, hdrgm XMP with a GContainer directory and an MPF index)
/// followed by an 8 x 4 gray gain map (left half black, right half white)
/// carrying `gain_segments` (APP number, payload).
fn ultra_hdr_file(gain_segments: &[(u8, Vec<u8>)], with_index: bool) -> Vec<u8> {
    use jpeg_encoder::{ColorType, Encoder};
    let gray: Vec<u8> = (0..32).map(|i| if i % 8 < 4 { 0 } else { 255 }).collect();
    let mut gm = Vec::new();
    let mut enc = Encoder::new(&mut gm, 95);
    for (nr, d) in gain_segments {
        enc.add_app_segment(*nr, d.clone()).unwrap();
    }
    enc.encode(&gray, 8, 4, ColorType::Luma).unwrap();
    let xmp = format!(
        r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:hdrgm="http://ns.adobe.com/hdr-gain-map/1.0/" xmlns:Container="http://ns.google.com/photos/1.0/container/" xmlns:Item="http://ns.google.com/photos/1.0/container/item/" hdrgm:Version="1.0"><Container:Directory><rdf:Seq><rdf:li rdf:parseType="Resource"><Container:Item Item:Semantic="Primary" Item:Mime="image/jpeg"/></rdf:li><rdf:li rdf:parseType="Resource"><Container:Item Item:Semantic="GainMap" Item:Mime="image/jpeg" Item:Length="{}"/></rdf:li></rdf:Seq></Container:Directory></rdf:Description></rdf:RDF></x:xmpmeta>"#,
        gm.len()
    );
    let mut main = Vec::new();
    let mut enc = Encoder::new(&mut main, 90);
    enc.add_exif_metadata(&tiff(vec![(0x0112, V::S(vec![6]))], vec![], vec![])).unwrap();
    if with_index {
        let mut x = super::jpeg::XMP_PREFIX.to_vec();
        x.extend_from_slice(xmp.as_bytes());
        enc.add_app_segment(1, x).unwrap();
        enc.add_app_segment(2, mpf_for(0, 0)).unwrap();
    }
    enc.encode(&vec![128u8; 32 * 16 * 3], 32, 16, ColorType::Rgb).unwrap();
    if with_index {
        // Now that the main image's size is known, point the MPF entry at the gain map.
        let at = find(&main, b"MPF\0").unwrap();
        let real = mpf_for((main.len() - (at + 4)) as u32, gm.len() as u32);
        main[at..at + real.len()].copy_from_slice(&real);
    }
    main.extend(gm);
    main
}

#[test]
fn ultra_hdr_gain_map_file() {
    let file = ultra_hdr_file(&[(1, gain_xmp_segment("2"))], true);
    let i = inspect_impl(&file, "jpg", file.len() as u64);
    assert!(i.warnings.is_empty(), "{:?}", i.warnings);
    let g = &group(&i, "HDR gain map").fields;
    assert_eq!(value(g, "Format"), "Ultra HDR (Adobe gain map XMP)");
    assert!(value(g, "Gain map image").starts_with("Multi-picture image 2, 8 x 4, one channel (gray)"), "{}", value(g, "Gain map image"));
    assert_eq!(value(g, "Boost"), "1x to 4x");
    assert_eq!(value(g, "Screen headroom"), "1x to 4x");
    assert!(value(&group(&i, "Multi-picture (MPF)").fields, "Image 2").starts_with("HDR gain map"));
    assert_eq!(value(&i.capture, "HDR gain map"), "Yes, highlights up to 4x brighter");

    let map = gainmap::load_bytes(&file).ok().expect("a gain map");
    assert_eq!((map.channels, map.source.as_str()), (1, "multi-picture image 2"));
    assert_eq!(map.image.dimensions(), (4, 8), "turned like the photo (EXIF orientation 6)");
    // Turned a quarter clockwise: the white right half is now the bottom half.
    assert!(map.image.get_pixel(1, 1).0[0] < 60 && map.image.get_pixel(1, 6).0[0] > 200, "{:?}", map.image);
    assert_eq!(map.legend()[1], "Black: 1x, white: 4x brighter");
    // The whole chain: white stays SDR white (a quarter of the x4 peak) where the map is black and reaches the peak where it is white.
    let base = image::RgbaImage::from_pixel(4, 8, image::Rgba([255, 255, 255, 255]));
    let out = gainmap::render_hdr(&base, &gainmap::prepare(&map, 4, 8), &map.meta, 4.0);
    assert!(out.get_pixel(1, 0).0[0] < 170 && out.get_pixel(1, 7).0[0] > 230, "{:?}", out);

    // ISO 21496-1 next to the XMP: the ISO values win.
    let mut iso = super::jpeg::ISO_GAINMAP_PREFIX.to_vec();
    iso.push(0);
    iso.extend(gainmap::tests::iso_payload(true, false));
    let file = ultra_hdr_file(&[(1, gain_xmp_segment("2")), (2, iso)], true);
    let i = inspect_impl(&file, "jpg", file.len() as u64);
    assert!(i.warnings.is_empty(), "{:?}", i.warnings);
    let g = &group(&i, "HDR gain map").fields;
    assert_eq!(value(g, "Format"), "ISO 21496-1 gain map");
    assert!(g[0].note.contains("also has Ultra HDR metadata"), "{}", g[0].note);
    assert_eq!(value(g, "Boost per channel"), "red 1-4x, green 1-2.8x, blue 1-2x");
    assert_eq!(gainmap::load_bytes(&file).ok().expect("a gain map").meta.kind, gainmap::GainMapKind::Iso);

    // Without MPF or GContainer the gain map is still found after the main image.
    let file = ultra_hdr_file(&[(1, gain_xmp_segment("2"))], false);
    assert_eq!(gainmap::load_bytes(&file).ok().expect("a gain map").source, "the image after the main picture");
    assert!(inspect_impl(&file, "jpg", file.len() as u64).other.iter().all(|g| g.name != "HDR gain map"), "nothing announced it");

    // Damaged metadata: a warning, and the views say why.
    let file = ultra_hdr_file(&[(1, gain_xmp_segment("lots"))], true);
    let e = gainmap::load_bytes(&file).err().expect("bad metadata");
    assert!(e.announced && e.reason.contains("hdrgm:GainMapMax"), "{e:?}");
    let i = inspect_impl(&file, "jpg", file.len() as u64);
    assert!(i.warnings.iter().any(|w| w.contains("gain map")), "{:?}", i.warnings);

    // A plain JPEG has none, and cut files never panic.
    let e = gainmap::load_bytes(&jpeg_with(&[])).err().expect("no gain map");
    assert!(!e.announced, "{e:?}");
    let good = ultra_hdr_file(&[(1, gain_xmp_segment("2"))], true);
    for cut in (0..good.len()).step_by(29) {
        let _ = gainmap::load_bytes(&good[..cut]);
        let _ = inspect_impl(&good[..cut], "jpg", cut as u64);
    }
}

#[test]
fn gain_map_files_on_disk() {
    let d = std::env::temp_dir().join(format!("lumen-gainmap-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    let jpg = d.join("a.jpg");
    std::fs::write(&jpg, ultra_hdr_file(&[(1, gain_xmp_segment("2"))], true)).unwrap();
    assert!(gainmap::load(&jpg).is_ok());
    // HEIC gain maps are recognized but not read.
    let heic = d.join("b.heic");
    let mut h = b"\0\0\0\x18ftypheic\0\0\0\0mif1heic".to_vec();
    h.extend_from_slice(b"\0\0\0\x30infe urn:com:apple:photo:2020:aux:hdrgainmap\0");
    std::fs::write(&heic, &h).unwrap();
    let e = gainmap::load(&heic).err().expect("not read");
    assert!(e.announced && e.reason.contains("JPEG"), "{e:?}");
    let png = d.join("c.png");
    std::fs::write(&png, b"\x89PNG\r\n\x1a\n").unwrap();
    assert!(!gainmap::load(&png).err().expect("none").announced);
    assert!(gainmap::load(&d.join("missing.jpg")).is_err());
    let _ = std::fs::remove_dir_all(&d);
}
