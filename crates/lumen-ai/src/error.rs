//! One error type for every failure the UI can show. `Display` is the short,
//! human-readable message from the plan's error table; `detail()` adds the
//! server's own text where there is one.

use std::fmt;
use std::net::IpAddr;
use std::time::Duration;

/// Which timeout fired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeoutKind {
    /// TCP (and TLS) connect.
    Connect,
    /// No answer or no first token within `timeout_first_token_s`.
    FirstToken,
    /// The whole request exceeded `timeout_total_s` (or a probe its 15 s).
    Total,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AiError {
    /// The host field could not be parsed.
    Address(String),
    /// The host name did not resolve.
    Dns { host: String, detail: String },
    /// A resolved address is outside the local network while "Only allow
    /// local network servers" is on.
    NotLocal { addr: IpAddr },
    /// A public address over plain HTTP.
    PublicNeedsHttps { addr: IpAddr },
    /// A host name resolved to addresses that are not allowed (`addrs`),
    /// either outside the local network or public over plain HTTP.
    HostNotLocal { host: String, addrs: Vec<IpAddr>, needs_https: bool },
    /// Connection refused, reset, or the host is unreachable.
    Unreachable { target: String, detail: String },
    /// TLS handshake or certificate failure.
    Tls(String),
    /// HTTPS is on but the server answered in plain HTTP (vLLM without
    /// `--ssl-certfile`). Holds the TLS library's message.
    PlainHttpServer(String),
    Timeout(TimeoutKind, Duration),
    /// 401 or 403.
    Unauthorized { status: u16, message: String, key_sent: bool },
    /// 404 on a model route: the model id is not served.
    ModelNotFound(String),
    /// 400 "maximum context length".
    ContextLength(String),
    /// 400 "At most N image(s)". `limit` is parsed from the message.
    ImageLimit { limit: Option<u32>, message: String },
    /// 400 naming `response_format` / `json_schema` (structured outputs off
    /// or unsupported by this server).
    ResponseFormatRejected(String),
    /// 400 naming a vLLM-only body field (`chat_template_kwargs`, `top_k`...)
    /// on a server that is not vLLM.
    UnsupportedParameter(String),
    /// 400 saying the model takes no images.
    NotMultimodal(String),
    /// 413.
    PayloadTooLarge(String),
    /// 429, 502, 503, 504 (retryable before the first token).
    Busy { status: u16, message: String, retry_after: Option<Duration> },
    /// Any other HTTP error status.
    Http { status: u16, message: String },
    /// The server answered something that is not the OpenAI protocol.
    Protocol(String),
    /// `/v1/models` returned an empty list.
    NoModels,
    /// The user pressed Cancel.
    Cancelled,
    /// Credential Manager / DPAPI failure.
    Secret(String),
    /// Image preparation or encoding failed.
    Image(String),
    /// Bad input from the caller (no image, wrong preset arity...).
    Invalid(String),
}

impl AiError {
    /// Only these may be retried, and only before the first streamed token.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            AiError::Unreachable { .. } | AiError::Busy { .. } | AiError::Timeout(TimeoutKind::Connect, _)
        )
    }

    /// The server's own words, when they add something to `Display`.
    pub fn detail(&self) -> Option<&str> {
        match self {
            AiError::Dns { detail, .. } | AiError::Unreachable { detail, .. } | AiError::PlainHttpServer(detail) => Some(detail),
            // "Unauthorized" or "Forbidden" only repeats the status.
            AiError::Unauthorized { message, .. } if ["unauthorized", "forbidden", "not authenticated"].contains(&message.trim().to_ascii_lowercase().as_str()) => None,
            AiError::Unauthorized { message, .. }
            | AiError::Busy { message, .. }
            | AiError::Http { message, .. }
            | AiError::ImageLimit { message, .. } => Some(message),
            AiError::ModelNotFound(m)
            | AiError::ContextLength(m)
            | AiError::ResponseFormatRejected(m)
            | AiError::UnsupportedParameter(m)
            | AiError::NotMultimodal(m)
            | AiError::PayloadTooLarge(m) => Some(m),
            _ => None,
        }
        .map(String::as_str)
        .filter(|s| !s.is_empty())
    }
}

impl fmt::Display for AiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AiError::Address(m) => write!(f, "Invalid server address: {m}"),
            AiError::Dns { host, .. } => write!(f, "Server not reachable: could not resolve '{host}'"),
            AiError::NotLocal { addr } => write!(
                f,
                "{addr} is not a local network address. Untick 'Only allow local network servers' to allow it (HTTPS required)"
            ),
            AiError::PublicNeedsHttps { addr } => {
                write!(f, "{addr} is a public address: turn on HTTPS to send images to it")
            }
            AiError::HostNotLocal { host, addrs, needs_https } => {
                let list = addrs.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ");
                if *needs_https {
                    write!(f, "{host} ({list}) is a public server: turn on HTTPS to send images to it")
                } else {
                    write!(
                        f,
                        "{host} resolves to {list}, outside the local network. Untick 'Only allow local network servers' to allow it (HTTPS required)"
                    )
                }
            }
            AiError::Unreachable { target, .. } => write!(f, "Server not reachable at {target}"),
            AiError::Tls(m) => write!(f, "Server not reachable: TLS failed ({m})"),
            AiError::PlainHttpServer(_) => write!(f, "The server speaks plain HTTP: turn HTTPS off"),
            AiError::Timeout(TimeoutKind::Connect, d) => {
                write!(f, "Server not reachable: no answer within {} s (is the computer on, and the address right?)", d.as_secs())
            }
            AiError::Timeout(TimeoutKind::FirstToken, d) => write!(
                f,
                "No response within {} s (model loading or very large request). Raise the first-token timeout or send fewer images",
                d.as_secs()
            ),
            AiError::Timeout(TimeoutKind::Total, d) => write!(f, "Request took longer than {} s and was stopped", d.as_secs()),
            AiError::Unauthorized { status, key_sent: false, .. } => {
                write!(f, "Server requires an API key ({status}): type it under API key in Settings > AI")
            }
            AiError::Unauthorized { status, .. } => write!(f, "API key rejected ({status}): check the key in Settings > AI"),
            AiError::ModelNotFound(_) => write!(f, "Model not loaded on server (refresh the list)"),
            AiError::ContextLength(_) => write!(f, "Request too large: reduce max image side or tiles"),
            AiError::ImageLimit { limit: Some(n), .. } => write!(f, "Server accepts at most {n} image(s) per request"),
            AiError::ImageLimit { limit: None, .. } => write!(f, "Server rejected the number of images"),
            AiError::ResponseFormatRejected(_) => write!(f, "Server does not support structured output (response_format)"),
            AiError::UnsupportedParameter(_) => write!(f, "Server rejected a vLLM-specific request field"),
            AiError::NotMultimodal(_) => write!(f, "This model does not accept images"),
            AiError::PayloadTooLarge(_) => write!(f, "Request too large for the server (413): reduce image size or use JPEG"),
            AiError::Busy { status, .. } => write!(f, "Server busy ({status})"),
            AiError::Http { status, message } if !message.is_empty() => write!(f, "Server error {status}: {message}"),
            AiError::Http { status, .. } => write!(f, "Server error {status}"),
            AiError::Protocol(m) => write!(f, "Unexpected server response: {m}"),
            AiError::NoModels => write!(f, "Server returned no models"),
            AiError::Cancelled => write!(f, "Cancelled"),
            AiError::Secret(m) => write!(f, "Key storage failed: {m}"),
            AiError::Image(m) => write!(f, "Image preparation failed: {m}"),
            AiError::Invalid(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for AiError {}
