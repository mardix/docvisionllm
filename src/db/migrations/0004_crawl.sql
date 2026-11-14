CREATE TABLE IF NOT EXISTS crawl_sites (
  site TEXT PRIMARY KEY,
  created_at BIGINT NOT NULL,
  last_job_id TEXT,
  last_run_at BIGINT,
  last_status TEXT
);
CREATE TABLE IF NOT EXISTS crawl_pages (
  site TEXT NOT NULL,
  url TEXT NOT NULL,
  page_key TEXT NOT NULL,
  job_id TEXT,
  status TEXT NOT NULL,
  title TEXT,
  content_hash TEXT,
  fetched_at BIGINT,
  converted_at BIGINT,
  error TEXT,
  PRIMARY KEY (site, url)
);
CREATE INDEX IF NOT EXISTS idx_crawl_pages_status ON crawl_pages(site, status, url)
