//! The HTTP client every driver shares: one User-Agent, one timeout policy,
//! retries for the transient failures self-hosted forges are prone to, and
//! error messages that say what a status code most likely means.

use std::time::Duration;

use reqwest::{Method, RequestBuilder, StatusCode, header};
use serde::de::DeserializeOwned;

use crate::auth::Credential;

/// Plainly a program, not a browser. Two kinds of edge protection sit in
/// front of forges, and they want opposite things: Cloudflare's browser
/// integrity check refuses some generic library User-Agents (a 403 that
/// reads exactly like a token without the right scope), while Anubis-style
/// walls (code.forgejo.org) answer anything claiming to be "Mozilla" with a
/// JavaScript challenge page. A named tool passes both.
pub const USER_AGENT: &str = concat!("gitforge-mcp/", env!("CARGO_PKG_VERSION"));

/// Pauses before retrying a POST or DELETE whose connection was refused,
/// so the request was never sent.
const CONNECT_BACKOFF: [Duration; 2] = [Duration::from_millis(500), Duration::from_millis(1000)];

const SECONDS_PER_MINUTE: u64 = 60;

/// How much of a forge's refusal message to keep.
const MAX_MESSAGE_CHARS: usize = 300;

/// How a token is presented. Gitea and Forgejo use `token <value>`; GitHub
/// uses `Bearer <value>`. A Basic credential stays Basic auth under `Token`,
/// but under `Bearer` its password is sent as a bearer token, as GitHub
/// needs: so a system that takes Basic auth (Jenkins) must use `Token`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenScheme {
    Token,
    Bearer,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum ApiError {
    #[error("{host} is unreachable: {detail}")]
    Unreachable { host: String, detail: String },
    #[error("{host} refused the request ({status}): {hint}")]
    Refused {
        host: String,
        status: u16,
        hint: String,
    },
    #[error("{host} has no {path} (404)")]
    NotFound { host: String, path: String },
    #[error("{host} returned {status} for {path}")]
    Status {
        host: String,
        status: u16,
        path: String,
    },
    #[error("{host} rejected the request ({status}): {message}")]
    Rejected {
        host: String,
        status: u16,
        message: String,
    },
    #[error("{what} is not supported on this forge yet")]
    Unsupported { what: String },
    /// The forge can't serve this now, for a reason it gives.
    #[error("{reason}")]
    Unavailable { reason: String },
    /// GitHub's rate limits: the hourly quota, or a secondary limit on
    /// bursts. Asking again before the limit lifts only extends it.
    #[error("{host} is rate-limiting this credential: {detail}")]
    RateLimited { host: String, detail: String },
    #[error("{host} sent a response that could not be read: {detail}")]
    Decode { host: String, detail: String },
}

impl ApiError {
    /// True when the host could not be reached at all, as opposed to
    /// answering with an error: the case where cached capabilities apply.
    pub fn is_unreachable(&self) -> bool {
        matches!(self, ApiError::Unreachable { .. })
    }
}

/// What a form POST was answered with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FormReply {
    /// Where a redirect, or a 201, points.
    pub location: Option<String>,
}

#[derive(Clone)]
pub struct Http {
    client: reqwest::Client,
    /// For form POSTs, whose redirect is the answer rather than somewhere
    /// to go: made on first use.
    no_redirect: std::sync::OnceLock<reqwest::Client>,
    host: String,
    credential: Option<Credential>,
    scheme: TokenScheme,
    extra_headers: Vec<(&'static str, &'static str)>,
}

fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(20))
}

impl Http {
    pub fn new(host: &str, credential: Option<Credential>, scheme: TokenScheme) -> Self {
        let client = client_builder()
            .build()
            .expect("the HTTP client configuration is static and valid");
        Self {
            client,
            no_redirect: std::sync::OnceLock::new(),
            host: host.to_owned(),
            credential,
            scheme,
            extra_headers: Vec::new(),
        }
    }

    fn no_redirect(&self) -> &reqwest::Client {
        self.no_redirect.get_or_init(|| {
            client_builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("the HTTP client configuration is static and valid")
        })
    }

    pub fn with_header(mut self, name: &'static str, value: &'static str) -> Self {
        self.extra_headers.push((name, value));
        self
    }

    pub fn has_credential(&self) -> bool {
        self.credential.is_some()
    }

    fn request(&self, method: Method, url: &str) -> RequestBuilder {
        self.request_on(&self.client, method, url)
    }

    fn request_on(&self, client: &reqwest::Client, method: Method, url: &str) -> RequestBuilder {
        let mut req = client
            .request(method, url)
            .header(header::ACCEPT, "application/json");
        for (name, value) in &self.extra_headers {
            req = req.header(*name, *value);
        }
        match &self.credential {
            Some(Credential::Token(token)) => {
                let prefix = match self.scheme {
                    TokenScheme::Token => "token",
                    TokenScheme::Bearer => "Bearer",
                };
                req.header(
                    header::AUTHORIZATION,
                    format!("{prefix} {}", token.expose()),
                )
            }
            // GitHub stopped accepting passwords in 2021, so what a git
            // credential helper (`gh auth git-credential`, a credential
            // manager) holds for it is a token, and its GraphQL API takes
            // tokens only as Bearer.
            Some(Credential::Basic { password, .. }) if self.scheme == TokenScheme::Bearer => req
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", password.expose()),
                ),
            Some(Credential::Basic { username, password }) => {
                req.basic_auth(username, Some(password.expose()))
            }
            None => req,
        }
    }

    /// GETs `url` and decodes JSON. Retries twice on connection failures
    /// and on 502/503/504, which self-hosted forges behind a proxy return
    /// intermittently.
    pub async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T, ApiError> {
        let resp = self.get(url).await?;
        resp.json::<T>().await.map_err(|e| ApiError::Decode {
            host: self.host.clone(),
            detail: e.to_string(),
        })
    }

    /// GETs `url`; `Ok(None)` on 404.
    pub async fn get_json_opt<T: DeserializeOwned>(
        &self,
        url: &str,
    ) -> Result<Option<T>, ApiError> {
        match self.get_json(url).await {
            Ok(v) => Ok(Some(v)),
            Err(ApiError::NotFound { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Up to `max_pages` pages from `url(page)`, stopping at the first page
    /// shorter than `page_size`. `items` takes a page's items out of its
    /// body.
    pub async fn paged<B: DeserializeOwned, T>(
        &self,
        max_pages: u32,
        page_size: usize,
        url: impl Fn(u32) -> String,
        items: impl Fn(B) -> Vec<T>,
    ) -> Result<Vec<T>, ApiError> {
        let mut all = Vec::new();
        for page in 1..=max_pages {
            let batch = items(self.get_json(&url(page)).await?);
            let done = batch.len() < page_size;
            all.extend(batch);
            if done {
                break;
            }
        }
        Ok(all)
    }

    /// GETs `url` as text, keeping at most the last `max_bytes`. `range`
    /// asks for only the end of the resource (`bytes=-N`); servers that
    /// ignore it send everything, which is then cut here. Returns the text
    /// and whether it is only the end of the resource.
    pub async fn get_text_tail(
        &self,
        url: &str,
        range_bytes: Option<u64>,
        max_bytes: usize,
    ) -> Result<(String, bool), ApiError> {
        let range = range_bytes.map(|n| format!("bytes=-{n}"));
        let mut resp = self.get_with(url, range.as_deref()).await?;
        // 206 alone doesn't mean part of the log is missing: a log shorter
        // than the range comes back whole, still as 206. Content-Range says
        // where the bytes start.
        let mut partial = resp.status() == StatusCode::PARTIAL_CONTENT
            && resp
                .headers()
                .get(header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .is_none_or(range_starts_after_zero);
        let mut buf: Vec<u8> = Vec::new();
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    buf.extend_from_slice(&chunk);
                    if buf.len() > max_bytes * 2 {
                        buf.drain(..buf.len() - max_bytes);
                        partial = true;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    return Err(ApiError::Unreachable {
                        host: self.host.clone(),
                        detail: describe_transport_error(&e),
                    });
                }
            }
        }
        if buf.len() > max_bytes {
            buf.drain(..buf.len() - max_bytes);
            partial = true;
        }
        Ok((String::from_utf8_lossy(&buf).into_owned(), partial))
    }

    /// The `name` header of `url`'s response, whatever its status: some
    /// servers name themselves in a header even on a refusal (Jenkins's
    /// `X-Jenkins`). Only failing to reach `url` is an error.
    pub async fn header_of(&self, url: &str, name: &str) -> Result<Option<String>, ApiError> {
        let resp =
            self.request(Method::GET, url)
                .send()
                .await
                .map_err(|e| ApiError::Unreachable {
                    host: self.host.clone(),
                    detail: describe_transport_error(&e),
                })?;
        Ok(resp
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned))
    }

    /// POSTs `body` as JSON and returns the response's JSON, or `None` for
    /// an empty response (204). Never retried once sent: a write that may
    /// have happened mustn't happen twice. Only a connection that failed
    /// before the request went out is tried again.
    pub async fn post_json(
        &self,
        url: &str,
        body: &serde_json::Value,
    ) -> Result<Option<serde_json::Value>, ApiError> {
        self.send_json(Method::POST, url, body).await
    }

    /// PUTs `body` as JSON, as `post_json` does a POST.
    pub async fn put_json(
        &self,
        url: &str,
        body: &serde_json::Value,
    ) -> Result<Option<serde_json::Value>, ApiError> {
        self.send_json(Method::PUT, url, body).await
    }

    /// PATCHes `body` as JSON, as `post_json` does a POST.
    pub async fn patch_json(
        &self,
        url: &str,
        body: &serde_json::Value,
    ) -> Result<Option<serde_json::Value>, ApiError> {
        self.send_json(Method::PATCH, url, body).await
    }

    /// Runs a GraphQL query or mutation and returns its `data`. GraphQL
    /// answers 200 with an `errors` list when it refuses, so those become
    /// errors here. Sent once, like any POST.
    pub async fn graphql(
        &self,
        url: &str,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<serde_json::Value, ApiError> {
        let body = serde_json::json!({ "query": query, "variables": variables });
        let reply = self
            .post_json(url, &body)
            .await?
            .unwrap_or(serde_json::Value::Null);
        match graphql_error(&self.host, &reply) {
            Some(e) => Err(e),
            None => Ok(reply["data"].clone()),
        }
    }

    async fn send_json(
        &self,
        method: Method,
        url: &str,
        body: &serde_json::Value,
    ) -> Result<Option<serde_json::Value>, ApiError> {
        let resp = self.send_once(method, url, Some(body)).await?;
        let bytes = resp.bytes().await.map_err(|e| ApiError::Decode {
            host: self.host.clone(),
            detail: e.to_string(),
        })?;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| ApiError::Decode {
                host: self.host.clone(),
                detail: e.to_string(),
            })
    }

    /// Sends a write. Only a connection refused before the request went out
    /// is tried again; once sent, never.
    async fn send_once(
        &self,
        method: Method,
        url: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<reqwest::Response, ApiError> {
        let resp = self
            .send_unchecked(&self.client, method, url, |req| match body {
                Some(body) => req.json(body),
                None => req,
            })
            .await?;
        self.check(url, resp).await
    }

    /// POSTs `form`, URL-encoded, with `headers` added, as a web page's
    /// form does. A redirect is the answer, not followed. Sent once, like
    /// any POST.
    pub async fn post_form(
        &self,
        url: &str,
        form: &[(String, String)],
        headers: &[(String, String)],
    ) -> Result<FormReply, ApiError> {
        let body: String = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(form)
            .finish();
        let resp = self
            .send_unchecked(self.no_redirect(), Method::POST, url, |req| {
                let mut req = req
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(body.clone());
                for (name, value) in headers {
                    req = req.header(name.as_str(), value.as_str());
                }
                req
            })
            .await?;
        let location = resp
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        if !resp.status().is_redirection() {
            self.check(url, resp).await?;
        }
        Ok(FormReply { location })
    }

    /// GETs `url` and decodes JSON, with the cookies the response sets as
    /// one `Cookie` header value; `Ok(None)` on 404. For a token tied to
    /// the session it was issued in (Jenkins's CSRF crumb).
    pub async fn get_json_opt_with_cookies<T: DeserializeOwned>(
        &self,
        url: &str,
    ) -> Result<Option<(T, Option<String>)>, ApiError> {
        let resp = match self.get(url).await {
            Err(ApiError::NotFound { .. }) => return Ok(None),
            other => other?,
        };
        let cookies: Vec<&str> = resp
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter_map(|v| v.split(';').next())
            .map(str::trim)
            .filter(|v| v.contains('='))
            .collect();
        let cookie = (!cookies.is_empty()).then(|| cookies.join("; "));
        let body = resp.json::<T>().await.map_err(|e| ApiError::Decode {
            host: self.host.clone(),
            detail: e.to_string(),
        })?;
        Ok(Some((body, cookie)))
    }

    /// Sends a request built by `with` on `client`, retrying only a
    /// connection refused before it went out. Its status is not checked.
    async fn send_unchecked(
        &self,
        client: &reqwest::Client,
        method: Method,
        url: &str,
        with: impl Fn(RequestBuilder) -> RequestBuilder,
    ) -> Result<reqwest::Response, ApiError> {
        let mut attempt = 0;
        loop {
            let req = with(self.request_on(client, method.clone(), url));
            match req.send().await {
                Err(e) if e.is_connect() && attempt < CONNECT_BACKOFF.len() => {
                    tokio::time::sleep(CONNECT_BACKOFF[attempt]).await;
                    attempt += 1;
                }
                Err(e) => {
                    return Err(ApiError::Unreachable {
                        host: self.host.clone(),
                        detail: describe_transport_error(&e),
                    });
                }
                Ok(resp) => return Ok(resp),
            }
        }
    }

    /// DELETEs `url`. Like a POST, never retried once sent.
    pub async fn delete(&self, url: &str) -> Result<(), ApiError> {
        self.send_once(Method::DELETE, url, None).await?;
        Ok(())
    }

    async fn get(&self, url: &str) -> Result<reqwest::Response, ApiError> {
        self.get_with(url, None).await
    }

    async fn get_with(
        &self,
        url: &str,
        range: Option<&str>,
    ) -> Result<reqwest::Response, ApiError> {
        const BACKOFF: [Duration; 2] = [Duration::from_millis(500), Duration::from_millis(1500)];
        let mut attempt = 0;
        loop {
            let mut req = self.request(Method::GET, url);
            if let Some(range) = range {
                req = req.header(header::RANGE, range);
            }
            let outcome = req.send().await;
            let retryable = match &outcome {
                Err(e) => e.is_connect() || e.is_timeout(),
                Ok(r) => matches!(
                    r.status(),
                    StatusCode::BAD_GATEWAY
                        | StatusCode::SERVICE_UNAVAILABLE
                        | StatusCode::GATEWAY_TIMEOUT
                ),
            };
            if retryable && attempt < BACKOFF.len() {
                tokio::time::sleep(BACKOFF[attempt]).await;
                attempt += 1;
                continue;
            }
            return match outcome {
                Err(e) => Err(ApiError::Unreachable {
                    host: self.host.clone(),
                    detail: describe_transport_error(&e),
                }),
                Ok(resp) => self.check(url, resp).await,
            };
        }
    }

    async fn check(
        &self,
        url: &str,
        resp: reqwest::Response,
    ) -> Result<reqwest::Response, ApiError> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let limit = rate_limit(resp.headers(), chrono::Utc::now().timestamp());
        // Forges explain a refusal in a JSON `message`, often naming the
        // scope the token lacks; GitHub adds an `errors` list to a 422. A
        // proxy's refusal has none.
        let message = if status.is_client_error() && status != StatusCode::NOT_FOUND {
            resp.json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|v| refusal_message(&v))
        } else {
            None
        };
        let path = reqwest::Url::parse(url)
            .map(|u| u.path().to_owned())
            .unwrap_or_else(|_| url.to_owned());
        let host = self.host.clone();
        let limited = status == StatusCode::TOO_MANY_REQUESTS
            || (status == StatusCode::FORBIDDEN
                && (limit.is_some()
                    || message
                        .as_deref()
                        .is_some_and(|m| m.to_lowercase().contains("rate limit"))));
        if limited {
            return Err(ApiError::RateLimited {
                host,
                detail: limit.unwrap_or_else(|| {
                    message.unwrap_or_else(|| "wait a minute before asking again".into())
                }),
            });
        }
        Err(match status {
            StatusCode::UNAUTHORIZED => ApiError::Refused {
                host,
                status: status.as_u16(),
                hint: message.unwrap_or_else(|| {
                    if self.has_credential() {
                        "the credential was not accepted (expired or revoked?)".into()
                    } else {
                        "no credential was found for this host".into()
                    }
                }),
            },
            StatusCode::FORBIDDEN => ApiError::Refused {
                host,
                status: status.as_u16(),
                hint: message.unwrap_or_else(|| {
                    "the credential lacks access, or a proxy in front of the forge refused the request"
                        .into()
                }),
            },
            StatusCode::NOT_FOUND => ApiError::NotFound { host, path },
            // A write the forge understood and refused: a merge that isn't
            // allowed, a run that can't be cancelled, a missing branch.
            StatusCode::BAD_REQUEST
            | StatusCode::METHOD_NOT_ALLOWED
            | StatusCode::CONFLICT
            | StatusCode::PRECONDITION_FAILED
            | StatusCode::PAYLOAD_TOO_LARGE
            | StatusCode::UNPROCESSABLE_ENTITY
            | StatusCode::LOCKED => ApiError::Rejected {
                host,
                status: status.as_u16(),
                message: message.unwrap_or_else(|| format!("{path} refused it")),
            },
            StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT => ApiError::Unreachable {
                host,
                detail: format!(
                    "{status} from the proxy in front of it: the forge is down, or you are off its network"
                ),
            },
            _ => ApiError::Status {
                host,
                status: status.as_u16(),
                path,
            },
        })
    }
}

/// What a refusal's JSON body says: its `message`, and for GitHub's 422s
/// what each entry of `errors` adds ("Validation Failed: A pull request
/// already exists for acme:feature").
fn refusal_message(body: &serde_json::Value) -> Option<String> {
    let message = body["message"].as_str()?;
    let details: Vec<String> = body["errors"]
        .as_array()
        .map(|errors| {
            errors
                .iter()
                .filter_map(|e| {
                    e.as_str().map(str::to_owned).or_else(|| {
                        e["message"].as_str().map(str::to_owned).or_else(|| {
                            let field = e["field"].as_str()?;
                            Some(format!(
                                "{field} {}",
                                e["code"].as_str().unwrap_or("invalid")
                            ))
                        })
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let text = if details.is_empty() {
        message.to_owned()
    } else {
        format!("{message}: {}", details.join("; "))
    };
    Some(text.chars().take(MAX_MESSAGE_CHARS).collect())
}

/// When a rate limit lifts, from GitHub's headers: `retry-after` (seconds)
/// for a secondary limit, or `x-ratelimit-remaining: 0` with
/// `x-ratelimit-reset` (Unix time) for the hourly quota. `None` if the
/// headers say no limit was hit.
fn rate_limit(headers: &header::HeaderMap, now_unix: i64) -> Option<String> {
    let value = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    if let Some(seconds) = value("retry-after").and_then(|v| v.trim().parse::<i64>().ok()) {
        return Some(format!(
            "a secondary rate limit: wait {seconds} seconds before asking again"
        ));
    }
    if value("x-ratelimit-remaining") != Some("0") {
        return None;
    }
    let reset = value("x-ratelimit-reset").and_then(|v| v.trim().parse::<i64>().ok());
    Some(
        match reset.and_then(|r| chrono::DateTime::from_timestamp(r, 0)) {
            Some(at) => format!(
                "the hourly request quota is used up until {} ({} minutes from now)",
                at.format("%H:%M UTC"),
                u64::try_from(at.timestamp() - now_unix)
                    .unwrap_or(0)
                    .div_ceil(SECONDS_PER_MINUTE)
            ),
            None => "the hourly request quota is used up".to_owned(),
        },
    )
}

/// The first error a GraphQL reply carries, if any.
fn graphql_error(host: &str, reply: &serde_json::Value) -> Option<ApiError> {
    let first = reply["errors"].as_array()?.first()?;
    let message: String = first["message"]
        .as_str()
        .unwrap_or("the query was refused")
        .chars()
        .take(MAX_MESSAGE_CHARS)
        .collect();
    let host = host.to_owned();
    Some(match first["type"].as_str() {
        Some("FORBIDDEN") => ApiError::Refused {
            host,
            status: StatusCode::FORBIDDEN.as_u16(),
            hint: message,
        },
        Some("NOT_FOUND") => ApiError::NotFound {
            host,
            path: message,
        },
        Some("RATE_LIMITED") => ApiError::RateLimited {
            host,
            detail: message,
        },
        _ => ApiError::Rejected {
            host,
            status: StatusCode::UNPROCESSABLE_ENTITY.as_u16(),
            message,
        },
    })
}

/// `bytes 1200-4095/4096` starts after zero; `bytes 0-4095/4096` is the
/// whole thing.
fn range_starts_after_zero(content_range: &str) -> bool {
    content_range
        .trim_start_matches("bytes ")
        .split('-')
        .next()
        .and_then(|start| start.trim().parse::<u64>().ok())
        .is_none_or(|start| start > 0)
}

/// The error and its causes, so a DNS failure, a refused connection and a
/// TLS certificate the OS doesn't trust (a corporate proxy re-signing
/// traffic, say) read differently. reqwest's messages name the URL, never
/// the headers, so no credential can appear here.
fn describe_transport_error(e: &reqwest::Error) -> String {
    let mut parts = vec![if e.is_timeout() {
        "timed out".to_owned()
    } else {
        e.to_string()
    }];
    let mut source = std::error::Error::source(e);
    while let Some(cause) = source {
        let text = cause.to_string();
        if !parts.iter().any(|p| p.contains(&text)) {
            parts.push(text);
        }
        source = cause.source();
    }
    parts.join(": ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_validation_errors_are_spelled_out() {
        let body = serde_json::json!({
            "message": "Validation Failed",
            "errors": [
                {"resource": "PullRequest", "code": "custom", "message": "A pull request already exists for acme:feature."},
                {"resource": "PullRequest", "field": "base", "code": "invalid"},
                "plain"
            ]
        });
        assert_eq!(
            refusal_message(&body).unwrap(),
            "Validation Failed: A pull request already exists for acme:feature.; base invalid; plain"
        );
        assert_eq!(
            refusal_message(&serde_json::json!({"message": "Not allowed"})).unwrap(),
            "Not allowed"
        );
        assert!(refusal_message(&serde_json::json!({})).is_none());
    }

    #[test]
    fn rate_limits_say_when_they_lift() {
        let mut h = header::HeaderMap::new();
        assert!(rate_limit(&h, 0).is_none());
        h.insert("x-ratelimit-remaining", "12".parse().unwrap());
        assert!(rate_limit(&h, 0).is_none());
        h.insert("x-ratelimit-remaining", "0".parse().unwrap());
        h.insert("x-ratelimit-reset", "1794360000".parse().unwrap());
        assert_eq!(
            rate_limit(&h, 1_794_360_000 - 600).unwrap(),
            "the hourly request quota is used up until 01:20 UTC (10 minutes from now)"
        );
        h.insert("retry-after", "60".parse().unwrap());
        assert!(rate_limit(&h, 0).unwrap().contains("wait 60 seconds"));
    }

    #[test]
    fn graphql_errors_become_api_errors() {
        let refused = serde_json::json!({"errors": [{"type": "FORBIDDEN", "message": "no"}]});
        assert!(matches!(
            graphql_error("h", &refused),
            Some(ApiError::Refused { status: 403, .. })
        ));
        let other = serde_json::json!({"data": null, "errors": [{"message": "Pull request is in clean status"}]});
        assert_eq!(
            graphql_error("h", &other).unwrap().to_string(),
            "h rejected the request (422): Pull request is in clean status"
        );
        assert!(graphql_error("h", &serde_json::json!({"data": {}})).is_none());
    }

    #[test]
    fn content_ranges() {
        assert!(!range_starts_after_zero("bytes 0-4095/4096"));
        assert!(range_starts_after_zero("bytes 1200-4095/4096"));
        assert!(range_starts_after_zero("garbage"));
    }
}
