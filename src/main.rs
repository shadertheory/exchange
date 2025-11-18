use derive_more::{Deref, Display};
use http_body_util::{BodyExt, Collected, Full, Limited};
use hyper::{
    Request, Response, StatusCode, Uri,
    body::{Body, Bytes, Incoming},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use reqwest::Client;
use serde::{
    Deserialize, Serialize,
    de::{self, DeserializeOwned},
};
use std::{fmt, fs, future::ready, str::FromStr, sync::Arc, time::Duration};
use thiserror::Error as AutoError;
use tokio::{net::TcpListener, sync::RwLock};

macro_rules! err {
    ($status:tt, $message:expr) => {{
        Response::builder()
            .status(StatusCode::$status)
            .body(Full::from(Bytes::from($message)))
            .unwrap()
    }};
}

#[derive(Debug, Deserialize, Clone)]
struct Config {
    routes: Vec<Route>,
    port: u16,
    #[serde(default = "Config::default_max_body")]
    max_body_bytes: u64,
    #[serde(default = "Config::default_pool_size")]
    connection_pool_size: usize,
    #[serde(default = "Config::default_expectation")]
    gateway_expectation: Duration,
    #[serde(default = "Config::default_timeout")]
    gateway_timeout: Duration,
}

impl Config {
    fn default_max_body() -> u64 {
        1024 * 1024
    }

    fn default_pool_size() -> usize {
        2usize * usize::from(std::thread::available_parallelism().unwrap())
    }

    const fn default_timeout() -> Duration {
        Duration::from_secs(5)
    }

    const fn default_expectation() -> Duration {
        Duration::from_secs(1)
    }
}

#[derive(Debug, Display, Deserialize)]
enum Expected {
    Uri,
}

#[derive(AutoError, Debug, Deserialize)]
pub enum Error {
    #[error("General serde error: {0}")]
    General(String),
    #[error("Expected {0} but data was not compatible")]
    Expected(Expected),
}

impl de::Error for Error {
    fn custom<T>(msg: T) -> Self
    where
        T: fmt::Display,
    {
        Self::General(msg.to_string())
    }
}

mod net_compat {
    use derive_more::{Deref, Display};
    use hyper::{Request, body::Body};
    use serde::{
        Deserialize,
        de::{self, DeserializeOwned, Visitor},
    };
    use std::{fmt, str::FromStr, sync::Arc};
    use thiserror::Error as AutoError;
    use tokio::sync::RwLock;

    use hyper::Uri;

    #[derive(Debug, Clone)]
    pub struct UriSerde(String);

    impl From<UriSerde> for hyper::Uri {
        fn from(uri_serde: UriSerde) -> Self {
            uri_serde
                .0
                .parse()
                .expect("UriSerde should contain valid URI")
        }
    }

    pub mod uri_serde {
        use super::*;
        use serde::{Deserialize, Deserializer};

        pub fn deserialize<'de, D>(deserializer: D) -> Result<hyper::Uri, D::Error>
        where
            D: Deserializer<'de>,
        {
            let uri_serde = UriSerde::deserialize(deserializer)?;
            Ok(uri_serde.into())
        }
    }

    impl TryFrom<Uri> for UriSerde {
        type Error = String;

        fn try_from(uri: Uri) -> Result<Self, Self::Error> {
            UriSerde::new(uri.to_string().as_str())
        }
    }

    impl AsRef<str> for UriSerde {
        fn as_ref(&self) -> &str {
            &self.0
        }
    }

    impl<'de> Deserialize<'de> for UriSerde {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: de::Deserializer<'de>,
        {
            deserializer.deserialize_string(UriSerdeVisitor)
        }
    }

    struct UriSerdeVisitor;

    impl<'de> Visitor<'de> for UriSerdeVisitor {
        type Value = UriSerde;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "a valid URI string")
        }

        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            UriSerde::new(v).map_err(de::Error::custom)
        }

        fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            UriSerde::new(&v).map_err(de::Error::custom)
        }
    }

    impl UriSerde {
        pub fn as_str(&self) -> &str {
            &self.0
        }

        pub fn into_string(self) -> String {
            self.0
        }

        fn valid(&self) -> bool {
            Self::validate(&self.0).is_ok()
        }

        fn new(str: &str) -> Result<Self, String> {
            Self::validate(str)?;
            Ok(UriSerde(str.to_string()))
        }

        fn validate(uri: &str) -> Result<(), String> {
            if uri.is_empty() {
                return Err("URI cannot be empty".to_string());
            }

            let mut pos = 0;

            // Parse scheme (optional but if present must be valid)
            if let Some(scheme_end) = uri.find(':') {
                let scheme = &uri[..scheme_end];
                if !scheme.is_empty() {
                    Self::validate_scheme(scheme)?;
                }
                pos = scheme_end + 1;
            }

            // Parse hier-part (authority + path)
            if uri[pos..].starts_with("//") {
                pos += 2;

                // Parse authority
                let authority_end = uri[pos..]
                    .find(|c| c == '/' || c == '?' || c == '#')
                    .map(|i| pos + i)
                    .unwrap_or(uri.len());

                let authority = &uri[pos..authority_end];
                Self::validate_authority(authority)?;
                pos = authority_end;
            }

            // Parse path
            let path_end = uri[pos..]
                .find(|c| c == '?' || c == '#')
                .map(|i| pos + i)
                .unwrap_or(uri.len());

            let path = &uri[pos..path_end];
            Self::validate_path(path)?;
            pos = path_end;

            // Parse query (optional)
            if pos < uri.len() && uri.as_bytes()[pos] == b'?' {
                pos += 1;
                let query_end = uri[pos..].find('#').map(|i| pos + i).unwrap_or(uri.len());

                let query = &uri[pos..query_end];
                Self::validate_query_or_fragment(query, "query")?;
                pos = query_end;
            }

            // Parse fragment (optional)
            if pos < uri.len() && uri.as_bytes()[pos] == b'#' {
                pos += 1;
                let fragment = &uri[pos..];
                Self::validate_query_or_fragment(fragment, "fragment")?;
            }

            Ok(())
        }

        fn validate_scheme(scheme: &str) -> Result<(), String> {
            let mut chars = scheme.chars();
            if let Some(first) = chars.next() {
                if !first.is_ascii_alphabetic() {
                    return Err(format!("Scheme must start with letter, found '{}'", first));
                }
                for ch in chars {
                    if !ch.is_ascii_alphanumeric() && ch != '+' && ch != '-' && ch != '.' {
                        return Err(format!("Invalid character '{}' in scheme", ch));
                    }
                }
            }
            Ok(())
        }

        fn validate_authority(authority: &str) -> Result<(), String> {
            if authority.is_empty() {
                return Ok(());
            }

            // Split userinfo from host
            let (host, port) = if let Some(at_pos) = authority.rfind('@') {
                let userinfo = &authority[..at_pos];
                Self::validate_userinfo(userinfo)?;
                Self::parse_host_port(&authority[at_pos + 1..])?
            } else {
                Self::parse_host_port(authority)?
            };

            Self::validate_host(host)?;
            if let Some(p) = port {
                Self::validate_port(p)?;
            }

            Ok(())
        }

        fn parse_host_port(s: &str) -> Result<(&str, Option<&str>), String> {
            // IPv6 addresses are in brackets
            if s.starts_with('[') {
                if let Some(bracket_end) = s.find(']') {
                    let host = &s[..=bracket_end];
                    let port = if s.len() > bracket_end + 1 {
                        if s.as_bytes()[bracket_end + 1] != b':' {
                            return Err("Expected ':' after IPv6 address".to_string());
                        }
                        Some(&s[bracket_end + 2..])
                    } else {
                        None
                    };
                    return Ok((host, port));
                } else {
                    return Err("Unclosed '[' in IPv6 address".to_string());
                }
            }

            // Regular host:port
            if let Some(colon_pos) = s.rfind(':') {
                Ok((&s[..colon_pos], Some(&s[colon_pos + 1..])))
            } else {
                Ok((s, None))
            }
        }

        fn validate_userinfo(userinfo: &str) -> Result<(), String> {
            for ch in userinfo.chars() {
                if !Self::is_userinfo_char(ch) {
                    return Err(format!("Invalid character '{}' in userinfo", ch));
                }
            }
            Ok(())
        }

        fn validate_path(path: &str) -> Result<(), String> {
            for ch in path.chars() {
                if !Self::is_path_char(ch) {
                    return Err(format!("Invalid character '{}' in path", ch));
                }
            }
            Ok(())
        }

        fn validate_query_or_fragment(s: &str, name: &str) -> Result<(), String> {
            for ch in s.chars() {
                if !Self::is_query_or_fragment_char(ch) {
                    return Err(format!("Invalid character '{}' in {}", ch, name));
                }
            }
            Ok(())
        }

        // RFC 3986 character classes
        fn is_userinfo_char(ch: char) -> bool {
            rfc3986::is_unreserved(ch)
                || rfc3986::is_sub_delim(ch)
                || ch == ':'
                || rfc3986::is_pct_encoded_start(ch)
        }

        fn is_path_char(ch: char) -> bool {
            rfc3986::is_unreserved(ch)
                || rfc3986::is_sub_delim(ch)
                || ch == ':'
                || ch == '@'
                || ch == '/'
                || rfc3986::is_pct_encoded_start(ch)
        }

        fn is_query_or_fragment_char(ch: char) -> bool {
            Self::is_path_char(ch) || ch == '?'
        }

        pub fn validate_host(host: &str) -> Result<(), String> {
            if host.is_empty() {
                return Err("Host cannot be empty".to_string());
            }

            // IPv6 literal
            if host.starts_with('[') && host.ends_with(']') {
                return Self::validate_ipv6(&host[1..host.len() - 1]);
            }

            // IPv4 or reg-name
            if Self::is_ipv4(host) {
                return Ok(());
            }

            // reg-name (domain name)
            for ch in host.chars() {
                if !Self::is_reg_name_char(ch) {
                    return Err(format!("Invalid character '{}' in host", ch));
                }
            }

            Ok(())
        }

        pub fn validate_ipv6(addr: &str) -> Result<(), String> {
            let parts: Vec<&str> = addr.split(':').collect();

            if parts.len() > 8 {
                return Err("IPv6 address has too many parts".to_string());
            }

            let mut double_colon_count = 0;
            for part in &parts {
                if part.is_empty() {
                    double_colon_count += 1;
                } else if part.len() > 4 {
                    return Err(format!("IPv6 part '{}' too long", part));
                } else {
                    for ch in part.chars() {
                        if !ch.is_ascii_hexdigit() {
                            return Err(format!("Invalid hex digit '{}' in IPv6", ch));
                        }
                    }
                }
            }

            if double_colon_count > 1 && parts.len() < 3 {
                return Err("Invalid IPv6 format".to_string());
            }

            Ok(())
        }

        pub fn is_ipv4(s: &str) -> bool {
            let parts: Vec<&str> = s.split('.').collect();
            if parts.len() != 4 {
                return false;
            }

            for part in parts {
                if part.is_empty() || part.len() > 3 {
                    return false;
                }
                if let Ok(num) = part.parse::<u16>() {
                    if num > 255 {
                        return false;
                    }
                } else {
                    return false;
                }
            }

            true
        }

        pub fn validate_port(port: &str) -> Result<(), String> {
            if port.is_empty() {
                return Err("Port cannot be empty".to_string());
            }

            for ch in port.chars() {
                if !ch.is_ascii_digit() {
                    return Err(format!("Invalid character '{}' in port", ch));
                }
            }

            let port_num: u32 = port
                .parse()
                .map_err(|_| "Port number too large".to_string())?;

            if port_num > 65535 {
                return Err("Port must be 0-65535".to_string());
            }

            Ok(())
        }

        fn is_reg_name_char(ch: char) -> bool {
            rfc3986::is_unreserved(ch)
                || rfc3986::is_sub_delim(ch)
                || rfc3986::is_pct_encoded_start(ch)
        }
    }

    // RFC 3986 character class utilities - reusable for any RFC 3986 parsing
    pub mod rfc3986 {
        pub fn is_unreserved(ch: char) -> bool {
            ch.is_ascii_alphanumeric() || ch == '-' || ch == '.' || ch == '_' || ch == '~'
        }

        pub fn is_sub_delim(ch: char) -> bool {
            matches!(
                ch,
                '!' | '$' | '&' | '\'' | '(' | ')' | '*' | '+' | ',' | ';' | '='
            )
        }

        pub fn is_pct_encoded_start(ch: char) -> bool {
            ch == '%'
        }

        pub fn is_gen_delim(ch: char) -> bool {
            matches!(ch, ':' | '/' | '?' | '#' | '[' | ']' | '@')
        }

        pub fn is_reserved(ch: char) -> bool {
            is_gen_delim(ch) || is_sub_delim(ch)
        }
    }
}
#[derive(Debug, Deserialize, Clone)]
struct Route {
    #[serde(with = "net_compat::uri_serde")]
    pattern: Uri,
    #[serde(with = "net_compat::uri_serde")]
    target: Uri,
    #[serde(default)]
    wildcard: bool,
}

struct Proxy {
    max_body_bytes: u64,
    gateway_expectation: Duration,
    gateway_timeout: Duration,
    routes: Vec<Route>,
    client: reqwest::Client,
}

pub type Global = Arc<RwLock<Proxy>>;

async fn handle_request(req: Request<Incoming>, state: Global) -> Response<Full<Bytes>> {
    let path = req.uri().path();
    let query = req.uri().query();
    let method = req.method().clone();
    let headers = req.headers().clone();

    let state = state.write().await;

    let target = state.routes.iter().find_map(|route| {
        if route.wildcard {
            path.starts_with(&route.pattern.to_string())
                .then(|| &route.target)
        } else {
            (path == route.pattern).then(|| &route.target)
        }
    });
    let path = path.trim_start_matches("/");

    let target = match target {
        Some(t) => t,
        None => {
            return err!(NOT_FOUND, "pluh could not find that one for ya");
        }
    };

    let target_uri = match query {
        Some(q) => format!("{}{}?{}", target, path, q),
        None => format!("{}{}", target, path),
    };

    use http_body_util::BodyExt;
    let Ok(body_bytes) = Limited::new(req.into_body(), state.max_body_bytes as usize)
        .collect()
        .await
        .map(Collected::to_bytes)
    else {
        return err!(PAYLOAD_TOO_LARGE, "bro its a megabyte");
    };

    let mut reqwest = state.client.request(method, &target_uri);

    for (key, value) in headers.iter() {
        if key != "host" && key != "content-length" {
            reqwest = reqwest.header(key, value);
        }
    }

    if !body_bytes.is_empty() {
        reqwest = reqwest.body(body_bytes);
    }

    match reqwest.send().await {
        Ok(response) => {
            let status = response.status();
            let headers = response.headers().clone();

            let body_bytes = match response.bytes().await {
                Ok(bytes) => bytes,
                Err(e) => {
                    eprintln!("Failed to read response body: {}", e);
                    return err!(BAD_GATEWAY, "Failed to read response from server");
                }
            };

            let mut response = Response::builder().status(status);

            for (key, value) in headers {
                let Some(key) = key else { continue };
                response = response.header(key, value);
            }

            let response = response.body(Full::new(Bytes::from(body_bytes))).unwrap();

            response
        }
        Err(e) => {
            eprintln!("Failed to read response: {e}");
            return err!(
                BAD_GATEWAY,
                "i tried to understand this server's response, but i could not for the life of me"
            );
        }
    }
}

#[tokio::main]
async fn main() {
    run().await.expect("failed")
}

async fn run() -> anyhow::Result<()> {
    let config_file = fs::read_to_string("proxy.toml")?;
    let config = toml::from_str(&config_file)?;
    let Config {
        routes,
        port,
        max_body_bytes,
        connection_pool_size,
        gateway_expectation,
        gateway_timeout,
    } = config;

    let client = Client::builder()
        .pool_max_idle_per_host(connection_pool_size)
        .pool_idle_timeout(Duration::from_secs(90))
        .timeout(gateway_timeout)
        .tcp_keepalive(Duration::from_secs(60))
        .http2_adaptive_window(true)
        .build()?;

    let proxy = Proxy {
        client,
        routes,
        max_body_bytes,
        gateway_expectation,
        gateway_timeout,
    };

    let state = Arc::new(RwLock::new(proxy));

    let listener = TcpListener::bind(&format!("0.0.0.0:{port}")).await?;

    println!("\nProxy server running on http://0.0.0.0:{port}");
    println!("Press Ctrl+C to stop\n");

    // Accept connections in a loop
    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let state = state.clone();

        // Spawn a task to handle the connection
        tokio::spawn(async move {
            let state = state.clone();
            // Use HTTP/1 connection builder
            let service = service_fn(async |req| -> Result<_, anyhow::Error> {
                Ok(handle_request(req, state.clone()).await)
            });

            http1::Builder::new()
                .serve_connection(io, service)
                .await
                .expect("failed to serve connection")
        });
    }
}
