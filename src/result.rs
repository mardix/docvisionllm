//! Result schema and streaming serialization.
//!
//! A result file is one JSON object written as three fragments:
//! header (per request: ids, status, locations, metadata), body (content-derived, cacheable)
//! and trailer (error, LLM accounting, timing). Cache hits copy the body byte range verbatim.

use crate::llm::LlmSection;
use crate::markdown::{ChunkRange, ChunksSer};
use crate::rpc::{AppError, ErrorBody};
use crate::source::Format;
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: u32 = 2;

/// An extracted image as listed in the result. `path` is relative to the Markdown file.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct ImageMeta {
    pub path: String,
    pub format: String,
    pub bytes: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Where it appears ("slide 3", "page 2", "workbook"), when known.
    pub location: Option<String>,
    pub alt: Option<String>,
}

impl ImageMeta {
    /// File name inside the images folder (`<hash>.<ext>`).
    pub fn file_name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Header<'a> {
    pub schema_version: u32,
    pub operation: &'a str,
    pub job_id: Option<&'a str>,
    pub request_id: &'a str,
    pub status: &'a str,
    pub cache_hit: bool,
    pub src_file: &'a str,
    /// Size of the source document (`null` when it could not be fetched or converted).
    pub src_bytes: Option<u64>,
    /// The destination as requested (a folder or a `.jsonl` location).
    pub dest_file: Option<&'a str>,
    /// Where this result was written as JSON Lines (`null` without a destination).
    pub output_file: Option<&'a str>,
    /// Where extracted images were written (`null` without a destination or without images).
    pub images_dir: Option<&'a str>,
    /// Where the zip is (or will be, via `job.zip`) for an async job; `zip_bytes` is 0 until it
    /// exists.
    pub zip_file: Option<&'a str>,
    pub zip_bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Statistics {
    pub original_total_pages: Option<u32>,
    pub original_page_count_method: Option<&'static str>,
    pub original_page_count_exact: bool,
    pub markdown_estimated_total_pages: u64,
    pub markdown_words_per_page: u32,
    pub total_words: u64,
    pub word_count_method: &'static str,
    pub total_characters: u64,
    pub content_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sheet_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slide_count: Option<u32>,
    pub chunk_count: usize,
    /// Images extracted (`extract_images`).
    pub image_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct FeatureStatus {
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl FeatureStatus {
    pub fn disabled() -> FeatureStatus {
        FeatureStatus { status: "disabled", method: None, error: None }
    }
    pub fn done(method: &'static str) -> FeatureStatus {
        FeatureStatus { status: "completed", method: Some(method), error: None }
    }
    pub fn failed(method: &'static str, e: String) -> FeatureStatus {
        FeatureStatus { status: "failed", method: Some(method), error: Some(e) }
    }
    /// The LLM did not deliver; the local method was used instead (`error` says why).
    pub fn fallback(e: String) -> FeatureStatus {
        FeatureStatus { status: "completed", method: Some("local_fallback"), error: Some(e) }
    }
}

/// What a document is about, for search: written by the LLM with the title and summary.
#[derive(Debug, Clone, Default, Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Search {
    pub keywords: Vec<String>,
    pub entities: Vec<String>,
    pub questions: Vec<String>,
    pub topics: Vec<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LanguageDetection {
    pub code: String,
    pub method: &'static str,
    pub confidence: f64,
}

/// Content-derived part of a result (shared by cache hits and single-flight joiners).
#[derive(Debug, Clone, Default)]
pub struct Body {
    pub format: Option<Format>,
    pub content: String,
    pub title: Option<String>,
    pub summary: Option<String>,
    /// Terms, entities and questions for finding this document (`gen_search`).
    pub search: Option<Search>,
    /// Structured data matching `extract_schema` (`None` when not requested or failed).
    pub extracted: Option<serde_json::Value>,
    /// Extracted images (`extract_images`), referenced from `content` as `images/<file>`.
    pub images: Vec<ImageMeta>,
    pub chunks: Vec<ChunkRange>,
    pub language: Option<String>,
    pub language_detection: Option<LanguageDetection>,
    pub translated_content: Option<String>,
    pub translated_language: Option<String>,
    pub statistics: Statistics,
    pub translated_statistics: Option<Statistics>,
    pub feature_status: BTreeMap<&'static str, FeatureStatus>,
    pub warnings: Vec<String>,
    pub partial: bool,
    /// Stage durations of the conversion that produced this body (not serialized).
    pub stages: Stages,
}

impl Serialize for Body {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct View<'a> {
            format: Option<Format>,
            title: &'a Option<String>,
            summary: &'a Option<String>,
            language: &'a Option<String>,
            language_detection: &'a Option<LanguageDetection>,
            search: &'a Option<Search>,
            content: &'a str,
            extracted: &'a Option<serde_json::Value>,
            images: &'a [ImageMeta],
            chunks: Option<ChunksSer<'a>>,
            translated_content: &'a Option<String>,
            translated_language: &'a Option<String>,
            statistics: &'a Statistics,
            translated_statistics: &'a Option<Statistics>,
            feature_status: &'a BTreeMap<&'static str, FeatureStatus>,
            warnings: &'a [String],
        }
        View {
            format: self.format,
            title: &self.title,
            summary: &self.summary,
            language: &self.language,
            language_detection: &self.language_detection,
            search: &self.search,
            content: &self.content,
            extracted: &self.extracted,
            images: &self.images,
            // `null` when chunking was off (or produced nothing), so every result has the key.
            chunks: (!self.chunks.is_empty()).then_some(ChunksSer { content: &self.content, chunks: &self.chunks }),
            translated_content: &self.translated_content,
            translated_language: &self.translated_language,
            statistics: &self.statistics,
            translated_statistics: &self.translated_statistics,
            feature_status: &self.feature_status,
            warnings: &self.warnings,
        }
        .serialize(s)
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Stages {
    pub fetch_ms: u64,
    pub extraction_ms: u64,
    pub enrichment_ms: u64,
    pub translation_ms: u64,
    pub storage_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Timing {
    pub queue_ms: u64,
    pub processing_ms: u64,
    pub total_ms: u64,
    pub stages: Stages,
    pub llm_wall_ms: u64,
    pub llm_call_sum_ms: u64,
    pub llm_retry_wait_ms: u64,
}

#[derive(Serialize)]
pub struct Trailer<'a> {
    pub metadata: Option<&'a serde_json::Map<String, serde_json::Value>>,
    /// Hash of the source bytes (`b3:<hex>`), to spot changed documents.
    pub content_hash: Option<&'a str>,
    pub llm: &'a LlmSection,
    pub timing: &'a Timing,
    /// When a URL source was fetched, and when this result was produced (RFC 3339, UTC).
    pub fetched_at: Option<&'a str>,
    pub converted_at: &'a str,
    pub error: Option<&'a ErrorBody>,
}

pub enum BodySrc<'a> {
    Body(&'a Body),
    /// A body fragment copied from a cached result file.
    Raw(&'a [u8]),
}

/// Writes `{fields}` of a serialized struct without the surrounding braces.
struct Fragment<'w, W: Write> {
    inner: &'w mut W,
    first: bool,
    held: Option<u8>,
}

impl<W: Write> Write for Fragment<'_, W> {
    fn write(&mut self, mut buf: &[u8]) -> io::Result<usize> {
        let n = buf.len();
        if self.first && !buf.is_empty() {
            self.first = false;
            buf = &buf[1..];
        }
        if buf.is_empty() {
            return Ok(n);
        }
        if let Some(h) = self.held.take() {
            self.inner.write_all(&[h])?;
        }
        self.inner.write_all(&buf[..buf.len() - 1])?;
        self.held = Some(buf[buf.len() - 1]);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn write_fields<W: Write>(w: &mut W, v: &impl Serialize) -> io::Result<()> {
    let mut f = Fragment { inner: w, first: true, held: None };
    serde_json::to_writer(&mut f, v).map_err(io::Error::other)
}

/// Counts bytes and enforces `DOCVISION_MAX_RESULT_BYTES`.
pub struct Counting<W> {
    pub inner: W,
    pub n: u64,
    pub limit: u64,
}

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.n += buf.len() as u64;
        if self.n > self.limit {
            return Err(io::Error::new(io::ErrorKind::FileTooLarge, "result exceeds DOCVISION_MAX_RESULT_BYTES"));
        }
        self.inner.write_all(buf)?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Serialize a full result (`{header fields, body fields, trailer fields}` and a newline);
/// returns the body byte range within the output. A crawl page uses its own header and trailer.
pub fn write_result<W: Write>(w: W, limit: u64, h: &impl Serialize, body: BodySrc, t: &impl Serialize) -> io::Result<(u64, u64, W)> {
    let mut c = Counting { inner: w, n: 0, limit };
    c.write_all(b"{")?;
    write_fields(&mut c, h)?;
    c.write_all(b",")?;
    let start = c.n;
    match body {
        BodySrc::Body(b) => write_fields(&mut c, b)?,
        BodySrc::Raw(r) => c.write_all(r)?,
    }
    let end = c.n;
    c.write_all(b",")?;
    write_fields(&mut c, t)?;
    // One result is one JSON Lines record.
    c.write_all(b"}\n")?;
    c.flush()?;
    Ok((start, end, c.inner))
}

pub fn result_bytes(limit: u64, h: &Header, body: BodySrc, t: &Trailer) -> Result<Vec<u8>, AppError> {
    write_result(Vec::new(), limit, h, body, t).map(|(_, _, v)| v).map_err(map_io)
}

pub fn map_io(e: io::Error) -> AppError {
    if e.kind() == io::ErrorKind::FileTooLarge {
        AppError::new(413, "result_too_large", e.to_string()).stage("storage")
    } else {
        AppError::internal(format!("result serialization failed: {}", e.kind())).stage("storage")
    }
}

/// Size of the body fragment when serialized (used to compute self-referential file sizes).
pub fn body_len(body: BodySrc) -> io::Result<u64> {
    match body {
        BodySrc::Raw(r) => Ok(r.len() as u64),
        BodySrc::Body(b) => {
            let mut c = Counting { inner: io::sink(), n: 0, limit: u64::MAX };
            write_fields(&mut c, b)?;
            Ok(c.n)
        }
    }
}

/// Write a result file atomically (temp + rename). Blocking: call on the CPU lane.
pub fn write_file_atomic(path: &Path, limit: u64, h: &Header, body: BodySrc, t: &Trailer) -> io::Result<(u64, u64, u64)> {
    let tmp = path.with_extension("tmp");
    let f = std::fs::File::create(&tmp)?;
    let res = write_result(io::BufWriter::with_capacity(64 * 1024, f), limit, h, body, t).and_then(|(s, e, w)| {
        let f = w.into_inner().map_err(|e| e.into_error())?;
        f.sync_data()?;
        Ok((s, e, f.metadata()?.len()))
    });
    match res {
        Ok(r) => {
            std::fs::rename(&tmp, path)?;
            Ok(r)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

// ---------------------------------------------------------------- destinations

#[derive(Debug, Clone)]
pub enum Dest {
    Local(PathBuf),
    #[cfg_attr(not(feature = "s3"), allow(dead_code))]
    S3(crate::source::S3Loc, String),
}

/// A destination is a `.jsonl` file, or a folder (ending in `/`) where the output is named after
/// the source file: `hello.pdf` -> `hello.pdf.jsonl`.
pub fn parse_dest(s: &str, source: &str) -> Result<Dest, AppError> {
    let bad = |m: &str| AppError::bad_request("invalid_destination", m.to_string());
    let (loc, query) = if s.starts_with("s3://") { s.split_once('?').map_or((s, None), |(a, q)| (a, Some(q))) } else { (s, None) };
    let in_folder;
    let s = if loc.ends_with('/') {
        let name = crate::source::file_name(source);
        let name = name.trim();
        let name = if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) { "document" } else { name };
        // S3 locations are URLs: the name is percent-encoded so `#`, `?`, `%` or spaces survive.
        let name = if loc.starts_with("s3://") {
            const KEY: &percent_encoding::AsciiSet =
                &percent_encoding::NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');
            percent_encoding::utf8_percent_encode(name, KEY).to_string()
        } else {
            name.to_string()
        };
        in_folder = format!("{loc}{name}.jsonl{}", query.map(|q| format!("?{q}")).unwrap_or_default());
        in_folder.as_str()
    } else {
        s
    };
    let key_or_path = if s.starts_with("s3://") { s.split('?').next().unwrap_or(s) } else { s };
    if !key_or_path.ends_with(".jsonl") || key_or_path.ends_with("/.jsonl") {
        return Err(bad("destination must be a folder ending in / or name a .jsonl file"));
    }
    if s.starts_with('/') {
        return Ok(Dest::Local(PathBuf::from(s)));
    }
    if let Some(p) = s.strip_prefix("file://") {
        return Ok(Dest::Local(PathBuf::from(p)));
    }
    if s.starts_with("s3://") {
        if !cfg!(feature = "s3") {
            return Err(AppError::feature_not_compiled("s3"));
        }
        return crate::source::parse_s3(s).map(|l| Dest::S3(l, crate::source::sanitize(s))).map_err(|e| bad(&e));
    }
    Err(bad("destination must be an absolute path, file:/// URL or s3:// location"))
}

/// `report.jsonl` -> `report.zip`: where the zip is written.
pub fn zip_sibling(out: &str) -> String {
    format!("{}.zip", out.strip_suffix(".jsonl").unwrap_or(out))
}

/// Write a zip of the result (`<name>.jsonl`) and its images (`images/…`) atomically and
/// return its size. Entries are stored without compression (pictures barely shrink); files are
/// copied from disk, never held in memory. Blocking: call on the CPU lane.
pub fn write_zip(path: &Path, name: &str, result: &Path, images: &[(PathBuf, String)]) -> io::Result<u64> {
    use zip::write::SimpleFileOptions;
    let tmp = path.with_extension(format!("zip-{}.tmp", uuid::Uuid::now_v7()));
    let res = (|| {
        let f = std::fs::File::create(&tmp)?;
        let mut z = zip::ZipWriter::new(io::BufWriter::with_capacity(64 * 1024, f));
        let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored).large_file(true);
        let entries = std::iter::once((result.to_path_buf(), format!("{name}.jsonl")))
            .chain(images.iter().map(|(src, file)| (src.clone(), format!("images/{file}"))));
        for (src, entry) in entries {
            let mut f = std::fs::File::open(&src)?;
            z.start_file(entry, opts).map_err(io::Error::other)?;
            io::copy(&mut f, &mut z)?;
        }
        let w = z.finish().map_err(io::Error::other)?;
        let f = w.into_inner().map_err(|e| e.into_error())?;
        f.sync_data()?;
        f.metadata().map(|m| m.len())
    })();
    match res {
        Ok(n) => {
            std::fs::rename(&tmp, path)?;
            Ok(n)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Base name for the result inside a zip: the destination's file name without `.jsonl`, or the
/// source file name.
pub fn zip_name(dest: Option<&Dest>, source: &str) -> String {
    let from_dest = dest.and_then(|d| {
        let shown = d.display();
        let base = shown.split('?').next().unwrap_or(&shown).rsplit('/').next()?.to_string();
        let base = percent_encoding::percent_decode_str(&base).decode_utf8_lossy().to_string();
        base.strip_suffix(".jsonl").map(str::to_string)
    });
    let name = from_dest.unwrap_or_else(|| crate::source::file_name(source).trim().to_string());
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) { "document".into() } else { name }
}

impl Dest {
    /// The `.jsonl` file this destination writes.
    pub fn display(&self) -> String {
        match self {
            Dest::Local(p) => p.display().to_string(),
            Dest::S3(_, s) => s.clone(),
        }
    }
    /// A sibling of the output file (`images/`, `<name>.zip`), keeping an S3 query string.
    fn sibling(&self, f: impl Fn(&str) -> String) -> String {
        match self {
            Dest::Local(p) => f(&p.display().to_string()),
            Dest::S3(_, s) => {
                let (base, q) = s.split_once('?').map(|(a, b)| (a, Some(b))).unwrap_or((s, None));
                let d = f(base);
                q.map(|q| format!("{d}?{q}")).unwrap_or(d)
            }
        }
    }
    /// Where extracted images go: an `images/` folder next to the output file.
    pub fn images_display(&self) -> String {
        self.sibling(|out| match out.rsplit_once('/') {
            Some((dir, _)) => format!("{dir}/images/"),
            None => "images/".to_string(),
        })
    }
    pub fn zip_display(&self) -> String {
        self.sibling(zip_sibling)
    }
}

fn exists_err(what: &str) -> AppError {
    AppError::new(409, "destination_exists", format!("{what} already exists; set options.overwrite=true to replace it")).stage("storage")
}

fn local_put(src: Option<&Path>, bytes: Option<&[u8]>, dest: &Path, overwrite: bool) -> Result<(), AppError> {
    let io =
        |e: io::Error| AppError::new(502, "destination_write_failed", format!("cannot write destination: {}", e.kind())).stage("storage");
    let dir = dest.parent().ok_or_else(|| AppError::bad_request("invalid_destination", "destination has no parent directory"))?;
    let real_dir = std::fs::canonicalize(dir).map_err(io)?;
    if crate::source::forbidden_path(dest) || crate::source::forbidden_path(&real_dir) {
        return Err(AppError::bad_request("invalid_destination", "this location can't be used as a destination"));
    }
    let tmp = dir.join(format!(".docvision-{}.tmp", uuid::Uuid::now_v7()));
    match (src, bytes) {
        (Some(s), _) => std::fs::copy(s, &tmp).map(|_| ()).map_err(io)?,
        (_, Some(b)) => std::fs::write(&tmp, b).map_err(io)?,
        _ => return Ok(()),
    }
    let res = if overwrite {
        std::fs::rename(&tmp, dest).map_err(io)
    } else {
        // hard_link fails if the destination exists: atomic create-if-absent.
        match std::fs::hard_link(&tmp, dest) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(exists_err(&dest.display().to_string())),
            Err(e) => Err(io(e)),
        }
    };
    let _ = std::fs::remove_file(&tmp);
    res
}

#[cfg(feature = "s3")]
pub fn s3_store(loc: &crate::source::S3Loc) -> Result<(object_store::aws::AmazonS3, object_store::path::Path), String> {
    use object_store::aws::AmazonS3Builder;
    // DSN credentials apply only to this store; never to process-global AWS settings.
    let mut b = match (&loc.access_key, &loc.secret_key) {
        (Some(a), Some(s)) => {
            let mut b = AmazonS3Builder::new().with_access_key_id(a.expose()).with_secret_access_key(s.expose());
            if let Some(t) = &loc.session_token {
                b = b.with_token(t.expose());
            }
            b
        }
        _ => AmazonS3Builder::from_env(),
    };
    b = b.with_bucket_name(&loc.bucket);
    match (&loc.region, &loc.access_key) {
        (Some(r), _) => b = b.with_region(r),
        // DSN without a region: us-east-1. Plain s3:// keeps the region from the environment.
        (None, Some(_)) => b = b.with_region("us-east-1"),
        (None, None) => {}
    }
    if let Ok(ep) = std::env::var("DOCVISION_S3_ENDPOINT") {
        b = b.with_endpoint(ep).with_allow_http(true).with_virtual_hosted_style_request(false);
    }
    let store = b.build().map_err(|e| format!("invalid S3 configuration: {e}"))?;
    Ok((store, object_store::path::Path::from(loc.key.as_str())))
}

#[cfg(feature = "s3")]
async fn s3_put(loc: &crate::source::S3Loc, key: &str, data: bytes::Bytes, overwrite: bool, shown: &str) -> Result<(), AppError> {
    use object_store::{ObjectStore, PutMode, PutOptions};
    let mut l = loc.clone();
    l.key = key.to_string();
    let (store, path) = s3_store(&l).map_err(|e| AppError::bad_request("invalid_destination", e))?;
    let mode = if overwrite { PutMode::Overwrite } else { PutMode::Create };
    match store.put_opts(&path, data.into(), PutOptions { mode, ..Default::default() }).await {
        Ok(_) => Ok(()),
        Err(object_store::Error::AlreadyExists { .. }) | Err(object_store::Error::Precondition { .. }) => Err(exists_err(shown)),
        Err(_) => Err(AppError::new(502, "destination_write_failed", format!("cannot write {shown}")).stage("storage")),
    }
}

/// Publish the result (`.jsonl`) and its images (`images/` next to it) to the destination,
/// plus a built zip (`<name>.zip`). With `zip_only`, only the zip is written. Everything is
/// written before the caller marks completion; any failure is returned, never hidden.
/// `images`: (stored path, file name); named by content hash, so an existing file is already
/// the same image.
pub async fn publish(
    dest: &Dest,
    result_file: &Path,
    overwrite: bool,
    images: &[(PathBuf, String)],
    zip: Option<&Path>,
    zip_only: bool,
) -> Result<(), AppError> {
    if let Some(z) = zip {
        publish_zip(dest, z, overwrite).await?;
    }
    if zip_only {
        return Ok(());
    }
    match dest {
        Dest::Local(p) => {
            let (p, src, images) = (p.clone(), result_file.to_path_buf(), images.to_vec());
            tokio::task::spawn_blocking(move || {
                local_put(Some(&src), None, &p, overwrite)?;
                if !images.is_empty() {
                    let io = |e: io::Error| {
                        AppError::new(502, "destination_write_failed", format!("cannot write images: {}", e.kind())).stage("storage")
                    };
                    let dir = p.parent().unwrap_or(Path::new("/")).join("images");
                    std::fs::create_dir_all(&dir).map_err(io)?;
                    for (src, name) in &images {
                        let target = dir.join(name);
                        if !target.exists() {
                            local_put(Some(src), None, &target, true)?;
                        }
                    }
                }
                Ok(())
            })
            .await
            .map_err(|_| AppError::internal("publish task failed"))?
        }
        #[cfg(feature = "s3")]
        Dest::S3(loc, shown) => {
            let data = tokio::fs::read(result_file).await.map_err(|_| AppError::internal("cannot read result file"))?;
            s3_put(loc, &loc.key, data.into(), overwrite, shown).await?;
            let prefix = loc.key.rsplit_once('/').map(|(d, _)| format!("{d}/images/")).unwrap_or_else(|| "images/".into());
            for (src, name) in images {
                let data = tokio::fs::read(src).await.map_err(|_| AppError::internal("cannot read extracted image"))?;
                s3_put(loc, &format!("{prefix}{name}"), data.into(), true, &dest.images_display()).await?;
            }
            Ok(())
        }
        #[cfg(not(feature = "s3"))]
        Dest::S3(..) => Err(AppError::feature_not_compiled("s3")),
    }
}

/// Write a built zip next to the destination's Markdown (`<name>.zip`).
pub async fn publish_zip(dest: &Dest, zip: &Path, overwrite: bool) -> Result<(), AppError> {
    match dest {
        Dest::Local(p) => {
            let (target, zip) = (PathBuf::from(zip_sibling(&p.display().to_string())), zip.to_path_buf());
            tokio::task::spawn_blocking(move || local_put(Some(&zip), None, &target, overwrite))
                .await
                .map_err(|_| AppError::internal("publish task failed"))?
        }
        #[cfg(feature = "s3")]
        Dest::S3(loc, _) => {
            let data = tokio::fs::read(zip).await.map_err(|_| AppError::internal("cannot read zip"))?;
            s3_put(loc, &zip_sibling(&loc.key), data.into(), overwrite, &dest.zip_display()).await
        }
        #[cfg(not(feature = "s3"))]
        Dest::S3(..) => Err(AppError::feature_not_compiled("s3")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragments_compose_valid_json() {
        let body =
            Body { content: "# Hi\n\ntext\n".into(), chunks: crate::markdown::analyze("# Hi\n\ntext\n", 100, &[]).1, ..Default::default() };
        let llm = LlmSection::default();
        let timing = Timing::default();
        let h = Header {
            schema_version: SCHEMA_VERSION,
            operation: "convert",
            job_id: Some("j"),
            request_id: "r",
            status: "completed",
            cache_hit: false,
            src_file: "/a.md",
            src_bytes: Some(4),
            dest_file: None,
            output_file: None,
            images_dir: None,
            zip_file: None,
            zip_bytes: 0,
        };
        let t = Trailer {
            metadata: None,
            content_hash: None,
            llm: &llm,
            timing: &timing,
            fetched_at: None,
            converted_at: "2026-01-01T00:00:00Z",
            error: None,
        };
        let (s, e, out) = write_result(Vec::new(), u64::MAX, &h, BodySrc::Body(&body), &t).unwrap();
        assert!(out.ends_with(b"}\n") && !out[..out.len() - 1].contains(&b'\n'), "one JSON Lines record");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["content"], "# Hi\n\ntext\n");
        assert_eq!(v["chunks"][0]["headings"][0], "Hi");
        assert_eq!(v["job_id"], "j");
        assert_eq!(body_len(BodySrc::Body(&body)).unwrap(), e - s);
        // Reuse the body bytes with a different header (cache hit).
        let raw = out[s as usize..e as usize].to_vec();
        let h2 = Header { request_id: "r2", cache_hit: true, ..h };
        let (_, _, out2) = write_result(Vec::new(), u64::MAX, &h2, BodySrc::Raw(&raw), &t).unwrap();
        let v2: serde_json::Value = serde_json::from_slice(&out2).unwrap();
        assert_eq!(v2["request_id"], "r2");
        assert_eq!(v2["content"], v["content"]);
        assert!(write_result(Vec::new(), 10, &h, BodySrc::Body(&body), &t).is_err());
        // Without chunks, the key is still there.
        let (_, _, out3) = write_result(Vec::new(), u64::MAX, &h, BodySrc::Body(&Body::default()), &t).unwrap();
        let v3: serde_json::Value = serde_json::from_slice(&out3).unwrap();
        assert!(v3.get("chunks").is_some_and(|c| c.is_null()) && v3.get("search").is_some());
    }

    #[test]
    fn destinations() {
        assert_eq!(zip_sibling("/x/a.jsonl"), "/x/a.zip");
        assert!(parse_dest("/x/a.md", "/in/x.pdf").is_err());
        assert!(parse_dest("/x/a.json", "/in/x.pdf").is_err());
        // Folders: the output is named after the source file, extension kept.
        let d = parse_dest("/out/", "/in/hello.pdf").unwrap();
        assert_eq!(
            (d.display(), d.images_display(), d.zip_display()),
            ("/out/hello.pdf.jsonl".into(), "/out/images/".into(), "/out/hello.pdf.zip".into())
        );
        assert_eq!(parse_dest("/out/", "https://x.com/files/Q3%20report.docx?sig=1").unwrap().display(), "/out/Q3 report.docx.jsonl");
        assert_eq!(parse_dest("/out/", "https://x.com/").unwrap().display(), "/out/document.jsonl");
        assert_eq!(zip_name(Some(&d), "/in/hello.pdf"), "hello.pdf");
        #[cfg(feature = "s3")]
        {
            let d = parse_dest("s3://b/out/?region=eu-west-1", "s3://in/a b.pdf").unwrap();
            assert_eq!(d.display(), "s3://b/out/a%20b.pdf.jsonl?region=eu-west-1");
            assert_eq!(d.zip_display(), "s3://b/out/a%20b.pdf.zip?region=eu-west-1");
        }
    }
}
