//! `crawl` and `site.*`: a small local site, crawled into a local crawl store.

#![cfg(feature = "s3")]

mod common;

use common::*;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

/// (path, query, headers) of every request the site received.
type Seen = Arc<Mutex<Vec<(String, String, axum::http::HeaderMap)>>>;

fn page(title: &str, links: &[&str]) -> String {
    let a: String = links.iter().map(|l| format!("<a href=\"{l}\">{l}</a> ")).collect();
    format!(
        "<!DOCTYPE html><html><head><title>{title}</title></head><body><h1>{title}</h1><p>{}</p><p>{a}</p></body></html>",
        format!("This page is about {title}. ").repeat(20)
    )
}

/// `/` -> `/a`, `/b`, `/private/x` (robots.txt), another host, an image; `/a` -> `/c`;
/// `/moved` redirects to the same server under another host name.
async fn site() -> (String, Seen) {
    use axum::extract::{Request, State};
    use axum::response::IntoResponse;
    let seen: Seen = Default::default();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let app = axum::Router::new()
        .fallback(move |State(seen): State<Seen>, req: Request| async move {
            let (path, query) = (req.uri().path().to_string(), req.uri().query().unwrap_or("").to_string());
            seen.lock().unwrap().push((path.clone(), query, req.headers().clone()));
            let html = |b: String| ([("content-type", "text/html")], b).into_response();
            match path.as_str() {
                "/" => html(page("Home", &["/a", "/b#top", "/private/x", "https://other.example/x", "/logo.png", "mailto:x@y.z"])),
                "/a" => html(page("Alpha", &["/c", "/"])),
                "/b" => html(page("Beta", &["/a"])),
                "/c" => html(page("Gamma", &["/d"])),
                "/d" => html(page("Delta", &[])),
                "/moved" => ([("location", format!("http://localhost:{port}/landing"))], axum::http::StatusCode::FOUND).into_response(),
                "/landing" => html(page("Landing", &[])),
                "/robots.txt" => "User-agent: *\nDisallow: /private\n".into_response(),
                _ => axum::http::StatusCode::NOT_FOUND.into_response(),
            }
        })
        .with_state(seen.clone());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://127.0.0.1:{port}"), seen)
}

fn local() -> Value {
    json!({"title_method": "local", "summary_method": "local", "detect_language": false, "gen_search": false})
}

async fn crawl_server() -> (TestServer, std::path::PathBuf) {
    let store = temp_dir("store");
    let st = store.clone();
    let s = server_with(None, None, move |v| {
        v.insert("DOCVISION_CRAWL_STORE".into(), st.to_str().unwrap().into());
    })
    .await;
    (s, store)
}

fn lines(b: &[u8]) -> Vec<Value> {
    b.split(|c| *c == b'\n').filter(|l| !l.is_empty()).map(|l| serde_json::from_slice(l).unwrap()).collect()
}

#[tokio::test]
async fn crawls_a_site_into_jsonl() {
    let (base, seen) = site().await;
    let (s, store) = crawl_server().await;
    let mut o = local();
    o["max_depth"] = json!(2);
    let payload = json!({
        "source": format!("{base}/"),
        "metadata": {"customer": "acme"},
        "request": {"headers": {"X-Key": "secret"}, "query": {"token": "t1"}},
    });
    let (st, v) = s.rpc("crawl", payload.clone(), o.clone()).await;
    assert_eq!(st, 202, "{v}");
    assert_eq!(v["data"]["site"], "127.0.0.1");
    let job_id = v["data"]["job_id"].as_str().unwrap().to_string();
    let j = s.wait_job(&job_id).await;
    assert_eq!(j["status"], "completed", "{j}");
    let r = &j["result"];
    assert_eq!(
        (r["operation"].as_str(), r["pages"]["converted"].as_u64(), r["pages"]["failed"].as_u64()),
        (Some("crawl"), Some(4), Some(0)),
        "{r}"
    );

    // Depth 2: /, /a and /b, then /c; /d (depth 3), robots.txt-disallowed, other hosts and images are not fetched.
    let paths: Vec<String> = seen.lock().unwrap().iter().map(|(p, _, _)| p.clone()).collect();
    for p in ["/", "/a", "/b", "/c", "/robots.txt"] {
        assert!(paths.contains(&p.to_string()), "{p} in {paths:?}");
    }
    assert!(!paths.iter().any(|p| p == "/d" || p.starts_with("/private") || p == "/logo.png"), "{paths:?}");
    assert_eq!(paths.iter().filter(|p| *p == "/a").count(), 1, "each page once");
    // The request's header and query parameter reached every page of the site.
    for (p, q, h) in seen.lock().unwrap().iter().filter(|(p, _, _)| p != "/robots.txt") {
        assert_eq!(h.get("x-key").map(|v| v.to_str().unwrap()), Some("secret"), "{p}");
        assert!(q.contains("token=t1"), "{p}?{q}");
    }

    // The site's rows: one per page, same fields as convert, secrets left out.
    let site_file = std::fs::read(store.join("sites/127.0.0.1/pages.jsonl")).unwrap();
    let rows = lines(&site_file);
    assert_eq!(rows.len(), 4);
    let a = rows.iter().find(|r| r["url"] == format!("{base}/a")).unwrap();
    assert_eq!(
        (a["operation"].as_str(), a["job_id"].as_str(), a["site"].as_str()),
        (Some("crawl"), Some(job_id.as_str()), Some("127.0.0.1"))
    );
    assert_eq!((a["status"].as_str(), a["title"].as_str()), (Some("completed"), Some("Alpha")));
    assert!(a["content"].as_str().unwrap().contains("This page is about Alpha"));
    assert_eq!(a["metadata"]["customer"], "acme");
    assert!(a["content_hash"].as_str().unwrap().starts_with("b3:") && a["fetched_at"].is_string() && a["converted_at"].is_string());
    assert_eq!(a["llm"]["totals"]["calls"], 0);
    assert!(a.get("chunks").is_some() && a.get("search").is_some(), "every row has the same keys");
    assert!(!String::from_utf8_lossy(&site_file).contains("t1") && !String::from_utf8_lossy(&site_file).contains("secret"));
    assert!(store.join("sites/127.0.0.1/pages/a.jsonl").exists() && store.join("sites/127.0.0.1/pages/_root.jsonl").exists());
    assert_eq!(
        std::fs::read_to_string(store.join("sites/127.0.0.1/pages/a.md")).unwrap(),
        a["content"].as_str().unwrap(),
        "each page's Markdown"
    );
    let run_file = std::fs::read(store.join(format!("sites/127.0.0.1/runs/{job_id}/pages.jsonl"))).unwrap();
    assert_eq!(lines(&run_file).len(), 4);
    assert!(r["site_pages_file"].as_str().unwrap().ends_with("/sites/127.0.0.1/pages.jsonl"), "{r}");
    // The run's rows are also served by the API.
    let got = s.client.get(format!("{}/_/jobs/{job_id}/pages.jsonl", s.url)).header("x-access-token", TOKEN).send().await.unwrap();
    assert_eq!(lines(&got.bytes().await.unwrap()).len(), 4);

    // site.*
    let (_, v) = s.rpc("site.list", json!({}), json!({})).await;
    let site = &v["data"]["sites"][0];
    assert_eq!(
        (site["site"].as_str(), site["pages"].as_u64(), site["last_status"].as_str()),
        (Some("127.0.0.1"), Some(4), Some("completed")),
        "{v}"
    );
    let (_, v) = s.rpc("site.pages", json!({"site": "127.0.0.1", "limit": 3}), json!({})).await;
    assert_eq!(v["data"]["pages"].as_array().unwrap().len(), 3, "{v}");
    let (_, v2) = s.rpc("site.pages", json!({"site": "127.0.0.1", "cursor": v["data"]["next_cursor"]}), json!({})).await;
    assert_eq!(v2["data"]["pages"].as_array().unwrap().len(), 1, "{v2}");
    let (st, v) = s.rpc("site.page", json!({"site": "127.0.0.1", "url": format!("{base}/a")}), json!({})).await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["data"]["row"]["title"], "Alpha");

    // A second run converts nothing that didn't change.
    let (_, v) = s.rpc("crawl", payload, o).await;
    let j = s.wait_job(v["data"]["job_id"].as_str().unwrap()).await;
    assert_eq!((j["result"]["pages"]["converted"].as_u64(), j["result"]["pages"]["unchanged"].as_u64()), (Some(0), Some(4)), "{j}");
    assert_eq!(lines(&std::fs::read(store.join("sites/127.0.0.1/pages.jsonl")).unwrap()).len(), 4, "the site still has its pages");

    // The index can be rebuilt from the store.
    let (st, v) = s.rpc("site.reindex", json!({"site": "127.0.0.1"}), json!({})).await;
    assert_eq!((st, v["data"]["pages"].as_u64()), (200, Some(4)), "{v}");

    // A crawl has no zip.
    let (st, _) = s.rpc("job.zip", json!({"job_id": job_id}), json!({})).await;
    assert_eq!(st, 400);
    // A finished crawl can't be cancelled.
    let (st, v) = s.rpc("job.cancel", json!({"job_id": job_id}), json!({})).await;
    assert_eq!((st, v["error"]["code"].as_str()), (409, Some("job_finished")));
}

#[tokio::test]
async fn limits_and_errors() {
    let (base, _) = site().await;
    let (s, _) = crawl_server().await;
    let mut o = local();
    o["max_pages"] = json!(2);
    o["use_sitemap"] = json!(false);
    let (_, v) = s.rpc("crawl", json!({"source": format!("{base}/"), "site": "small"}), o).await;
    let j = s.wait_job(v["data"]["job_id"].as_str().unwrap()).await;
    assert_eq!(j["status"], "limit_reached", "{j}");
    assert_eq!(j["result"]["pages"]["converted"], 2);

    for (payload, opts, code) in [
        (json!({"source": format!("{base}/")}), json!({"execution": "sync"}), "invalid_options"),
        (json!({"source": "/local/file.pdf"}), json!({}), "invalid_source"),
        (json!({"source": format!("{base}/"), "site": "bad/name"}), json!({}), "invalid_payload"),
        (json!({"source": format!("{base}/"), "request": {"headers": {"Host": "x"}}}), json!({}), "invalid_request"),
    ] {
        let (st, v) = s.rpc("crawl", payload, opts).await;
        assert_eq!((st, v["error"]["code"].as_str()), (400, Some(code)), "{v}");
    }
    // Crawl options belong to crawl; request belongs to http(s) sources.
    let p = s.write("a.md", b"# A\n");
    let (st, v) = s.convert_sync(&p, json!({"max_pages": 3})).await;
    assert_eq!((st, v["error"]["code"].as_str()), (400, Some("invalid_options")));
    let (st, v) = s.rpc("convert", json!({"source": p, "request": {"headers": {"X": "1"}}}), json!({})).await;
    assert_eq!((st, v["error"]["code"].as_str()), (400, Some("invalid_request")));
    // render=on needs the browser container.
    let (st, v) = s.rpc("convert", json!({"source": format!("{base}/")}), json!({"render": "on"})).await;
    assert_eq!((st, v["error"]["code"].as_str()), (400, Some("feature_not_configured")));
}

#[tokio::test]
async fn no_store_no_crawl() {
    let s = server().await;
    let (st, v) = s.rpc("crawl", json!({"source": "https://example.com/"}), json!({})).await;
    assert_eq!((st, v["error"]["code"].as_str()), (400, Some("feature_not_configured")), "{v}");
}

#[tokio::test]
async fn request_headers_stay_on_the_source_host() {
    let (base, seen) = site().await;
    let s = server().await;
    let opts = local();
    let (st, v) = s
        .rpc(
            "convert",
            json!({"source": format!("{base}/moved"), "request": {"headers": {"Authorization": "Bearer t"}, "cookies": {"s": "1"}}}),
            opts,
        )
        .await;
    assert_eq!(st, 202, "{v}");
    let j = s.wait_job(v["data"]["job_id"].as_str().unwrap()).await;
    assert_eq!(j["status"], "completed", "{j}");
    assert_eq!(j["result"]["title"], "Landing");
    let seen = seen.lock().unwrap();
    let moved = &seen.iter().find(|(p, _, _)| p == "/moved").unwrap().2;
    let landing = &seen.iter().find(|(p, _, _)| p == "/landing").unwrap().2;
    assert_eq!(moved.get("authorization").unwrap(), "Bearer t");
    assert_eq!(moved.get("cookie").unwrap(), "s=1");
    assert!(landing.get("authorization").is_none() && landing.get("cookie").is_none(), "a redirect to another host drops them");
    assert!(landing.get("user-agent").unwrap().to_str().unwrap().starts_with("docvision/"));
}

/// Needs a real browser: `DOCVISION_TEST_BROWSER_URL=http://127.0.0.1:9222` (e.g. the
/// `chromedp/headless-shell` container, or Chrome started with `--remote-debugging-port=9222`).
#[tokio::test]
async fn renders_javascript_pages() {
    let Ok(browser) = std::env::var("DOCVISION_TEST_BROWSER_URL") else { return };
    use axum::response::IntoResponse;
    let seen: Seen = Default::default();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    let app = axum::Router::new()
        .fallback(move |axum::extract::State(seen): axum::extract::State<Seen>, req: axum::extract::Request| async move {
            seen.lock().unwrap().push((req.uri().path().to_string(), String::new(), req.headers().clone()));
            let body = match req.uri().path() {
                // An empty shell filled in by JavaScript, after a fetch.
                "/" => r#"<!DOCTYPE html><html><head><title>App</title></head><body><div id="root"></div>
                    <script>fetch('/data').then(r => r.text()).then(t => {
                      document.getElementById('root').innerHTML = '<h1>Rendered</h1><p>' + t + '</p><a href="/next">next</a>';
                    });</script></body></html>"#
                    .to_string(),
                "/data" => "Loaded by JavaScript. ".repeat(20),
                _ => "<html><body><p>next</p></body></html>".to_string(),
            };
            ([("content-type", "text/html")], body).into_response()
        })
        .with_state(seen.clone());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let s = server_with(None, None, move |v| {
        v.insert("DOCVISION_BROWSER_URL".into(), browser);
    })
    .await;
    let opts = local();
    let (st, v) = s.rpc("convert", json!({"source": format!("{base}/"), "request": {"headers": {"X-Key": "k"}}}), opts.clone()).await;
    assert_eq!(st, 202, "{v}");
    let j = s.wait_job(v["data"]["job_id"].as_str().unwrap()).await;
    assert_eq!(j["status"], "completed", "{j}");
    let c = j["result"]["content"].as_str().unwrap();
    assert!(c.contains("# Rendered") && c.contains("Loaded by JavaScript."), "render=auto ran the page's JavaScript: {c}");
    // The page's own request (from the browser) got the request header too.
    let data = seen.lock().unwrap().iter().filter(|(p, _, _)| p == "/data").map(|(_, _, h)| h.get("x-key").cloned()).collect::<Vec<_>>();
    assert_eq!(data.len(), 1, "{data:?}");
    assert_eq!(data[0].as_ref().unwrap(), "k");
    // render=off keeps the empty shell.
    let mut off = opts.clone();
    off["render"] = json!("off");
    off["cache"] = json!("bypass");
    let (_, v) = s.convert_sync_url(&format!("{base}/"), off).await;
    assert!(!v["data"]["content"].as_str().unwrap_or("").contains("Loaded by JavaScript"), "{v}");
}
