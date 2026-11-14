# Backlog

Ideas that were discussed and shelved. Nothing here is scheduled; each item is something to think about and may eventually be built.

<br>

## Large results

**The problem:** a result can be big. A 10 MB Markdown document becomes 20 MB or more of JSON, because the text appears twice (`content`, plus every chunk's own text) alongside the LLM call records. Today the whole result is sent whenever a caller asks for it, which is the intended behavior for now (the content is wanted for full-text indexing, e.g. SQLite FTS5).

Where a large result travels today:

| Path | Sends the content? |
| --- | --- |
| The `202` reply to `convert` | No |
| Webhook, default event | No (status, paths, sizes, statistics) |
| Webhook with `"$"` or `"$.content"` in the body template | Yes |
| `job.get` | Yes, the whole result by default |
| Destination files | Written to disk or S3, not sent |

What already helps, with no changes:

- A webhook body template that picks only what's needed, e.g. `{"id": "$.metadata.ref", "title": "$.title", "content": "$.content"}`, or `"$.chunks"` instead of `content` when indexing per chunk. Sending both doubles the payload.
- A destination plus the default webhook event: the receiver reads `md_file` / `docv_file` itself, so nothing large goes through the webhook.
- `job.get` with `fields` returns only the listed top-level fields.
- `/rpc` responses are compressed (zstd or gzip) when the client sends `Accept-Encoding`.

### 1. Opt-in gzip for webhook bodies

- **What:** a webhook option such as `"compress": "gzip"` that sends the body with `Content-Encoding: gzip`. Markdown shrinks about 4–5×.
- **Why opt-in:** a sender can't know whether the receiver accepts compressed request bodies, and many frameworks don't decompress them automatically.
- **Notes:** the gzip library (`flate2`) is already in the build. The signature should cover the bytes as sent (the compressed body). Whole-result bodies are streamed from disk today, so compression should stream too rather than load the file.

### 2. Webhook timeout that scales with body size

- **What:** keep 10 s for small bodies and allow more for large ones, e.g. 10 s plus 1 s per MB, capped at a few minutes.
- **Why:** each attempt has a fixed 10 s limit for the whole request. A large body on a slow link times out, and all 6 retries then re-send the full body.
- **Related:** receivers often cap request bodies (nginx defaults to 1 MB). A `413` is not retried, so the delivery ends as `failed`. Worth documenting, and possibly surfacing more clearly in the delivery status.

### 3. Make `job.get` light by default

- **What:** return status, paths, sizes, title, summary, statistics, usage and timing, but not `content`, `chunks` or `translated_content` unless asked (`fields`, or a new `include` list).
- **Why:** polling a job stays small however large the document is.
- **Cost:** a breaking change for callers that rely on getting `content` without asking.

### 4. Plain download for the Markdown

- **What:** e.g. `GET /_/jobs/{job_id}/content`, token-protected, streamed as `text/markdown` and compressed in transit.
- **Why:** callers without a destination get the raw file directly, with no JSON wrapper to download and parse.

### 5. Chunks without their text

- **What:** an option to return each chunk as start/end positions into `content` (plus headings, pages and token count) instead of repeating the text.
- **Why:** removes the duplication for callers who want both `content` and `chunks` in one response.

### Decided against

- **Dropping `content` automatically above some size.** A field that sometimes disappears is a surprise for callers. An explicit rule (light by default, ask for the heavy parts) behaves the same for a 1 KB note and a 10 MB book.

<br>

## Anthropic provider

The `anthropic` provider sends PDFs and images inline (base64), which covers pieces up to Anthropic's 32 MB request limit. Not built yet:

- **Files API for very large pieces.** Upload a piece once and reference it by ID, as the OpenAI and Gemini adapters do. Only matters for PDFs whose ~16-page pieces exceed roughly 20 MB (image-heavy scans).
- **Prompt caching.** The fixed instructions are far below the minimum cacheable size today. Worth revisiting if prompts grow, or for repeated extraction against the same schema.
- **Refusal fallback.** When Claude declines a document the request fails with the reason. Anthropic offers server-side fallback to another model for the newest models; it is a beta feature and was left out.
- **Effort as an option.** Effort is fixed (low for transcription and summaries, medium for extraction). It could become a request option if quality on hard scans needs it.

<br>

## Zip

`create_zip`, `zip_only` and `job.zip` are built (v0.4.0). Left out:

### 1. Rewrite the destination's JSON result after `job.zip`

- **What:** when `job.zip` builds a zip later, update `zip_bytes` in the `.docv.json` at the destination (and the local result file), not just in `job.get` and the webhook. (Once outputs are JSONL only, this applies to the `.jsonl` file, without the self-referential size problem.)
- **Why:** today the destination's JSON still says `zip_bytes: 0`, so anyone who reads only the destination files can't tell the zip exists.
- **Notes:** `docv_bytes` is self-referential, so the file has to be rewritten until its size is stable, as at conversion time. With `overwrite: false` the rewrite would conflict with the existing file.

### 2. Resume `zip.*` webhooks after a restart

- **What:** pending `zip.completed` / `zip.failed` deliveries are retried after a restart, as `job.finished` deliveries are.
- **Why:** a restart during the retry window drops the event. The workaround is to call `job.zip` again, which replies at once with the existing zip.
- **Notes:** the zip event IDs end in `_zip-completed`, so `resume` skips them today. It would need the event kind stored, or parsed back from the ID, and the zip state from `<job_id>.zip.json`.

### 3. Body templates for zip events

- **What:** let `webhook.body` (or a separate `zip_body`) shape the `zip.*` events.
- **Why:** receivers built around a custom `job.finished` template get zip events in the fixed shape.

### 4. Update history when `job.zip` builds a zip

- **What:** record the new `zip_bytes` on the request's history entry.
- **Why:** history (and the dashboard's request details) only shows zips made during conversion; a later zip shows as "—".

### 5. Optional compression

- **What:** e.g. `"zip_compression": "deflate"`.
- **Why:** entries are stored uncompressed because pictures barely shrink. A zip that is mostly text (long Markdown, few or no images) would shrink about 4–5×.

### Decided against

- **Zips for sync conversions.** A sync conversion has no job to keep the zip with, so `create_zip` and `zip_only` require async.

<br>

## Input formats

### Decided against

- **HEIC / HEIF (iPhone photos).** There is no reliable pure-Rust decoder: support would need the system `libheif` (LGPL, plus an HEVC decoder with patent questions), or sending HEIC only to Gemini, which accepts it natively. Callers convert to PNG, JPEG or WebP instead; HEIC returns `415 unsupported_format`.

<br>

## Web capture: rendering, snapshots and crawling

**Status:** built: rendering in `convert` (`render`, `wait_for`, `wait_ms`) with the browser in its own container, `payload.request`, `crawl` with the S3 crawl store, `site.*`, `job.cancel`, `page.finished`, JSONL-only output, and `search` / language in the one enrichment call. Left out for now: `fetch` snapshots (capture without converting, `page.html`, screenshots), `scroll`, `main_content`, `wait_until`, page versions, `crawl.convert`, combined site outputs, and per-request DNS checks for internal addresses in the browser (link-local and metadata addresses are refused).

**The idea:** fetch web pages with a real browser (JavaScript, dynamic content), save what was fetched, and convert it to Markdown now or later. Today an HTTPS source is fetched as plain HTML and converted in one step, so pages built by JavaScript come out empty.

**The approach:** capture first, convert later. `fetch` saves a **snapshot** of a page, `convert` accepts a snapshot as its source, and the site is never fetched twice.

### The browser container

- **Where:** headless Chrome runs in its own container (e.g. `chromedp/headless-shell` or `browserless`), reached over the DevTools protocol: `DOCVISION_BROWSER_URL=ws://browser:3000`. The Rust side uses `chromiumoxide`, behind a `web-render` build feature.
- **Why its own container:** the service binary stays lean (Chrome is ~300 MB and memory-hungry), a crash or exploit stays out of the service process, and the browser can scale on its own and be restarted freely.
- **Per request:** a fresh, isolated browser context (no shared cookies or storage), closed afterwards. Images, fonts and media are blocked unless they are being captured.
- **Without the container:** URLs are fetched as plain HTML, as today.

### 1. Rendering in `convert`

| Option | Default | Meaning |
| --- | --- | --- |
| `render` | `auto` | `off`: plain fetch. `on`: always use the browser. `auto`: plain fetch first, then the browser if the page looks empty (little text, a bare `<div id="root">`) |
| `wait_until` | `network_idle` | `load`, `dom_ready` or `network_idle` (no requests for ~500 ms) |
| `wait_for` | — | A CSS selector to wait for |
| `wait_ms` | — | An extra fixed wait, within a hard page timeout (~30 s) |
| `scroll` | `false` | Scroll down in steps so lazy-loaded content appears (capped) |
| `main_content` | `true` | Keep the article; drop navigation, footer, cookie banners and ads (Readability-style) |
| `screenshot` | `false` | Also save a full-page PNG |

- **Flow:**
  1. Navigate and wait as configured.
  2. Take the rendered HTML, including shadow DOM and same-origin iframes.
  3. Extract the main content.
  4. Run the existing HTML converter.
- **Non-HTML responses:** if the URL turns out to be a PDF or DOCX, the browser is skipped and the normal pipeline runs.

### 2. `fetch`: snapshots of one URL or a list

```json
{"operation": "fetch",
 "payload": {"urls": ["https://a.com/x", "https://b.com/y"], "destination": "s3://bucket/web/"},
 "options": {"render": "auto", "capture_images": true, "screenshot": false}}
```

- **Batches:** each URL is its own job inside a batch, so one slow or blocked site doesn't hold up the rest. `batch.get` shows progress (done, failed, pending). Lists are capped, e.g. 1,000 URLs per request.
- **Webhooks:** optionally one per page, plus a `batch.completed` event with a manifest of every page, its snapshot path and status.
- **A snapshot** is a folder at the destination (S3 or local):

  ```
  s3://bucket/web/example.com/blog/post-1/
  ├── page.html        rendered HTML, image links rewritten to images/
  ├── images/…         images captured during the page load (named by hash, stored once)
  ├── screenshot.png   optional
  └── page.json        URL, final URL, status, title, fetch time, links found
  ```

- **Images** come from the browser's own network traffic during the page load: never downloaded twice, and exactly what the page showed. The saved HTML opens offline.
- **Downloading:** from the S3 path, or a token-protected route like the zip download.
- **Converting:** `convert` with `"source": "<snapshot path or ID>"` reuses the captured HTML and images. `extract_images`, zips and the rest work as for documents.
- **Fetching a URL again:** skip it, overwrite it or keep dated versions. Overwrite by default, as destinations already do.
- **Later:** export to WARC, the standard web-archive format, if ever needed.

### 3. `crawl`: sites kept permanently on S3

`convert` does not crawl: it fetches one URL and converts it, as today. Crawling exists only through the crawl API, and crawls are kept permanently.

**Kept simple:** a crawl runs the normal conversion on many pages. Each page produces the same output as `convert` (one row per page), with no chunks, no embeddings and no search. Everything is saved as JSON Lines.

**Organized by site, not by crawl.** A site is the permanent home and grows over time; a crawl is one run that adds or updates pages in it.

#### The crawl store

- **Where:** one permanent S3 location, set on the server: `DOCVISION_CRAWL_STORE=s3://bucket/crawls/`. S3 is required: without it, the crawl operations return `feature_not_configured`. A `destination` on the request can override it for one site.
- **Layout:**

  ```
  s3://bucket/crawls/
  └── sites/
      └── docs.example.com/                 ← site key
          ├── site.json                     the site: hosts, created, last run, page count, settings
          ├── pages.jsonl                   the output: one row per page, same fields as a convert result
          ├── catalog.jsonl                 one line per page: URL → key, title, hash, status, dates
          ├── assets/
          │   └── 3f2a9c1b7d4e5a60.png      images, stored once per site by hash
          ├── pages/
          │   ├── _root/                    https://docs.example.com/
          │   ├── guide/install/            https://docs.example.com/guide/install
          │   │   ├── page.html             latest capture (image links → ../../assets/…)
          │   │   ├── page.jsonl            the page's row (one line, same as its pages.jsonl line)
          │   │   ├── page.json             capture info: URL, final URL, status, hash, fetched_at, links
          │   │   ├── screenshot.png        optional
          │   │   └── versions/
          │   │       └── 2026-11-13T10-02-11Z/   an earlier capture, kept only when the content changed
          │   └── search~a91f03c2/          https://docs.example.com/search?q=api (query string → short hash)
          └── runs/
              └── <job_id>/
                  ├── pages.jsonl           the rows this run produced
                  └── report.json           what this run did: new, changed, unchanged, gone, failed
  ```

- **Site key:** the hostname by default (`docs.example.com`), readable in the S3 console. A site can be named (`"site": "acme-docs"`), e.g. to group `example.com` and `docs.example.com`. `www.` and the bare domain count as one site unless told otherwise.
- **Page folders follow the URL path** (`guide/install/`). A query string, an over-long path, or characters S3 handles badly get a short hash suffix (`search~a91f03c2`). The full URL is always in `page.json` and the catalog; nothing relies on parsing folder names.
- **The latest capture sits at the page root:** the current row (with the Markdown in `content`) for a URL is always at `pages/<path>/page.jsonl`.
- **Versions** are saved only when the content changes. An unchanged page just updates `last_seen`, so re-crawls stay small.
- **Images are stored once per site** in `assets/` (the same logo and diagrams appear on every page). Each page's HTML and Markdown link to them with relative paths, so a downloaded site opens offline.
- **Run reports** are history: what each crawl found and changed. Pages hold the current state.
- **Pages that disappear** are marked `gone` in the catalog. Their files are kept; nothing is deleted.

#### S3 is the source of truth, the database is the index

- S3 holds everything (`site.json`, `catalog.jsonl`, pages, reports), so a site is complete and readable without the service.
- The database holds a queryable copy (`crawl_sites`, `crawl_pages`, `crawl_runs`) for filtering and paging, plus the queue of pages to fetch during a run.
- These tables are excluded from retention: crawls are kept.
- `site.reindex` rebuilds the tables from S3, so losing or moving the database never loses a crawl, and a second deployment can use the same store.

#### Runs

- **One active run per site.** A second request for the same site returns `409 crawl_running` with the current run's `job_id`.
- **Each run adds to the site.** The first seeds it. Later runs add newly found pages, re-check known ones (`If-None-Match` / `If-Modified-Since`, then the content hash), save versions only for changed pages and mark missing ones `gone`.
- **Targeted runs:** `"urls": [...]` fetches just those pages instead of following links, e.g. to add a few pages to an existing site.
- **How a run works:**
  - Pages to fetch are `queued` rows in the database. A run survives restarts, can be cancelled, and a URL is never queued twice: the URL is unique per site.
  - For each page, a worker checks `robots.txt` (cached per site), waits for the site's next allowed slot, renders it, saves the capture, then pulls out the links. Links are normalized, checked against scope, patterns and depth, and new ones are queued. `429` / `503` puts the page back in the queue with a delay.
  - The run ends when the queue is empty, or on `max_pages`, `max_depth`, its time limit or its storage cap. Its final state says which: `completed`, `limit_reached`, `cancelled`, `failed` or `interrupted`.
  - Crawl pages run at low priority and share the browser concurrency cap, so a big crawl never slows down normal `convert` traffic.
- **Options:**

  ```json
  {"operation": "crawl",
   "payload": {"start": "https://docs.example.com/", "site": "acme-docs",
               "metadata": {"customer": "acme", "project": "kb-2026"}},
   "options": {"max_pages": 500, "max_depth": 3, "same_site": true,
               "include": ["/docs/*"], "exclude": ["*/tag/*", "*?page=*"],
               "use_sitemap": true, "summary_method": "llm", "extract_images": false}}
  ```

- **Scope:** same site by default, with include and exclude patterns. URLs are normalized (lowercase host, no `#fragment`, tracking parameters removed, the page's canonical URL respected).
- **Crawler traps** (endless calendars, filter combinations, session IDs in URLs): the page cap, depth cap, URL normalization and an overall time limit.
- **Politeness:** respect `robots.txt` and its crawl delay, 1–2 concurrent requests per site, back off on `429` and `503`.
- **Notifications:** the same webhooks as `convert`: `job.finished` when the run ends (totals, the `pages.jsonl` paths, report path), and optionally `page.finished` per page (the row) so a receiver can index pages as they arrive. See [One API for convert and crawl](#one-api-for-convert-and-crawl).

#### Output: `pages.jsonl`

Every page is converted like a regular `convert` and becomes one row, with the same fields as a `convert` result (see [Convert: one combined prompt](#convert-one-combined-prompt)):

```json
{"url": "https://docs.example.com/guide/install",
 "site": "docs.example.com",
 "operation": "crawl",
 "job_id": "01a1…",
 "status": "completed",
 "title": "Installing the CLI",
 "summary": "How to install the CLI on macOS, Linux and Windows, including proxies.",
 "language": "en",
 "search": {"keywords": ["install", "CLI", "proxy"], "questions": ["How do I install the CLI?"], "type": "how-to"},
 "content": "# Installing the CLI\n\n## Requirements\n…",
 "extracted": null,
 "images": [],
 "statistics": {"total_words": 1840, "content_bytes": 9120, "image_count": 0},
 "warnings": [],
 "metadata": {"customer": "acme", "project": "kb-2026", "tags": ["docs"]},
 "content_hash": "b3:9f1c…",
 "llm": {"provider": "openai", "model": "gpt-…", "calls": 1, "input_tokens": 3120, "output_tokens": 410},
 "fetched_at": "2026-11-13T10:02:11Z",
 "converted_at": "2026-11-13T10:02:14Z"}
```

- **Same pipeline and options as `convert`:** title, summary, language, `extract_schema`, `extract_images` (off by default; images go to the site's `assets/`), the LLM. Chunks are not produced.
- **Title, summary, language and `search` come from one LLM call per page** (the combined prompt below). `gen_title`, `gen_summary` and the methods stay switchable, so a crawl can run with no LLM calls at all (local title, no summary, local language).
- **Failed pages are rows too**, with `status: "failed"` and `error`, so the file is a complete record and failures are easy to filter out.
- **`metadata`:** the same field as on `convert`. Passed once when the crawl starts and copied as-is onto every row, never interpreted. On a re-crawl, a row carries the metadata of the run that last fetched it; `job_id` says which.
- **Two files:**
  - `sites/<site>/pages.jsonl` is the site now: the latest row per page, rewritten at the end of each run. Pages marked `gone` are left out.
  - `runs/<job_id>/pages.jsonl` holds the rows that run produced.
- **Unchanged pages are not converted again:** the content hash is compared, and the previous row is kept, so re-crawls cost no LLM calls for pages that didn't change.
- `page.jsonl` and `page.json` next to each capture are the reference copies; they are what re-crawls and `site.reindex` work from.
- **No `.md` files are written:** the Markdown is the row's `content`.

#### The API

A crawl is a job: `job.get`, `job.wait`, `job.cancel` and `job.zip` work on it. The site store has its own operations (`site.list`, `site.pages`, `site.page`, `site.reindex`). The full list is under [One API for convert and crawl](#one-api-for-convert-and-crawl).

#### Things to keep in mind

- **Large catalogs:** for very big sites (100k+ pages), `catalog.jsonl` is rewritten at the end of each run, not per page, and could be split by path. The database remains the way to query.
- **Folder name collisions** (`/A` vs `/a`, or a path and the same path with a query) are resolved by the hash suffix; the catalog is the final authority.
- **Writes are not all-or-nothing.** If a run crashes halfway, the pages written so far are valid; the run is marked `interrupted` and the next run picks up where it stopped.

#### Later (left out to keep the scope small)

- `crawl.convert`: convert a site's saved pages again, filtered, with new options (another model, a schema, a translation), without fetching.
- Asking questions over a site, without embeddings. For small to medium sites: send all of `pages.jsonl` (as Markdown, with prompt caching) to the model; otherwise full-text search (SQLite FTS5 or PostgreSQL) over the rows, then send the top pages. A vector database is not needed at this size: it costs the same per question as keyword search and adds infrastructure.
- Combined outputs per site: an `llms.txt`-style index, a zip. (A single `site.md` is not needed: anyone can build it from `pages.jsonl`.)
- Scheduled re-crawls (e.g. weekly), with "what changed" webhooks.

### Convert: one combined prompt

Changes to `convert` itself, so a single conversion and a crawled page return the same format (`convert` is one page, a crawl is many):

- **`metadata` (already exists):** the caller's own JSON object, returned as-is in the result and webhooks. Crawls use the same field; no new `attributes` field is needed.
- **One combined LLM call** for title, summary and language detection when they are all on, using structured output: `{"title": …, "summary": …, "language": …}`, split into the usual fields. One call instead of up to three, for `convert` and every crawled page. `language_method: local` still works without an LLM call.
- **`search`: a summary for search, not for people** (`gen_search`, **on by default** for both `convert` and `crawl`). Produced in the same combined call, a few hundred extra output tokens, about 1 KB per page:

  ```json
  "search": {
    "keywords": ["install", "CLI", "proxy", "HTTPS_PROXY", "npm", "Homebrew", "setup", "installation"],
    "entities": ["docvision CLI", "Node 20", "Windows", "macOS", "Linux", "HTTPS_PROXY"],
    "questions": ["How do I install the CLI?", "How do I install behind a corporate proxy?",
                  "What Node version is required?", "Why does the installer fail with EACCES?"],
    "topics": ["installation", "networking"],
    "type": "how-to"
  }
  ```

  - `keywords`: terms people would search with, including synonyms the page itself may not use. `entities`: products, versions, settings, error codes, people. `questions`: the questions the page answers, in the user's words (the most useful field). `topics` and `type` (how-to, reference, FAQ, changelog, pricing…) for filtering and grouping.
  - **Why:** better keyword search without embeddings (index these fields next to `content`, weighted higher), and picking pages without reading them: send the model a compact index (title, URL, summary, `questions`; ~100–200 tokens per page instead of 2,000+), let it choose the few pages that answer, and send only those in full. 300 pages become ~50k tokens of index instead of ~600k of content. If embeddings are ever added, embedding `questions` + `summary` usually retrieves better than raw content.
  - **Limits:** it helps find the right page; answers still come from the content. For documents too long for one call, the fields describe what the model saw. Written in the document's language. Regenerated when the content hash changes. Quality depends on the model: check a sample of `questions` when choosing a cheap model for crawls.
- **The same row shape:** the fields shown under `pages.jsonl` above (`url`/`src_file`, `status`, `title`, `summary`, `language`, `search`, `content`, `extracted`, `images`, `statistics`, `warnings`, `metadata`, `content_hash`). `convert` keeps its extra fields (chunks, file paths and sizes, LLM usage, timing).

### One API for convert and crawl

**The rule:** `convert` and `crawl` take the same payload, the same options with the same defaults, return the same result fields and send the same webhooks. The differences: `crawl` follows links (a few extra options) and always runs in the background; `convert` handles one document and can also run sync.

#### Operations

| Operation | Status | Purpose |
| --- | --- | --- |
| `convert` | exists | One document or URL. Async by default, `sync` allowed |
| `crawl` | new | A site: follows links and converts every page. Always async |
| `summarize`, `chunk`, `extract` | exist | Text-only helpers |
| `job.get`, `job.wait` | exist | Both kinds of job. For a crawl: progress, counts, `pages.jsonl` paths (not every page inline) |
| `job.cancel` | new | Stop a job. For a crawl, pages already saved stay |
| `job.zip` | exists | Zip of a job's output (for a crawl, the run's pages, as a later option) |
| `site.list` | new | Sites in the crawl store, with page counts and last run |
| `site.pages` | new | A site's pages, paginated and filtered by status, path prefix and dates |
| `site.page` | new | One URL: its row, file paths and versions |
| `site.reindex` | new | Rebuild the database index from S3 |
| `history.list`, `history.get`, `stats`, `health` | exist | Unchanged; crawls appear as `crawl` requests |

No separate `fetch` operation: capture-only is the shared option `convert_markdown: false`.

New HTTP route: `GET /_/jobs/{job_id}/pages.jsonl` (token required), a crawl run's rows for setups that can't read S3.

#### Payload

| Field | `convert` | `crawl` |
| --- | --- | --- |
| `source` | The document or URL | The start URL (a list for targeted runs) |
| `destination` | Optional | Optional; the crawl store is the default |
| `site` | — | Optional; the hostname by default |
| `request` | Optional (HTTP(S) sources) | Optional |
| `metadata`, `webhook`, `requester_id` | Same | Same |

#### `request`: headers, query parameters, cookies

```json
"request": {
  "headers": {"Authorization": "Bearer abc…", "Accept-Language": "fr-FR"},
  "query": {"lang": "fr", "token": "xyz"},
  "cookies": {"session": "…"},
  "user_agent": "acme-kb-sync/1.0"
}
```

- **What:** `headers` and `cookies` are sent with every request for the source; `query` is added to every URL fetched (on a crawl, links found on pages don't carry it, so it is re-added to each); `user_agent` replaces `docvision/<version>`. Rendered pages get the same headers and cookies.
- **Only the source's hosts get them** (and the site's other hosts on a crawl, e.g. `www.` and the bare domain). Never third-party hosts (CDNs, analytics, widgets, images from other domains), and redirects to another host drop them. In the browser this is done per request by interception, not globally. Per-host settings (`"request": [{"host": …, "headers": …}]`) can come later.
- **They are secrets,** handled like `llm_api_key`:
  - Stored encrypted with the job (async jobs and crawl resumes need them) and deleted with it.
  - Never in results, `pages.jsonl`, history, logs, webhooks or the dashboard. `job.get` shows only header names.
  - Never stored in snapshots or `page.json`.
  - `query` values are removed from URLs shown in results and rows.
- **Rules:**
  - Not allowed: `Host`, `Content-Length`, `Connection` and `x-docvision-*` (as for webhook headers).
  - Limits: e.g. 32 headers and 8 KiB in total.
  - HTTP(S) sources only (`400` for S3 and local files).
- **Cache:** a hash of `request` is part of the cache key (the hash, never the values), so a page fetched with `Accept-Language: fr` or a login is never served to another request.
- **Note for the docs:** pages fetched with credentials may contain private content, and crawls are kept.

#### Options

- **Shared, same defaults:** every option `convert` has today (`ocr`, `cache`, `allow_partial`, `priority`, `gen_title`, `gen_summary`, `gen_chunks`, the methods, `chunk_size`, `chunk_overlap`, `detect_language`, `translate_to`, `extract_schema`, `extract_images`, `create_zip`, `zip_only`, `overwrite`, the `llm_*` options), plus the new `gen_search`, the rendering options (`render`, `wait_for`, `wait_ms`, `scroll`, `main_content`, `screenshot`) and `convert_markdown` (default `true`; `false` captures only).
- **Crawl only:** `max_pages`, `max_depth`, `same_site`, `include`, `exclude`, `use_sitemap`, `respect_robots`. Rejected on `convert` (`400 invalid_options`).
- **`execution`:** `async` or `sync` on `convert`; `crawl` is always async (`sync` is a `400`).
- **New defaults** (breaking for `convert` callers, so they go in a minor version with a changelog note):

  | Option | Today | New default |
  | --- | --- | --- |
  | `gen_search` | — | `true` |
  | `detect_language` | `false` | `true` |
  | `gen_chunks` | `true` | `false` |

  With the combined prompt, title, summary, language and `search` cost one LLM call, so turning language and search on by default adds output tokens, not calls. Callers that rely on `chunks` must now ask for them.

#### Results

- **One shape:** a `convert` result and each crawl row share `schema_version`, `operation` (`convert` or `crawl`), `job_id` (the job that produced it: the conversion, or the crawl run; there is no separate crawl ID), `status`, `title`, `summary`, `language`, `search`, `content`, `extracted`, `images`, `chunks` (when `gen_chunks`), `statistics`, `warnings`, `metadata`, `content_hash`, `llm` (that document's usage: provider, model, calls, input and output tokens) and `converted_at` (when it was converted). `fetched_at` is set for URL sources.
- **`convert` adds** its job-level fields: `request_id`, `cache_hit`, file paths and sizes, the full `llm` call records, `timing`.
- **A crawl row adds** `url` and `site`. The run's totals, usage and timing are in `job.get` and the run report, not repeated on every row.
- A reader that knows only the row fields reads both.

#### The JSONL format (reference)

Each line is one JSON object (pretty-printed here). `schema_version` becomes `2`, since files and fields change.

`convert` (`report.pdf.jsonl`, one line):

```json
{
  "schema_version": 2,
  "operation": "convert",
  "job_id": "01a3…",
  "request_id": "01a2…",
  "status": "completed",
  "cache_hit": false,

  "src_file": "s3://bucket/in/report.pdf",
  "src_bytes": 4821337,
  "dest_file": "s3://bucket/out/",
  "output_file": "s3://bucket/out/report.pdf.jsonl",
  "images_dir": "s3://bucket/out/images/",
  "zip_file": "s3://bucket/out/report.pdf.zip",
  "zip_bytes": 0,

  "format": "pdf",
  "title": "Quarterly Report Q3",
  "summary": "Revenue grew 12% on strong enterprise sales; margins held despite higher cloud costs.",
  "language": "en",
  "search": {
    "keywords": ["quarterly report", "Q3", "revenue", "margin", "enterprise", "cloud costs"],
    "entities": ["Acme Corp", "Q3 2026", "EMEA"],
    "questions": ["How much did revenue grow in Q3?", "Why did cloud costs rise?"],
    "topics": ["finance", "earnings"],
    "type": "report"
  },
  "content": "# Quarterly Report Q3\n\nRevenue grew 12% …\n\n![Revenue chart](images/3f2a9c1b7d4e5a60.png)\n…",
  "extracted": null,
  "images": [
    {"path": "images/3f2a9c1b7d4e5a60.png", "format": "png", "bytes": 48213,
     "width": 640, "height": 480, "location": "page 2", "alt": "Revenue chart"}
  ],
  "chunks": null,
  "translated_content": null,
  "translated_language": null,
  "statistics": {"original_total_pages": 14, "total_words": 5120, "total_characters": 31840,
                 "content_bytes": 32410, "markdown_estimated_total_pages": 11, "image_count": 1, "chunk_count": 0},
  "feature_status": {"title": {"status": "completed", "method": "llm"}, "summary": {"status": "completed", "method": "llm"},
                     "language": {"status": "completed", "method": "llm"}, "search": {"status": "completed", "method": "llm"},
                     "chunks": {"status": "disabled"}},
  "warnings": [],
  "metadata": {"customer": "acme", "ref": "abc-123"},
  "content_hash": "b3:9f1c…",

  "llm": {"provider": "openai", "model": "gpt-…", "calls": 2, "input_tokens": 41230, "output_tokens": 2310,
          "records": [{"purpose": "ocr", "status": "ok", "input_tokens": 37800, "output_tokens": 1880, "duration_ms": 9120},
                      {"purpose": "enrich", "status": "ok", "input_tokens": 3430, "output_tokens": 430, "duration_ms": 2040}]},
  "timing": {"queue_ms": 12, "processing_ms": 11310, "total_ms": 11322},
  "fetched_at": null,
  "converted_at": "2026-11-13T10:02:14Z",
  "error": null
}
```

`crawl` (`pages.jsonl`, one line per page):

```json
{
  "schema_version": 2,
  "operation": "crawl",
  "job_id": "01a4…",
  "url": "https://docs.example.com/guide/install",
  "site": "docs.example.com",
  "status": "completed",

  "format": "html",
  "title": "Installing the CLI",
  "summary": "How to install the CLI on macOS, Linux and Windows, including proxies.",
  "language": "en",
  "search": {"keywords": ["install", "CLI", "proxy", "HTTPS_PROXY", "setup", "installation"],
             "entities": ["Node 20", "HTTPS_PROXY", "Homebrew"],
             "questions": ["How do I install the CLI?", "How do I install behind a corporate proxy?"],
             "topics": ["installation", "networking"], "type": "how-to"},
  "content": "# Installing the CLI\n\n## Requirements\n\nYou need Node 20 or later …",
  "extracted": null,
  "images": [],
  "chunks": null,
  "translated_content": null,
  "translated_language": null,
  "statistics": {"total_words": 1840, "total_characters": 10230, "content_bytes": 9120, "image_count": 0, "chunk_count": 0},
  "feature_status": {"title": {"status": "completed", "method": "llm"}, "…": "…"},
  "warnings": [],
  "metadata": {"customer": "acme"},
  "content_hash": "b3:9f1c…",

  "llm": {"provider": "openai", "model": "gpt-…", "calls": 1, "input_tokens": 3120, "output_tokens": 410},
  "fetched_at": "2026-11-13T10:02:11Z",
  "converted_at": "2026-11-13T10:02:14Z",
  "error": null
}
```

A failed page:

```json
{"schema_version": 2, "operation": "crawl", "job_id": "01a4…", "url": "https://docs.example.com/old/page", "site": "docs.example.com",
 "status": "failed", "title": null, "summary": null, "language": null, "search": null, "content": null,
 "metadata": {"customer": "acme"}, "llm": null, "fetched_at": "2026-11-13T10:03:40Z", "converted_at": null,
 "error": {"code": "http_404", "message": "the page returned 404", "stage": "fetch"}}
```

| Group | Fields | `convert` | crawl row |
| --- | --- | --- | --- |
| Identity | `schema_version`, `operation`, `job_id`, `status`, `error` | ✓ | ✓ |
| | `request_id`, `cache_hit` | ✓ | — |
| | `url`, `site` | — | ✓ |
| Files | `src_file`, `src_bytes`, `dest_file`, `output_file`, `images_dir`, `zip_file`, `zip_bytes` | ✓ | — |
| Document | `format`, `title`, `summary`, `language`, `search`, `content`, `extracted`, `images`, `chunks`, `translated_content`, `translated_language`, `statistics`, `feature_status`, `warnings` | ✓ | ✓ |
| Yours | `metadata` | ✓ | ✓ |
| Tracking | `content_hash`, `llm` (totals), `fetched_at`, `converted_at` | ✓ | ✓ |
| | `llm.records` (each call), `timing` | ✓ | — |

- **Same keys on every line:** disabled features are `null` (`chunks`, `extracted`, `translated_*`), which keeps readers and database imports simple.
- **`output_bytes` is not in the file** (a file can't simply contain its own size); it is reported in `job.get` and the webhook.
- **Crawl rows carry `llm` totals only;** the run's per-call records are in the run report.
- **Dates** are UTC, RFC 3339.

#### Output files: JSONL only (breaking)

- **No more `.md` or `.docv.json` files.** Every output is JSON Lines: `convert` writes one line, a crawl writes one line per page. The Markdown is the `content` field.
- **`convert` to a folder** writes `<name>.jsonl` (e.g. `report.pdf.jsonl`), not a shared `pages.jsonl`, so conversions into the same folder never share a file. A destination naming a file must end in `.jsonl`.
- **Images** still go to `images/` next to the file; `content` keeps its `images/<hash>.<ext>` links, which work once the content is written out next to that folder.
- **Result fields:** `md_file`/`md_bytes` and `docv_file`/`docv_bytes` become `output_file`/`output_bytes` (`output_bytes` only in `job.get` and the webhook, not inside the file). The content size stays in `statistics.content_bytes`. `images_dir`, `zip_file` and `zip_bytes` stay.
- **Simpler code:** the self-referential `docv_bytes` (rewriting the file until its size is stable) goes away.
- **Unchanged:** API replies (`job.get`, sync responses) return the same JSON object; the service's internal result storage.
- **Zip:** holds the `.jsonl` and `images/` (no `.md` or `summary.md`; both are fields of the line). `zip_only` writes only the zip.
- `GET /_/jobs/{job_id}/pages.jsonl` works for both kinds of job: one line for `convert`, many for a crawl.
- **Breaking** for anyone reading `.md` / `.docv.json` files or the `md_file` / `docv_file` fields: ship it in a clearly marked release with a migration note.

#### Webhooks

Same mechanism and rules for both: signing, retries, `fire_and_forget`, body templates, several endpoints.

| Event | `convert` | `crawl` |
| --- | --- | --- |
| `job.finished` | When the document is done | When the run is done: totals and `pages.jsonl` paths |
| `page.finished` | — | Optional (`webhook.events: ["page.finished"]`), one per page, carrying the row |
| `zip.completed` / `zip.failed` | After `job.zip` | Same |

A receiver that handles `convert` handles `crawl` unchanged; it only needs to know `page.finished` if it subscribes.

### Restrictions to watch for

**Security (most important)**
- **Reaching internal addresses (SSRF).** Page scripts, redirects and sub-resources can all reach `localhost`, `169.254.169.254` (cloud credentials), `10.x` and so on. Every browser request, not just the first, must go through a filter or proxy that blocks private address ranges. The check must also be repeated after DNS resolution, so a site can't switch to an internal address mid-load. Crawling raises the stakes: a page can link to internal addresses on purpose.
- **Hostile pages:**
  - Keep Chrome's sandbox on.
  - Run the browser as a non-root user, with no access to the service's data.
  - Turn off downloads, `file://`, the clipboard and notifications.
  - Hard limits on page time and memory.
- **Credentials:** never forward the service's token or cookies, and never sign in to sites.

**Legal and politeness**
- **`robots.txt` and terms of service:** respect `robots.txt` by default, with a server-level setting to override it for sites you are allowed to fetch. Use an honest user agent (`docvision/<version>`).
- **Copyright and personal data:** fetched content belongs to the site owner and may contain personal data (GDPR). Snapshots and results are kept for the retention period.
- **Rate limits per site:** a batch of URLs on one domain must not look like an attack.

**Practical limits**
- **Bot protection** (Cloudflare, CAPTCHAs, fingerprinting) blocks headless browsers on some sites. Don't try to get around it: return a clear `blocked` error, not an empty page.
- **Login walls and paywalls** return the teaser or the login page; detect and flag them.
- **Cookie banners** are mostly removed by `main_content`. Clicking "accept" automatically raises consent issues, so it is left out.
- **Infinite scroll** needs the scroll cap.
- **Pages change:** web sources need their own cache expiry (e.g. 1 hour), plus `cache: refresh`.
- **Resources:** each browser tab uses ~100–300 MB. Renders need their own concurrency cap (e.g. `DOCVISION_BROWSER_CONCURRENCY=4`) that feeds the memory budget. A render costs 10–100× a plain fetch, hence `render: auto`.
- **Storage** grows quickly with images and crawls. Images stored once by hash help; a size cap per batch or crawl is still needed.
- **What doesn't render:** canvas-drawn content, text inside images (use `ocr` with the screenshot), and cross-origin iframes such as embedded videos and widgets.

### Order to build in

1. Browser container + `render` in `convert` (single pages).
2. `fetch`: one URL or a list, snapshots to S3 or local, batches, converting from a snapshot.
3. Capturing images and screenshots into snapshots.
4. `convert`: the combined title / summary / language / `search` prompt.
5. `crawl`: the crawl store on S3, runs, `pages.jsonl`, `job.cancel`, `site.*`, `page.finished` webhooks.

Each step is useful on its own and builds on the one before.
