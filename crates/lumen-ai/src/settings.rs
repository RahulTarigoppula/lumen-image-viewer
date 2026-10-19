//! AI settings (stored by the app under `ai` in settings.json), server
//! address parsing, base URL derivation and the local-network policy.
//!
//! The host field accepts whatever a user is likely to paste: `192.168.1.20`,
//! `192.168.1.20:8000`, `workstation`, `http://192.168.1.20:8000/v1`,
//! `https://box.lan/v1/chat/completions`, `[fd00::5]:8000` or a bare IPv6
//! address. A bare host gets the port field and the HTTPS toggle as the
//! user set them by hand, else 8000 and plain HTTP; a pasted URL brings its
//! own scheme and port (which a later bare host does not inherit).

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};

use serde::{Deserialize, Serialize};

use crate::error::AiError;
use crate::secret::KeyStore;

/// vLLM's default port.
pub const DEFAULT_PORT: u16 = 8000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageEncoding {
    /// Lossless; the model judges the real artifacts, not Lumen's encoder.
    #[default]
    Png,
    /// JPEG q95 4:4:4, for slow links.
    Jpeg,
}

/// How models report bounding boxes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoordConvention {
    /// 0..1000 relative to the image as sent (Qwen3/3.5, Gemma).
    #[default]
    Relative1000,
    /// Pixels of the image as sent (Qwen2.5-VL).
    AbsolutePixels,
}

/// A user-defined preset (see `prompts::Preset::from_custom`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomPreset {
    pub name: String,
    pub system_prompt: String,
    /// Task text; `{metrics}` is replaced by the metric context, if any.
    pub user_template: String,
    /// Ask for the strict JSON quality report instead of Markdown.
    pub structured: bool,
    /// Takes an A/B pair instead of one image.
    pub pair: bool,
}

/// Settings under `ai` in settings.json. The API key is never stored here,
/// only where it lives (`api_key_ref`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiSettings {
    /// Master switch; off by default.
    pub enabled: bool,
    /// IP, host name or pasted URL (see the module docs).
    pub host: String,
    pub port: u16,
    pub https: bool,
    /// The port field was changed by hand (not taken from a pasted URL), so
    /// a bare host keeps it.
    pub port_by_user: bool,
    /// The HTTPS toggle was changed by hand.
    pub https_by_user: bool,
    /// Path before `/v1` when the server sits behind a reverse proxy
    /// (`https://box.lan/vllm/v1` gives `/vllm`). Normally empty.
    pub path_prefix: String,
    pub api_key_ref: Option<KeyStore>,
    /// Model id picked from `/v1/models` (or typed by hand).
    pub model: Option<String>,
    /// Preset id, see `prompts::PresetId`.
    pub preset: String,
    pub custom_presets: Vec<CustomPreset>,
    pub timeout_connect_s: u32,
    pub timeout_first_token_s: u32,
    pub timeout_total_s: u32,
    pub max_completion_tokens: u32,
    /// Stored rounded, so settings.json shows 0.2 and not 0.20000000298.
    #[serde(serialize_with = "round_f32")]
    pub temperature: f32,
    /// Long side of the downscaled full view.
    pub max_side_overview: u32,
    /// Native ROI tile side (multiple of 32).
    pub tile_side: u32,
    /// Images per request; must not exceed the server's `--limit-mm-per-prompt`.
    pub max_images: u32,
    pub encoding: ImageEncoding,
    /// Sends `chat_template_kwargs.enable_thinking = false` (Qwen3.x).
    pub disable_thinking: bool,
    pub coord_convention: CoordConvention,
    /// PEM of a self-signed CA for HTTPS to a workstation.
    pub custom_ca_pem: Option<String>,
    pub lan_only: bool,
    /// Also treat 100.64.0.0/10 (CGNAT, Tailscale) as local.
    pub allow_cgnat: bool,
    /// Show the send preview for every request (except to servers in
    /// `trusted_servers`); off: only for the first request to each server.
    pub always_confirm: bool,
    pub include_filename: bool,
    pub include_exif: bool,
    /// Leave objective metrics out of the prompt to avoid anchoring.
    pub blind: bool,
    /// Run comparisons twice with A and B swapped and report agreement.
    pub swap_check: bool,
    /// Servers (`Endpoint::server_key`) the user said not to confirm again.
    pub trusted_servers: Vec<String>,
    /// Image limits learned from 400 errors, keyed by `server_key|model`.
    pub image_limits: BTreeMap<String, u32>,
}

impl Default for AiSettings {
    fn default() -> Self {
        AiSettings {
            enabled: false,
            host: String::new(),
            port: DEFAULT_PORT,
            https: false,
            port_by_user: false,
            https_by_user: false,
            path_prefix: String::new(),
            api_key_ref: None,
            model: None,
            preset: "quality".into(),
            custom_presets: Vec::new(),
            timeout_connect_s: 5,
            timeout_first_token_s: 120,
            timeout_total_s: 600,
            max_completion_tokens: 1500,
            temperature: 0.2,
            max_side_overview: 1536,
            tile_side: 1024,
            max_images: 6,
            encoding: ImageEncoding::Png,
            disable_thinking: true,
            coord_convention: CoordConvention::Relative1000,
            custom_ca_pem: None,
            lan_only: true,
            allow_cgnat: false,
            always_confirm: true,
            include_filename: false,
            include_exif: false,
            blind: false,
            swap_check: false,
            trusted_servers: Vec::new(),
            image_limits: BTreeMap::new(),
        }
    }
}

impl AiSettings {
    /// The server this configuration points at.
    pub fn endpoint(&self) -> Result<Endpoint, AiError> {
        let input = parse_server_input(&self.host)?;
        let https = input.https.unwrap_or(self.https);
        let port = match (input.port, input.https) {
            (Some(p), _) => p,
            // A pasted URL without a port means the scheme's port.
            (None, Some(true)) => 443,
            (None, Some(false)) => 80,
            (None, None) if self.port == 0 => DEFAULT_PORT,
            (None, None) => self.port,
        };
        let prefix = if input.prefix.is_empty() { normalize_prefix(&self.path_prefix) } else { input.prefix };
        Ok(Endpoint { https, host: input.host, port, prefix })
    }

    /// Folds a typed or pasted address into `host`, `port`, `https` and
    /// `path_prefix`, so the fields show what will be used. Call it when the
    /// host field loses focus or before Test connection.
    ///
    /// A URL or `host:port` sets the scheme and port it names. A bare host
    /// gets the port and HTTPS setting the user chose by hand, else vLLM's
    /// defaults (8000, plain HTTP): a scheme or port that came with an
    /// earlier pasted URL never sticks to the next server. A bare host also
    /// drops an earlier URL's path prefix.
    pub fn apply_server_input(&mut self, text: &str) -> Result<Endpoint, AiError> {
        let input = parse_server_input(text)?;
        if input.https.is_some() {
            self.https_by_user = false;
        }
        if input.port.is_some() || input.https.is_some() {
            self.port_by_user = false;
        }
        if !self.https_by_user {
            self.https = input.https.unwrap_or(false);
        }
        if !self.port_by_user {
            self.port = DEFAULT_PORT;
        }
        self.host = input.host.clone();
        self.path_prefix = input.prefix.clone();
        let ep = Endpoint {
            https: self.https,
            port: match (input.port, input.https) {
                (Some(p), _) => p,
                // A pasted URL without a port means the scheme's port.
                (None, Some(true)) => 443,
                (None, Some(false)) => 80,
                (None, None) => self.port.max(1),
            },
            host: input.host,
            prefix: input.prefix,
        };
        self.port = ep.port;
        Ok(ep)
    }

    pub fn net_policy(&self) -> NetPolicy {
        NetPolicy { lan_only: self.lan_only, allow_cgnat: self.allow_cgnat }
    }

    /// The image limit to plan for: the setting, capped by what this server
    /// and model told us earlier.
    pub fn effective_max_images(&self, server_key: &str, model: &str) -> u32 {
        let learned = self.image_limits.get(&limit_key(server_key, model)).copied().unwrap_or(u32::MAX);
        self.max_images.clamp(1, 64).min(learned.max(1))
    }

    /// Records the limit reported by an `AiEvent::ImageLimitLearned`.
    pub fn remember_image_limit(&mut self, server_key: &str, model: &str, limit: u32) {
        self.image_limits.insert(limit_key(server_key, model), limit.max(1));
    }

    pub fn is_trusted(&self, server_key: &str) -> bool {
        self.trusted_servers.iter().any(|s| s == server_key)
    }

    /// Whether the send preview must be shown before this request: always,
    /// unless the user said "Do not ask again" for this server. With
    /// `always_confirm` off, confirming the first request to a server also
    /// trusts it (see `confirmed`).
    pub fn needs_confirmation(&self, server_key: &str) -> bool {
        !self.is_trusted(server_key)
    }

    /// A previewed request to `server_key` was sent; `dont_ask` is "Do not
    /// ask again for this server". Returns true when the server is now
    /// trusted. Never changes `always_confirm`.
    pub fn confirmed(&mut self, server_key: &str, dont_ask: bool) -> bool {
        if (dont_ask || !self.always_confirm) && !self.is_trusted(server_key) {
            self.trusted_servers.push(server_key.to_string());
            return true;
        }
        false
    }

    /// Non-fatal problems to show next to the server fields.
    pub fn warnings(&self, has_key: bool) -> Vec<String> {
        let mut out = Vec::new();
        if let Ok(ep) = self.endpoint() {
            if has_key && !ep.https && !ep.is_loopback_literal() {
                out.push("The API key is sent unencrypted over HTTP. Use HTTPS if other people share this network.".into());
            }
        }
        if !self.tile_side.is_multiple_of(32) {
            out.push("Tile side is rounded down to a multiple of 32.".into());
        }
        out
    }
}

fn round_f32<S: serde::Serializer>(v: &f32, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_f64((*v as f64 * 1000.0).round() / 1000.0)
}

fn limit_key(server_key: &str, model: &str) -> String {
    format!("{server_key}|{model}")
}

/// A parsed host field. `https` and `port` are `None` when not given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerInput {
    pub https: Option<bool>,
    /// Host name (lowercase) or IP literal without brackets.
    pub host: String,
    pub port: Option<u16>,
    /// Path before `/v1`, "" or "/x/y".
    pub prefix: String,
}

/// Parses `host`, `host:port`, `[v6]:port`, a bare IPv6 address, or an
/// `http(s)://` URL with an optional path (a trailing `/v1`, `/v1/models` or
/// `/v1/chat/completions` is dropped).
pub fn parse_server_input(text: &str) -> Result<ServerInput, AiError> {
    let bad = |m: &str| Err(AiError::Address(m.to_string()));
    let s = text.trim();
    if s.is_empty() {
        return bad("enter the server's IP address or host name");
    }
    if s.chars().any(|c| c.is_whitespace()) {
        return bad("the address contains spaces");
    }
    let lower = s.to_ascii_lowercase();
    let (https, rest) = if lower.starts_with("http://") {
        (Some(false), &s[7..])
    } else if lower.starts_with("https://") {
        (Some(true), &s[8..])
    } else if let Some(i) = s.find("://") {
        return Err(AiError::Address(format!("unsupported scheme '{}', use http or https", &s[..i])));
    } else {
        (None, s)
    };
    // Query and fragment never matter for a base URL.
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if authority.contains('@') {
        return bad("remove the user name from the address; enter the API key in its own field");
    }
    if authority.is_empty() {
        return bad("the address has no host");
    }

    let (host, port_text) = if let Some(inner) = authority.strip_prefix('[') {
        let Some(end) = inner.find(']') else { return bad("missing ']' after the IPv6 address") };
        let after = &inner[end + 1..];
        let port = match after {
            "" => None,
            p if p.starts_with(':') => Some(&p[1..]),
            _ => return bad("unexpected text after ']'"),
        };
        let h = &inner[..end];
        if h.parse::<Ipv6Addr>().is_err() {
            return Err(AiError::Address(check_zone(h).unwrap_or_else(|| format!("'{h}' is not an IPv6 address"))));
        }
        (h.to_string(), port)
    } else {
        match authority.matches(':').count() {
            0 => (authority.to_string(), None),
            1 => {
                let (h, p) = authority.split_once(':').unwrap();
                (h.to_string(), Some(p))
            }
            _ => {
                // More than one colon without brackets: a bare IPv6 address.
                if authority.parse::<Ipv6Addr>().is_err() {
                    return Err(AiError::Address(check_zone(authority).unwrap_or_else(|| {
                        "an IPv6 address with a port must be written as [address]:port".to_string()
                    })));
                }
                (authority.to_string(), None)
            }
        }
    };

    let port = match port_text {
        None => None,
        Some("") => return bad("the port after ':' is empty"),
        Some(p) => match p.parse::<u16>() {
            Ok(0) | Err(_) => return Err(AiError::Address(format!("'{p}' is not a valid port (1 to 65535)"))),
            Ok(n) => Some(n),
        },
    };

    let host = if host.contains(':') { host.to_ascii_lowercase() } else { validate_hostname(&host)? };
    Ok(ServerInput { https, host, port, prefix: strip_api_path(path) })
}

fn check_zone(h: &str) -> Option<String> {
    h.contains('%').then(|| "IPv6 zone ids (%) are not supported; use a ULA (fd..) or IPv4 address".to_string())
}

fn validate_hostname(h: &str) -> Result<String, AiError> {
    let h = h.trim_end_matches('.').to_ascii_lowercase();
    if h.parse::<Ipv4Addr>().is_ok() {
        return Ok(h);
    }
    if h.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return Err(AiError::Address(format!("'{h}' is not a complete IPv4 address")));
    }
    if h.len() > 253 {
        return Err(AiError::Address("the host name is too long".into()));
    }
    for label in h.split('.') {
        let ok = !label.is_empty()
            && label.len() <= 63
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            && !label.starts_with('-')
            && !label.ends_with('-');
        if !ok {
            return Err(AiError::Address(format!("'{h}' is not a valid host name")));
        }
    }
    Ok(h)
}

/// "/v1", "/v1/models", "/vllm/v1/chat/completions/" -> "", "", "/vllm".
fn strip_api_path(path: &str) -> String {
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let keep = segs.iter().rposition(|s| s.eq_ignore_ascii_case("v1")).unwrap_or(segs.len());
    let mut kept = &segs[..keep];
    // A pasted health or version URL.
    if keep == segs.len() {
        if let Some(last) = kept.last() {
            if ["health", "version", "models"].contains(&last.to_ascii_lowercase().as_str()) {
                kept = &kept[..kept.len() - 1];
            }
        }
    }
    kept.iter().map(|s| format!("/{s}")).collect()
}

fn normalize_prefix(p: &str) -> String {
    strip_api_path(p)
}

/// A resolved server location. All URLs are derived from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub https: bool,
    pub host: String,
    pub port: u16,
    pub prefix: String,
}

impl Endpoint {
    fn host_in_url(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        }
    }

    /// `http://192.168.1.20:8000`
    pub fn origin(&self) -> String {
        format!("{}://{}:{}", if self.https { "https" } else { "http" }, self.host_in_url(), self.port)
    }

    /// `http://192.168.1.20:8000/v1`
    pub fn base_url(&self) -> String {
        format!("{}{}/v1", self.origin(), self.prefix)
    }

    /// A server route outside `/v1`, such as `/health` or `/version`.
    pub fn route(&self, path: &str) -> String {
        format!("{}{}{}", self.origin(), self.prefix, path)
    }

    /// An API route under `/v1`, such as `/models`.
    pub fn api(&self, path: &str) -> String {
        format!("{}{}", self.base_url(), path)
    }

    /// `192.168.1.20:8000`, for the status badge.
    pub fn display(&self) -> String {
        format!("{}:{}", self.host_in_url(), self.port)
    }

    /// Identifies the server in caches and the trusted list.
    pub fn server_key(&self) -> String {
        format!("{}{}", self.origin(), self.prefix)
    }

    /// Credential Manager target name.
    pub fn credential_target(&self) -> String {
        format!("Lumen/ai/{}", self.display())
    }

    /// True for `localhost` and loopback literals (no DNS lookup).
    pub fn is_loopback_literal(&self) -> bool {
        self.host == "localhost" || self.host.parse::<IpAddr>().map(|ip| ip.is_loopback()).unwrap_or(false)
    }
}

/// Where an address belongs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpScope {
    Loopback,
    /// RFC 1918.
    Private,
    LinkLocal,
    /// IPv6 fc00::/7.
    UniqueLocal,
    /// 100.64.0.0/10.
    Cgnat,
    Public,
    /// Unspecified, multicast, broadcast, documentation: never a server.
    Invalid,
}

pub fn ip_scope(ip: IpAddr) -> IpScope {
    match ip {
        IpAddr::V4(v4) => v4_scope(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4_scope(v4);
            }
            let s = v6.segments();
            if v6.is_loopback() {
                IpScope::Loopback
            } else if v6.is_unspecified() || v6.is_multicast() || (s[0] == 0x2001 && s[1] == 0x0db8) {
                IpScope::Invalid
            } else if s[0] & 0xfe00 == 0xfc00 {
                IpScope::UniqueLocal
            } else if s[0] & 0xffc0 == 0xfe80 {
                IpScope::LinkLocal
            } else {
                IpScope::Public
            }
        }
    }
}

fn v4_scope(ip: Ipv4Addr) -> IpScope {
    let o = ip.octets();
    if ip.is_loopback() {
        IpScope::Loopback
    } else if ip.is_private() {
        IpScope::Private
    } else if ip.is_link_local() {
        IpScope::LinkLocal
    } else if o[0] == 100 && (o[1] & 0xc0) == 64 {
        IpScope::Cgnat
    } else if ip.is_unspecified() || ip.is_multicast() || ip.is_broadcast() || ip.is_documentation() || o[0] == 0 {
        IpScope::Invalid
    } else {
        IpScope::Public
    }
}

/// Which addresses a request may go to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetPolicy {
    pub lan_only: bool,
    pub allow_cgnat: bool,
}

impl Default for NetPolicy {
    fn default() -> Self {
        NetPolicy { lan_only: true, allow_cgnat: false }
    }
}

impl NetPolicy {
    pub fn is_local(&self, ip: IpAddr) -> bool {
        match ip_scope(ip) {
            IpScope::Loopback | IpScope::Private | IpScope::LinkLocal | IpScope::UniqueLocal => true,
            IpScope::Cgnat => self.allow_cgnat,
            IpScope::Public | IpScope::Invalid => false,
        }
    }

    /// Checks one address. Public addresses need `lan_only` off and HTTPS.
    pub fn check(&self, ip: IpAddr, https: bool) -> Result<(), AiError> {
        if self.is_local(ip) {
            return Ok(());
        }
        if ip_scope(ip) == IpScope::Invalid || self.lan_only {
            return Err(AiError::NotLocal { addr: ip });
        }
        if !https {
            return Err(AiError::PublicNeedsHttps { addr: ip });
        }
        Ok(())
    }

    /// Checks every address; one bad address fails the whole host, so a
    /// name with both a LAN and a public record is refused.
    pub fn check_all(&self, addrs: &[SocketAddr], https: bool) -> Result<(), AiError> {
        addrs.iter().try_for_each(|a| self.check(a.ip(), https))
    }

    /// `check_all` for the addresses `host` resolved to. When a host name
    /// fails, the error names it with every address that is not allowed
    /// (not only the first, which is often an IPv6 address the user never
    /// sees).
    pub fn check_host(&self, host: &str, addrs: &[SocketAddr], https: bool) -> Result<(), AiError> {
        let Err(first) = self.check_all(addrs, https) else { return Ok(()) };
        if host.parse::<IpAddr>().is_ok() {
            return Err(first);
        }
        let mut bad: Vec<IpAddr> = Vec::new();
        for a in addrs {
            if self.check(a.ip(), https).is_err() && !bad.contains(&a.ip()) {
                bad.push(a.ip());
            }
        }
        // IPv4 first, as people know their addresses.
        bad.sort_by_key(|ip| ip.is_ipv6());
        let needs_https = matches!(first, AiError::PublicNeedsHttps { .. });
        Err(AiError::HostNotLocal { host: host.to_string(), addrs: bad, needs_https })
    }
}

/// Resolves the endpoint's host (IP literals skip DNS).
pub fn resolve(ep: &Endpoint) -> Result<Vec<SocketAddr>, AiError> {
    if let Ok(ip) = ep.host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, ep.port)]);
    }
    match (ep.host.as_str(), ep.port).to_socket_addrs() {
        Ok(it) => {
            let mut v: Vec<SocketAddr> = it.collect();
            v.dedup();
            if v.is_empty() {
                Err(AiError::Dns { host: ep.host.clone(), detail: "no addresses".into() })
            } else {
                Ok(v)
            }
        }
        Err(e) => Err(AiError::Dns { host: ep.host.clone(), detail: e.to_string() }),
    }
}

/// Resolves and checks the endpoint against the policy.
pub fn resolve_checked(ep: &Endpoint, policy: &NetPolicy) -> Result<Vec<SocketAddr>, AiError> {
    let addrs = resolve(ep)?;
    policy.check_host(&ep.host, &addrs, ep.https)?;
    Ok(addrs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(host: &str) -> Endpoint {
        AiSettings { host: host.into(), ..Default::default() }.endpoint().unwrap()
    }

    #[test]
    fn bare_ip_gets_defaults() {
        let e = ep("192.168.1.20");
        assert_eq!(e.base_url(), "http://192.168.1.20:8000/v1");
        assert_eq!(e.route("/health"), "http://192.168.1.20:8000/health");
        assert_eq!(e.display(), "192.168.1.20:8000");
        assert_eq!(e.credential_target(), "Lumen/ai/192.168.1.20:8000");
    }

    #[test]
    fn host_port_and_urls() {
        assert_eq!(ep("192.168.1.20:9000").base_url(), "http://192.168.1.20:9000/v1");
        assert_eq!(ep(" http://192.168.1.20:8000/v1 ").base_url(), "http://192.168.1.20:8000/v1");
        assert_eq!(ep("http://192.168.1.20:8000/v1/").base_url(), "http://192.168.1.20:8000/v1");
        assert_eq!(ep("http://192.168.1.20:8000").base_url(), "http://192.168.1.20:8000/v1");
        assert_eq!(ep("HTTP://Workstation.LAN:8001/v1/chat/completions").base_url(), "http://workstation.lan:8001/v1");
        assert_eq!(ep("https://box.lan/v1").base_url(), "https://box.lan:443/v1");
        assert_eq!(ep("http://box.lan/v1/models").base_url(), "http://box.lan:80/v1");
        assert_eq!(ep("https://box.lan/vllm/v1").base_url(), "https://box.lan:443/vllm/v1");
        assert_eq!(ep("http://box:8000/health").route("/health"), "http://box:8000/health");
        assert_eq!(ep("workstation").base_url(), "http://workstation:8000/v1");
        assert_eq!(ep("localhost:8000").base_url(), "http://localhost:8000/v1");
        assert_eq!(ep("http://10.0.0.5:8000/v1?x=1#frag").base_url(), "http://10.0.0.5:8000/v1");
    }

    #[test]
    fn https_toggle_and_port_field() {
        let s = AiSettings { host: "10.1.2.3".into(), port: 8443, https: true, ..Default::default() };
        assert_eq!(s.endpoint().unwrap().base_url(), "https://10.1.2.3:8443/v1");
        // A URL's scheme wins over the toggle.
        let s = AiSettings { host: "http://10.1.2.3:8000".into(), https: true, ..Default::default() };
        assert_eq!(s.endpoint().unwrap().base_url(), "http://10.1.2.3:8000/v1");
        // Port 0 in old settings falls back to the default.
        let s = AiSettings { host: "10.1.2.3".into(), port: 0, ..Default::default() };
        assert_eq!(s.endpoint().unwrap().port, 8000);
    }

    #[test]
    fn ipv6_forms() {
        assert_eq!(ep("::1").base_url(), "http://[::1]:8000/v1");
        assert_eq!(ep("fd00::5").base_url(), "http://[fd00::5]:8000/v1");
        assert_eq!(ep("[fd00::5]:9000").base_url(), "http://[fd00::5]:9000/v1");
        assert_eq!(ep("http://[FE80::1]:8000/v1").base_url(), "http://[fe80::1]:8000/v1");
        assert_eq!(ep("[::1]").display(), "[::1]:8000");
    }

    #[test]
    fn rejects_bad_input() {
        for bad in [
            "",
            "   ",
            "ftp://host",
            "user@host",
            "http://user:pw@host:8000",
            "192.168.1",
            "host:abc",
            "host:0",
            "host:70000",
            "host:",
            "[fd00::5",
            "[fd00::5]x",
            "fe80::1%eth0",
            "[nope]:80",
            "bad_-host-",
            "a b",
            "fd00::5::1:8000",
        ] {
            assert!(parse_server_input(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn apply_server_input_folds_fields() {
        let mut s = AiSettings::default();
        let e = s.apply_server_input("https://192.168.1.20:8443/proxy/v1").unwrap();
        assert_eq!(e.base_url(), "https://192.168.1.20:8443/proxy/v1");
        assert_eq!((s.host.as_str(), s.port, s.https, s.path_prefix.as_str()), ("192.168.1.20", 8443, true, "/proxy"));
        assert_eq!(s.endpoint().unwrap(), e);
        // A bad input leaves the fields alone.
        assert!(s.apply_server_input("http://").is_err());
        assert_eq!(s.host, "192.168.1.20");
        // A bare IP keeps a port the user set by hand.
        let mut s = AiSettings { port: 8001, port_by_user: true, ..Default::default() };
        s.apply_server_input("10.0.0.7").unwrap();
        assert_eq!(s.endpoint().unwrap().base_url(), "http://10.0.0.7:8001/v1");
    }

    #[test]
    fn earlier_url_scheme_and_port_do_not_stick() {
        let mut s = AiSettings::default();
        s.apply_server_input("https://8.8.8.8:8000").unwrap();
        assert!(s.https);
        let e = s.apply_server_input("127.0.0.1").unwrap();
        assert_eq!(e.base_url(), "http://127.0.0.1:8000/v1");
        assert_eq!(s.endpoint().unwrap(), e);
        s.apply_server_input("127.0.0.1:8001").unwrap();
        assert_eq!(s.port, 8001);
        assert_eq!(s.apply_server_input("192.168.1.250").unwrap().base_url(), "http://192.168.1.250:8000/v1");
        s.apply_server_input("https://box.lan/vllm/v1").unwrap();
        assert_eq!(s.apply_server_input("box2").unwrap().base_url(), "http://box2:8000/v1", "no stale prefix");
        // Choices made by hand stay until a URL says otherwise.
        let mut s = AiSettings { https: true, https_by_user: true, port: 8443, port_by_user: true, ..Default::default() };
        assert_eq!(s.apply_server_input("10.0.0.9").unwrap().base_url(), "https://10.0.0.9:8443/v1");
        assert_eq!(s.apply_server_input("http://10.0.0.9:9000").unwrap().base_url(), "http://10.0.0.9:9000/v1");
        assert!(!s.https_by_user && !s.port_by_user);
        assert_eq!(s.apply_server_input("10.0.0.9").unwrap().base_url(), "http://10.0.0.9:8000/v1");
    }

    #[test]
    fn temperature_is_stored_rounded() {
        let j = serde_json::to_value(AiSettings::default()).unwrap();
        assert_eq!(j["temperature"].to_string(), "0.2");
    }

    #[test]
    fn host_names_list_every_rejected_address() {
        let addrs: Vec<SocketAddr> = ["[2606:2800:21f:cb07:6820:80da:af6b:8b2c]:80".parse().unwrap(), "93.184.215.14:80".parse().unwrap()].to_vec();
        let e = NetPolicy::default().check_host("example.com", &addrs, false).unwrap_err();
        let m = e.to_string();
        assert!(m.starts_with("example.com resolves to 93.184.215.14, 2606:2800:21f:cb07:6820:80da:af6b:8b2c"), "{m}");
        // IP literals keep the plain message.
        let ip = [addrs[1]];
        assert!(matches!(NetPolicy::default().check_host("93.184.215.14", &ip, false), Err(AiError::NotLocal { .. })));
    }

    #[test]
    fn lan_ranges() {
        let p = NetPolicy::default();
        let local = |s: &str| p.is_local(s.parse().unwrap());
        for ok in [
            "127.0.0.1",
            "127.255.0.9",
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.20",
            "169.254.10.10",
            "::1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "::ffff:192.168.1.5",
        ] {
            assert!(local(ok), "{ok} is local");
        }
        for no in [
            "8.8.8.8",
            "172.15.255.255",
            "172.32.0.1",
            "192.169.0.1",
            "11.0.0.1",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "2606:4700::1111",
            "fec0::1",
            "::ffff:8.8.8.8",
            "::",
        ] {
            assert!(!local(no), "{no} is not local");
        }
        let cg = NetPolicy { lan_only: true, allow_cgnat: true };
        assert!(cg.is_local("100.64.0.1".parse().unwrap()));
        assert!(cg.is_local("100.127.255.254".parse().unwrap()));
        assert!(!cg.is_local("100.128.0.1".parse().unwrap()));
        assert!(!cg.is_local("100.63.255.255".parse().unwrap()));
    }

    #[test]
    fn public_policy() {
        let ip: IpAddr = "8.8.8.8".parse().unwrap();
        assert_eq!(NetPolicy::default().check(ip, true), Err(AiError::NotLocal { addr: ip }));
        let open = NetPolicy { lan_only: false, allow_cgnat: false };
        assert_eq!(open.check(ip, false), Err(AiError::PublicNeedsHttps { addr: ip }));
        assert!(open.check(ip, true).is_ok());
        // Never a server, whatever the policy.
        assert!(open.check("0.0.0.0".parse().unwrap(), true).is_err());
        // Every address of a host must pass.
        let mixed = ["192.168.1.2:8000".parse().unwrap(), "8.8.8.8:8000".parse().unwrap()];
        assert!(NetPolicy::default().check_all(&mixed, false).is_err());
    }

    #[test]
    fn resolves_localhost_and_checks_every_address() {
        let e = ep("localhost");
        let addrs = resolve_checked(&e, &NetPolicy::default()).unwrap();
        assert!(!addrs.is_empty());
        assert!(addrs.iter().all(|a| a.ip().is_loopback()));
        let pub_ep = ep("8.8.8.8");
        assert!(matches!(resolve_checked(&pub_ep, &NetPolicy::default()), Err(AiError::NotLocal { .. })));
    }

    #[test]
    fn learned_limits_and_trust() {
        let mut s = AiSettings::default();
        assert_eq!(s.effective_max_images("k", "m"), 6);
        s.remember_image_limit("k", "m", 1);
        assert_eq!(s.effective_max_images("k", "m"), 1);
        assert_eq!(s.effective_max_images("k", "other"), 6);
        assert!(s.needs_confirmation("k"));
        // Always confirm: a plain Send trusts nothing.
        assert!(!s.confirmed("k", false));
        assert!(s.needs_confirmation("k"));
        // "Do not ask again" trusts this server only, and leaves the switch on.
        assert!(s.confirmed("k", true));
        assert!(!s.needs_confirmation("k") && s.needs_confirmation("other") && s.always_confirm);
        // Without Always confirm, the first confirmed request trusts the server.
        s.always_confirm = false;
        assert!(s.confirmed("other", false));
        assert!(!s.needs_confirmation("other"));
    }

    #[test]
    fn key_over_http_warns() {
        let s = AiSettings { host: "192.168.1.20".into(), ..Default::default() };
        assert_eq!(s.warnings(true).len(), 1);
        assert!(s.warnings(false).is_empty());
        let s = AiSettings { host: "127.0.0.1".into(), ..Default::default() };
        assert!(s.warnings(true).is_empty());
    }

    #[test]
    fn settings_round_trip_and_defaults() {
        let s: AiSettings = serde_json::from_str(r#"{"host":"10.0.0.2"}"#).unwrap();
        assert_eq!(s.port, 8000);
        assert!(s.lan_only && s.always_confirm && s.disable_thinking && !s.enabled);
        let j = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<AiSettings>(&j).unwrap(), s);
    }
}
