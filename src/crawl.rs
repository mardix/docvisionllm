//! `crawl`: follow a site's links and convert every page like `convert` (same options, same
//! fields), saving one JSON Lines row per page in the crawl store (`DOCVISION_CRAWL_STORE`),
//! organized by site:
//!
//! ```text
//! sites/<site>/pages.jsonl                 the site now: the latest row of every page
//! sites/<site>/pages/<key>.jsonl           each page's row
//! sites/<site>/images/<file>               extracted images (rows link to them as images/<file>)
//! sites/<site>/runs/<job_id>/pages.jsonl   the rows one run produced
//! ```
//!
//! The database keeps an index of the pages for the `site.*` operations; `site.reindex`
//! rebuilds it from the store. A run is a job (`job.get`, `job.wait`, `job.cancel`).

use crate::App;
use crate::db::{CrawlPageRow, JobRow, RequestFinish, RequestRow, WriteOp, now_ms, rfc3339};
use crate::gateway::Prio;
use crate::jobs::{self, JobSpec, StoredJob};
use crate::llm::{Accounting, CallRecord, LlmTotals};
use crate::result::{self, Body, BodySrc};
use crate::rpc::{AppError, CacheMode, ConvertPayload, Execution, Options, ReqCtx, requester_id, typed};
use crate::source::{self, FetchRequest, Source, site_host};
use dashmap::DashMap;
use object_store::{ObjectStore, PutPayload};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use url::Url;

const DEFAULT_MAX_PAGES: u32 = 100;
const DEFAULT_MAX_DEPTH: u32 = 3;
/// Pages fetched at once per crawl.
const PER_SITE: usize = 2;

// ---------------------------------------------------------------- running crawls

/// Progress of a running crawl.
#[derive(Default)]
pub struct Run {
    pub site: String,
    done: AtomicU64,
    failed: AtomicU64,
    unchanged: AtomicU64,
    queued: AtomicU64,
    cancel: AtomicBool,
    calls: Mutex<Vec<CallRecord>>,
}

impl Run {
    fn pages(&self) -> Value {
        json!({
            "converted": self.done.load(Relaxed),
            "unchanged": self.unchanged.load(Relaxed),
            "failed": self.failed.load(Relaxed),
            "queued": self.queued.load(Relaxed),
        })
    }
}

#[derive(Default)]
pub struct Crawls {
    /// site -> job_id of its running crawl (one per site).
    by_site: DashMap<String, String>,
    by_job: DashMap<String, Arc<Run>>,
}

impl Crawls {
    /// `{site, pages}` of a running crawl.
    pub fn progress(&self, job_id: &str) -> Option<Value> {
        self.by_job.get(job_id).map(|r| json!({"site": r.site, "pages": r.pages()}))
    }
    /// Ask a running crawl to stop; pages already saved stay.
    pub fn cancel(&self, job_id: &str) -> bool {
        self.by_job.get(job_id).map(|r| r.cancel.store(true, Relaxed)).is_some()
    }
}

// ---------------------------------------------------------------- the crawl store

pub struct Store {
    os: Arc<dyn ObjectStore>,
    prefix: String,
    /// How paths are shown: `s3://bucket/prefix` (no credentials) or a local folder, and an
    /// S3 `?region=` to keep.
    shown: String,
    query: String,
}

fn store_err(what: &str) -> AppError {
    AppError::new(502, "crawl_store_failed", format!("cannot {what} the crawl store")).stage("storage")
}

impl Store {
    pub fn open(cfg: &str) -> Result<Store, AppError> {
        let bad = |m: String| AppError::new(500, "crawl_store_invalid", m);
        if cfg.starts_with('/') {
            std::fs::create_dir_all(cfg).map_err(|e| bad(format!("DOCVISION_CRAWL_STORE: {}", e.kind())))?;
            let os = object_store::local::LocalFileSystem::new_with_prefix(cfg).map_err(|e| bad(e.to_string()))?;
            return Ok(Store {
                os: Arc::new(os),
                prefix: String::new(),
                shown: cfg.trim_end_matches('/').to_string(),
                query: String::new(),
            });
        }
        let (base, q) = cfg.split_once('?').map(|(a, b)| (a, format!("?{b}"))).unwrap_or((cfg, String::new()));
        let loc = source::parse_s3(&format!("{}/.keep{q}", base.trim_end_matches('/')))
            .map_err(|e| bad(format!("DOCVISION_CRAWL_STORE: {e}")))?;
        let prefix = loc.key.strip_suffix(".keep").unwrap_or("").to_string();
        let (os, _) = result::s3_store(&loc).map_err(bad)?;
        let shown = source::sanitize(base).trim_end_matches('/').to_string();
        let query = loc.region.as_ref().map(|r| format!("?region={r}")).unwrap_or_default();
        Ok(Store { os: Arc::new(os), prefix, shown, query })
    }

    fn path(&self, rel: &str) -> object_store::path::Path {
        object_store::path::Path::from(format!("{}{rel}", self.prefix))
    }

    pub fn display(&self, rel: &str) -> String {
        format!("{}/{rel}{}", self.shown, self.query)
    }

    async fn put(&self, rel: &str, data: impl Into<PutPayload>) -> Result<(), AppError> {
        self.os.put(&self.path(rel), data.into()).await.map(|_| ()).map_err(|_| store_err("write to"))
    }

    async fn get(&self, rel: &str) -> Result<Option<bytes::Bytes>, AppError> {
        match self.os.get(&self.path(rel)).await {
            Ok(r) => r.bytes().await.map(Some).map_err(|_| store_err("read from")),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(_) => Err(store_err("read from")),
        }
    }

    /// Paths (relative to the store) of the objects under `rel`.
    async fn list(&self, rel: &str) -> Result<Vec<String>, AppError> {
        use futures_util::TryStreamExt;
        let items: Vec<_> = self.os.list(Some(&self.path(rel))).try_collect().await.map_err(|_| store_err("list"))?;
        let skip = self.path("").as_ref().len();
        Ok(items.into_iter().map(|m| m.location.as_ref()[skip..].trim_start_matches('/').to_string()).collect())
    }
}

fn store(app: &App) -> Result<Store, AppError> {
    let cfg = app
        .cfg
        .crawl_store
        .as_deref()
        .ok_or_else(|| AppError::bad_request("feature_not_configured", "crawling needs a crawl store (DOCVISION_CRAWL_STORE)"))?;
    Store::open(cfg)
}

fn page_rel(site: &str, key: &str) -> String {
    format!("sites/{site}/pages/{key}.jsonl")
}

/// A readable, stable object name for a page: its path, cleaned, with a short hash of the
/// whole URL when it has a query string or the path had to be changed.
fn page_key(u: &Url) -> String {
    let path = u.path().trim_matches('/');
    let joined = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|seg| {
            let s: String = seg.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' }).collect();
            if s == "." || s == ".." { "_".into() } else { s }
        })
        .collect::<Vec<String>>()
        .join("/");
    let changed = joined != path;
    let mut key = if joined.is_empty() { "_root".to_string() } else { joined };
    if u.query().is_some() || changed || key.len() > 200 {
        key.truncate(200);
        key = format!("{key}~{}", &blake3::hash(u.as_str().as_bytes()).to_hex()[..8]);
    }
    key
}

// ---------------------------------------------------------------- links, scope, robots.txt

/// Drop the fragment and tracking parameters; only http(s).
fn normalize(mut u: Url) -> Option<Url> {
    if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none() {
        return None;
    }
    u.set_fragment(None);
    if u.query().is_some() {
        let keep: Vec<(String, String)> = u
            .query_pairs()
            .filter(|(k, _)| !(k.starts_with("utm_") || matches!(k.as_ref(), "gclid" | "fbclid" | "mc_cid" | "mc_eid")))
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        u.set_query(None);
        if !keep.is_empty() {
            u.query_pairs_mut().extend_pairs(keep);
        }
    }
    Some(u)
}

/// Links of an HTML page, resolved against `base`.
fn links(html: &str, base: &Url) -> Vec<Url> {
    let Ok(dom) = tl::parse(html, tl::ParserOptions::default()) else { return Vec::new() };
    dom.nodes()
        .iter()
        .filter_map(|n| n.as_tag())
        .filter(|t| t.name().as_utf8_str().eq_ignore_ascii_case("a"))
        .filter_map(|t| t.attributes().get("href").flatten().map(|h| crate::extract::html::decode_entities(h.as_utf8_str().trim())))
        .filter(|h| !h.starts_with('#') && !h.starts_with("mailto:") && !h.starts_with("javascript:"))
        .filter_map(|h| base.join(&h).ok())
        .filter_map(normalize)
        .collect()
}

/// `*` matches anything; `prefix`: the pattern only has to match the start (robots.txt rules).
fn glob(pat: &str, s: &str, prefix: bool) -> bool {
    let (p, t) = (pat.as_bytes(), s.as_bytes());
    let (mut i, mut j, mut star, mut mark) = (0, 0, None, 0);
    while j < t.len() {
        if i < p.len() && p[i] == b'*' {
            star = Some(i);
            mark = j;
            i += 1;
        } else if i < p.len() && p[i] == t[j] {
            i += 1;
            j += 1;
        } else if i == p.len() && prefix {
            return true;
        } else if let Some(si) = star {
            i = si + 1;
            mark += 1;
            j = mark;
        } else {
            return false;
        }
    }
    p[i..].iter().all(|c| *c == b'*')
}

#[derive(Default)]
struct Robots {
    /// (allow, pattern)
    rules: Vec<(bool, String)>,
    delay: Option<Duration>,
    sitemaps: Vec<String>,
}

impl Robots {
    /// The groups for `*` and for docvision apply.
    fn parse(txt: &str) -> Robots {
        let mut r = Robots::default();
        let (mut agents, mut in_rules) = (Vec::<String>::new(), false);
        for line in txt.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((k, v)) = line.split_once(':') else { continue };
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            let applies = |a: &[String]| a.iter().any(|a| a == "*" || a.contains("docvision"));
            match k.as_str() {
                "user-agent" => {
                    if in_rules {
                        agents.clear();
                        in_rules = false;
                    }
                    agents.push(v.to_ascii_lowercase());
                }
                "allow" | "disallow" => {
                    in_rules = true;
                    if applies(&agents) && !v.is_empty() {
                        r.rules.push((k == "allow", v.to_string()));
                    }
                }
                "crawl-delay" => {
                    if applies(&agents) {
                        r.delay = v.parse::<f64>().ok().map(|s| Duration::from_secs_f64(s.clamp(0.0, 30.0)));
                    }
                }
                "sitemap" => r.sitemaps.push(v.to_string()),
                _ => {}
            }
        }
        r
    }

    /// The longest matching rule wins; `allow` wins a tie.
    fn allowed(&self, u: &Url) -> bool {
        let target = match u.query() {
            Some(q) => format!("{}?{q}", u.path()),
            None => u.path().to_string(),
        };
        let mut best: Option<(usize, bool)> = None;
        for (allow, pat) in &self.rules {
            let hit = match pat.strip_suffix('$') {
                Some(p) => glob(p, &target, false),
                None => glob(pat, &target, true),
            };
            if hit && best.is_none_or(|(len, a)| pat.len() > len || (pat.len() == len && *allow && !a)) {
                best = Some((pat.len(), *allow));
            }
        }
        best.is_none_or(|(_, a)| a)
    }
}

/// Up to 512 KiB of a text resource (robots.txt, sitemaps), or nothing.
async fn fetch_text(u: &Url, request: Option<&FetchRequest>, cap: usize) -> Option<String> {
    let (resp, _) = source::get(u, request).await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let b = resp.bytes().await.ok()?;
    Some(String::from_utf8_lossy(&b[..b.len().min(cap)]).into_owned())
}

/// Page URLs from the site's sitemaps (sitemap indexes followed, at most 20 sitemaps).
async fn sitemap_urls(start: &Url, robots: &Robots, request: Option<&FetchRequest>, limit: usize) -> Vec<Url> {
    let mut todo: VecDeque<Url> = robots.sitemaps.iter().filter_map(|s| Url::parse(s).ok()).collect();
    if todo.is_empty()
        && let Ok(u) = start.join("/sitemap.xml")
    {
        todo.push_back(u);
    }
    let (mut out, mut fetched) = (Vec::new(), 0);
    while let Some(sm) = todo.pop_front() {
        if fetched >= 20 || out.len() >= limit || site_host(&sm) != site_host(start) {
            continue;
        }
        fetched += 1;
        let Some(xml) = fetch_text(&sm, request, 10 << 20).await else { continue };
        let index = xml.contains("<sitemapindex");
        for loc in xml.split("<loc>").skip(1).filter_map(|s| s.split("</loc>").next()) {
            let loc = crate::extract::html::decode_entities(loc.trim());
            match Url::parse(&loc).ok().and_then(normalize) {
                Some(u) if index => todo.push_back(u),
                Some(u) if out.len() < limit => out.push(u),
                _ => {}
            }
        }
    }
    out
}

// ---------------------------------------------------------------- the crawl operation

pub async fn crawl(app: &Arc<App>, ctx: &ReqCtx, payload: Value, options: Value) -> Result<axum::response::Response, AppError> {
    let payload: ConvertPayload = typed(payload, "payload")?;
    let opts: Options = typed(options, "options")?;
    opts.validate()?;
    if opts.execution == Execution::Sync {
        return Err(AppError::bad_request("invalid_options", "crawl always runs in the background (execution must be async)"));
    }
    if opts.wants_zip() || payload.destination.is_some() {
        return Err(AppError::bad_request(
            "invalid_options",
            "a crawl is saved in the crawl store; destination, create_zip and zip_only are for convert",
        ));
    }
    jobs::check_fetch(app, &payload, &opts)?;
    if payload.webhooks().len() > crate::webhook::MAX_WEBHOOKS {
        return Err(AppError::bad_request("invalid_webhook", format!("at most {} webhooks", crate::webhook::MAX_WEBHOOKS)));
    }
    for w in payload.webhooks() {
        crate::webhook::validate(w)?;
    }
    requester_id(payload.requester_id.as_deref())?;
    let start = match source::parse(&payload.source)? {
        Source::Https(u) => normalize(u).ok_or_else(|| AppError::bad_request("invalid_source", "crawl needs an http(s) start URL"))?,
        _ => return Err(AppError::bad_request("invalid_source", "crawl needs an http(s) start URL")),
    };
    let site = payload.site.clone().unwrap_or_else(|| site_host(&start)).to_ascii_lowercase();
    if site.is_empty()
        || site.len() > 100
        || !site.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        || site.starts_with('.')
    {
        return Err(AppError::bad_request("invalid_payload", "site must be 1-100 letters, digits, `.`, `-` or `_`"));
    }
    let store = store(app)?;
    jobs::endpoint(app, &opts, opts.extract_schema.is_some())?;
    if app.crawls.by_job.len() >= app.cfg.max_crawls {
        return Err(AppError::new(429, "too_many_crawls", "the maximum number of crawls is running; retry later").retry_after(60));
    }
    let job_id = uuid::Uuid::now_v7().to_string();
    match app.crawls.by_site.entry(site.clone()) {
        dashmap::Entry::Occupied(e) => {
            let mut err = AppError::new(409, "crawl_running", format!("site `{site}` is already being crawled"));
            err.data = serde_json::to_vec(&json!({"job_id": e.get()})).ok();
            return Err(err);
        }
        dashmap::Entry::Vacant(e) => {
            e.insert(job_id.clone());
        }
    }
    let run = Arc::new(Run { site: site.clone(), ..Default::default() });
    app.crawls.by_job.insert(job_id.clone(), run.clone());
    let stored = StoredJob {
        payload: payload.clone(),
        options: Options { llm_api_key: None, ..opts.clone() },
        api_key: opts.llm_api_key.as_ref().map(|k| k.expose().clone()),
    };
    let sealed = jobs::seal(app.cfg.encryption_key.expose(), &serde_json::to_vec(&stored).map_err(|_| AppError::internal("encode job"))?);
    let ops = vec![
        WriteOp::InsertRequest(RequestRow {
            request_id: ctx.request_id.clone(),
            operation: "crawl".into(),
            mode: Some("async".into()),
            source: Some(source::sanitize(&payload.source)),
            options: Some(opts.sanitized().to_string()),
            metadata: payload.metadata.as_ref().map(|m| Value::Object(m.clone()).to_string()),
            request_status: "accepted".into(),
            execution_status: Some("running".into()),
            job_id: Some(job_id.clone()),
            http_status: Some(202),
            created_at: ctx.created_at,
            requester_id: requester_id(payload.requester_id.as_deref()).ok(),
        }),
        WriteOp::InsertJob(JobRow {
            job_id: job_id.clone(),
            request_id: ctx.request_id.clone(),
            state: "running".into(),
            priority: "low".into(),
            created_at: ctx.created_at,
            started_at: Some(now_ms()),
            finished_at: None,
            result_path: None,
            error: None,
            secrets: Some(sealed),
            cache_key: None,
            has_webhook: !payload.webhooks().is_empty(),
        }),
        WriteOp::CrawlSite { site: site.clone(), at: now_ms(), job_id: job_id.clone(), status: "running".into() },
    ];
    if !app.writer.send_durable(ops).await {
        app.crawls.by_site.remove(&site);
        app.crawls.by_job.remove(&job_id);
        return Err(AppError::new(503, "db_unavailable", "the crawl could not be durably recorded; retry later").retry_after(5));
    }
    app.jobs.register(&job_id);
    app.jobs.set(&job_id, "running");
    let spec = Arc::new(JobSpec {
        request_id: ctx.request_id.clone(),
        job_id: Some(job_id.clone()),
        payload,
        options: opts,
        created_at: ctx.created_at,
        enqueued: Instant::now(),
        enrich_deadline: None,
    });
    tokio::spawn(run_crawl(app.clone(), spec, start, store, run));
    Ok(crate::rpc::ok("crawl", &ctx.request_id, 202, &json!({"status": "running", "job_id": job_id, "site": site})))
}

/// Everything one crawl's page tasks share.
struct Ctx {
    app: Arc<App>,
    spec: Arc<JobSpec>,
    site: String,
    start: Url,
    store: Store,
    run: Arc<Run>,
    robots: Robots,
    /// The local copy of this run's rows (`results/<job_id>.pages.jsonl`).
    run_file: tokio::sync::Mutex<tokio::fs::File>,
    /// Earliest time of the next fetch (robots.txt crawl delay).
    next_fetch: tokio::sync::Mutex<tokio::time::Instant>,
}

impl Ctx {
    fn job_id(&self) -> &str {
        self.spec.job_id.as_deref().unwrap_or("")
    }
    fn request(&self) -> Option<&FetchRequest> {
        self.spec.payload.request.as_ref()
    }
    /// Whether a link is followed: same site (unless `same_site: false`), include/exclude
    /// patterns on the path and query, robots.txt, and not an obvious non-document file.
    fn in_scope(&self, u: &Url) -> bool {
        let o = &self.spec.options;
        if o.same_site.unwrap_or(true) && site_host(u) != site_host(&self.start) {
            return false;
        }
        let path = u.path().to_ascii_lowercase();
        const SKIP: [&str; 20] = [
            ".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg", ".ico", ".bmp", ".mp4", ".mp3", ".webm", ".mov", ".zip", ".gz", ".tar",
            ".css", ".js", ".woff", ".woff2", ".exe",
        ];
        if SKIP.iter().any(|e| path.ends_with(e)) {
            return false;
        }
        let target = match u.query() {
            Some(q) => format!("{}?{q}", u.path()),
            None => u.path().to_string(),
        };
        let matches = |pats: &Option<Vec<String>>| pats.as_ref().map(|v| v.iter().any(|p| glob(p, &target, false)));
        if matches(&o.include) == Some(false) || matches(&o.exclude) == Some(true) {
            return false;
        }
        !o.respect_robots.unwrap_or(true) || self.robots.allowed(u)
    }
}

#[derive(Serialize)]
struct PageHead<'a> {
    schema_version: u32,
    operation: &'static str,
    job_id: &'a str,
    url: &'a str,
    site: &'a str,
    status: &'a str,
}

#[derive(Serialize)]
struct Totals<'a> {
    totals: &'a LlmTotals,
}

#[derive(Serialize)]
struct PageTail<'a> {
    metadata: Option<&'a serde_json::Map<String, Value>>,
    content_hash: Option<&'a str>,
    llm: Totals<'a>,
    fetched_at: Option<&'a str>,
    converted_at: Option<&'a str>,
    error: Option<&'a crate::rpc::ErrorBody>,
}

struct Outcome {
    url: Url,
    depth: u32,
    attempt: u32,
    links: Vec<Url>,
    /// 429/503: fetch again after this many seconds.
    retry_after: Option<u64>,
}

async fn run_crawl(app: Arc<App>, spec: Arc<JobSpec>, start: Url, store: Store, run: Arc<Run>) {
    let started = Instant::now();
    let job_id = spec.job_id.clone().unwrap_or_default();
    let site = run.site.clone();
    let opts = &spec.options;
    let request = spec.payload.request.as_ref();
    let robots = match (opts.respect_robots.unwrap_or(true), start.join("/robots.txt")) {
        (true, Ok(u)) => fetch_text(&u, request, 512 << 10).await.map(|t| Robots::parse(&t)).unwrap_or_default(),
        _ => Robots::default(),
    };
    let run_path = app.results_dir().join(format!("{job_id}.pages.jsonl"));
    let file = match tokio::fs::File::create(&run_path).await {
        Ok(f) => f,
        Err(_) => {
            let e = AppError::internal("cannot create the run file");
            return finish(&app, &spec, &run, &store, "failed", Some(e), started).await;
        }
    };
    let max_pages = opts.max_pages.unwrap_or(DEFAULT_MAX_PAGES) as usize;
    let max_depth = opts.max_depth.unwrap_or(DEFAULT_MAX_DEPTH);
    let delay = robots.delay.unwrap_or_default();
    let cx = Arc::new(Ctx {
        app: app.clone(),
        spec: spec.clone(),
        site: site.clone(),
        start: start.clone(),
        store,
        run: run.clone(),
        robots,
        run_file: tokio::sync::Mutex::new(file),
        next_fetch: tokio::sync::Mutex::new(tokio::time::Instant::now()),
    });

    let mut seen: HashSet<String> = HashSet::new();
    let mut frontier: VecDeque<(Url, u32, u32)> = VecDeque::new();
    // The start URL is always fetched; links and sitemap entries are checked against the scope.
    seen.insert(start.to_string());
    frontier.push_back((start.clone(), 0, 0));
    if opts.use_sitemap.unwrap_or(true) && max_depth > 0 {
        for u in sitemap_urls(&start, &cx.robots, request, max_pages).await {
            if cx.in_scope(&u) && seen.insert(u.to_string()) {
                frontier.push_back((u, 1, 0));
            }
        }
    }
    let mut tasks = tokio::task::JoinSet::new();
    let mut taken = 0usize;
    loop {
        while tasks.len() < PER_SITE && taken < max_pages && !run.cancel.load(Relaxed) {
            let Some((u, depth, attempt)) = frontier.pop_front() else { break };
            taken += 1;
            let cx = cx.clone();
            tasks.spawn(async move {
                // Politeness: robots.txt crawl delay between fetches.
                {
                    let mut next = cx.next_fetch.lock().await;
                    tokio::time::sleep_until(*next).await;
                    *next = tokio::time::Instant::now() + delay;
                }
                page(&cx, u, depth, attempt).await
            });
        }
        run.queued.store(frontier.len() as u64, Relaxed);
        let Some(joined) = tasks.join_next().await else { break };
        let Ok(o) = joined else { continue };
        if let Some(secs) = o.retry_after {
            // Rate-limited: wait, then try once more (it counts as one page).
            taken -= 1;
            tokio::time::sleep(Duration::from_secs(secs.min(60))).await;
            frontier.push_front((o.url, o.depth, o.attempt + 1));
            continue;
        }
        if o.depth < max_depth {
            for l in o.links {
                if cx.in_scope(&l) && seen.insert(l.to_string()) {
                    frontier.push_back((l, o.depth + 1, 0));
                }
            }
        }
    }
    let status = if run.cancel.load(Relaxed) {
        "cancelled"
    } else if !frontier.is_empty() {
        "limit_reached"
    } else {
        "completed"
    };
    finish(&app, &spec, &run, &cx.store, status, None, started).await;
}

/// Fetch, convert (unless unchanged since the last run) and save one page.
async fn page(cx: &Ctx, url: Url, depth: u32, attempt: u32) -> Outcome {
    let app = &cx.app;
    let opts = &cx.spec.options;
    let mut out = Outcome { url: url.clone(), depth, attempt, links: Vec::new(), retry_after: None };
    let shown = cx.request().map(|r| r.strip_query(&url)).unwrap_or_else(|| url.clone());
    let name = source::file_name(shown.as_str());
    let t = Instant::now();
    let fetched = jobs::fetch_source(app, opts, cx.request(), &Source::Https(url.clone()), &name).await;
    let (staged, format) = match fetched {
        Ok(v) => v,
        Err(e) => {
            if let Some(secs) = e.retry_after.filter(|_| attempt == 0) {
                out.retry_after = Some(secs);
                return out;
            }
            save(cx, &shown, None, Err(e), None, None).await;
            return out;
        }
    };
    let fetch_ms = t.elapsed().as_millis() as u64;
    if format == crate::source::Format::Html {
        let (path, base) = (staged.path.clone(), staged.final_url.clone().unwrap_or_else(|| url.clone()));
        out.links = app
            .cpu
            .run(move || std::fs::read(path).map(|b| links(&String::from_utf8_lossy(&b), &base)).unwrap_or_default())
            .await
            .unwrap_or_default();
    }
    let hash = format!("b3:{}", staged.hash.to_hex());
    let fetched_at = staged.fetched_at;
    // Unchanged since the last run: keep its row, no new conversion.
    if opts.cache == CacheMode::Use
        && let Ok(Some(prev)) = app.db.crawl_page(&cx.site, shown.as_str()).await
        && prev.content_hash.as_deref() == Some(hash.as_str())
        && matches!(prev.status.as_str(), "completed" | "partial")
    {
        cx.run.unchanged.fetch_add(1, Relaxed);
        app.writer.send(WriteOp::CrawlPage(CrawlPageRow { job_id: Some(cx.job_id().to_string()), fetched_at, ..prev }));
        return out;
    }
    let page_spec = Arc::new(JobSpec {
        request_id: cx.spec.request_id.clone(),
        job_id: cx.spec.job_id.clone(),
        payload: ConvertPayload { source: shown.to_string(), ..cx.spec.payload.clone() },
        options: cx.spec.options.clone(),
        created_at: cx.spec.created_at,
        enqueued: Instant::now(),
        enrich_deadline: None,
    });
    let acct = Arc::new(Accounting::new(cx.spec.request_id.clone(), Some(app.writer.clone())));
    let res = async {
        let mut mem = app.admission.reserve(app.admission.estimate(Some(staged.size), Some(format)), true).await?;
        jobs::convert_staged(app, &page_spec, staged, format, name, fetch_ms, Prio::Low, Prio::Low, &mut mem, acct.clone(), true).await
    }
    .await;
    let calls = match &res {
        Ok(c) if c.cache_hit => Vec::new(),
        _ => acct.section().calls,
    };
    cx.run.calls.lock().unwrap().extend(calls.iter().cloned());
    save(cx, &shown, Some(&hash), res, fetched_at, Some(&calls)).await;
    out
}

/// Write a page's row: its object in the store, this run's file, the index, and `page.finished`.
async fn save(
    cx: &Ctx,
    url: &Url,
    hash: Option<&str>,
    res: Result<jobs::Converted, AppError>,
    fetched_at: Option<i64>,
    calls: Option<&[CallRecord]>,
) {
    let app = &cx.app;
    let (status, body, err) = match res {
        Ok(c) => (if c.body.partial() { "partial" } else { "completed" }, Some(c.body), None),
        Err(e) => ("failed", None, Some(e)),
    };
    let images = body.as_ref().map(jobs::image_metas).unwrap_or_default();
    let totals = crate::llm::totals(calls.unwrap_or_default());
    let now = now_ms();
    let (fetched_s, converted_s) = (fetched_at.map(rfc3339), body.as_ref().map(|_| rfc3339(now)));
    let eb = err.as_ref().map(|e| e.body());
    let head = PageHead {
        schema_version: result::SCHEMA_VERSION,
        operation: "crawl",
        job_id: cx.job_id(),
        url: url.as_str(),
        site: &cx.site,
        status,
    };
    let tail = PageTail {
        metadata: cx.spec.payload.metadata.as_ref(),
        content_hash: hash,
        llm: Totals { totals: &totals },
        fetched_at: fetched_s.as_deref(),
        converted_at: converted_s.as_deref(),
        error: eb.as_ref(),
    };
    let empty = Body::default();
    let src = body.as_ref().map(|b| b.src()).unwrap_or(BodySrc::Body(&empty));
    let row = match result::write_result(Vec::new(), app.cfg.max_result_bytes, &head, src, &tail) {
        Ok((_, _, row)) => row,
        Err(e) => {
            tracing::warn!(target: "docvisionllm::crawl", url = %url, error = %e, "page row could not be written");
            return;
        }
    };
    let key = page_key(url);
    let saved = async {
        for m in &images {
            let data = tokio::fs::read(app.images_dir().join(m.file_name()))
                .await
                .map_err(|_| AppError::internal("cannot read an extracted image"))?;
            cx.store.put(&format!("sites/{}/images/{}", cx.site, m.file_name()), data).await?;
        }
        if let Some(b) = &body {
            cx.store.put(&format!("sites/{}/pages/{key}.md", cx.site), jobs::content_of(b).into_bytes()).await?;
        }
        cx.store.put(&page_rel(&cx.site, &key), row.clone()).await
    }
    .await;
    if let Err(e) = saved {
        tracing::warn!(target: "docvisionllm::crawl", url = %url, error = %e.message, "page could not be saved");
        cx.run.failed.fetch_add(1, Relaxed);
        return;
    }
    {
        use tokio::io::AsyncWriteExt;
        let _ = cx.run_file.lock().await.write_all(&row).await;
    }
    let title = body.as_ref().and_then(|b| b.fields(&["title"]).remove("title")).and_then(|t| t.as_str().map(str::to_string));
    app.writer.send(WriteOp::CrawlPage(CrawlPageRow {
        site: cx.site.clone(),
        url: url.to_string(),
        page_key: key,
        job_id: Some(cx.job_id().to_string()),
        status: status.into(),
        title,
        content_hash: hash.map(str::to_string),
        fetched_at,
        converted_at: body.as_ref().map(|_| now),
        error: eb.as_ref().and_then(|e| serde_json::to_string(e).ok()),
    }));
    if err.is_some() { &cx.run.failed } else { &cx.run.done }.fetch_add(1, Relaxed);
    let hooks: Vec<_> =
        cx.spec.payload.webhooks().iter().enumerate().filter(|(_, w)| w.events.iter().any(|e| e == "page.finished")).collect();
    if !hooks.is_empty() {
        let data: Value = serde_json::from_slice(&row).unwrap_or(Value::Null);
        for (i, w) in hooks {
            crate::webhook::schedule_event(
                app.clone(),
                cx.job_id().to_string(),
                cx.spec.request_id.clone(),
                w.clone(),
                i,
                "page.finished",
                Some(url.to_string()),
                data.clone(),
            );
        }
    }
}

/// End a run: mark pages it did not reach `gone` (complete runs), rebuild the site's
/// `pages.jsonl`, upload the run's rows, record the result and send `job.finished`.
async fn finish(
    app: &Arc<App>,
    spec: &Arc<JobSpec>,
    run: &Arc<Run>,
    store: &Store,
    mut status: &'static str,
    mut err: Option<AppError>,
    started: Instant,
) {
    let job_id = spec.job_id.clone().unwrap_or_default();
    let site = run.site.clone();
    let run_path = app.results_dir().join(format!("{job_id}.pages.jsonl"));
    let (site_rel, run_rel) = (format!("sites/{site}/pages.jsonl"), format!("sites/{site}/runs/{job_id}/pages.jsonl"));
    if err.is_none() {
        let mut ops = Vec::new();
        if status == "completed" {
            ops.push(WriteOp::CrawlGone { site: site.clone(), job_id: job_id.clone() });
        }
        app.writer.send_durable(ops).await;
        app.writer.flush().await;
        let published = async {
            // The site now: every page's latest row, in URL order (`gone` pages left out).
            let mut all = Vec::new();
            let mut after = String::new();
            loop {
                let rows = app.db.crawl_pages(&site, None, None, &after, 500).await.map_err(jobs::db_err)?;
                let Some(last) = rows.last() else { break };
                after = last.url.clone();
                for r in rows.iter().filter(|r| r.status != "gone") {
                    if let Some(b) = store.get(&page_rel(&site, &r.page_key)).await? {
                        all.extend_from_slice(&b);
                    }
                }
            }
            store.put(&site_rel, all).await?;
            let rows = tokio::fs::read(&run_path).await.map_err(|_| AppError::internal("cannot read the run file"))?;
            store.put(&run_rel, rows).await
        }
        .await;
        if let Err(e) = published {
            status = "failed";
            err = Some(e);
        }
    }
    let totals = crate::llm::totals(&run.calls.lock().unwrap());
    let shown_src = source::sanitize(&spec.payload.source);
    let eb = err.as_ref().map(|e| e.body());
    let result = json!({
        "schema_version": result::SCHEMA_VERSION,
        "operation": "crawl",
        "job_id": job_id,
        "request_id": spec.request_id,
        "status": status,
        "site": site,
        "src_file": shown_src,
        "pages": run.pages(),
        "pages_file": store.display(&run_rel),
        "site_pages_file": store.display(&site_rel),
        "metadata": spec.payload.metadata,
        "llm": {"totals": totals},
        "timing": {"total_ms": started.elapsed().as_millis() as u64},
        "error": eb,
    });
    let path = app.results_dir().join(format!("{job_id}.json"));
    let mut bytes = serde_json::to_vec(&result).unwrap_or_default();
    bytes.push(b'\n');
    if tokio::fs::write(&path, &bytes).await.is_err() {
        tracing::error!(target: "docvisionllm::crawl", job_id = %job_id, "crawl result could not be written");
    }
    let error_json = eb.as_ref().and_then(|e| serde_json::to_string(e).ok());
    let terminal = vec![
        WriteOp::JobFinished {
            job_id: job_id.clone(),
            state: status.into(),
            at: now_ms(),
            result_path: Some(path.display().to_string()),
            error: error_json.clone(),
        },
        WriteOp::FinishRequest(RequestFinish {
            request_id: spec.request_id.clone(),
            request_status: "accepted".into(),
            execution_status: Some(status.into()),
            execution_stage: err.as_ref().and_then(|e| e.stage.map(str::to_string)),
            finished_at: now_ms(),
            timings: Some(json!({"total_ms": started.elapsed().as_millis() as u64}).to_string()),
            statistics: Some(json!({"pages": run.pages(), "files": {"output_file": store.display(&site_rel)}}).to_string()),
            usage: serde_json::to_string(&totals).ok(),
            error: error_json,
            result_available: true,
            ..Default::default()
        }),
        WriteOp::CrawlSite { site: site.clone(), at: now_ms(), job_id: job_id.clone(), status: status.into() },
    ];
    app.writer.send_durable(terminal).await;
    app.crawls.by_job.remove(&job_id);
    app.crawls.by_site.remove(&site);
    app.jobs.finish(&job_id, status);
    for (i, w) in spec.payload.webhooks().iter().enumerate() {
        crate::webhook::schedule(app.clone(), job_id.clone(), spec.request_id.clone(), w.clone(), i, path.clone(), 0);
    }
}

// ---------------------------------------------------------------- site.* operations

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SiteP {
    #[serde(default)]
    site: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    prefix: Option<String>,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
}

fn need<'a>(v: &'a Option<String>, what: &str) -> Result<&'a str, AppError> {
    v.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| AppError::bad_request("invalid_payload", format!("payload.{what} is required")))
}

fn page_json(store: &Store, p: &CrawlPageRow) -> Value {
    json!({
        "url": p.url,
        "status": p.status,
        "title": p.title,
        "content_hash": p.content_hash,
        "job_id": p.job_id,
        "fetched_at": p.fetched_at.map(rfc3339),
        "converted_at": p.converted_at.map(rfc3339),
        "error": p.error.as_deref().and_then(|e| serde_json::from_str::<Value>(e).ok()),
        "file": store.display(&page_rel(&p.site, &p.page_key)),
    })
}

/// `site.list`, `site.pages`, `site.page`, `site.reindex`.
pub async fn site_op(app: &Arc<App>, ctx: &ReqCtx, op: &str, payload: Value) -> Result<axum::response::Response, AppError> {
    let p: SiteP = typed(payload, "payload")?;
    let store = store(app)?;
    let data = match op {
        "site.list" => {
            let rows = app.db.crawl_sites().await.map_err(jobs::db_err)?;
            let sites: Vec<Value> = rows
                .into_iter()
                .map(|(site, created, job, run_at, st, pages)| {
                    json!({
                        "site": site,
                        "pages": pages,
                        "created_at": rfc3339(created),
                        "last_job_id": job,
                        "last_run_at": run_at.map(rfc3339),
                        "last_status": st,
                        "pages_file": store.display(&format!("sites/{site}/pages.jsonl")),
                    })
                })
                .collect();
            json!({"sites": sites})
        }
        "site.pages" => {
            let site = need(&p.site, "site")?;
            let limit = p.limit.unwrap_or(100);
            if !(1..=1000).contains(&limit) {
                return Err(AppError::bad_request("invalid_payload", "limit must be between 1 and 1000"));
            }
            let after = p.cursor.clone().unwrap_or_default();
            let rows = app.db.crawl_pages(site, p.status.as_deref(), p.prefix.as_deref(), &after, limit).await.map_err(jobs::db_err)?;
            let next = (rows.len() as i64 == limit).then(|| rows.last().map(|r| r.url.clone())).flatten();
            json!({"site": site, "pages": rows.iter().map(|r| page_json(&store, r)).collect::<Vec<_>>(), "next_cursor": next})
        }
        "site.page" => {
            let (site, url) = (need(&p.site, "site")?, need(&p.url, "url")?);
            let row = app
                .db
                .crawl_page(site, url)
                .await
                .map_err(jobs::db_err)?
                .ok_or_else(|| AppError::new(404, "page_not_found", "this URL is not in the site"))?;
            let page = match store.get(&page_rel(site, &row.page_key)).await? {
                Some(b) => serde_json::from_slice::<Value>(&b).unwrap_or(Value::Null),
                None => Value::Null,
            };
            let mut v = page_json(&store, &row);
            v["row"] = page;
            v
        }
        "site.reindex" => reindex(app, &store, p.site.as_deref()).await?,
        _ => unreachable!(),
    };
    Ok(crate::rpc::ok(op, &ctx.request_id, 200, &data))
}

/// Rebuild the index from the store: every `sites/<site>/pages/*.jsonl` object is read back.
async fn reindex(app: &Arc<App>, store: &Store, only: Option<&str>) -> Result<Value, AppError> {
    let objects = store.list(&only.map(|s| format!("sites/{s}/")).unwrap_or_else(|| "sites/".into())).await?;
    let (mut pages, mut sites) = (0u64, HashSet::new());
    for rel in objects {
        let Some((site, key)) =
            rel.strip_prefix("sites/").and_then(|r| r.split_once("/pages/")).and_then(|(s, k)| Some((s, k.strip_suffix(".jsonl")?)))
        else {
            continue;
        };
        if app.crawls.by_site.contains_key(site) {
            return Err(AppError::new(409, "crawl_running", format!("site `{site}` is being crawled; reindex after the run")));
        }
        let Some(b) = store.get(&rel).await? else { continue };
        let Ok(row) = serde_json::from_slice::<Value>(&b) else { continue };
        let s = |k: &str| row[k].as_str().map(str::to_string);
        let t = |k: &str| {
            row[k]
                .as_str()
                .and_then(|v| time::OffsetDateTime::parse(v, &time::format_description::well_known::Rfc3339).ok())
                .map(|t| (t.unix_timestamp_nanos() / 1_000_000) as i64)
        };
        let Some(url) = s("url") else { continue };
        if sites.insert(site.to_string()) {
            app.writer.send(WriteOp::CrawlSite {
                site: site.into(),
                at: now_ms(),
                job_id: s("job_id").unwrap_or_default(),
                status: "reindexed".into(),
            });
        }
        app.writer.send(WriteOp::CrawlPage(CrawlPageRow {
            site: site.into(),
            url,
            page_key: key.into(),
            job_id: s("job_id"),
            status: s("status").unwrap_or_else(|| "completed".into()),
            title: s("title"),
            content_hash: s("content_hash"),
            fetched_at: t("fetched_at"),
            converted_at: t("converted_at"),
            error: row.get("error").filter(|e| !e.is_null()).map(Value::to_string),
        }));
        pages += 1;
    }
    app.writer.flush().await;
    Ok(json!({"sites": sites.len(), "pages": pages}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_globs() {
        let u = |s: &str| Url::parse(s).unwrap();
        assert_eq!(page_key(&u("https://d.com/")), "_root");
        assert_eq!(page_key(&u("https://d.com/guide/install")), "guide/install");
        assert!(page_key(&u("https://d.com/search?q=api")).starts_with("search~"));
        assert!(page_key(&u("https://d.com/a%20b/c")).starts_with("a_20b/c~"));
        assert!(glob("/docs/*", "/docs/a/b", false) && !glob("/docs/*", "/blog/a", false));
        assert!(glob("*/tag/*", "/x/tag/y", false));
        assert_eq!(normalize(u("https://d.com/a?utm_source=x&id=2#top")).unwrap().as_str(), "https://d.com/a?id=2");
        let r = Robots::parse(
            "User-agent: *\nDisallow: /private\nAllow: /private/ok\nCrawl-delay: 1\nSitemap: https://d.com/s.xml\n\nUser-agent: other\nDisallow: /",
        );
        assert!(
            !r.allowed(&u("https://d.com/private/x")) && r.allowed(&u("https://d.com/private/ok/1")) && r.allowed(&u("https://d.com/pub"))
        );
        assert_eq!((r.delay, r.sitemaps.len()), (Some(Duration::from_secs(1)), 1));
        let l = links("<a href=\"/x#y\">x</a><a href=\"mailto:a@b\">m</a><a href=\"https://o.com/p\">o</a>", &u("https://d.com/dir/"));
        assert_eq!(l.iter().map(Url::as_str).collect::<Vec<_>>(), ["https://d.com/x", "https://o.com/p"]);
    }
}
