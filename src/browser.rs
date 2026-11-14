//! Rendering web pages in a headless Chrome running in its own container
//! (`DOCVISION_BROWSER_URL`, e.g. `http://browser:9222`), over the DevTools protocol.
//!
//! Each render gets a fresh browser context (no shared cookies or storage) and one WebSocket.
//! Every request the page makes is intercepted: `payload.request` headers go only to the source's
//! site, link-local and cloud-metadata addresses are refused, and images, media and fonts are
//! skipped (they don't change the text).

use crate::rpc::AppError;
use crate::source::{FetchRequest, site_host};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;

const PAGE_TIMEOUT: Duration = Duration::from_secs(30);
const WAIT_FOR_TIMEOUT: Duration = Duration::from_secs(10);
/// The network counts as idle after this long without a new request.
const IDLE: Duration = Duration::from_millis(500);

pub struct Browser {
    base: url::Url,
    sem: Semaphore,
}

pub struct Page {
    pub html: String,
    /// The page's URL after redirects.
    pub url: url::Url,
}

fn err(m: impl Into<String>) -> AppError {
    AppError::new(502, "render_failed", m.into()).stage("fetch")
}

impl Browser {
    pub fn new(base: &str, concurrency: usize) -> Result<Browser, String> {
        let base = url::Url::parse(base).map_err(|e| format!("DOCVISION_BROWSER_URL: {e}"))?;
        Ok(Browser { base, sem: Semaphore::new(concurrency) })
    }

    /// The browser's WebSocket endpoint. Chrome only answers DevTools requests whose Host is an
    /// IP address or `localhost` (and reports its own address), so the configured host is
    /// resolved and its IP used for both requests.
    async fn ws_url(&self) -> Result<String, AppError> {
        let unreachable = || err("the browser container is not reachable");
        let (host, port) = (self.base.host_str().unwrap_or("localhost"), self.base.port_or_known_default().unwrap_or(9222));
        let addrs = tokio::net::lookup_host((host, port)).await.map_err(|_| unreachable())?;
        let mut found = None;
        for addr in addrs {
            let r = crate::source::http_client().get(format!("http://{addr}/json/version")).timeout(Duration::from_secs(5)).send().await;
            if let Ok(r) = r {
                found = Some((addr, r));
                break;
            }
        }
        let (addr, resp) = found.ok_or_else(unreachable)?;
        let v: Value = resp.json().await.map_err(|_| err("the browser container did not describe itself"))?;
        let ws = v["webSocketDebuggerUrl"].as_str().ok_or_else(|| err("the browser container has no DevTools endpoint"))?;
        let mut u = url::Url::parse(ws).map_err(|e| err(e.to_string()))?;
        let _ = u.set_ip_host(addr.ip());
        let _ = u.set_port(Some(addr.port()));
        Ok(u.to_string())
    }

    /// Load `u`, wait for it to settle, and return the rendered HTML.
    pub async fn render(
        &self,
        u: &url::Url,
        request: Option<&FetchRequest>,
        wait_for: Option<&str>,
        wait_ms: Option<u64>,
        max_bytes: u64,
    ) -> Result<Page, AppError> {
        let _permit = self.sem.acquire().await.map_err(|_| err("browser unavailable"))?;
        let (ws, _) = tokio_tungstenite::connect_async(self.ws_url().await?).await.map_err(|_| err("cannot connect to the browser"))?;
        let mut c = Cdp { ws, next: 0, site: site_host(u), request: request.cloned(), loaded: false, last_request: Instant::now() };
        let ctx = c.call("Target.createBrowserContext", json!({"disposeOnDetach": true}), None).await?;
        let ctx = ctx["browserContextId"].as_str().unwrap_or_default().to_string();
        let res = c.page(&ctx, u, wait_for, wait_ms, max_bytes).await;
        let _ = c.call("Target.disposeBrowserContext", json!({"browserContextId": ctx}), None).await;
        let _ = c.ws.close(None).await;
        res
    }
}

struct Cdp {
    ws: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    next: u64,
    site: String,
    request: Option<FetchRequest>,
    loaded: bool,
    last_request: Instant,
}

impl Cdp {
    async fn page(
        &mut self,
        ctx: &str,
        u: &url::Url,
        wait_for: Option<&str>,
        wait_ms: Option<u64>,
        max_bytes: u64,
    ) -> Result<Page, AppError> {
        let t = self.call("Target.createTarget", json!({"url": "about:blank", "browserContextId": ctx}), None).await?;
        let a = self.call("Target.attachToTarget", json!({"targetId": t["targetId"], "flatten": true}), None).await?;
        let s = a["sessionId"].as_str().unwrap_or_default().to_string();
        let s = Some(s.as_str());
        self.call("Fetch.enable", json!({"patterns": [{"urlPattern": "*"}]}), s).await?;
        self.call("Page.enable", json!({}), s).await?;
        let nav = self.call("Page.navigate", json!({"url": u.as_str()}), s).await?;
        if let Some(e) = nav["errorText"].as_str().filter(|e| !e.is_empty()) {
            return Err(err(format!("the page did not load ({e})")));
        }
        // Loaded, then no new request for a moment (or the time limit).
        let deadline = Instant::now() + PAGE_TIMEOUT;
        while Instant::now() < deadline && !(self.loaded && self.last_request.elapsed() >= IDLE) {
            self.pump(Duration::from_millis(100)).await?;
        }
        if let Some(sel) = wait_for {
            let deadline = Instant::now() + WAIT_FOR_TIMEOUT;
            let expr = format!("!!document.querySelector({})", Value::from(sel));
            while !self.eval(&expr, s).await?.as_bool().unwrap_or(false) {
                if Instant::now() >= deadline {
                    return Err(err(format!("`{sel}` did not appear on the page")));
                }
                self.pump(Duration::from_millis(200)).await?;
            }
        }
        if let Some(ms) = wait_ms {
            let until = Instant::now() + Duration::from_millis(ms);
            while Instant::now() < until {
                self.pump(until - Instant::now()).await?;
            }
        }
        let out = self.eval("JSON.stringify([location.href, document.documentElement.outerHTML])", s).await?;
        let (href, html): (String, String) = serde_json::from_str(out.as_str().unwrap_or("")).map_err(|_| err("cannot read the page"))?;
        if html.len() as u64 > max_bytes {
            return Err(AppError::new(413, "input_too_large", "the rendered page exceeds DOCVISION_MAX_INPUT_BYTES").stage("fetch"));
        }
        Ok(Page { html: format!("<!DOCTYPE html>\n{html}"), url: url::Url::parse(&href).unwrap_or_else(|_| u.clone()) })
    }

    async fn eval(&mut self, expr: &str, s: Option<&str>) -> Result<Value, AppError> {
        let r = self.call("Runtime.evaluate", json!({"expression": expr, "returnByValue": true}), s).await?;
        Ok(r["result"]["value"].clone())
    }

    async fn send(&mut self, method: &str, params: Value, session: Option<&str>) -> Result<u64, AppError> {
        self.next += 1;
        let mut m = json!({"id": self.next, "method": method, "params": params});
        if let Some(s) = session {
            m["sessionId"] = s.into();
        }
        self.ws.send(Message::text(m.to_string())).await.map_err(|_| err("the browser connection closed"))?;
        Ok(self.next)
    }

    /// Send a command and wait for its answer, handling events that arrive meanwhile.
    async fn call(&mut self, method: &str, params: Value, session: Option<&str>) -> Result<Value, AppError> {
        let id = self.send(method, params, session).await?;
        let deadline = Instant::now() + PAGE_TIMEOUT;
        loop {
            let m = self.recv(deadline).await?.ok_or_else(|| err(format!("the browser did not answer {method}")))?;
            if m["id"].as_u64() == Some(id) {
                return match m.get("error") {
                    Some(e) => Err(err(format!("{method}: {}", e["message"].as_str().unwrap_or("failed")))),
                    None => Ok(m["result"].clone()),
                };
            }
            self.event(&m).await?;
        }
    }

    /// Handle events for up to `d`.
    async fn pump(&mut self, d: Duration) -> Result<(), AppError> {
        let until = Instant::now() + d;
        while let Some(m) = self.recv(until).await? {
            self.event(&m).await?;
        }
        Ok(())
    }

    async fn recv(&mut self, until: Instant) -> Result<Option<Value>, AppError> {
        loop {
            let msg = match tokio::time::timeout_at(until, self.ws.next()).await {
                Err(_) => return Ok(None),
                Ok(None) | Ok(Some(Err(_))) => return Err(err("the browser connection closed")),
                Ok(Some(Ok(m))) => m,
            };
            if let Message::Text(t) = msg {
                return Ok(serde_json::from_str(t.as_str()).ok());
            }
        }
    }

    async fn event(&mut self, m: &Value) -> Result<(), AppError> {
        let session = m["sessionId"].as_str().map(str::to_string);
        match m["method"].as_str() {
            Some("Page.loadEventFired") => self.loaded = true,
            Some("Fetch.requestPaused") => {
                self.last_request = Instant::now();
                let p = &m["params"];
                let id = p["requestId"].clone();
                let (method, params) = self.decide(p);
                let params = if params.is_null() { json!({"requestId": id}) } else { merge(params, json!({"requestId": id})) };
                // Answers to these come back later and are ignored.
                self.send(method, params, session.as_deref()).await?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Let a paused request through (with `payload.request` for the source's site) or refuse it.
    fn decide(&self, p: &Value) -> (&'static str, Value) {
        let refuse = json!({"errorReason": "BlockedByClient"});
        if matches!(p["resourceType"].as_str(), Some("Image" | "Media" | "Font")) {
            return ("Fetch.failRequest", refuse);
        }
        let Some(u) = p["request"]["url"].as_str().and_then(|u| url::Url::parse(u).ok()) else { return ("Fetch.failRequest", refuse) };
        match u.scheme() {
            "http" | "https" if blocked_host(&u) => return ("Fetch.failRequest", refuse),
            "http" | "https" => {}
            "data" | "blob" => return ("Fetch.continueRequest", Value::Null),
            _ => return ("Fetch.failRequest", refuse),
        }
        let Some(r) = self.request.as_ref().filter(|_| site_host(&u) == self.site) else { return ("Fetch.continueRequest", Value::Null) };
        let mut headers: Vec<Value> = p["request"]["headers"]
            .as_object()
            .map(|h| h.iter().map(|(k, v)| json!({"name": k, "value": v})).collect())
            .unwrap_or_default();
        for (k, v) in r.headers_for(&u, &self.site) {
            headers.retain(|h| !h["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(&k)));
            headers.push(json!({"name": k, "value": v}));
        }
        ("Fetch.continueRequest", json!({"url": r.apply_query(&u).as_str(), "headers": headers}))
    }
}

fn merge(mut a: Value, b: Value) -> Value {
    if let (Some(a), Value::Object(b)) = (a.as_object_mut(), b) {
        a.extend(b);
    }
    a
}

/// Cloud metadata and link-local addresses are never fetched from a page.
fn blocked_host(u: &url::Url) -> bool {
    match u.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_link_local() || ip.is_unspecified(),
        Some(url::Host::Ipv6(ip)) => ip.is_unspecified() || (ip.segments()[0] & 0xffc0) == 0xfe80,
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("metadata.google.internal") || d.eq_ignore_ascii_case("metadata"),
        None => true,
    }
}

/// Whether an HTML page looks empty without JavaScript: under 200 visible characters outside
/// `<script>`, `<style>`, `<noscript>` and `<template>`.
pub fn looks_empty(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    let (mut i, mut visible) = (0, 0usize);
    let b = lower.as_bytes();
    while i < b.len() {
        if b[i] == b'<' {
            let skip = ["script", "style", "noscript", "template"].iter().find(|t| lower[i + 1..].starts_with(*t));
            let end = match skip {
                Some(t) => lower[i..].find(&format!("</{t}")).map(|e| i + e),
                None => Some(i),
            };
            i = end.and_then(|e| lower[e..].find('>').map(|g| e + g + 1)).unwrap_or(b.len());
        } else {
            if !b[i].is_ascii_whitespace() {
                visible += 1;
                if visible >= 200 {
                    return false;
                }
            }
            i += 1;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_pages() {
        assert!(looks_empty("<html><body><div id=\"root\"></div><script>var x = 'lots of code here'.repeat(100)</script></body></html>"));
        assert!(!looks_empty(&format!("<html><body><p>{}</p></body></html>", "Real words here. ".repeat(20))));
        assert!(blocked_host(&url::Url::parse("http://169.254.169.254/latest").unwrap()));
        assert!(!blocked_host(&url::Url::parse("https://example.com/").unwrap()));
    }
}
