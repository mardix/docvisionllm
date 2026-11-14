# docvisionllm

A lean HTTP service that converts documents to Markdown, and can extract structured JSON from them.

- **PDFs and images** are transcribed by an LLM (OpenAI, Gemini, Anthropic Claude or any OpenAI-compatible API).
- **DOCX, XLSX, PPTX, HTML, Markdown, text** (plus ODT/ODS/ODP/EPUB with `odf-epub`) are parsed natively, without an LLM.
- **Web pages** are fetched as they are, or rendered in a headless Chrome (its own container) when they need JavaScript. **Whole sites** can be crawled.
- Each result is one JSON Lines record: the Markdown, a title, a summary, the language, search terms, statistics, LLM usage and timings.
- **Structured data:** pass a JSON Schema, and the result also carries the data it describes (a receipt's merchant and total, an invoice's line items, …).

One binary, ~10 MiB idle, ~25 ms startup, SQLite by default.

<br>

## Quick start

```bash
./scripts/setup-env.sh          # creates .env with a random token
./scripts/run-local.sh          # builds and starts on :4242
./scripts/run-local.sh --mock   # same, with a fake LLM (no API key needed)
```

<br>

## Configuration

Set these as environment variables or in `.env`. Only `DOCVISION_TOKEN` is required.

| Variable | Default | Description |
| --- | --- | --- |
| `DOCVISION_TOKEN` | — | Access token, at least 6 characters. Use a long random one (`scripts/setup-env.sh` makes 40) for anything reachable from a network |
| `DOCVISION_BIND` | `0.0.0.0:4242` | Listen address |
| `DOCVISION_DATA_DIR` | `./data` | Database, downloads, results |
| `DOCVISION_DATABASE_URL` | SQLite in `DATA_DIR` | `postgres://…` needs the `postgres` feature |
| `DOCVISION_ENCRYPTION_KEY` | derived from token | 32 bytes, base64. Encrypts stored secrets |
| `DOCVISION_LOG_LEVEL` | `info` | |
| `DOCVISION_LOG_FORMAT` | `json` | `json` or `text` |
| `DOCVISION_MEMORY_BUDGET` | 60% of memory | Size or percent |
| `DOCVISION_MAX_CONCURRENT_JOBS` | `512` | Async jobs running at once |
| `DOCVISION_QUEUE_DEPTH` | `1024` | Async jobs waiting. When full: `429` + `Retry-After` |
| `DOCVISION_MAX_INPUT_BYTES` | `200MiB` | Largest source document |
| `DOCVISION_JOB_TIMEOUT` | `15m` | |
| `DOCVISION_RESULT_RETENTION` | `24h` | Results, jobs and cache |
| `DOCVISION_HISTORY_RETENTION` | `30d` | Request history |
| `DOCVISION_S3_ENDPOINT` | — | S3-compatible store (MinIO, R2, …) |
| `DOCVISION_BROWSER_URL` | — | The browser container's DevTools endpoint (`http://browser:9222`). Rendering web pages needs it |
| `DOCVISION_BROWSER_CONCURRENCY` | `4` | Pages rendered at once (each uses ~100–300 MB in the browser container) |
| `DOCVISION_CRAWL_STORE` | — | Where crawled sites are kept: `s3://bucket/crawls/` or a folder (`./data/crawls` locally, `/data/crawls` in Docker). A folder is created at startup. `crawl` needs it |
| `DOCVISION_MAX_CRAWLS` | `2` | Crawls running at once |

Sizes accept `KB`, `MiB`, `GB`, …. Durations accept `ms`, `s`, `m`, `h`, `d`.

### Default LLM

Requests can override any of these with the `llm_*` options.

| Variable | Default | Description |
| --- | --- | --- |
| `DOCVISION_LLM_PROVIDER` | `openai` | `openai`, `gemini`, `anthropic`, or any other name for an OpenAI-compatible API |
| `DOCVISION_LLM_MODEL` | — | The LLM is enabled once this is set |
| `DOCVISION_LLM_API_KEY` | — | |
| `DOCVISION_LLM_BASE_URL` | public endpoint | Required for providers other than `openai`, `gemini` and `anthropic` |
| `DOCVISION_LLM_MAX_CONCURRENCY` | `64` | Calls in flight. Halved automatically on `429`/`503` |
| `DOCVISION_LLM_MAX_RETRIES` | `3` | Retries on network errors, `429` and `5xx` |
| `DOCVISION_LLM_MAX_OUTPUT_TOKENS` | `16384` | Model output limit. Also sets the PDF pages per call |

### S3 credentials

- **Server credentials:** for plain `s3://bucket/key`, set `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION` (and `AWS_SESSION_TOKEN` if needed). On AWS, an IAM role works without keys.
- **Per request:** `s3://KEY:SECRET@bucket/key?region=eu-west-1&session_token=…`. Percent-encode `/`, `+` and `@` in the keys.
- **Region:** `?region=` if given. Otherwise plain `s3://` uses `AWS_REGION`, and URLs with credentials use `us-east-1`.

<br>

## Endpoints

| Route | Auth | Purpose |
| --- | --- | --- |
| `POST /rpc` | token | All operations (below) |
| `GET /_/dashboard` | token, entered in the page | Dashboard: live activity, in-progress requests, paginated request history and failures (filter by requester, operation, format, status) with file paths and sizes, and forms to run `convert`, `summarize`, `chunk` and `extract` |
| `GET /_/doc` | none | This README as a web page |
| `GET /_/images/{file}` | token | An extracted image, by the file name listed in a result's `images` |
| `GET /_/jobs/{job_id}/zip` | token | The zip of a job without a destination (see [Zip](#zip)) |
| `GET /_/jobs/{job_id}/pages.jsonl` | token | A job's rows as JSON Lines: one line for `convert`, one per page for a crawl run |
| `GET /_/db` | token | Download a snapshot of the SQLite database (the dashboard has a button for it). `404` when the database is PostgreSQL |
| `GET /metrics` | token | Prometheus metrics |
| `GET /livez` | none | `200 ok` while the process is up |

The token goes in the `X-ACCESS-TOKEN` header.

<br>

## Request and response

```json
{
  "operation": "convert",
  "payload": {},
  "options": {}
}
```

Every response has the same envelope:

```json
{
  "ok": true,
  "operation": "convert",
  "request_id": "01a10e96-…",
  "data": {},
  "error": null
}
```

On failure, `ok` is `false` and the error looks like `{"code": "…", "message": "…", "stage": "…"}`. If processing had started, `data` holds the partial result.

**No uploads.** Reference documents by path, URL or S3 location. Base64, `data:` URLs and multipart bodies return `415`.

<br>

## Operations

| Operation | Purpose |
| --- | --- |
| `convert` | Convert a document or web page (async by default) |
| `crawl` | Crawl a site and convert every page (always async). See [crawl](#crawl) |
| `summarize` | Title and summary for text |
| `chunk` | Split text into chunks |
| `extract` | Structured JSON from text, following a JSON Schema |
| `job.get` | A job's state and result |
| `job.wait` | Wait until a job changes state |
| `job.zip` | Build the zip for a finished job |
| `job.cancel` | Stop a running crawl |
| `site.list`, `site.pages`, `site.page`, `site.reindex` | Crawled sites and their pages. See [Sites](#sites) |
| `history.list` | Past requests, newest first |
| `history.get` | One request with its events and LLM calls |
| `stats` | Activity summary (what the dashboard shows) |
| `health` | Readiness, version and compiled features |

<br>

### convert

```json
{
  "operation": "convert",
  "payload": {
    "source": "https://example.com/report.pdf",
    "destination": "s3://bucket/out/",
    "metadata": {
      "ref": "abc-123"
    },
    "requester_id": "billing-service",
    "webhook": {
      "url": "https://example.com/hook"
    }
  },
  "options": {
    "execution": "async"
  }
}
```

**Payload fields**

| Field | Description |
| --- | --- |
| `source` | **Required.** Absolute path, `file:///…`, `https://…`, or `s3://bucket/key` |
| `destination` | Async only. A folder (ending in `/`) or a `.jsonl` path, local or S3 (details below) |
| `request` | Headers, query parameters and cookies for fetching an `http(s)` source (details below) |
| `metadata` | Any object. Returned in results and webhooks |
| `requester_id` | Who is asking: a name or ID, up to 128 characters. Default `unknown`. Used to filter history and in `stats` (also accepted by `summarize`, `chunk` and `extract`) |
| `webhook` | Async only. One webhook or an array of them. See [Webhooks](#webhooks) |

**Sources:** `https://` always works. `http://` is accepted only for `localhost`/`127.0.0.1`. Redirects must stay on HTTPS. Web pages are fetched as `docvision/<version>`.

**Request:** for sources behind a login, an API gateway or a locale switch.

```json
"request": {
  "headers": {"Authorization": "Bearer …", "Accept-Language": "fr-FR"},
  "query": {"lang": "fr", "token": "…"},
  "cookies": {"session": "…"},
  "user_agent": "acme-kb-sync/1.0"
}
```

- `headers` and `cookies` are sent with every request to the source's site, and `query` is added to every URL fetched there (also to each page of a crawl, and to a rendered page's own requests).
- **Only the source's site gets them** (`www.` and the bare domain count as one): never other hosts such as CDNs or analytics, and a redirect to another host drops them.
- **They are secrets:** stored encrypted with the job, never written to results, history, logs or webhooks, and `query` values are removed from the URLs results show.
- At most 64 entries and 8 KiB in total. `Host`, `Content-Length`, `Connection` and `x-docvision-*` are not allowed. Only for `http(s)` sources.

**Destination:** the result is written as JSON Lines: one line, the same record the API returns, with the Markdown in `content`. No `.md` file is written.

- **A folder** (ending in `/`): the file is named after the source file, keeping its extension, so documents with the same name but different formats never collide.

  ```
  "destination": "s3://bucket/out/"        source: …/hello.pdf

  s3://bucket/out/hello.pdf.jsonl          ← the result
  s3://bucket/out/images/…                 ← extracted images (extract_images)
  ```

- **A `.jsonl` file:** the result goes there.

Existing files are replaced. Set `overwrite: false` to keep them instead; the job then fails with `409 destination_exists`.

**Response:** async returns `202` with this `data`. Sync returns `200` with the full [result](#result).

```json
{
  "status": "queued",
  "job_id": "01a10e96-…",
  "queue_position": 1,
  "estimated_start_ms": 0,
  "cache_hit": false
}
```

<br>

### Options

All options are optional, and all are top-level keys in `options`.

**Execution**

| Option | Default | Values |
| --- | --- | --- |
| `execution` | `async` | `async`, `sync` |
| `priority` | `normal` | `normal`, `low` (async only) |
| `cache` | `use` | `use`, `bypass`, `refresh` |
| `allow_partial` | `false` | Accept partial extraction (`status: "partial"`) |
| `overwrite` | `true` | Replace existing destination files. `false` fails the job with `409 destination_exists` instead |
| `create_zip` | `false` | Also write a zip with the result (`.jsonl`) and the images. Async only. See [Zip](#zip) |
| `zip_only` | `false` | Write only the zip to the destination, not the `.jsonl` and `images/`. Turns on `create_zip` |

**PDF and images**

| Option | Default | Values |
| --- | --- | --- |
| `ocr` | `on` | `on`: LLM reads every page. `auto`: native text, LLM for scanned pages. `off`: native text only |
| `pdf_skip_local_processing` | `true` with `ocr: on` | `false` renders pages to images first (`pdf-render` feature) |

**How PDFs are processed**

- The page count is read from the PDF itself, so it is always exact. Limit: 2,000 pages.
- Pages are transcribed in batches of about 16, sized to the model's output limit (`DOCVISION_LLM_MAX_OUTPUT_TOKENS` ÷ ~800 tokens per page), all running in parallel.
- **Each batch gets its own small PDF** cut from the original, holding only its pages. Calls pay input tokens for their own pages only, and provider per-file limits (size, page count) apply to each piece, not the whole document.
- A piece over 8 MB is uploaded to the provider's Files API (OpenAI, Gemini) instead of being sent inline. Anthropic and OpenAI-compatible APIs always get pieces inline; Anthropic's limit is 32 MB per request. Pieces and uploads are removed when the job ends.
- If a batch's output is cut off at the model's limit, the piece is split in half and retried, down to single pages.
- PDFs that can't be split (encrypted or malformed) are sent whole, with each call naming its page range.
- With `ocr: auto`, only the scanned pages are sent, as pieces of their own.
- Meaningful figures (charts, diagrams, photos, maps, tables shown as images) get a short factual description at the point where they appear; decorative elements such as logos and borders are ignored. A page with no text gets a description of what it shows.
- The LLM receives the PDF itself, so it uses the text layer and the page images. Page-by-page images are only sent with `pdf_skip_local_processing: false` (`pdf-render` feature).

**How images are processed**

- PNG, JPEG and WebP are sent to the LLM as they are. Other image types (HEIC, GIF, TIFF, BMP) return `415 unsupported_format`.
- The model transcribes all visible text and adds a short factual description of content that isn't text (photos, charts, diagrams, logos, screenshots). An image with no text at all becomes a description, so every image yields searchable Markdown.
- If the model still returns nothing, the result is `completed` with empty `content` and the warning `the model returned no content for this image`.

**Cache**

- A conversion is cached by the document's content, the options that affect the output, and the provider and model. A repeat is served from the cache with `cache_hit: true`, no LLM calls and no extraction time.
- Empty results are never cached, so a blank answer can't repeat.
- `cache: "bypass"` skips the cache for one request; `cache: "refresh"` converts again and replaces the cached result.

**Enrichment**

| Option | Default | Values |
| --- | --- | --- |
| `gen_title` | `true` | Off: `title: null` |
| `gen_summary` | `true` | Off: `summary: null` |
| `gen_search` | `true` | Keywords, entities and questions for finding the document. Off: `search: null` |
| `gen_chunks` | `false` | On: `chunks` lists the document's chunks. Off: `chunks: null` |
| `title_method` | `llm` | `llm`, `local` |
| `summary_method` | `llm` | `llm`, `local` (extractive) |
| `chunk_size` | `512` | Tokens (≈ characters ÷ 4). 128 to 200000; smaller values become 128 |
| `chunk_overlap` | 15% of `chunk_size` | Tokens repeated from the previous chunk. `0` disables it |
| `detect_language` | `true` | |
| `language_method` | `llm` | `llm`, `local` (`lang-detect` feature). If the LLM fails, the local detector is used |
| `translate_to` | `null` | A BCP-47 tag (`translate` feature) |
| `words_per_page` | `500` | For `markdown_estimated_total_pages` |

**One LLM call:** title, summary, language and `search` come from a single call to the model (one JSON answer). Turning on language and search adds output tokens, not calls.

**Search:** `search` describes the document for finding it, not for reading: `keywords` (terms people would search with, including synonyms the text may not use), `entities` (products, organizations, people, versions, settings, error codes), `questions` (questions the document answers, as a reader would ask them), `topics` and `type` (how-to, reference, faq, article, report, …). Index these fields next to `content` for better keyword search, or give a model the titles, summaries and `questions` of many documents to choose which ones to read in full.

**Chunking:** chunks follow the document's structure. A heading starts a new chunk, and paragraphs, tables and code are split at line boundaries. A heading is never returned on its own: it stays with the text under it, even when a sub-heading comes first. Chunks under a quarter of `chunk_size` are merged into a neighbour. Each chunk's `headings` holds its heading path.

**Images:** `extract_images: true` extracts the pictures embedded in DOCX, PPTX, XLSX, ODT/ODS/ODP, EPUB and HTML (`data:` images). See [Extracted images](#extracted-images).

**Local title:** the document's own title (Office/ODF/EPUB metadata, HTML `<title>`, Markdown `# `), else the first heading, else the file name. If an LLM title fails, the local title is used and `feature_status.title.method` is `local_fallback`.

**Web pages** (need `DOCVISION_BROWSER_URL`)

| Option | Default | Values |
| --- | --- | --- |
| `render` | `auto` | `auto`: render in the browser when the page looks empty without JavaScript (under 200 visible characters). `on`: always render HTML pages. `off`: never |
| `wait_for` | — | With rendering: wait until this CSS selector matches (up to 10 s) |
| `wait_ms` | — | With rendering: wait this much longer (at most 20000) |

A rendered page is loaded in a fresh browser context, waits until loaded and the network has been quiet for 0.5 s (30 s at most), then `wait_for` / `wait_ms`, and the resulting HTML is converted. Images, media and fonts are not downloaded, and link-local and cloud-metadata addresses are refused. Without the browser container, `render: on` returns `400 feature_not_configured` and `auto` converts the page as fetched.

**LLM**

| Option | Default | Description |
| --- | --- | --- |
| `llm_provider` | `DOCVISION_LLM_PROVIDER` | `openai`, `gemini`, `anthropic`, or a custom name |
| `llm_model` | `DOCVISION_LLM_MODEL` | |
| `llm_api_key` | `DOCVISION_LLM_API_KEY` | Never logged, stored encrypted |
| `llm_base_url` | `DOCVISION_LLM_BASE_URL` | Required for custom (OpenAI-compatible) providers |

A provider other than the default needs its own `llm_model` and `llm_api_key`. The default key is never sent to another provider.

**Providers**

| Provider | Endpoint used | Notes |
| --- | --- | --- |
| `openai` | `https://api.openai.com/v1` | Chat Completions. Reads PDFs and images natively; large pieces go through the Files API |
| `gemini` | `https://generativelanguage.googleapis.com/v1beta` | Reads PDFs and images natively; large pieces go through the Files API |
| `anthropic` | `https://api.anthropic.com/v1` | Claude Messages API. Reads PDFs and images natively, sent inline. Runs at low effort for transcription and summaries, a step higher for structured extraction. If Claude declines a document, the request fails with `provider_error` and the reason |
| any other name | your `llm_base_url` | An OpenAI-compatible API (Ollama, vLLM, OpenRouter, …). Whether PDFs work depends on that API |

Example for Claude:

```bash
DOCVISION_LLM_PROVIDER=anthropic
DOCVISION_LLM_MODEL=claude-opus-5-5
DOCVISION_LLM_API_KEY=your-api-key
```

The service never picks a model for you. Token usage is reported the same way for every provider: `input_tokens` is everything sent, `cached_tokens` is the part served from the provider's cache, and `output_tokens` includes any reasoning the model did.

**Structured extraction**

| Option | Default | Description |
| --- | --- | --- |
| `extract_schema` | `null` | A JSON Schema with `"type": "object"` at the top, up to 16 KiB. The result's `extracted` field follows it |

See [Structured extraction](#structured-extraction) for how it works.

<br>

### Result

A result is one JSON Lines record: the API returns it, and a destination gets it as a `.jsonl` file (one line).

```json
{
  "schema_version": 2,
  "operation": "convert",
  "job_id": "01a10e96-…",
  "request_id": "01a10e95-…",
  "status": "completed",
  "cache_hit": false,
  "src_file": "https://example.com/report.pdf",
  "src_bytes": 4821337,
  "dest_file": "s3://bucket/out/",
  "output_file": "s3://bucket/out/report.pdf.jsonl",
  "images_dir": "s3://bucket/out/images/",
  "zip_file": "s3://bucket/out/report.pdf.zip",
  "zip_bytes": 0,
  "format": "pdf",
  "title": "Quarterly Report Q3",
  "summary": "Revenue grew …",
  "language": "en",
  "language_detection": {"code": "en", "method": "llm", "confidence": 0.97},
  "search": {
    "keywords": ["quarterly report", "Q3", "revenue", "margin"],
    "entities": ["Acme Corp", "Q3 2026"],
    "questions": ["How much did revenue grow in Q3?", "Why did cloud costs rise?"],
    "topics": ["finance"],
    "type": "report"
  },
  "content": "# Quarterly Report\n\n…",
  "extracted": null,
  "images": [],
  "chunks": null,
  "translated_content": null,
  "translated_language": null,
  "statistics": {
    "original_total_pages": 12,
    "markdown_estimated_total_pages": 8,
    "total_words": 3912,
    "total_characters": 24310,
    "content_bytes": 24655,
    "chunk_count": 0
  },
  "feature_status": {
    "title": {"status": "completed", "method": "llm"},
    "summary": {"status": "completed", "method": "llm"},
    "language": {"status": "completed", "method": "llm"},
    "search": {"status": "completed", "method": "llm"},
    "chunks": {"status": "disabled"}
  },
  "warnings": [],
  "metadata": {"ref": "abc-123"},
  "content_hash": "b3:9f1c…",
  "llm": {
    "calls": [],
    "totals": {
      "provider": "openai",
      "model": "your-model",
      "calls": 2,
      "failed_calls": 0,
      "input_tokens": 9100,
      "output_tokens": 5300,
      "total_tokens": 14400,
      "cached_tokens": 0,
      "reasoning_tokens": null,
      "usage_complete": true
    }
  },
  "timing": {
    "queue_ms": 12,
    "processing_ms": 21480,
    "total_ms": 21492,
    "stages": {"fetch_ms": 310, "extraction_ms": 18350, "enrichment_ms": 2600, "translation_ms": 0, "storage_ms": 40}
  },
  "fetched_at": "2026-11-13T10:02:11Z",
  "converted_at": "2026-11-13T10:02:33Z",
  "error": null
}
```

- `status` is `completed`, `partial` or `failed`.
- **Every result has the same keys.** A feature that is off is `null` (`chunks`, `search`, `extracted`, `translated_*`).
- **Files and sizes** (sizes are in bytes; credentials are never included in paths):

  | Field | Meaning | `null` when |
  | --- | --- | --- |
  | `src_file`, `src_bytes` | The source document and its size | `src_bytes`: the source couldn't be fetched or converted |
  | `dest_file` | The destination as you requested it (a folder or a `.jsonl` location) | no destination (always for sync) |
  | `output_file` | Where this result was written | no destination, `zip_only`, or writing failed |
  | `images_dir` | Where extracted images were written | no destination, no images, or `zip_only` |
  | `zip_file`, `zip_bytes` | Where the zip is, or will be once `job.zip` builds it. `zip_bytes` is `0` until it exists | `zip_file`: sync conversions (`zip_bytes` is then `0`) |

  The size of the written file is `output_bytes` in the `job.finished` webhook and in history (a file can't contain its own size). The Markdown's size is `statistics.content_bytes`.
- `content_hash` is a hash of the source's bytes, to spot documents that changed. `fetched_at` is when a URL source was fetched, `converted_at` when this result was produced (UTC).
- `extracted` holds the structured data when `extract_schema` was given (see below).
- `feature_status` reports each optional feature (`title`, `summary`, `language`, `search`, `chunks`, `translation`, `extraction`) as `completed`, `failed`, `skipped` or `disabled`, with the method used and any error.
- `llm.calls` has one record per attempt (provider, model, status, tokens, duration), including retries and failures.
- Token counts the provider did not report are `null`, never `0`. The service does not calculate prices; use the token counts.

<br>

### Extracted images

With `"extract_images": true`, pictures embedded in the document are saved as files and referenced from the Markdown where they appear:

```markdown
Revenue grew 12% this quarter.

![Revenue chart](images/3f2a9c1b7d4e5a60.png)
```

| Format | Images taken from | Placement |
| --- | --- | --- |
| DOCX | Pictures in the body (`word/media/`) | Where they appear, with the picture's alt text |
| PPTX | Pictures on each slide | In the slide's section, with alt text |
| XLSX | `xl/media/` | Listed under an `## Images` section after the sheets |
| ODT, ODS, ODP | `Pictures/` | Where they appear |
| EPUB | Images inside the book | Where they appear, with alt text |
| HTML | Embedded `data:` images | Where they appear. Linked images (`https://…`) are never fetched; their alt text is kept |

- **One file per picture.** Files are named by a hash of their content, so a picture used twice (or in two documents) is stored once.
- **Skipped:** icons smaller than 32×32 pixels or 256 bytes, objects that aren't images, and anything past 500 images or 64 MiB per document (with a warning).
- **Formats are kept as stored:** PNG, JPEG, GIF, WebP, BMP, TIFF, SVG, EMF or WMF. Nothing is re-encoded.
- **Where the files go:**
  - With a destination, they are written to an `images/` folder next to the `.jsonl` file, so the references work once the content is saved next to it. The result's `images_dir` says where.
  - Without one, download them with `GET /_/images/{file}` (token required). They are kept as long as results (`DOCVISION_RESULT_RETENTION`).
- **In the result**, `images` lists each one:

  ```json
  "images": [
    {
      "path": "images/3f2a9c1b7d4e5a60.png",
      "format": "png",
      "bytes": 48213,
      "width": 640,
      "height": 480,
      "location": "slide 2",
      "alt": "Revenue chart"
    }
  ]
  ```

- **PDFs** (with the `pdf-native` feature): images drawn directly on a page are extracted. JPEG and JPEG 2000 images are kept as they are; 8-bit RGB or grayscale images are saved as PNG. Each image is placed at the end of the text for its page (or for the batch of pages it was read in). Fax (CCITT) and JBIG2 images, and images nested inside forms or patterns, are skipped with a warning. Pages that are mostly a picture are still treated as scans and sent for OCR.
- **Not covered yet:** charts stored as chart data rather than pictures, and images linked from other servers.

<br>

### Zip

A zip holds the result and its images in one file:

```
report.pdf.zip
├── report.pdf.jsonl      the result (the Markdown is its content)
└── images/…              the extracted images (extract_images)
```

Files are stored without compression (pictures barely shrink), so the zip is quick to build. The result inside was written just before the zip, so it shows `zip_bytes: 0`.

**When converting:** `"create_zip": true` (async jobs). The zip is written next to the result (`report.pdf.zip`), or, without a destination, kept with the job and downloaded with `GET /_/jobs/{job_id}/zip`. Add `"zip_only": true` to write only the zip, without the separate `.jsonl` and `images/`.

**Later:** every async result carries `zip_file` (where the zip is or will be) and `zip_bytes` (`0` until it exists). To make it afterwards:

```json
{
  "operation": "job.zip",
  "payload": {"job_id": "01a10e96-…"}
}
```

- Replies `202` with `{"status": "running", "zip_file": …, "zip_bytes": 0}` and builds the zip from the stored result and images. No LLM work is repeated.
- When it is done, each of the job's webhooks gets a `zip.completed` event (`zip.failed` if it could not be built) with `job_id`, `request_id`, `zip_file`, `zip_bytes` and `error`.
- `job.get` and `job.wait` show the same in `zip` (`null` when no zip was asked for).
- Asking again once the zip exists replies `200` with its size. Two requests at once build it only once.
- The job must have finished (`409 job_not_finished` / `409 job_failed`), and its result and images must still be stored (`410 expired` / `410 images_expired`; they are kept for `DOCVISION_RESULT_RETENTION`).
- A later zip only adds the file; the `.jsonl` at the destination is not rewritten, so it still shows `zip_bytes: 0`; use `job.get` or the webhook for the size.

<br>

### crawl

Crawls a site from a start URL and converts every page exactly like `convert`: same payload, same options and defaults, same fields per page, same webhooks. It always runs in the background, and needs a crawl store (`DOCVISION_CRAWL_STORE`).

```json
{
  "operation": "crawl",
  "payload": {
    "source": "https://docs.example.com/",
    "site": "acme-docs",
    "metadata": {"customer": "acme"},
    "request": {"headers": {"Authorization": "Bearer …"}},
    "webhook": {"url": "https://example.com/hook", "events": ["page.finished"]}
  },
  "options": {
    "max_pages": 500,
    "max_depth": 3,
    "include": ["/docs/*"],
    "exclude": ["*/tag/*"]
  }
}
```

Replies `202` with `{"status": "running", "job_id": …, "site": …}`.

**Crawl options** (rejected by `convert`):

| Option | Default | Description |
| --- | --- | --- |
| `max_pages` | `100` | Pages fetched in this run, at most 10000. When pages are left, the run ends as `limit_reached` |
| `max_depth` | `3` | Links followed from the start URL (sitemap pages count as depth 1) |
| `same_site` | `true` | Only follow links to the start URL's site (`www.` and the bare domain count as one) |
| `include`, `exclude` | — | Patterns on the URL path and query, `*` matches anything: `["/docs/*"]`, `["*/tag/*", "*?page=*"]` |
| `use_sitemap` | `true` | Also start from the URLs in the site's sitemaps (`robots.txt` `Sitemap:` lines, or `/sitemap.xml`) |
| `respect_robots` | `true` | Follow `robots.txt` rules and crawl delay for `*` and `docvision` |

`site` names the site in the store (default: the start URL's host; letters, digits, `.`, `-`, `_`). `destination`, `create_zip` and `zip_only` don't apply.

**How a run works**

- Pages are fetched 2 at a time, with the `robots.txt` crawl delay between fetches. A `429` or `503` is retried once after `Retry-After` (at most 60 s).
- Links are taken from each HTML page, without `#fragments` and tracking parameters (`utm_*`, `gclid`, `fbclid`), and each URL is fetched once per run. Images, media, scripts and archives are not fetched; PDFs, Office files and other documents are converted.
- **Unchanged pages are not converted again:** if a page's bytes are the same as in the site's last run, its row is kept and no LLM call is made.
- With `extract_schema`, every page is extracted with the same schema. With `render`, pages are rendered in the browser like `convert`.
- `job.get` shows `progress` while it runs (`{"site", "pages": {"converted", "unchanged", "failed", "queued"}}`), and `job.cancel` stops it; pages already saved stay.
- The run ends as `completed`, `limit_reached`, `cancelled` or `failed`. A run that ends `completed` marks the site's pages it did not reach as `gone`.
- One crawl per site at a time (`409 crawl_running`, with its `job_id`), and at most `DOCVISION_MAX_CRAWLS` at once (`429`).

**What is saved:** crawls are kept in the crawl store, organized by site, until you delete them:

```
sites/<site>/pages.jsonl                 the site now: the latest row of every page (gone pages left out)
sites/<site>/pages/<path>.jsonl          each page's row
sites/<site>/images/<file>               extracted images (rows link to them as images/<file>)
sites/<site>/runs/<job_id>/pages.jsonl   the rows one run produced
```

Each row has the same fields as a `convert` result, plus `url` and `site` (and `operation: "crawl"`). The run's totals are in `job.get`, not on every row, so `llm` holds only `totals`:

```json
{"schema_version": 2, "operation": "crawl", "job_id": "01a4…", "url": "https://docs.example.com/guide/install", "site": "acme-docs",
 "status": "completed", "format": "html", "title": "Installing the CLI", "summary": "…", "language": "en", "search": {"…": "…"},
 "content": "# Installing the CLI\n\n…", "extracted": null, "images": [], "chunks": null, "statistics": {"…": "…"},
 "feature_status": {"…": "…"}, "warnings": [], "metadata": {"customer": "acme"}, "content_hash": "b3:9f1c…",
 "llm": {"totals": {"calls": 1, "input_tokens": 3120, "output_tokens": 410, "…": "…"}},
 "fetched_at": "2026-11-13T10:02:11Z", "converted_at": "2026-11-13T10:02:14Z", "error": null}
```

A page that failed is a row too, with `status: "failed"` and `error`. `request` values never appear in rows: their `query` parameters are removed from `url`.

**The result** of the run (`job.get` → `result`, and the `job.finished` webhook) says where everything is:

```json
{"schema_version": 2, "operation": "crawl", "job_id": "01a4…", "status": "completed", "site": "acme-docs",
 "src_file": "https://docs.example.com/", "pages": {"converted": 212, "unchanged": 40, "failed": 3, "queued": 0},
 "pages_file": "s3://bucket/crawls/sites/acme-docs/runs/01a4…/pages.jsonl",
 "site_pages_file": "s3://bucket/crawls/sites/acme-docs/pages.jsonl",
 "metadata": {"customer": "acme"}, "llm": {"totals": {"…": "…"}}, "timing": {"total_ms": 384200}, "error": null}
```

The run's rows can also be downloaded with `GET /_/jobs/{job_id}/pages.jsonl` while the run's result is kept.

<br>

### Sites

The database keeps an index of crawled pages; the crawl store holds the pages themselves.

| Operation | Payload | Returns |
| --- | --- | --- |
| `site.list` | — | `sites`: each site with its page count, last run and `pages_file` |
| `site.pages` | `site`, optional `status`, `prefix` (URL prefix), `limit` (1–1000, default 100), `cursor` | `pages` in URL order (url, status, title, content_hash, job_id, dates, error, file) and `next_cursor` |
| `site.page` | `site`, `url` | That page's index entry plus its full `row` |
| `site.reindex` | optional `site` | Rebuilds the index from the store (all sites, or one); `{"sites", "pages"}` |

`site.reindex` reads every `sites/<site>/pages/*.jsonl` back, so a lost or new database, or a second deployment pointed at the same store, gets the same index.

<br>

### summarize

```json
{
  "operation": "summarize",
  "payload": {
    "source_content": "# Notes\n\nWe agreed to …"
  },
  "options": {
    "title_method": "local"
  }
}
```

- Pass the text in `source_content`, or a text/Markdown file in `source`. `requester_id` and `metadata` are optional.
- Options: `gen_title`, `gen_summary`, `title_method`, `summary_method`, `llm_*`.
- Returns `title`, `summary`, `feature_status`, `warnings` and `llm`.

<br>

### chunk

```json
{
  "operation": "chunk",
  "payload": {
    "source_content": "# Guide\n\n…"
  },
  "options": {
    "chunk_size": 800,
    "chunk_overlap": 80
  }
}
```

- Returns `{"chunks": [...]}`, in the same format as the result's chunks.
- Inline text up to 4 MiB is accepted by `summarize`, `chunk` and `extract`. Other requests are limited to 256 KB.

<br>

### extract

Structured JSON from text or Markdown you already have. To extract from a PDF, image or Office file, use `convert` with `extract_schema` instead.

```json
{
  "operation": "extract",
  "payload": {
    "source_content": "Corner Café … Latte x2 9.00 … Total 9.00 USD",
    "requester_id": "expenses"
  },
  "options": {
    "extract_schema": {
      "type": "object",
      "required": ["merchant", "total"],
      "properties": {
        "merchant": { "type": "string" },
        "total": { "type": "number" },
        "currency": { "type": ["string", "null"] }
      }
    }
  }
}
```

- `source_content` or a text/Markdown `source`, as in `summarize`.
- `extract_schema` is required. The `llm_*` options are accepted.
- Returns `extracted`, `feature_status`, `warnings` and `llm`.
- If the model can't produce data that matches the schema, it returns `422 extraction_failed`.

<br>

### Structured extraction

Add `extract_schema` to `convert` (or use `extract` for plain text) to get JSON back alongside the Markdown. For example, converting a receipt photo:

```json
{
  "operation": "convert",
  "payload": {
    "source": "s3://bucket/receipts/r-1042.jpg"
  },
  "options": {
    "execution": "sync",
    "extract_schema": {
      "type": "object",
      "required": ["merchant", "total"],
      "properties": {
        "merchant": { "type": "string" },
        "date": { "type": ["string", "null"], "description": "YYYY-MM-DD" },
        "currency": { "type": ["string", "null"] },
        "total": { "type": "number" },
        "items": {
          "type": "array",
          "items": {
            "type": "object",
            "properties": {
              "name": { "type": "string" },
              "qty": { "type": "number" },
              "price": { "type": "number" }
            }
          }
        }
      }
    }
  }
}
```

The result keeps `content` (the Markdown) and adds:

```json
{
  "extracted": {
    "merchant": "Corner Café",
    "date": "2026-10-04",
    "currency": "USD",
    "total": 9.0,
    "items": [
      { "name": "Latte", "qty": 2, "price": 4.5 }
    ]
  },
  "feature_status": {
    "extraction": { "status": "completed", "method": "llm" }
  }
}
```

**How it works**

- The document is converted to Markdown first. One extra LLM call then reads that Markdown and returns JSON for your schema, so it works for every format.
- The model is told to use only facts from the document and to return `null` for anything that is missing, never to guess. Use `["string", "null"]` style types for fields that may be absent.
- OpenAI and Anthropic enforce the schema with structured outputs. Gemini and OpenAI-compatible APIs use JSON mode.
- Every answer is checked against `type`, `properties`, `required`, `items` and `enum`. Other keywords (`description`, `format`, …) guide the model but aren't checked. If the answer doesn't match, the model gets one retry with the reason.
- **If extraction fails,** the conversion still succeeds: `extracted` is `null`, `feature_status.extraction` is `failed` with the reason, and a warning is added. On `extract`, a failure returns `422 extraction_failed`.
- **Requirements:** `extract_schema` needs an LLM. Without one configured (or passed in `llm_*`), the request is rejected with `400 provider_not_configured`.
- **Long documents:** content beyond the model's input budget is cut, with a warning.
- **Caching:** the schema is part of the cache key, so a different schema means a new extraction.
- **Webhooks:** templates can pick fields, e.g. `{"total": "$.extracted.total"}`.

<br>

### job.get

```json
{
  "operation": "job.get",
  "payload": {
    "job_id": "01a10e96-…",
    "fields": ["status", "content", "title"]
  }
}
```

- Returns `job_id`, `request_id`, `status` (`queued`, `running`, `completed`, `partial`, `failed`; crawls also `limit_reached`, `cancelled`), timestamps, `error`, `zip` (see [Zip](#zip)), `progress` (a running crawl), `webhooks` (delivery status) and `result`.
- `result` is `null` until the job is done.
- `fields` limits the result to those top-level fields.

<br>

### job.cancel

```json
{"operation": "job.cancel", "payload": {"job_id": "01a4…"}}
```

Stops a running crawl (`202`, `{"status": "cancelling"}`); it ends as `cancelled`, and the pages already saved stay. Other jobs can't be cancelled (`409`).

<br>

### job.wait

```json
{
  "operation": "job.wait",
  "payload": {
    "job_id": "01a10e96-…",
    "timeout_ms": 30000
  }
}
```

Waits for a state change (30 s at most). Returns the same data as `job.get` without `result`, plus `changed: true|false`.

<br>

### history.list

```json
{
  "operation": "history.list",
  "payload": {
    "limit": 25,
    "cursor": null,
    "operation": "convert",
    "execution": "async",
    "execution_status": "completed",
    "requester_id": "billing-service",
    "format": "pdf",
    "in_progress": false,
    "created_from": "2026-10-01T00:00:00Z",
    "created_to": "2026-10-02T00:00:00Z"
  }
}
```

- Every filter is optional. `limit` is 1–100.
- `in_progress: true` returns only requests that are `queued` or `running`.
- `format` filters by detected document format (`pdf`, `docx`, `xlsx`, `pptx`, `html`, `markdown`, `text`, `png`, `jpeg`, `webp`, …).
- Each item includes `requester_id`, `format`, `llm_provider` and `llm_model` (`null` when no LLM was used), status, timings, statistics and token usage.
- Each item also carries the file fields of the result: `source`, `src_bytes`, `dest_file`, `output_file`, `output_bytes`, `zip_file` and `zip_bytes`. They stay in history after the result itself has expired.
- `cache_hit` says whether the request was served from the result cache.
- Returns `{"items": [...], "has_more": true, "next_cursor": "…"}`.
- Pass `next_cursor` back as `cursor` to get the next page.
- History never stores tokens, credentials, document content or prompts.

<br>

### history.get

```json
{
  "operation": "history.get",
  "payload": {
    "request_id": "01a10e96-…",
    "limit": 100
  }
}
```

Returns `request` (the same fields as a `history.list` item, plus options and metadata), `events` (stage timeline) and `calls` (LLM attempts). Events and calls are paginated with `events_cursor` and `calls_cursor`.

<br>

### stats

```json
{
  "operation": "stats",
  "payload": {
    "window": "24h"
  }
}
```

- `window` is `1h`, `24h`, `7d` or `30d`.
- Returns request counts by status, operation, format, top requesters (`by_requester`) and models (`by_model`); duration avg/p50/p95/max; pages and words; LLM calls and tokens; recent failures; and a live snapshot (running, queued, memory, provider state).

<br>

### health

```json
{
  "operation": "health"
}
```

- Returns `status`, `version`, `features`, `uptime_ms`, `database` (`sqlite` or `postgres`), `db_writer`, `queue`, `memory` and the `providers` breaker states (`closed`, `open`, `half_open`).
- Returns `503 not_ready` when the database writer is down or lagging.

<br>

## Webhooks

Called after an async job finishes (`webhooks` feature). `payload.webhook` is one webhook object, or an array of up to 10. Each endpoint is delivered independently, with its own event ID, retries and status.

```json
[
  {
    "url": "https://example.com/hook",
    "headers": {
      "Authorization": "Bearer xyz"
    },
    "secret": "hmac-secret",
    "body": {
      "doc_id": "$.metadata.ref",
      "markdown": "$.content",
      "title": "$.title",
      "first_chunk": "$.chunks[0].content",
      "source": "docvision"
    }
  },
  {
    "url": "https://example.com/notify",
    "fire_and_forget": true
  }
]
```

| Field | Description |
| --- | --- |
| `url` | `https://` (`http://` only for localhost) |
| `headers` | Extra headers, up to 32. Stored encrypted |
| `secret` | Adds an `x-docvision-signature` header |
| `body` | Optional JSON template, up to 16 KiB |
| `fire_and_forget` | `false`. When `true`, one attempt and no retries. Any HTTP response marks it `sent` (the status code is ignored). Only a connection failure marks it `failed` |
| `events` | Extra events besides `job.finished`: `["page.finished"]` sends each crawled page's row as it is saved |

**Body template**

| Value | Becomes |
| --- | --- |
| `"$.content"`, `"$.metadata.ref"` | That field of the [result](#result). Missing fields become `null` |
| `"$.chunks[0].content"` or `"$.chunks.0.content"` | An array item |
| `"$"` | The whole result. As the entire `body`, the result file is streamed as-is |
| `"$.event_id"`, `"$.event"` | Event ID and `"job.finished"` |
| `"$$text"` | The literal string `"$text"` |
| anything else | Sent unchanged |

Without `body`, a short event is sent: `event_id`, `event`, `job_id`, `request_id`, `operation`, `status`, the file fields (`src_file`, `src_bytes`, `dest_file`, `output_file`, `output_bytes`, `images_dir`, `zip_file`, `zip_bytes`; for a crawl `site`, `pages`, `pages_file`, `site_pages_file`), `metadata`, `statistics`, `llm` (totals), `timing` and `error`. It never includes the content, so the receiver can use the paths and sizes to decide what to fetch.

**Page events** (crawls, with `"events": ["page.finished"]`): one `page.finished` event per saved page, with `event_id`, `event`, `job_id`, `request_id` and the page's row (including its `content`), signed and retried like `job.finished`. A receiver can index pages as they arrive instead of waiting for the run to end.

**Zip events:** after `job.zip`, a `zip.completed` or `zip.failed` event (`event_id`, `event`, `job_id`, `request_id`, `status`, `zip_file`, `zip_bytes`, `error`) is sent to the same endpoints, signed and retried the same way, with its own `event_id`. `body` templates apply only to `job.finished`.

**Headers sent**

| Header | Description |
| --- | --- |
| `x-docvision-event-id` | Same on every retry; use it to deduplicate |
| `x-docvision-timestamp` | Unix seconds |
| `x-docvision-signature` | `sha256=` + hex HMAC-SHA256(`secret`, `timestamp + "." + body`) |

**Delivery status** (in `job.get` → `webhooks`, one entry per endpoint, in request order): `pending`, `delivered`, `sent` (fire-and-forget) or `failed`.

**Retries:** network errors, `429` and `5xx` are retried at 0 s, 30 s, 2 min, 10 min, 20 min and 30 min. Other `4xx` responses are not retried. Delivery is at-least-once, and a failed webhook never changes the job status.

<br>

## Errors

| HTTP | Codes |
| --- | --- |
| `400` | `invalid_json`, `invalid_envelope`, `unknown_operation`, `invalid_payload`, `invalid_options`, `invalid_source`, `invalid_destination`, `invalid_webhook`, `invalid_cursor`, `feature_not_compiled`, `provider_not_configured`, `conversion_required` |
| `401` | `unauthorized` |
| `404` | `job_not_found`, `request_not_found`, `source_not_found` |
| `409` | `destination_exists` |
| `413` | `body_too_large`, `input_too_large`, `inline_text_too_large`, `result_too_large`, `archive_too_large`, `image_too_large`, `too_many_pages`, `job_too_large` |
| `415` | `upload_not_supported`, `unsupported_media_type`, `unsupported_format` (includes legacy `.doc`/`.xls`/`.ppt`) |
| `422` | `corrupt_document`, `unusable_document`, `scanned_pages`, `ocr_required`, `partial_extraction`, `render_failed`, `extraction_failed` |
| `429` | `queue_full` (with `Retry-After`) |
| `502` | `source_fetch_failed`, `source_read_failed`, `provider_error`, `provider_auth_failed`, `destination_write_failed` |
| `503` | `busy`, `db_unavailable`, `provider_unavailable`, `not_ready` |
| `504` | `source_timeout`, `provider_timeout`, `job_timeout` |

<br>

## Security

The access token grants full use of the service, so treat it like a password and give it only to trusted callers. With it, a caller can:

- **Read local files** the service can read (by absolute path). `/proc`, `/sys` and `/dev` are always refused, after symlinks are resolved, so the process environment and its secrets can't be read. In Docker the service sees only its image and what you mount; mount document folders read-only.
- **Write destinations** (`.jsonl` files and `images/`) anywhere the service can write, replacing existing files of those names unless `overwrite: false`. In Docker, that's `/data` and any writable mounts.
- **Fetch URLs** over `https://` (and `http://` on localhost only), including hosts on your internal network. Webhooks follow the same rule. Run the service where it can only reach what it should.
- **Render pages and crawl sites** through the browser container. A page's own requests can reach whatever the browser container can reach; link-local and cloud-metadata addresses (`169.254.x.x`, `metadata.google.internal`) are refused, but other internal addresses are not, so give the browser container only the network access it needs. Crawls respect `robots.txt` by default; fetching other people's sites is subject to their terms.

- **Download the database** (`GET /_/db`, SQLite only): the full request history, including sources, metadata and error messages. Stored job secrets (API keys, S3 credentials, webhook headers) stay encrypted inside it.

What the service guarantees:

- The token is compared in constant time. Tokens, API keys, S3 credentials and webhook headers/secrets are never logged and never stored in plain text: job secrets are encrypted, and history keeps no credentials, signed URL queries, document content or prompts.
- The server's LLM API key is only ever sent to the configured LLM endpoint. A request that points `llm_base_url` elsewhere must bring its own `llm_api_key`.
- API keys echoed in provider error messages are masked.
- Uploads, archives (zip-bomb limits), images (pixel limits), PDFs (page limits) and request bodies are bounded. The Docker image runs as non-root on a read-only filesystem.

<br>

## Build features

| Feature | Default | Adds |
| --- | --- | --- |
| `s3` | on | S3 sources and destinations, `crawl` and the `site.*` operations (the crawl store) |
| `webhooks` | on | Completion webhooks |
| `pdf-native` | on | Native PDF text (`ocr: auto` / `off`) and per-batch PDF splitting |
| `translate` | on | `translate_to` |
| `lang-detect` | on | Local language detection |
| `odf-epub` | on | ODT, ODS, ODP, EPUB |
| `postgres` | on | PostgreSQL (SQLite stays the default database) |
| `pdf-render` | off | Render PDF pages to images for image-only models. Needs the Pdfium library at runtime, so it isn't in the static binary or Docker image |

Everything except `pdf-render` is in the default build (about 11.9 MB). To add it, or to build a smaller binary without some features:

```bash
cargo build --release --features pdf-render
cargo build --release --no-default-features --features s3,webhooks,pdf-native
```

<br>

## Docker

**Docker Compose** (`docker-compose.yml` in the repo):

```yaml
services:
  docvisionllm:
    image: ghcr.io/mardix/docvisionllm:latest
    container_name: docvisionllm
    restart: unless-stopped
    environment:
      DOCVISION_TOKEN: ${DOCVISION_TOKEN:?set DOCVISION_TOKEN}
      DOCVISION_LLM_PROVIDER: ${DOCVISION_LLM_PROVIDER:-openai}
      DOCVISION_LLM_MODEL: ${DOCVISION_LLM_MODEL:-}
      DOCVISION_LLM_API_KEY: ${DOCVISION_LLM_API_KEY:-}
      DOCVISION_LLM_BASE_URL: ${DOCVISION_LLM_BASE_URL:-}
      DOCVISION_BROWSER_URL: http://browser:9222
      DOCVISION_CRAWL_STORE: ${DOCVISION_CRAWL_STORE:-}
    ports:
      - "4242:4242"
    volumes:
      - docvision-data:/data
      # - ./documents:/documents:ro
    read_only: true
    mem_limit: 512M
    cpus: 1.0

  browser:
    image: chromedp/headless-shell:latest
    container_name: docvision-browser
    restart: unless-stopped
    shm_size: 1g
    mem_limit: 2g
    cpus: 2.0

volumes:
  docvision-data:
```

**The browser container** runs headless Chrome for JavaScript pages (`render`, `crawl`), so the service's own image stays small. It has no published port: only the service talks to it. Remove the `browser` service and `DOCVISION_BROWSER_URL` if you don't need rendering; web pages are then converted as fetched.

```bash
DOCVISION_TOKEN=your-long-random-token DOCVISION_LLM_MODEL=your-model DOCVISION_LLM_API_KEY=your-key docker compose up -d
```

Compose also reads these values from a `.env` file next to `docker-compose.yml`. Any other `DOCVISION_*` setting from [Configuration](#configuration) can be added under `environment`.

**Plain Docker:**

```bash
docker build -t docvisionllm .
docker run --read-only -v docvision-data:/data -p 4242:4242 --env-file .env docvisionllm
```

- The image is distroless and runs as non-root. `/data` is the only folder it writes to.
- Mount documents read-only, e.g. `-v /srv/docs:/docs:ro`, then use `"source": "/docs/a.pdf"`.
- On SIGTERM, running work gets 30 s to finish. Queued jobs resume on restart. A crawl that was running ends as failed (`interrupted`); start it again, and the pages it already converted are kept (unchanged pages are not converted again).

<br>

## Development

Requires Rust 1.88 or newer (`rustup update`, or `brew upgrade rust`).

```bash
./scripts/run-local.sh --debug     # fast rebuilds
cargo test --all-features          # uses a mock LLM
```

Set `CARGO_TARGET_DIR` outside synced folders (Dropbox, iCloud). `run-local.sh` uses `~/.cache/docvisionllm/target` by default.
