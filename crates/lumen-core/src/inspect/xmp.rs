//! A small, tolerant XMP (RDF/XML) reader: no XML dependency, never fails.
//!
//! Handles properties in attribute form (`prefix:Name="value"` on
//! rdf:Description) and element form (`<prefix:Name>value</prefix:Name>`),
//! rdf:Seq / rdf:Alt / rdf:Bag lists (joined with ", "), nested structures
//! (named "Parent / prefix:Name"), XML entities, comments and CDATA.
//! Mismatched tags are closed or ignored instead of failing.

/// Deepest element nesting kept (deeper start tags are ignored).
const MAX_DEPTH: usize = 128;

#[derive(Debug, Default)]
struct El {
    name: String,
    attrs: Vec<(String, String)>,
    kids: Vec<Node>,
}

#[derive(Debug)]
enum Node {
    El(El),
    Text(String),
}

/// A parsed XMP packet.
pub(crate) struct Doc {
    root: El,
}

/// Find an XMP packet anywhere in a file (`<x:xmpmeta` ... `</x:xmpmeta>`).
pub(crate) fn find_raw(b: &[u8]) -> Option<String> {
    let start = super::find(b, b"<x:xmpmeta").or_else(|| super::find(b, b"<x:xapmeta"))?;
    let rest = &b[start..];
    let end = super::find(rest, b"</x:xmpmeta>")
        .map(|e| e + "</x:xmpmeta>".len())
        .or_else(|| super::find(rest, b"</x:xapmeta>").map(|e| e + "</x:xapmeta>".len()))
        .unwrap_or(rest.len().min(4 << 20));
    Some(String::from_utf8_lossy(&rest[..end]).into_owned())
}

/// Decode the five XML entities and numeric character references.
pub(crate) fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(semi) = rest.bytes().take(12).position(|c| c == b';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..semi];
        let decoded = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ if ent.starts_with("#x") || ent.starts_with("#X") => u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32),
            _ if ent.starts_with('#') => ent[1..].parse::<u32>().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Parse one start tag at `s[i] == '<'`: (element, self-closing, index after '>').
fn parse_tag(s: &str, i: usize) -> Option<(El, bool, usize)> {
    let b = s.as_bytes();
    let mut j = i + 1;
    let name_start = j;
    while j < b.len() && !b[j].is_ascii_whitespace() && b[j] != b'>' && b[j] != b'/' {
        j += 1;
    }
    let name = s[name_start..j].to_string();
    if name.is_empty() {
        return None;
    }
    let mut attrs = Vec::new();
    loop {
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= b.len() {
            return None;
        }
        if b[j] == b'>' {
            return Some((El { name, attrs, kids: Vec::new() }, false, j + 1));
        }
        if b[j] == b'/' {
            j += 1;
            while j < b.len() && b[j] != b'>' {
                j += 1;
            }
            return if j < b.len() { Some((El { name, attrs, kids: Vec::new() }, true, j + 1)) } else { None };
        }
        let a0 = j;
        while j < b.len() && b[j] != b'=' && !b[j].is_ascii_whitespace() && b[j] != b'>' && b[j] != b'/' {
            j += 1;
        }
        let aname = s[a0..j].to_string();
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if j < b.len() && b[j] == b'=' {
            j += 1;
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < b.len() && (b[j] == b'"' || b[j] == b'\'') {
                let q = b[j];
                let v0 = j + 1;
                let end = b[v0..].iter().position(|&c| c == q)? + v0;
                attrs.push((aname, decode_entities(&s[v0..end])));
                j = end + 1;
            } else {
                // Unquoted value: up to whitespace or '>'.
                let v0 = j;
                while j < b.len() && !b[j].is_ascii_whitespace() && b[j] != b'>' {
                    j += 1;
                }
                attrs.push((aname, decode_entities(&s[v0..j])));
            }
        } else if aname.is_empty() {
            j += 1;
        }
    }
}

impl Doc {
    /// Build the element tree. Never fails; malformed parts are skipped.
    pub fn parse(s: &str) -> Doc {
        let b = s.as_bytes();
        let mut stack: Vec<El> = vec![El::default()];
        let mut i = 0;
        let close_top = |stack: &mut Vec<El>| {
            if stack.len() > 1 {
                let el = stack.pop().unwrap_or_default();
                if let Some(parent) = stack.last_mut() {
                    parent.kids.push(Node::El(el));
                }
            }
        };
        while i < b.len() {
            if b[i] != b'<' {
                let end = b[i..].iter().position(|&c| c == b'<').map_or(b.len(), |k| i + k);
                let text = &s[i..end];
                if !text.trim().is_empty() {
                    if let Some(top) = stack.last_mut() {
                        top.kids.push(Node::Text(decode_entities(text)));
                    }
                }
                i = end;
                continue;
            }
            let rest = &s[i..];
            if rest.starts_with("<!--") {
                i = rest.find("-->").map_or(b.len(), |k| i + k + 3);
            } else if rest.starts_with("<![CDATA[") {
                let end = rest.find("]]>").map_or(b.len(), |k| i + k);
                if let Some(top) = stack.last_mut() {
                    top.kids.push(Node::Text(s[i + 9..end.max(i + 9)].to_string()));
                }
                i = (end + 3).min(b.len());
            } else if rest.starts_with("<?") || rest.starts_with("<!") {
                i = rest.find('>').map_or(b.len(), |k| i + k + 1);
            } else if rest.starts_with("</") {
                let end = rest.find('>').map_or(b.len(), |k| i + k);
                let name = s[i + 2..end.max(i + 2)].trim();
                // Close up to the matching open element; ignore stray end tags.
                if let Some(pos) = stack.iter().rposition(|e| e.name == name) {
                    if pos > 0 {
                        while stack.len() > pos {
                            close_top(&mut stack);
                        }
                    }
                }
                i = (end + 1).min(b.len());
            } else {
                match parse_tag(s, i) {
                    Some((el, self_closing, next)) => {
                        if self_closing {
                            if let Some(top) = stack.last_mut() {
                                top.kids.push(Node::El(el));
                            }
                        } else if stack.len() < MAX_DEPTH {
                            stack.push(el);
                        }
                        i = next;
                    }
                    None => break,
                }
            }
        }
        while stack.len() > 1 {
            close_top(&mut stack);
        }
        Doc { root: stack.pop().unwrap_or_default() }
    }

    /// Whether the packet has an rdf:RDF element at all.
    pub fn has_rdf(&self) -> bool {
        fn walk(e: &El, depth: usize) -> bool {
            e.name == "rdf:RDF"
                || (depth < MAX_DEPTH
                    && e.kids.iter().any(|k| matches!(k, Node::El(c) if walk(c, depth + 1))))
        }
        walk(&self.root, 0)
    }

    /// All properties in document order as ("prefix:Name", display value).
    pub fn properties(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        find_desc(&self.root, &mut out, 0);
        out
    }

    /// GContainer:Directory items (Google Ultra HDR / motion photo / depth):
    /// the Item:* attributes (or child elements) of each item.
    pub fn container_items(&self) -> Vec<Vec<(String, String)>> {
        fn walk(e: &El, out: &mut Vec<Vec<(String, String)>>, depth: usize) {
            if depth > MAX_DEPTH {
                return;
            }
            let mut item: Vec<(String, String)> = e
                .attrs
                .iter()
                .filter(|(k, _)| k.starts_with("Item:"))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            for k in &e.kids {
                if let Node::El(c) = k {
                    if c.name.starts_with("Item:") {
                        item.push((c.name.clone(), text_of(c)));
                    }
                }
            }
            if item.iter().any(|(k, _)| k == "Item:Semantic") {
                out.push(item);
            }
            for k in &e.kids {
                if let Node::El(c) = k {
                    if !c.name.starts_with("Item:") {
                        walk(c, out, depth + 1);
                    }
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.root, &mut out, 0);
        out
    }
}

fn text_of(e: &El) -> String {
    let mut s = String::new();
    for k in &e.kids {
        if let Node::Text(t) = k {
            s.push_str(t);
        }
    }
    s.trim().to_string()
}

fn skip_attr(k: &str) -> bool {
    k.starts_with("xmlns") || k.starts_with("rdf:") || k.starts_with("xml:") || k.starts_with("x:")
}

fn join(path: &str, name: &str) -> String {
    if path.is_empty() { name.to_string() } else { format!("{path} / {name}") }
}

/// Display form of a value: huge base64 (embedded thumbnails, videos) becomes a size.
pub(crate) fn display_value(v: &str) -> String {
    let v = v.trim();
    if v.len() > 1000 && v.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'+' || c == b'/' || c == b'=' || c.is_ascii_whitespace()) {
        let bytes = v.bytes().filter(|c| !c.is_ascii_whitespace()).count() * 3 / 4;
        return format!("(binary data, {} KB)", super::thousands(bytes.div_ceil(1024) as u64));
    }
    super::clip(v, 2000)
}

fn push(out: &mut Vec<(String, String)>, name: String, value: &str) {
    if out.len() < 5000 {
        out.push((name, display_value(value)));
    }
}

fn find_desc(e: &El, out: &mut Vec<(String, String)>, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    for k in &e.kids {
        if let Node::El(c) = k {
            if c.name == "rdf:Description" {
                walk_desc(c, out, "", depth + 1);
            } else {
                find_desc(c, out, depth + 1);
            }
        }
    }
}

/// Properties of an rdf:Description or a structure (attributes, then child elements).
fn walk_desc(e: &El, out: &mut Vec<(String, String)>, path: &str, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    for (k, v) in &e.attrs {
        if !skip_attr(k) {
            push(out, join(path, k), v);
        }
    }
    for k in &e.kids {
        if let Node::El(c) = k {
            if c.name == "rdf:Description" {
                walk_desc(c, out, path, depth + 1);
            } else if !c.name.starts_with("rdf:") {
                property(c, out, path, depth + 1);
            }
        }
    }
}

fn is_container(name: &str) -> bool {
    matches!(name, "rdf:Seq" | "rdf:Bag" | "rdf:Alt")
}

fn property(c: &El, out: &mut Vec<(String, String)>, path: &str, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let name = join(path, &c.name);
    let parse_type = c.attrs.iter().find(|(k, _)| k == "rdf:parseType").map(|(_, v)| v.as_str());
    let kids: Vec<&El> = c.kids.iter().filter_map(|k| if let Node::El(e) = k { Some(e) } else { None }).collect();
    if parse_type == Some("Resource") {
        walk_desc(c, out, &name, depth + 1);
        return;
    }
    if let Some(list) = kids.iter().find(|k| is_container(&k.name)) {
        let mut items = Vec::new();
        for li in list.kids.iter().filter_map(|k| if let Node::El(e) = k { Some(e) } else { None }) {
            if li.name != "rdf:li" {
                continue;
            }
            let li_kids: Vec<&El> = li.kids.iter().filter_map(|k| if let Node::El(e) = k { Some(e) } else { None }).collect();
            let li_struct = li.attrs.iter().any(|(k, v)| k == "rdf:parseType" && v == "Resource")
                || !li_kids.is_empty()
                || li.attrs.iter().any(|(k, _)| !skip_attr(k));
            if li_struct {
                walk_desc(li, out, &name, depth + 1);
            } else {
                let t = text_of(li);
                let t = if t.is_empty() {
                    li.attrs.iter().find(|(k, _)| k == "rdf:resource").map(|(_, v)| v.clone()).unwrap_or_default()
                } else {
                    t
                };
                if !t.is_empty() {
                    items.push(t);
                }
            }
        }
        if !items.is_empty() {
            push(out, name, &items.join(", "));
        }
        return;
    }
    if let Some(d) = kids.iter().find(|k| k.name == "rdf:Description") {
        walk_desc(d, out, &name, depth + 1);
        return;
    }
    if !kids.is_empty() {
        // Structure without rdf:parseType (not strictly valid, seen in the wild).
        for (k, v) in &c.attrs {
            if !skip_attr(k) {
                push(out, join(&name, k), v);
            }
        }
        for k in kids {
            property(k, out, &name, depth + 1);
        }
        return;
    }
    let text = text_of(c);
    let struct_attrs: Vec<&(String, String)> = c.attrs.iter().filter(|(k, _)| !skip_attr(k)).collect();
    if text.is_empty() && !struct_attrs.is_empty() {
        // Shorthand structure: <ns:Prop a:b="v"/>.
        for (k, v) in struct_attrs {
            push(out, join(&name, k), v);
        }
        return;
    }
    let value = if text.is_empty() {
        c.attrs.iter().find(|(k, _)| k == "rdf:resource").map(|(_, v)| v.clone()).unwrap_or_default()
    } else {
        text
    };
    push(out, name, &value);
}

/// Encrypted Google Camera blocks in the XMP: (property names, ignoring
/// case, as Google spelled them over the years; what the block is).
pub(crate) const GOOGLE_ENCRYPTED: [(&[&str], &str); 2] = [
    (&["GCamera:HdrPlusMakernote", "GCamera:hdrp_makernote"], "HDR+ notes"),
    (&["GCamera:ShotLogData", "GCamera:shot_log_data"], "shot log"),
];

/// Whether the property is one of [`GOOGLE_ENCRYPTED`].
pub(crate) fn is_google_encrypted(key: &str) -> bool {
    let leaf = key.rsplit(" / ").next().unwrap_or(key);
    GOOGLE_ENCRYPTED.iter().any(|(names, _)| names.iter().any(|n| leaf.eq_ignore_ascii_case(n)))
}

/// Display value of an encrypted block (base64 text): its size, not the text.
pub(crate) fn encrypted_value(v: &str) -> String {
    match v.strip_prefix("(binary data, ") {
        Some(rest) => format!("(encrypted Google data, {rest}"),
        None => format!("(encrypted Google data, {} bytes)", v.trim().len() * 3 / 4),
    }
}

/// Hover notes for well-known XMP properties.
pub(crate) fn note_for(key: &str) -> &'static str {
    let leaf = key.rsplit(" / ").next().unwrap_or(key);
    if is_google_encrypted(key) {
        return if leaf.to_ascii_lowercase().contains("shot") {
            "Encrypted Google Camera shot log (capture details); only Google's own tools read it."
        } else {
            "Encrypted Google HDR+ processing data: the photo was merged from several frames. Only Google's own tools read it."
        };
    }
    match leaf {
        "GCamera:MotionPhoto" | "Camera:MotionPhoto" => "1 = this is a motion photo with a short video stored after the image.",
        "GCamera:MotionPhotoVersion" | "Camera:MotionPhotoVersion" => "Version of the motion photo format.",
        "GCamera:MicroVideo" => "1 = older Google motion photo format with an embedded video.",
        "GCamera:MicroVideoVersion" => "Version of the older Google motion photo format.",
        "GCamera:MicroVideoOffset" => "Size of the embedded video in bytes, counted from the end of the file.",
        "GCamera:MotionPhotoPresentationTimestampUs" | "GCamera:MicroVideoPresentationTimestampUs" => "Video frame (in microseconds) that matches the still photo.",
        "GCamera:SpecialTypeID" => "Google Camera mode identifier (for example portrait or night sight).",
        "GCamera:BurstID" => "Shared by all photos of one burst.",
        "GCamera:BurstPrimary" => "1 = the photo the camera picked as the best of its burst.",
        "GCamera:PortraitNote" | "GCamera:PortraitRequest" | "GCamera:PortraitVersion" => "Portrait mode (background blur) data of the Google Camera app.",
        "GCamera:DisableAutoCreation" => "Google Photos creations (collages, animations) the camera asked not to make from this photo.",
        "hdrgm:Version" => "Ultra HDR gain map version: the file carries an HDR gain map.",
        "hdrgm:GainMapMin" => "Boost at gain map black, as a power of 2 (0 = x1, no change).",
        "hdrgm:GainMapMax" => "Boost at gain map white, as a power of 2 (2 = x4 brighter).",
        "hdrgm:Gamma" => "Curve applied to the gain map values before use (1 = straight).",
        "hdrgm:OffsetSDR" | "hdrgm:OffsetHDR" => "Small value added before the boost and removed after it, so dark pixels can be brightened too.",
        "hdrgm:HDRCapacityMin" => "Screen headroom (power of 2) from which the gain map starts to apply.",
        "hdrgm:HDRCapacityMax" => "Screen headroom (power of 2) at which the gain map applies fully.",
        "hdrgm:BaseRenditionIsHDR" => "True when the main image is the HDR version and the map makes the SDR one.",
        "HDRGainMap:HDRGainMapVersion" => "Apple HDR gain map version.",
        "Item:Semantic" => "Role of an embedded item: Primary image, GainMap, Depth or MotionPhoto.",
        "Item:Length" => "Size of the embedded item in bytes.",
        "Item:Mime" => "File type of the embedded item.",
        "GDepth:Format" => "Format of the embedded depth map (portrait mode).",
        "GPano:ProjectionType" => "Panorama / photo sphere projection.",
        "xmp:CreatorTool" => "Software that created the file.",
        "xmp:Rating" => "Star rating (0 to 5).",
        "photoshop:DateCreated" => "Date the photo was taken.",
        "dc:creator" => "Author.",
        "dc:rights" => "Copyright notice.",
        "crs:Version" => "Adobe Camera Raw / Lightroom version that edited the image.",
        _ => "",
    }
}
