//! HTML writer: one self-contained page (pictures inlined as base64) that
//! reads top to bottom: summary, side-by-side images, each image with its
//! comments, detail crops and the measurement table. Prints well too.

use super::{label_color, Align, El, Picture, Report, Slide, PAGE_H, PAGE_W};

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\n' => o.push_str("<br>"),
            c => o.push(c),
        }
    }
    o
}

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut o = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        o.push(T[(n >> 18) as usize & 63] as char);
        o.push(T[(n >> 12) as usize & 63] as char);
        o.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        o.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    o
}

fn img(p: &Picture, alt: &str) -> String {
    format!(r#"<img src="data:image/jpeg;base64,{}" width="{}" height="{}" alt="{}">"#, base64(&p.jpeg), p.w, p.h, esc(alt))
}

fn badge(i: usize, label: &str) -> String {
    let c = label_color(i);
    format!(r#"<span class="badge" style="background:rgb({},{},{})">{}</span>"#, c[0], c[1], c[2], esc(label))
}

pub fn render(r: &Report) -> String {
    let title = if r.title.trim().is_empty() { "Image comparison" } else { r.title.as_str() };
    let mut h = String::new();
    h.push_str(&format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{}</title><style>
:root{{--ink:#20242c;--muted:#68707c;--rule:#dee2e8;--panel:#f5f7fa;--accent:#2f6feb}}
*{{box-sizing:border-box}}body{{margin:0;font:15px/1.5 "Segoe UI",system-ui,sans-serif;color:var(--ink);background:#fff}}
main{{max-width:1200px;margin:0 auto;padding:32px 20px 64px}}
h1{{font-size:30px;margin:0 0 4px}}h2{{font-size:20px;margin:40px 0 12px;padding-bottom:6px;border-bottom:1px solid var(--rule)}}
.meta{{color:var(--muted);margin:0 0 20px}}.summary{{background:var(--panel);border-left:3px solid var(--accent);padding:12px 16px;white-space:normal}}
.grid{{display:grid;gap:16px;grid-template-columns:repeat(auto-fit,minmax(240px,1fr))}}
figure{{margin:0}}figure img{{width:100%;height:auto;display:block;border:1px solid var(--rule)}}
figcaption{{display:flex;gap:8px;align-items:center;font-weight:600;margin-bottom:6px;overflow:hidden;white-space:nowrap;text-overflow:ellipsis}}
.badge{{color:#fff;font-weight:700;border-radius:4px;padding:0 7px;font-size:13px}}
.item{{display:grid;grid-template-columns:minmax(0,2fr) minmax(220px,1fr);gap:20px;margin-bottom:28px}}
.side{{background:var(--panel);padding:12px 16px}}.details{{color:var(--muted);font-size:13px;margin-bottom:8px}}
ol{{padding-left:20px;margin:8px 0 0}}table{{border-collapse:collapse;width:100%}}th,td{{padding:7px 10px;border-bottom:1px solid var(--rule);text-align:center}}
th{{background:var(--accent);color:#fff}}td:first-child,th:first-child{{text-align:left;font-weight:600}}tr:nth-child(even) td{{background:var(--panel)}}
.note{{color:var(--muted);font-size:13px}}footer{{color:var(--muted);font-size:12px;margin-top:48px}}
@media (max-width:720px){{.item{{grid-template-columns:1fr}}}}
@media print{{h2{{break-before:page}}.item{{break-inside:avoid}}}}
</style></head><body><main>"#,
        esc(title)
    ));
    h.push_str(&format!("<h1>{}</h1>", esc(title)));
    let by: Vec<&str> = [r.date.as_str(), r.author.as_str()].into_iter().filter(|s| !s.is_empty()).collect();
    h.push_str(&format!(r#"<p class="meta">{} image{} compared{}</p>"#, r.items.len(), if r.items.len() == 1 { "" } else { "s" }, if by.is_empty() { String::new() } else { format!(" · {}", esc(&by.join(" · "))) }));
    if !r.summary.trim().is_empty() {
        h.push_str(&format!(r#"<div class="summary">{}</div>"#, esc(r.summary.trim())));
    }
    if r.sections.overview && !r.items.is_empty() {
        h.push_str("<h2>Side by side</h2><div class=\"grid\">");
        for (i, it) in r.items.iter().enumerate() {
            h.push_str(&format!("<figure><figcaption>{} {}</figcaption>{}</figure>", badge(i, &it.label), esc(&it.name), img(&it.picture, &it.name)));
        }
        h.push_str("</div>");
    }
    if r.sections.per_image && !r.items.is_empty() {
        h.push_str("<h2>Each image</h2>");
        for (i, it) in r.items.iter().enumerate() {
            h.push_str(&format!(r#"<section class="item"><figure><figcaption>{} {}</figcaption>{}</figure><div class="side">"#, badge(i, &it.label), esc(&it.name), img(&it.picture, &it.name)));
            if !it.details.is_empty() {
                h.push_str(&format!(r#"<div class="details">{}</div>"#, esc(&it.details)));
            }
            if !it.comment.trim().is_empty() {
                h.push_str(&format!("<div>{}</div>", esc(it.comment.trim())));
            }
            if !it.notes.is_empty() {
                h.push_str("<ol>");
                for n in &it.notes {
                    h.push_str(&format!("<li>{}</li>", if n.trim().is_empty() { "(marked)".to_string() } else { esc(n.trim()) }));
                }
                h.push_str("</ol>");
            }
            if it.comment.trim().is_empty() && it.notes.is_empty() {
                h.push_str(r#"<div class="note">No comments.</div>"#);
            }
            h.push_str("</div></section>");
        }
    }
    if r.sections.details && !r.details.is_empty() {
        h.push_str("<h2>Details</h2>");
        for (k, d) in r.details.iter().enumerate() {
            let t = if d.title.trim().is_empty() { format!("Detail {}", k + 1) } else { d.title.clone() };
            h.push_str(&format!("<h3>{}</h3><div class=\"grid\">", esc(&t)));
            for (label, p) in &d.crops {
                let i = r.items.iter().position(|it| &it.label == label).unwrap_or(0);
                let name = r.items.get(i).map(|it| it.name.as_str()).unwrap_or("");
                h.push_str(&format!("<figure><figcaption>{} {}</figcaption>{}</figure>", badge(i, label), esc(name), img(p, label)));
            }
            h.push_str("</div>");
            if !d.comment.trim().is_empty() {
                h.push_str(&format!(r#"<p class="summary">{}</p>"#, esc(d.comment.trim())));
            }
        }
    }
    if r.sections.metrics {
        if let Some(t) = r.metrics.as_ref().filter(|t| !t.rows.is_empty()) {
            h.push_str("<h2>Measured quality</h2><table><thead><tr>");
            for c in &t.header {
                h.push_str(&format!("<th>{}</th>", esc(c)));
            }
            h.push_str("</tr></thead><tbody>");
            for row in &t.rows {
                h.push_str("<tr>");
                for c in row {
                    h.push_str(&format!("<td>{}</td>", esc(c)));
                }
                h.push_str("</tr>");
            }
            h.push_str("</tbody></table>");
            if !t.note.is_empty() {
                h.push_str(&format!(r#"<p class="note">{}</p>"#, esc(&t.note)));
            }
        }
    }
    h.push_str("<footer>Made with Lumen</footer></main></body></html>");
    h
}

fn pct(v: f32, of: f32) -> String {
    format!("{:.3}%", v / of * 100.0)
}

fn css_rgb(c: [u8; 3]) -> String {
    format!("rgb({},{},{})", c[0], c[1], c[2])
}

/// Laid-out slides as one page of scalable 16:9 sheets: every element keeps
/// its position and size relative to the sheet, so the page looks like the
/// PowerPoint and PDF versions at any window width.
pub fn render_slides(slides: &[Slide], title: &str) -> String {
    let title = if title.trim().is_empty() { "Lumen report" } else { title.trim() };
    let mut h = String::new();
    h.push_str(&format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{}</title><style>
*{{box-sizing:border-box}}body{{margin:0;background:#e9ecf1;font-family:"Segoe UI",Helvetica,Arial,sans-serif}}
main{{max-width:1280px;margin:0 auto;padding:24px 16px 48px}}
.sheet{{position:relative;width:100%;aspect-ratio:{}/{};background:#fff;margin:0 0 24px;box-shadow:0 2px 10px rgba(0,0,0,.12);container-type:inline-size;overflow:hidden}}
.el{{position:absolute;overflow:hidden}}.tx{{white-space:pre-wrap;line-height:1.25}}
.el img{{width:100%;height:100%;display:block}}
table{{border-collapse:collapse;width:100%;height:100%}}th,td{{border-bottom:1px solid #dee2e8;padding:0 .6cqw;text-align:center;font-size:1.15cqw}}
th{{background:#2f6feb;color:#fff}}td:first-child,th:first-child{{text-align:left;font-weight:600}}tr:nth-child(even) td{{background:#f5f7fa}}
@media print{{body{{background:#fff}}main{{padding:0;max-width:none}}.sheet{{box-shadow:none;break-after:page;margin:0}}}}
</style></head><body><main>"#,
        esc(title),
        PAGE_W,
        PAGE_H
    ));
    for s in slides {
        h.push_str(r#"<section class="sheet">"#);
        for e in &s.els {
            let bx = match e {
                El::Fill { bx, .. } | El::Frame { bx, .. } | El::Image { bx, .. } | El::Table { bx, .. } => *bx,
                El::Text(t) => t.bx,
            };
            let pos = format!("left:{};top:{};width:{};height:{}", pct(bx.x, PAGE_W), pct(bx.y, PAGE_H), pct(bx.w, PAGE_W), pct(bx.h, PAGE_H));
            match e {
                El::Fill { color, .. } => h.push_str(&format!(r#"<div class="el" style="{pos};background:{}"></div>"#, css_rgb(*color))),
                El::Frame { color, width, .. } => h.push_str(&format!(r#"<div class="el" style="{pos};border:{:.3}cqw solid {}"></div>"#, width / PAGE_W * 100.0, css_rgb(*color))),
                El::Image { pic, .. } => h.push_str(&format!(r#"<div class="el" style="{pos}">{}</div>"#, img(pic, ""))),
                El::Text(t) => h.push_str(&format!(
                    r#"<div class="el tx" style="{pos};font-size:{:.3}cqw;font-weight:{};color:{};text-align:{}">{}</div>"#,
                    t.size / PAGE_W * 100.0,
                    if t.bold { 700 } else { 400 },
                    css_rgb(t.color),
                    if t.align == Align::Center { "center" } else { "left" },
                    esc(&t.text)
                )),
                El::Table { table, .. } => {
                    h.push_str(&format!(r#"<div class="el" style="{pos}"><table><thead><tr>"#));
                    for c in &table.header {
                        h.push_str(&format!("<th>{}</th>", esc(c)));
                    }
                    h.push_str("</tr></thead><tbody>");
                    for row in &table.rows {
                        h.push_str("<tr>");
                        for c in row {
                            h.push_str(&format!("<td>{}</td>", esc(c)));
                        }
                        h.push_str("</tr>");
                    }
                    h.push_str("</tbody></table></div>");
                }
            }
        }
        h.push_str("</section>");
    }
    h.push_str("</main></body></html>");
    h
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64_matches_known_values() {
        assert_eq!(super::base64(b""), "");
        assert_eq!(super::base64(b"f"), "Zg==");
        assert_eq!(super::base64(b"fo"), "Zm8=");
        assert_eq!(super::base64(b"foobar"), "Zm9vYmFy");
    }
    #[test]
    fn html_escapes_text() {
        let r = super::super::tests::sample(1);
        let h = super::render(&r);
        assert!(h.contains("&lt;visible&gt;") && !h.contains("<visible>"));
    }
}
