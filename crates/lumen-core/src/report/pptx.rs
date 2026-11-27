//! PowerPoint writer: a minimal, valid Office Open XML package (one master,
//! one blank layout, a theme) with one slide per laid-out page. Text boxes
//! stay editable; pictures are embedded JPEGs.

use super::{layout, xml_escape, Align, Box2, Brand, El, Report, Slide, Table, INK, MUTED, PAGE_H, PAGE_W, RULE};
use anyhow::Result;
use std::io::Write;

/// Points to EMU.
fn emu(pt: f32) -> i64 {
    (pt as f64 * 12700.0).round() as i64
}

fn hex(c: [u8; 3]) -> String {
    format!("{:02X}{:02X}{:02X}", c[0], c[1], c[2])
}

const NS: &str = r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main""#;

fn xfrm(b: Box2) -> String {
    format!(r#"<a:xfrm><a:off x="{}" y="{}"/><a:ext cx="{}" cy="{}"/></a:xfrm>"#, emu(b.x), emu(b.y), emu(b.w).max(1), emu(b.h).max(1))
}

fn run(text: &str, size: f32, bold: bool, color: [u8; 3]) -> String {
    format!(
        r#"<a:r><a:rPr lang="en-US" sz="{}" b="{}" dirty="0"><a:solidFill><a:srgbClr val="{}"/></a:solidFill><a:latin typeface="Segoe UI"/><a:cs typeface="Segoe UI"/></a:rPr><a:t>{}</a:t></a:r>"#,
        (size * 100.0).round() as i32,
        u8::from(bold),
        hex(color),
        xml_escape(text)
    )
}

fn paragraphs(text: &str, size: f32, bold: bool, color: [u8; 3], align: Align) -> String {
    let algn = match align {
        Align::Left => "l",
        Align::Center => "ctr",
        Align::Right => "r",
    };
    let mut o = String::new();
    for line in text.split('\n') {
        o.push_str(&format!(r#"<a:p><a:pPr algn="{algn}"><a:lnSpc><a:spcPct val="100000"/></a:lnSpc></a:pPr>"#));
        if !line.is_empty() {
            o.push_str(&run(line, size, bold, color));
        }
        o.push_str(&format!(r#"<a:endParaRPr lang="en-US" sz="{}"/></a:p>"#, (size * 100.0).round() as i32));
    }
    o
}

struct Ctx {
    next_id: u32,
    /// (rId, media file name) for this slide.
    rels: Vec<(String, String)>,
}

impl Ctx {
    fn id(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }
}

fn shape_rect(cx: &mut Ctx, b: Box2, fill: Option<[u8; 3]>, line: Option<([u8; 3], f32)>) -> String {
    let id = cx.id();
    let fill = match fill {
        Some(c) => format!(r#"<a:solidFill><a:srgbClr val="{}"/></a:solidFill>"#, hex(c)),
        None => "<a:noFill/>".into(),
    };
    let ln = match line {
        Some((c, w)) => format!(r#"<a:ln w="{}"><a:solidFill><a:srgbClr val="{}"/></a:solidFill></a:ln>"#, emu(w), hex(c)),
        None => "<a:ln><a:noFill/></a:ln>".into(),
    };
    format!(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="{id}" name="Shape {id}"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr>{}<a:prstGeom prst="rect"><a:avLst/></a:prstGeom>{fill}{ln}</p:spPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:endParaRPr lang="en-US"/></a:p></p:txBody></p:sp>"#,
        xfrm(b)
    )
}

fn text_box(cx: &mut Ctx, b: Box2, text: &str, size: f32, bold: bool, color: [u8; 3], align: Align) -> String {
    let id = cx.id();
    // Text is pre-wrapped by the layout; PowerPoint may wrap a little
    // differently, so allow wrapping and keep zero insets to match.
    format!(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="{id}" name="Text {id}"/><p:cNvSpPr txBox="1"/><p:nvPr/></p:nvSpPr><p:spPr>{}<a:prstGeom prst="rect"><a:avLst/></a:prstGeom><a:noFill/></p:spPr><p:txBody><a:bodyPr wrap="square" lIns="0" tIns="0" rIns="0" bIns="0" anchor="t"><a:noAutofit/></a:bodyPr><a:lstStyle/>{}</p:txBody></p:sp>"#,
        xfrm(b),
        paragraphs(text, size, bold, color, align)
    )
}

fn picture(cx: &mut Ctx, b: Box2, media: &str) -> String {
    let id = cx.id();
    let rid = format!("rId{}", cx.rels.len() + 2);
    cx.rels.push((rid.clone(), media.to_string()));
    format!(
        r#"<p:pic><p:nvPicPr><p:cNvPr id="{id}" name="Picture {id}"/><p:cNvPicPr><a:picLocks noChangeAspect="1"/></p:cNvPicPr><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="{rid}"/><a:stretch><a:fillRect/></a:stretch></p:blipFill><p:spPr>{}<a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr></p:pic>"#,
        xfrm(b)
    )
}

fn table(cx: &mut Ctx, b: Box2, t: &Table, head: [u8; 3]) -> String {
    let id = cx.id();
    let cols = t.header.len().max(1);
    let first_w = (b.w * 0.34).min(260.0);
    let other_w = if cols > 1 { (b.w - first_w) / (cols - 1) as f32 } else { b.w };
    let row_h = b.h / (t.rows.len() + 1) as f32;
    let mut grid = String::new();
    for c in 0..cols {
        let w = if c == 0 && cols > 1 { first_w } else { other_w };
        grid.push_str(&format!(r#"<a:gridCol w="{}"/>"#, emu(w)));
    }
    let cell = |s: &str, bold: bool, color: [u8; 3], fill: Option<[u8; 3]>, center: bool| -> String {
        let f = fill.map(|c| format!(r#"<a:solidFill><a:srgbClr val="{}"/></a:solidFill>"#, hex(c))).unwrap_or_default();
        let border = format!(r#"<a:lnB w="6350"><a:solidFill><a:srgbClr val="{}"/></a:solidFill></a:lnB>"#, hex(RULE));
        format!(
            r#"<a:tc><a:txBody><a:bodyPr/><a:lstStyle/>{}</a:txBody><a:tcPr marL="68580" marR="68580" marT="34290" marB="34290" anchor="ctr"><a:lnL w="0"><a:noFill/></a:lnL><a:lnR w="0"><a:noFill/></a:lnR><a:lnT w="0"><a:noFill/></a:lnT>{border}{f}</a:tcPr></a:tc>"#,
            paragraphs(s, 11.0, bold, color, if center { Align::Center } else { Align::Left })
        )
    };
    let mut rows = format!(r#"<a:tr h="{}">"#, emu(row_h));
    for (c, h) in t.header.iter().enumerate() {
        rows.push_str(&cell(h, true, [255, 255, 255], Some(head), c > 0));
    }
    rows.push_str("</a:tr>");
    for (k, r) in t.rows.iter().enumerate() {
        rows.push_str(&format!(r#"<a:tr h="{}">"#, emu(row_h)));
        for c in 0..cols {
            let s = r.get(c).map(String::as_str).unwrap_or("");
            let fill = if k % 2 == 1 { Some([245, 247, 250]) } else { Some([255, 255, 255]) };
            rows.push_str(&cell(s, c == 0, if c == 0 { INK } else { MUTED }, fill, c > 0));
        }
        rows.push_str("</a:tr>");
    }
    format!(
        r#"<p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="{id}" name="Table {id}"/><p:cNvGraphicFramePr><a:graphicFrameLocks noGrp="1"/></p:cNvGraphicFramePr><p:nvPr/></p:nvGraphicFramePr><p:xfrm><a:off x="{}" y="{}"/><a:ext cx="{}" cy="{}"/></p:xfrm><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/table"><a:tbl><a:tblPr firstRow="1" bandRow="1"/><a:tblGrid>{grid}</a:tblGrid>{rows}</a:tbl></a:graphicData></a:graphic></p:graphicFrame>"#,
        emu(b.x),
        emu(b.y),
        emu(b.w),
        emu(b.h)
    )
}

/// One slide's XML and its picture relationships.
fn slide_xml(s: &Slide, media_name: &mut dyn FnMut(&std::sync::Arc<Vec<u8>>) -> String) -> (String, Vec<(String, String)>) {
    let mut cx = Ctx { next_id: 1, rels: Vec::new() };
    let mut body = String::new();
    for e in &s.els {
        match e {
            El::Fill { bx, color } => body.push_str(&shape_rect(&mut cx, *bx, Some(*color), None)),
            El::Frame { bx, color, width } => body.push_str(&shape_rect(&mut cx, *bx, None, Some((*color, *width)))),
            El::Image { bx, pic } => {
                let name = media_name(&pic.jpeg);
                body.push_str(&picture(&mut cx, *bx, &name));
            }
            El::Text(t) => body.push_str(&text_box(&mut cx, t.bx, &t.text, t.size, t.bold, t.color, t.align)),
            El::Table { bx, table: t, head } => body.push_str(&table(&mut cx, *bx, t, *head)),
        }
    }
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><p:sld {NS}><p:cSld><p:bg><p:bgPr><a:solidFill><a:srgbClr val="FFFFFF"/></a:solidFill><a:effectLst/></p:bgPr></p:bg><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr>{body}</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>"#
    );
    (xml, cx.rels)
}

fn notes_xml(text: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><p:notes {NS}><p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/><p:sp><p:nvSpPr><p:cNvPr id="2" name="Slide Image"/><p:cNvSpPr><a:spLocks noGrp="1" noRot="1" noChangeAspect="1"/></p:cNvSpPr><p:nvPr><p:ph type="sldImg"/></p:nvPr></p:nvSpPr><p:spPr/></p:sp><p:sp><p:nvSpPr><p:cNvPr id="3" name="Notes"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/><a:lstStyle/>{}</p:txBody></p:sp></p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:notes>"#,
        paragraphs(text, 12.0, false, INK, Align::Left)
    )
}

const THEME: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="Lumen"><a:themeElements><a:clrScheme name="Lumen"><a:dk1><a:srgbClr val="20242C"/></a:dk1><a:lt1><a:srgbClr val="FFFFFF"/></a:lt1><a:dk2><a:srgbClr val="181E2A"/></a:dk2><a:lt2><a:srgbClr val="F5F7FA"/></a:lt2><a:accent1><a:srgbClr val="2F6FEB"/></a:accent1><a:accent2><a:srgbClr val="EA7820"/></a:accent2><a:accent3><a:srgbClr val="22A05C"/></a:accent3><a:accent4><a:srgbClr val="8E54D6"/></a:accent4><a:accent5><a:srgbClr val="D64545"/></a:accent5><a:accent6><a:srgbClr val="0F9BB5"/></a:accent6><a:hlink><a:srgbClr val="2F6FEB"/></a:hlink><a:folHlink><a:srgbClr val="8E54D6"/></a:folHlink></a:clrScheme><a:fontScheme name="Lumen"><a:majorFont><a:latin typeface="Segoe UI Semibold"/><a:ea typeface=""/><a:cs typeface=""/></a:majorFont><a:minorFont><a:latin typeface="Segoe UI"/><a:ea typeface=""/><a:cs typeface=""/></a:minorFont></a:fontScheme><a:fmtScheme name="Lumen"><a:fillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:fillStyleLst><a:lnStyleLst><a:ln w="6350"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln w="12700"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln w="19050"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln></a:lnStyleLst><a:effectStyleLst><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle></a:effectStyleLst><a:bgFillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:bgFillStyleLst></a:fmtScheme></a:themeElements><a:objectDefaults/><a:extraClrSchemeLst/></a:theme>"#;

fn master_xml() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><p:sldMaster {NS}><p:cSld><p:bg><p:bgRef idx="1001"><a:schemeClr val="bg1"/></p:bgRef></p:bg><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr></p:spTree></p:cSld><p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/><p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst><p:txStyles><p:titleStyle><a:lvl1pPr><a:defRPr sz="2800"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mj-lt"/></a:defRPr></a:lvl1pPr></p:titleStyle><p:bodyStyle><a:lvl1pPr><a:defRPr sz="1400"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/></a:defRPr></a:lvl1pPr></p:bodyStyle><p:otherStyle><a:lvl1pPr><a:defRPr sz="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/></a:defRPr></a:lvl1pPr></p:otherStyle></p:txStyles></p:sldMaster>"#
    )
}

fn layout_xml() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><p:sldLayout {NS} type="blank" preserve="1"><p:cSld name="Blank"><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr></p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sldLayout>"#
    )
}

fn notes_master_xml() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><p:notesMaster {NS}><p:cSld><p:bg><p:bgRef idx="1001"><a:schemeClr val="bg1"/></p:bgRef></p:bg><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/><p:sp><p:nvSpPr><p:cNvPr id="2" name="Slide Image"/><p:cNvSpPr><a:spLocks noGrp="1" noRot="1" noChangeAspect="1"/></p:cNvSpPr><p:nvPr><p:ph type="sldImg" idx="2"/></p:nvPr></p:nvSpPr><p:spPr><a:xfrm><a:off x="381000" y="685800"/><a:ext cx="6096000" cy="3429000"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr></p:sp><p:sp><p:nvSpPr><p:cNvPr id="3" name="Notes"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="body" idx="3"/></p:nvPr></p:nvSpPr><p:spPr><a:xfrm><a:off x="685800" y="4343400"/><a:ext cx="5486400" cy="4114800"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:endParaRPr lang="en-US"/></a:p></p:txBody></p:sp></p:spTree></p:cSld><p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/><p:notesStyle><a:lvl1pPr><a:defRPr sz="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/></a:defRPr></a:lvl1pPr></p:notesStyle></p:notesMaster>"#
    )
}

/// Render the report as a `.pptx` file's bytes.
pub fn render(r: &Report) -> Result<Vec<u8>> {
    render_slides(&layout(r), &r.title, &r.author, &r.brand)
}

/// Render laid-out slides as a `.pptx` file's bytes (`title` and `author`
/// go into the document properties; the brand's accent becomes the theme's
/// first accent color, so new shapes match, and its company the document's).
pub fn render_slides(slides: &[Slide], title: &str, author: &str, brand: &Brand) -> Result<Vec<u8>> {
    let mut media: Vec<std::sync::Arc<Vec<u8>>> = Vec::new();
    let mut slide_parts = Vec::new();
    for s in slides {
        let mut name_of = |j: &std::sync::Arc<Vec<u8>>| {
            let k = match media.iter().position(|m| std::sync::Arc::ptr_eq(m, j)) {
                Some(k) => k,
                None => {
                    media.push(j.clone());
                    media.len() - 1
                }
            };
            format!("image{}.jpeg", k + 1)
        };
        slide_parts.push(slide_xml(s, &mut name_of));
    }

    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let deflate = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let stored = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let put = |zip: &mut zip::ZipWriter<_>, name: &str, data: &[u8], opts| -> Result<()> {
        zip.start_file(name, opts)?;
        zip.write_all(data)?;
        Ok(())
    };
    let n = slides.len();

    let mut ct = String::from(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="jpeg" ContentType="image/jpeg"/><Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/><Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/><Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/><Override PartName="/ppt/notesMasters/notesMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.notesMaster+xml"/><Override PartName="/ppt/theme/theme1.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/><Override PartName="/ppt/theme/theme2.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/><Override PartName="/ppt/presProps.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presProps+xml"/><Override PartName="/ppt/viewProps.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.viewProps+xml"/><Override PartName="/ppt/tableStyles.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.tableStyles+xml"/><Override PartName="/docProps/core.xml" ContentType="application/vnd.openxmlformats-package.core-properties+xml"/><Override PartName="/docProps/app.xml" ContentType="application/vnd.openxmlformats-officedocument.extended-properties+xml"/>"#);
    for i in 1..=n {
        ct.push_str(&format!(r#"<Override PartName="/ppt/slides/slide{i}.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>"#));
        if !slides[i - 1].notes.trim().is_empty() {
            ct.push_str(&format!(r#"<Override PartName="/ppt/notesSlides/notesSlide{i}.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.notesSlide+xml"/>"#));
        }
    }
    ct.push_str("</Types>");
    put(&mut zip, "[Content_Types].xml", ct.as_bytes(), deflate)?;

    put(&mut zip, "_rels/.rels", br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="docProps/core.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties" Target="docProps/app.xml"/></Relationships>"#, deflate)?;

    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    let core = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"><dc:title>{}</dc:title><dc:creator>{}</dc:creator><dcterms:created xsi:type="dcterms:W3CDTF">{now}</dcterms:created><dcterms:modified xsi:type="dcterms:W3CDTF">{now}</dcterms:modified></cp:coreProperties>"#,
        xml_escape(title),
        xml_escape(author)
    );
    put(&mut zip, "docProps/core.xml", core.as_bytes(), deflate)?;
    let company = if brand.company.is_empty() { String::new() } else { format!("<Company>{}</Company>", xml_escape(&brand.company)) };
    let app = format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties"><Application>Lumen</Application>{company}<Slides>{n}</Slides></Properties>"#);
    put(&mut zip, "docProps/app.xml", app.as_bytes(), deflate)?;

    let mut ids = String::new();
    let mut prels = String::from(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="slideMasters/slideMaster1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="theme/theme1.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/presProps" Target="presProps.xml"/><Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/viewProps" Target="viewProps.xml"/><Relationship Id="rId5" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/tableStyles" Target="tableStyles.xml"/><Relationship Id="rId6" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesMaster" Target="notesMasters/notesMaster1.xml"/>"#);
    for i in 1..=n {
        ids.push_str(&format!(r#"<p:sldId id="{}" r:id="rId{}"/>"#, 255 + i, 100 + i));
        prels.push_str(&format!(r#"<Relationship Id="rId{}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide{i}.xml"/>"#, 100 + i));
    }
    prels.push_str("</Relationships>");
    let pres = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><p:presentation {NS} saveSubsetFonts="1"><p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst><p:notesMasterIdLst><p:notesMasterId r:id="rId6"/></p:notesMasterIdLst><p:sldIdLst>{ids}</p:sldIdLst><p:sldSz cx="{}" cy="{}"/><p:notesSz cx="6858000" cy="9144000"/><p:defaultTextStyle><a:lvl1pPr><a:defRPr sz="1400"/></a:lvl1pPr></p:defaultTextStyle></p:presentation>"#,
        emu(PAGE_W),
        emu(PAGE_H)
    );
    put(&mut zip, "ppt/presentation.xml", pres.as_bytes(), deflate)?;
    put(&mut zip, "ppt/_rels/presentation.xml.rels", prels.as_bytes(), deflate)?;
    put(&mut zip, "ppt/presProps.xml", format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><p:presentationPr {NS}/>"#).as_bytes(), deflate)?;
    put(&mut zip, "ppt/viewProps.xml", format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><p:viewPr {NS}><p:normalViewPr><p:restoredLeft sz="15620"/><p:restoredTop sz="94660"/></p:normalViewPr><p:gridSpacing cx="76200" cy="76200"/></p:viewPr>"#).as_bytes(), deflate)?;
    put(&mut zip, "ppt/tableStyles.xml", br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><a:tblStyleLst xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" def="{5C22544A-7EE6-4342-B048-85BDC9FD1C3A}"/>"#, deflate)?;
    let theme = THEME.replace("2F6FEB", &hex(brand.accent));
    put(&mut zip, "ppt/theme/theme1.xml", theme.as_bytes(), deflate)?;
    put(&mut zip, "ppt/theme/theme2.xml", theme.as_bytes(), deflate)?;
    put(&mut zip, "ppt/slideMasters/slideMaster1.xml", master_xml().as_bytes(), deflate)?;
    put(&mut zip, "ppt/slideMasters/_rels/slideMaster1.xml.rels", br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="../theme/theme1.xml"/></Relationships>"#, deflate)?;
    put(&mut zip, "ppt/slideLayouts/slideLayout1.xml", layout_xml().as_bytes(), deflate)?;
    put(&mut zip, "ppt/slideLayouts/_rels/slideLayout1.xml.rels", br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="../slideMasters/slideMaster1.xml"/></Relationships>"#, deflate)?;
    put(&mut zip, "ppt/notesMasters/notesMaster1.xml", notes_master_xml().as_bytes(), deflate)?;
    put(&mut zip, "ppt/notesMasters/_rels/notesMaster1.xml.rels", br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="../theme/theme2.xml"/></Relationships>"#, deflate)?;

    for (i, (xml, rels)) in slide_parts.iter().enumerate() {
        let k = i + 1;
        put(&mut zip, &format!("ppt/slides/slide{k}.xml"), xml.as_bytes(), deflate)?;
        let mut rx = String::from(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>"#);
        for (rid, m) in rels {
            rx.push_str(&format!(r#"<Relationship Id="{rid}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/{m}"/>"#));
        }
        let has_notes = !slides[i].notes.trim().is_empty();
        if has_notes {
            rx.push_str(&format!(r#"<Relationship Id="rIdN" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide" Target="../notesSlides/notesSlide{k}.xml"/>"#));
        }
        rx.push_str("</Relationships>");
        put(&mut zip, &format!("ppt/slides/_rels/slide{k}.xml.rels"), rx.as_bytes(), deflate)?;
        if has_notes {
            put(&mut zip, &format!("ppt/notesSlides/notesSlide{k}.xml"), notes_xml(&slides[i].notes).as_bytes(), deflate)?;
            let nr = format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesMaster" Target="../notesMasters/notesMaster1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="../slides/slide{k}.xml"/></Relationships>"#);
            put(&mut zip, &format!("ppt/notesSlides/_rels/notesSlide{k}.xml.rels"), nr.as_bytes(), deflate)?;
        }
    }
    for (k, m) in media.iter().enumerate() {
        put(&mut zip, &format!("ppt/media/image{}.jpeg", k + 1), m, stored)?;
    }
    Ok(zip.finish()?.into_inner())
}

#[cfg(test)]
mod tests {
    #[test]
    fn package_has_core_parts_and_dedups_media() {
        let r = super::super::tests::sample(2);
        let bytes = super::render(&r).unwrap();
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let names: Vec<String> = (0..z.len()).map(|i| z.by_index(i).unwrap().name().unwrap().to_string()).collect();
        for want in ["[Content_Types].xml", "ppt/presentation.xml", "ppt/slides/slide1.xml", "ppt/theme/theme1.xml"] {
            assert!(names.iter().any(|n| n == want), "{want} missing");
        }
        // Two images are shared by overview, per-image and detail pages.
        assert_eq!(names.iter().filter(|n| n.starts_with("ppt/media/")).count(), 2);
        let mut s = String::new();
        std::io::Read::read_to_string(&mut z.by_name("ppt/slides/slide3.xml").unwrap(), &mut s).unwrap();
        assert!(s.contains("&amp;") && s.contains("&lt;visible&gt;"));
    }

    #[test]
    fn branding_reaches_the_package() {
        let read = |z: &mut zip::ZipArchive<std::io::Cursor<Vec<u8>>>, name: &str| {
            let mut s = String::new();
            std::io::Read::read_to_string(&mut z.by_name(name).unwrap(), &mut s).unwrap();
            s
        };
        let mut r = super::super::tests::sample(2);
        r.brand = super::super::tests::acme(Some((600, 150)));
        r.brand.company = "Acme & Sons".into();
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(super::render(&r).unwrap())).unwrap();
        let names: Vec<String> = (0..z.len()).map(|i| z.by_index(i).unwrap().name().unwrap().to_string()).collect();
        // The two images and the logo, which is stored once for every page.
        assert_eq!(names.iter().filter(|n| n.starts_with("ppt/media/")).count(), 3);
        assert!(read(&mut z, "ppt/theme/theme1.xml").contains(r#"<a:accent1><a:srgbClr val="C81E3C"/>"#));
        assert!(read(&mut z, "docProps/app.xml").contains("<Company>Acme &amp; Sons</Company>"));
        let slide = read(&mut z, "ppt/slides/slide2.xml");
        assert!(slide.contains("Confidential") && slide.contains(r#"<a:srgbClr val="C81E3C"/>"#));
        // Without branding the theme keeps Lumen blue and there is no company.
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(super::render(&super::super::tests::sample(2)).unwrap())).unwrap();
        assert!(read(&mut z, "ppt/theme/theme1.xml").contains(r#"<a:accent1><a:srgbClr val="2F6FEB"/>"#));
        assert!(!read(&mut z, "docProps/app.xml").contains("Company"));
    }
}
