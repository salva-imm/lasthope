//! The grimoire: a shelf of PDFs (`BOOKS_DIR`) read one page at a time.
//!
//! - Indexing: every PDF in the folder is hashed (SHA-256); only new or
//!   changed files are read again. Each page's text goes into `pages`, with
//!   FTS5 over it. A book with no extractable text (a scan) is flagged, not
//!   an error. Files that left the folder are marked missing, and their
//!   pages kept, so old questions can still be judged.
//! - Choosing: the Rust code picks the page, never the model: the next
//!   unread page of the current book, the page that best matches the oath
//!   (full-text search), a random page, or one I chose in the UI.
//! - Teaching: the model gets only that page's text, explains its core idea
//!   in three spoken sentences and writes two questions; my spoken answers
//!   are judged against the page. Every question, answer and verdict is
//!   stored; ones I got wrong are asked again in a later session.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Once;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::llm::{ChatMessage, Role, Sampling};
use crate::memory::{Memory, fts_query};
use crate::speech_out::split_sentences;
use crate::streak::Day;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS books (
    id         INTEGER PRIMARY KEY,
    path       TEXT NOT NULL UNIQUE,           -- relative to BOOKS_DIR
    title      TEXT NOT NULL,
    hash       TEXT NOT NULL,                  -- SHA-256 of the file
    pages      INTEGER NOT NULL DEFAULT 0,
    text_pages INTEGER NOT NULL DEFAULT 0,     -- pages with enough text to teach
    status     TEXT NOT NULL CHECK (status IN ('ok', 'no_text', 'failed')),
    error      TEXT,
    last_page  INTEGER NOT NULL DEFAULT 0,     -- the last page I read (0: none)
    missing    INTEGER NOT NULL DEFAULT 0,     -- the file has left the folder
    indexed_at TEXT NOT NULL DEFAULT (datetime('now', 'localtime'))
);

CREATE TABLE IF NOT EXISTS pages (
    id      INTEGER PRIMARY KEY,
    book_id INTEGER NOT NULL REFERENCES books(id) ON DELETE CASCADE,
    page    INTEGER NOT NULL,                  -- 1-based, as in the PDF viewer
    text    TEXT NOT NULL,
    UNIQUE (book_id, page)
);

CREATE VIRTUAL TABLE IF NOT EXISTS pages_fts USING fts5(
    text, content = 'pages', content_rowid = 'id', tokenize = 'porter unicode61'
);
CREATE TRIGGER IF NOT EXISTS pages_ai AFTER INSERT ON pages BEGIN
    INSERT INTO pages_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER IF NOT EXISTS pages_ad AFTER DELETE ON pages BEGIN
    INSERT INTO pages_fts(pages_fts, rowid, text) VALUES ('delete', old.id, old.text);
END;
CREATE TRIGGER IF NOT EXISTS pages_au AFTER UPDATE ON pages BEGIN
    INSERT INTO pages_fts(pages_fts, rowid, text) VALUES ('delete', old.id, old.text);
    INSERT INTO pages_fts(rowid, text) VALUES (new.id, new.text);
END;

-- every question she asked about a page, my answer and her verdict.
-- A question asked again (one I got wrong) is a new row pointing at the first.
CREATE TABLE IF NOT EXISTS questions (
    id          INTEGER PRIMARY KEY,
    book_id     INTEGER REFERENCES books(id) ON DELETE SET NULL,
    page        INTEGER NOT NULL,
    session_id  INTEGER REFERENCES sessions(id) ON DELETE SET NULL,
    question    TEXT NOT NULL,
    answer      TEXT,
    correct     INTEGER,                       -- NULL until judged
    judgement   TEXT,                          -- what she said of my answer
    first_id    INTEGER REFERENCES questions(id) ON DELETE CASCADE,
    asked_at    TEXT NOT NULL DEFAULT (datetime('now', 'localtime')),
    answered_at TEXT
);
CREATE INDEX IF NOT EXISTS questions_first ON questions(first_id);
"#;

/// A page with less text than this is a title, a figure or a blank: never
/// chosen for a lesson.
pub const MIN_PAGE_CHARS: i64 = 400;
/// Page text sent to the model, at most.
const MAX_PAGE_CHARS: usize = 6000;
/// Old questions I got wrong, asked again at the start of a session.
pub const REASK: u32 = 2;
/// A page used for a lesson this recently is not chosen again by the oath search.
const OATH_PAGE_REST_DAYS: u32 = 14;

pub const LESSON_SAMPLING: Sampling = Sampling { temperature: 0.5, seed: None, json: true };
pub const JUDGE_SAMPLING: Sampling = Sampling { temperature: 0.2, seed: None, json: true };

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pick {
    /// The next unread page of the current book.
    Next,
    /// The page that best matches the oath.
    Oath,
    Random,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Book {
    pub id: i64,
    pub path: String,
    pub title: String,
    pub pages: u32,
    pub text_pages: u32,
    /// "ok", "no_text" (a scan, or only images) or "failed" (unreadable).
    pub status: String,
    pub error: Option<String>,
    pub last_page: u32,
    pub missing: bool,
}

impl Book {
    pub fn readable(&self) -> bool {
        self.status == "ok" && !self.missing
    }
}

/// One page, as the model and the viewer get it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub book: i64,
    pub title: String,
    pub page: u32,
    pub pages: u32,
    pub text: String,
}

/// A question asked before, as the archive of the grimoire shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Asked {
    pub id: i64,
    pub book: Option<i64>,
    pub title: Option<String>,
    pub page: u32,
    pub question: String,
    pub answer: Option<String>,
    pub correct: Option<bool>,
    pub judgement: Option<String>,
    /// Asked again: the id of the first asking.
    pub first_id: Option<i64>,
    pub asked_at: String,
}

/// A question I got wrong and have not yet answered right.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Missed {
    pub first_id: i64,
    pub book: i64,
    pub page: u32,
    pub question: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct IndexReport {
    pub added: u32,
    pub changed: u32,
    pub unchanged: u32,
    pub no_text: u32,
    pub failed: u32,
    pub missing: u32,
}

impl IndexReport {
    pub fn describe(&self) -> String {
        let mut parts = vec![];
        for (n, what) in [
            (self.added, "new"),
            (self.changed, "changed"),
            (self.unchanged, "unchanged"),
            (self.no_text, "without text (scans?)"),
            (self.failed, "unreadable"),
            (self.missing, "gone from the folder"),
        ] {
            if n > 0 {
                parts.push(format!("{n} {what}"));
            }
        }
        if parts.is_empty() { "the shelf is empty".into() } else { parts.join(", ") }
    }
}

// --- indexing -----------------------------------------------------------------

/// Index `dir`: read new and changed PDFs, mark vanished ones. `note` hears
/// about each book as it is read.
pub async fn index(memory: &Memory, dir: &Path, note: impl Fn(String)) -> Result<IndexReport> {
    let mut report = IndexReport::default();
    // an absent folder (an unplugged disk) is not an empty shelf: nothing is marked gone
    anyhow::ensure!(dir.is_dir(), "no books folder at {} (set BOOKS_DIR)", dir.display());
    let root = dir.to_path_buf();
    let files = tokio::task::spawn_blocking(move || find_pdfs(&root)).await??;
    let known = memory.book_hashes().await?;
    for (rel, full) in &files {
        let bytes = tokio::fs::read(full).await.with_context(|| format!("reading {rel}"))?;
        let (hash, bytes) = tokio::task::spawn_blocking(move || (sha256_hex(&bytes), bytes)).await?;
        let previous = known.get(rel);
        if previous.is_some_and(|h| *h == hash) {
            memory.unmark_missing(rel).await?;
            report.unchanged += 1;
            continue;
        }
        note(format!("reading {rel}"));
        let extracted = extract(bytes).await;
        let status = memory.store_book(rel, &title_of(rel), &hash, &extracted).await?;
        match status {
            "ok" if previous.is_some() => report.changed += 1,
            "ok" => report.added += 1,
            "no_text" => report.no_text += 1,
            _ => report.failed += 1,
        }
    }
    let present: Vec<String> = files.into_iter().map(|(rel, _)| rel).collect();
    report.missing = memory.mark_missing_books(&present).await?;
    Ok(report)
}

/// Every `.pdf` under `dir` (recursively, hidden entries skipped), as
/// (path relative to `dir`, full path), sorted.
fn find_pdfs(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut out = vec![];
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).with_context(|| format!("listing {}", d.display()))? {
            let entry = entry?;
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf")) {
                if let Ok(rel) = path.strip_prefix(dir) {
                    out.push((rel.to_string_lossy().into_owned(), path.clone()));
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// "rust/The_Rust-Book.pdf" -> "The Rust Book"
fn title_of(rel: &str) -> String {
    let stem = Path::new(rel).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let t = stem.replace(['_', '-'], " ");
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.is_empty() { rel.to_string() } else { t }
}

/// The text of every page, or why the file could not be read. Runs on its
/// own thread: the PDF parser can be slow, and on odd files it panics.
async fn extract(bytes: Vec<u8>) -> Result<Vec<String>, String> {
    quiet_parser_panics();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let spawned = std::thread::Builder::new().name(PARSER_THREAD.into()).spawn(move || {
        let _ = tx.send(extract_pages(&bytes));
    });
    if let Err(e) = spawned {
        return Err(format!("cannot start the reader: {e}"));
    }
    rx.await.unwrap_or_else(|_| Err("the PDF reader crashed".into()))
}

const PARSER_THREAD: &str = "grimoire-reader";

/// The PDF parser panics on some fonts; a page that does is read as blank.
/// Those panics are expected, so they are not printed over the conversation.
fn quiet_parser_panics() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if std::thread::current().name() != Some(PARSER_THREAD) {
                default(info);
            }
        }));
    });
}

fn extract_pages(bytes: &[u8]) -> Result<Vec<String>, String> {
    let doc = catch_unwind(AssertUnwindSafe(|| -> Result<pdf_extract::Document, String> {
        let mut doc = pdf_extract::Document::load_mem(bytes).map_err(|e| e.to_string())?;
        if doc.is_encrypted() {
            doc.decrypt("").map_err(|_| "it is encrypted with a password".to_string())?;
        }
        Ok(doc)
    }))
    .map_err(|_| "the PDF reader crashed on it".to_string())??;
    let count = doc.get_pages().len() as u32;
    if count == 0 {
        return Err("it has no pages".into());
    }
    Ok((1..=count)
        .map(|n| {
            catch_unwind(AssertUnwindSafe(|| {
                let mut s = String::new();
                let mut out = pdf_extract::PlainTextOutput::new(&mut s);
                pdf_extract::output_doc_page(&doc, &mut out, n).ok().map(|()| s)
            }))
            .ok()
            .flatten()
            .map(|s| tidy(&s))
            .unwrap_or_default()
        })
        .collect())
}

/// Join the lines of extracted text: words split by a hyphen at a line end
/// are mended, and all runs of whitespace become one space.
pub fn tidy(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if out.ends_with('-') && out[..out.len() - 1].ends_with(|c: char| c.is_alphabetic())
            && line.starts_with(|c: char| c.is_lowercase())
        {
            out.pop();
        } else if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(line);
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

// --- the shelf in memory -------------------------------------------------------

type BookRow = (i64, String, String, i64, i64, String, Option<String>, i64, bool);

impl From<BookRow> for Book {
    fn from(r: BookRow) -> Self {
        let (id, path, title, pages, text_pages, status, error, last_page, missing) = r;
        Self {
            id,
            path,
            title,
            pages: pages as u32,
            text_pages: text_pages as u32,
            status,
            error,
            last_page: last_page as u32,
            missing,
        }
    }
}

type AskedRow = (i64, Option<i64>, Option<String>, i64, String, Option<String>, Option<bool>, Option<String>, Option<i64>, String);

impl From<AskedRow> for Asked {
    fn from(r: AskedRow) -> Self {
        let (id, book, title, page, question, answer, correct, judgement, first_id, asked_at) = r;
        Self { id, book, title, page: page as u32, question, answer, correct, judgement, first_id, asked_at }
    }
}

impl Memory {
    async fn book_hashes(&self) -> Result<HashMap<String, String>> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT path, hash FROM books").fetch_all(&self.pool).await?;
        Ok(rows.into_iter().collect())
    }

    async fn unmark_missing(&self, path: &str) -> Result<()> {
        sqlx::query("UPDATE books SET missing = 0 WHERE path = ?1 AND missing = 1")
            .bind(path)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Mark books not in `present` as gone; returns how many are newly gone.
    async fn mark_missing_books(&self, present: &[String]) -> Result<u32> {
        let rows: Vec<(i64, String)> =
            sqlx::query_as("SELECT id, path FROM books WHERE missing = 0").fetch_all(&self.pool).await?;
        let mut gone = 0;
        for (id, path) in rows {
            if !present.contains(&path) {
                sqlx::query("UPDATE books SET missing = 1 WHERE id = ?1").bind(id).execute(&self.pool).await?;
                gone += 1;
            }
        }
        Ok(gone)
    }

    /// Store (or replace) a book's pages. Its id, bookmark and questions
    /// survive a change of the file. Returns the status recorded.
    async fn store_book(
        &self,
        path: &str,
        title: &str,
        hash: &str,
        extracted: &Result<Vec<String>, String>,
    ) -> Result<&'static str> {
        let (status, error, pages): (&'static str, Option<&str>, &[String]) = match extracted {
            Err(e) => ("failed", Some(e.as_str()), &[]),
            Ok(p) if !p.iter().any(|t| t.len() as i64 >= MIN_PAGE_CHARS) => ("no_text", None, p.as_slice()),
            Ok(p) => ("ok", None, p.as_slice()),
        };
        let text_pages = pages.iter().filter(|t| t.len() as i64 >= MIN_PAGE_CHARS).count() as i64;
        let mut tx = self.pool.begin().await?;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO books (path, title, hash, pages, text_pages, status, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(path) DO UPDATE SET hash = ?3, pages = ?4, text_pages = ?5, status = ?6,
                 error = ?7, missing = 0, last_page = min(last_page, ?4),
                 indexed_at = datetime('now', 'localtime')
             RETURNING id",
        )
        .bind(path)
        .bind(title)
        .bind(hash)
        .bind(pages.len() as i64)
        .bind(text_pages)
        .bind(status)
        .bind(error)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM pages WHERE book_id = ?1").bind(id).execute(&mut *tx).await?;
        for (i, text) in pages.iter().enumerate() {
            sqlx::query("INSERT INTO pages (book_id, page, text) VALUES (?1, ?2, ?3)")
                .bind(id)
                .bind(i as i64 + 1)
                .bind(text)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(status)
    }

    /// Every book ever indexed, by title.
    pub async fn books(&self) -> Result<Vec<Book>> {
        let rows: Vec<BookRow> =
            sqlx::query_as(
                "SELECT id, path, title, pages, text_pages, status, error, last_page, missing
                 FROM books ORDER BY missing, title COLLATE NOCASE",
            )
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().map(Book::from).collect())
    }

    pub async fn book(&self, id: i64) -> Result<Option<Book>> {
        let row: Option<BookRow> = sqlx::query_as(
            "SELECT id, path, title, pages, text_pages, status, error, last_page, missing FROM books WHERE id = ?1",
        )
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(Book::from))
    }

    /// The book being read: the one chosen last, else the first readable one.
    pub async fn current_book(&self) -> Result<Option<Book>> {
        if let Some(id) = self.setting("grimoire_book").await?.and_then(|v| v.parse().ok()) {
            if let Some(book) = self.book(id).await?.filter(Book::readable) {
                return Ok(Some(book));
            }
        }
        Ok(self.books().await?.into_iter().find(Book::readable))
    }

    pub async fn set_current_book(&self, id: i64) -> Result<()> {
        self.set_setting("grimoire_book", &id.to_string()).await
    }

    pub async fn set_last_page(&self, book: i64, page: u32) -> Result<()> {
        sqlx::query("UPDATE books SET last_page = ?2 WHERE id = ?1").bind(book).bind(page).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn page(&self, book: i64, page: u32) -> Result<Option<Page>> {
        let row: Option<(String, i64, String)> = sqlx::query_as(
            "SELECT b.title, b.pages, p.text FROM pages p JOIN books b ON b.id = p.book_id
             WHERE p.book_id = ?1 AND p.page = ?2",
        )
        .bind(book)
        .bind(page)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(title, pages, text)| Page { book, title, page, pages: pages as u32, text }))
    }

    /// The first page with text after the bookmark.
    pub async fn next_page(&self, book: i64) -> Result<Option<u32>> {
        let page: Option<i64> = sqlx::query_scalar(
            "SELECT p.page FROM pages p JOIN books b ON b.id = p.book_id
             WHERE p.book_id = ?1 AND p.page > b.last_page AND length(p.text) >= ?2
             ORDER BY p.page LIMIT 1",
        )
        .bind(book)
        .bind(MIN_PAGE_CHARS)
        .fetch_optional(&self.pool)
        .await?;
        Ok(page.map(|p| p as u32))
    }

    /// The page of any readable book that best matches `text`, skipping
    /// pages taught recently while others match.
    pub async fn best_page_for(&self, text: &str) -> Result<Option<(i64, u32)>> {
        let Some(fts) = fts_query(text) else { return Ok(None) };
        for rest_days in [OATH_PAGE_REST_DAYS, 0] {
            let row: Option<(i64, i64)> = sqlx::query_as(
                "SELECT p.book_id, p.page FROM pages_fts
                 JOIN pages p ON p.id = pages_fts.rowid
                 JOIN books b ON b.id = p.book_id
                 WHERE pages_fts MATCH ?1 AND b.status = 'ok' AND b.missing = 0
                   AND length(p.text) >= ?2
                   AND NOT EXISTS (SELECT 1 FROM questions q WHERE q.book_id = p.book_id
                       AND q.page = p.page AND q.asked_at >= datetime('now', 'localtime', ?3))
                 ORDER BY bm25(pages_fts) LIMIT 1",
            )
            .bind(&fts)
            .bind(MIN_PAGE_CHARS)
            .bind(format!("-{rest_days} days"))
            .fetch_optional(&self.pool)
            .await?;
            if let Some((book, page)) = row {
                return Ok(Some((book, page as u32)));
            }
        }
        Ok(None)
    }

    /// A random page with text, from `book` or from any readable book.
    pub async fn random_page(&self, book: Option<i64>) -> Result<Option<(i64, u32)>> {
        let row: Option<(i64, i64)> = sqlx::query_as(
            "SELECT p.book_id, p.page FROM pages p JOIN books b ON b.id = p.book_id
             WHERE b.status = 'ok' AND b.missing = 0 AND length(p.text) >= ?1
               AND (?2 IS NULL OR p.book_id = ?2)
             ORDER BY random() LIMIT 1",
        )
        .bind(MIN_PAGE_CHARS)
        .bind(book)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(b, p)| (b, p as u32)))
    }

    /// The oath to read for: the newest one sworn for today or later.
    pub async fn oath_in_force(&self, today: Day) -> Result<Option<String>> {
        Ok(sqlx::query_scalar(
            "SELECT oath FROM sessions WHERE oath IS NOT NULL AND oath_day >= ?1 ORDER BY id DESC LIMIT 1",
        )
        .bind(today.to_string())
        .fetch_optional(&self.pool)
        .await?)
    }

    // --- questions ---------------------------------------------------------

    /// Record a question as asked; returns its id.
    pub async fn ask_question(
        &self,
        book: i64,
        page: u32,
        session: i64,
        question: &str,
        first_id: Option<i64>,
    ) -> Result<i64> {
        let r = sqlx::query(
            "INSERT INTO questions (book_id, page, session_id, question, first_id) VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(book)
        .bind(page)
        .bind((session != 0).then_some(session))
        .bind(question)
        .bind(first_id)
        .execute(&self.pool)
        .await?;
        Ok(r.last_insert_rowid())
    }

    pub async fn answer_question(&self, id: i64, answer: &str) -> Result<()> {
        sqlx::query("UPDATE questions SET answer = ?2, answered_at = datetime('now', 'localtime') WHERE id = ?1")
            .bind(id)
            .bind(answer)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn judge_question(&self, id: i64, correct: bool, judgement: &str) -> Result<()> {
        sqlx::query("UPDATE questions SET correct = ?2, judgement = ?3 WHERE id = ?1")
            .bind(id)
            .bind(correct)
            .bind(judgement)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Questions I got wrong and never since answered right, least recently
    /// asked first. Ones asked in the last twelve hours are left to rest.
    pub async fn missed_questions(&self, limit: u32) -> Result<Vec<Missed>> {
        let rows: Vec<(i64, i64, i64, String)> = sqlx::query_as(
            "SELECT o.id, o.book_id, o.page, o.question FROM questions o
             WHERE o.first_id IS NULL AND o.correct = 0 AND o.book_id IS NOT NULL
               AND NOT EXISTS (SELECT 1 FROM questions r WHERE r.first_id = o.id AND r.correct = 1)
               AND NOT EXISTS (SELECT 1 FROM questions r WHERE (r.id = o.id OR r.first_id = o.id)
                   AND r.asked_at >= datetime('now', 'localtime', '-12 hours'))
               AND EXISTS (SELECT 1 FROM pages p WHERE p.book_id = o.book_id AND p.page = o.page)
             ORDER BY (SELECT max(r.asked_at) FROM questions r WHERE r.id = o.id OR r.first_id = o.id), o.id
             LIMIT ?1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(first_id, book, page, question)| Missed { first_id, book, page: page as u32, question })
            .collect())
    }

    /// The latest questions, newest first, optionally of one book.
    pub async fn questions(&self, book: Option<i64>, limit: u32) -> Result<Vec<Asked>> {
        let rows: Vec<AskedRow> = sqlx::query_as(
            "SELECT q.id, q.book_id, b.title, q.page, q.question, q.answer, q.correct, q.judgement,
                    q.first_id, q.asked_at
             FROM questions q LEFT JOIN books b ON b.id = q.book_id
             WHERE ?1 IS NULL OR q.book_id = ?1
             ORDER BY q.id DESC LIMIT ?2",
        )
        .bind(book)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Asked::from).collect())
    }

    /// (answered, right) over all questions.
    pub async fn question_tally(&self) -> Result<(u32, u32)> {
        let (answered, right): (i64, i64) = sqlx::query_as(
            "SELECT count(*), COALESCE(sum(correct), 0) FROM questions WHERE correct IS NOT NULL",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok((answered as u32, right as u32))
    }
}

// --- spoken commands -------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadCmd {
    Read(Pick),
    /// Close the book: no more questions on this page.
    Leave,
}

/// "read the next page", "a random page", "read a page for my oath",
/// "leave the page". The whole utterance must be the command, give or take
/// a polite word, so talk about pages stays talk.
pub fn reading_command(text: &str) -> Option<ReadCmd> {
    const LEAD: &[&str] = &["okay", "ok", "alright", "now", "please", "lasthope", "last", "hope", "so", "then"];
    const TRAIL: &[&str] = &["please", "now", "lasthope", "last", "hope", "for", "me", "tonight"];
    const VERB: &[&str] = &["read", "open", "show", "give", "turn", "choose", "pick", "find", "take", "go", "to"];
    const GLUE: &[&str] = &["me", "us", "a", "an", "the", "any", "my", "of", "for", "from", "that", "which",
        "matches", "match", "fits", "about", "on", "book", "in"];
    let lower = text.to_lowercase().replace('’', "'");
    let words: Vec<&str> = lower
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .filter(|w| !w.is_empty())
        .collect();
    let mut w: &[&str] = &words;
    while let Some((first, rest)) = w.split_first() {
        if LEAD.contains(first) || *first == "let's" || *first == "lets" { w = rest } else { break }
    }
    while let Some((last, rest)) = w.split_last() {
        if TRAIL.contains(last) { w = rest } else { break }
    }
    let phrase = w.join(" ");
    match phrase.as_str() {
        "leave the page" | "close the book" | "stop reading" | "enough reading" | "put the book away"
        | "leave the book" => return Some(ReadCmd::Leave),
        "read on" | "keep reading" | "continue reading" | "next page" | "the next page" => {
            return Some(ReadCmd::Read(Pick::Next));
        }
        _ => {}
    }
    // a verb is required, then only glue words around the page's kind
    let (first, rest) = w.split_first()?;
    if !VERB.contains(first) {
        return None;
    }
    let core: Vec<&str> = rest.iter().copied().filter(|x| !VERB.contains(x) && !GLUE.contains(x)).collect();
    match core.as_slice() {
        ["next", "page"] | ["next", "unread", "page"] | ["page", "next"] => Some(ReadCmd::Read(Pick::Next)),
        ["random", "page"] => Some(ReadCmd::Read(Pick::Random)),
        ["page", "oath"] | ["oath", "page"] => Some(ReadCmd::Read(Pick::Oath)),
        _ => None,
    }
}

// --- what the model is asked ------------------------------------------------------

fn page_block(page: &Page) -> String {
    let text: String = page.text.chars().take(MAX_PAGE_CHARS).collect();
    format!(
        "# The page\nFrom the book \"{}\", page {}. Its text, taken from the PDF (the layout may be \
         broken: ignore running headers, footers and page numbers):\n\"\"\"\n{text}\n\"\"\"\n",
        page.title, page.page
    )
}

/// Teach `page`: her character, the page's text and nothing else from memory.
pub fn lesson_messages(persona: &str, page: &Page) -> Vec<ChatMessage> {
    let system = format!(
        "{persona}\n\n{}\n# Right now\n\
         You are reading this page with them. Reply with JSON only, no prose around it, in this shape:\n\
         {{\"explanation\": \"...\", \"questions\": [\"...\", \"...\"]}}\n\
         explanation: the core idea of the page in exactly three short sentences, spoken aloud in \
         your own voice: plain words, no lists, symbols, markdown or quotes from code. Use only what \
         the page says.\n\
         questions: two questions, each ending with a question mark, that test whether they \
         understood this page. Each must be answerable from this page alone, in a sentence or two spoken aloud. No yes or no \
         questions, nothing about page numbers, the author or the book itself.",
        page_block(page)
    );
    vec![
        ChatMessage::new(Role::System, system),
        ChatMessage::new(
            Role::User,
            "(They open the book at this page. Reply with the JSON object only: \
             {\"explanation\": \"three sentences\", \"questions\": [\"first\", \"second\"]})",
        ),
    ]
}

/// Judge my answer to `question` against `page`.
pub fn judge_messages(persona: &str, page: &Page, question: &str, answer: &str, last: bool) -> Vec<ChatMessage> {
    let system = format!(
        "{persona}\n\n{}\n# Right now\n\
         You asked them about this page: \"{question}\"\n\
         Their answer is in the next message. It was spoken and transcribed, so a word may be \
         misheard. Judge it against the page text only. It is right if it holds the essential \
         point, even in other words or briefly; it is wrong if it misses or contradicts the point, \
         or if they do not know.\n\
         Reply with JSON only, no prose around it: {{\"correct\": true or false, \"reply\": \"...\"}}\n\
         reply: one or two short spoken sentences. Say plainly whether they were right. If they \
         were wrong, give the right answer from the page in one sentence. {}",
        page_block(page),
        if last {
            "This was the last question on the page: ask nothing more."
        } else {
            "Ask nothing: the next question follows your reply."
        }
    );
    let answer = format!(
        "Their answer: \"{answer}\"\n(Reply with the JSON object only: {{\"correct\": true or false, \"reply\": \"...\"}})"
    );
    vec![ChatMessage::new(Role::System, system), ChatMessage::new(Role::User, answer)]
}

fn json_in(raw: &str) -> Option<serde_json::Value> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    serde_json::from_str(raw.get(start..=end)?).ok()
}

/// The lesson: the explanation to speak and two questions. If the model
/// answered in prose instead of JSON, its questions are the sentences that
/// end in a question mark, and the rest is the explanation.
pub fn parse_lesson(raw: &str) -> Option<(String, Vec<String>)> {
    parse_lesson_json(raw).or_else(|| parse_lesson_prose(raw))
}

fn parse_lesson_prose(raw: &str) -> Option<(String, Vec<String>)> {
    if raw.contains('{') {
        return None; // broken JSON: not prose to be read aloud
    }
    let (asked, told): (Vec<String>, Vec<String>) =
        split_sentences(raw).into_iter().partition(|s| s.trim_end().ends_with('?'));
    let explanation = told.join(" ").trim().to_string();
    let questions: Vec<String> = asked.iter().map(|q| as_question(q)).filter(|q| q.len() > 3).take(2).collect();
    (!explanation.is_empty() && !questions.is_empty()).then_some((explanation, questions))
}

fn parse_lesson_json(raw: &str) -> Option<(String, Vec<String>)> {
    let v = json_in(raw)?;
    let explanation = v.get("explanation")?.as_str()?.trim().to_string();
    let questions: Vec<String> = v
        .get("questions")?
        .as_array()?
        .iter()
        .filter_map(|q| q.as_str())
        .map(as_question)
        .filter(|q| q.len() > 3 && q.len() <= 400)
        .take(2)
        .collect();
    (!explanation.is_empty() && !questions.is_empty()).then_some((explanation, questions))
}

/// The verdict on my answer and what she says of it.
pub fn parse_judgement(raw: &str) -> Option<(bool, String)> {
    let v = json_in(raw)?;
    let correct = match v.get("correct")? {
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::String(s) => match s.trim().to_lowercase().as_str() {
            "true" | "yes" | "right" | "correct" => true,
            "false" | "no" | "wrong" | "incorrect" => false,
            _ => return None,
        },
        _ => return None,
    };
    let reply = v.get("reply").and_then(|r| r.as_str()).unwrap_or("").trim().to_string();
    let reply = if reply.is_empty() {
        (if correct { "Right." } else { "Not quite." }).to_string()
    } else {
        reply
    };
    Some((correct, reply))
}

/// One clean spoken question: "What packs?" stays a question, "Explain how
/// it packs" stays a command.
fn as_question(q: &str) -> String {
    const ASKING: &[&str] = &["what", "why", "how", "which", "who", "whom", "whose", "when", "where", "is",
        "are", "was", "were", "do", "does", "did", "can", "could", "should", "would", "will", "in", "according"];
    let q = q.trim().trim_start_matches(|c: char| c.is_ascii_digit() || matches!(c, '.' | ')' | '-' | ' '));
    let q = q.trim_end_matches(['.', '!', '?', ' ']).trim();
    let first = q.split_whitespace().next().unwrap_or("").to_lowercase();
    if ASKING.contains(&first.as_str()) { format!("{q}?") } else { format!("{q}.") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spoken_reading_commands() {
        for (said, expected) in [
            ("Read the next page.", Some(ReadCmd::Read(Pick::Next))),
            ("Okay, next page please.", Some(ReadCmd::Read(Pick::Next))),
            ("Read on.", Some(ReadCmd::Read(Pick::Next))),
            ("Open the next page of the book.", Some(ReadCmd::Read(Pick::Next))),
            ("Read a random page.", Some(ReadCmd::Read(Pick::Random))),
            ("Give me a random page.", Some(ReadCmd::Read(Pick::Random))),
            ("Read a page for my oath.", Some(ReadCmd::Read(Pick::Oath))),
            ("Find the page that matches my oath.", Some(ReadCmd::Read(Pick::Oath))),
            ("Leave the page.", Some(ReadCmd::Leave)),
            ("Stop reading, please.", Some(ReadCmd::Leave)),
            ("What's on the next page?", None),
            ("I read the next page yesterday.", None),
            ("The page about my oath was dull.", None),
            ("Read me something.", None),
        ] {
            assert_eq!(reading_command(said), expected, "{said:?}");
        }
    }

    #[test]
    fn extracted_text_is_tidied() {
        assert_eq!(tidy("  The ex-\nample  is\n\n  here.\n"), "The example is here.");
        assert_eq!(tidy("well-\nKnown"), "well- Known");
        assert_eq!(tidy("a\tb\n c"), "a b c");
    }

    #[test]
    fn titles_from_file_names() {
        assert_eq!(title_of("rust/The_Rust-Book.pdf"), "The Rust Book");
        assert_eq!(title_of("Mastering Ethereum.pdf"), "Mastering Ethereum");
    }

    #[test]
    fn lessons_and_judgements_are_read_from_json() {
        let raw = "```json\n{\"explanation\": \"A slot is 32 bytes. Values pack. Order matters.\", \
                   \"questions\": [\"1. How big is a slot.\", \"What packs\"]}\n```";
        let (e, q) = parse_lesson(raw).unwrap();
        assert_eq!(e, "A slot is 32 bytes. Values pack. Order matters.");
        assert_eq!(q, ["How big is a slot?", "What packs?"]);
        assert_eq!(as_question("Explain how values pack?"), "Explain how values pack.");
        assert_eq!(parse_lesson("{\"explanation\": \"x\", \"questions\": []}"), None);
        // prose instead of JSON: the questions are the sentences that ask
        let prose = "The page declares bitcoin the finest money because it solves trustless consensus, and \
                     it praises Rust for forcing safe code. Do you see how the author links the two? Can you \
                     explain why learning Rust is compared to a tough journey?";
        let (e, q) = parse_lesson(prose).unwrap();
        assert!(e.starts_with("The page declares bitcoin") && e.ends_with("safe code."), "{e}");
        assert_eq!(q, ["Do you see how the author links the two?",
            "Can you explain why learning Rust is compared to a tough journey?"]);
        assert_eq!(parse_lesson("Just a statement. Nothing asked."), None);
        assert_eq!(parse_judgement("{\"correct\": true, \"reply\": \"Right.\"}"), Some((true, "Right.".into())));
        assert_eq!(parse_judgement("{\"correct\": \"false\"}"), Some((false, "Not quite.".into())));
        assert_eq!(parse_judgement("no json"), None);
    }

    #[tokio::test]
    async fn the_shelf_bookmarks_and_missed_questions() {
        let path = std::env::temp_dir().join(format!("lasthope-grimoire-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let m = Memory::open(&path).await.unwrap();
        let long = |w: &str| format!("{w} ").repeat(100);
        let pages = vec![String::new(), long("storage slots"), long("gas refunds"), "short".to_string()];
        assert_eq!(m.store_book("a.pdf", "A", "h1", &Ok(pages.clone())).await.unwrap(), "ok");
        assert_eq!(m.store_book("scan.pdf", "Scan", "h2", &Ok(vec![String::new(); 3])).await.unwrap(), "no_text");
        assert_eq!(m.store_book("bad.pdf", "Bad", "h3", &Err("broken".into())).await.unwrap(), "failed");

        let a = m.current_book().await.unwrap().unwrap();
        assert_eq!((a.title.as_str(), a.pages, a.text_pages), ("A", 4, 2));
        // the next unread page skips the blank one, then follows the bookmark
        assert_eq!(m.next_page(a.id).await.unwrap(), Some(2));
        m.set_last_page(a.id, 2).await.unwrap();
        assert_eq!(m.next_page(a.id).await.unwrap(), Some(3));
        m.set_last_page(a.id, 3).await.unwrap();
        assert_eq!(m.next_page(a.id).await.unwrap(), None);
        assert_eq!(m.best_page_for("gas refunds are capped").await.unwrap(), Some((a.id, 3)));
        assert!(matches!(m.random_page(Some(a.id)).await.unwrap(), Some((_, 2 | 3))));

        // a changed file keeps its id and bookmark (within its new length)
        assert_eq!(m.store_book("a.pdf", "A", "h1b", &Ok(pages[..3].to_vec())).await.unwrap(), "ok");
        let a2 = m.book(a.id).await.unwrap().unwrap();
        assert_eq!((a2.pages, a2.last_page), (3, 3));
        assert_eq!(m.mark_missing_books(&["a.pdf".into()]).await.unwrap(), 2);
        assert_eq!(m.current_book().await.unwrap().map(|b| b.id), Some(a.id));

        // questions: a wrong one is asked again (not within twelve hours)
        let q1 = m.ask_question(a.id, 2, 0, "What is a slot?", None).await.unwrap();
        let q2 = m.ask_question(a.id, 2, 0, "What packs?", None).await.unwrap();
        m.answer_question(q1, "no idea").await.unwrap();
        m.judge_question(q1, false, "Not quite.").await.unwrap();
        m.answer_question(q2, "small values").await.unwrap();
        m.judge_question(q2, true, "Right.").await.unwrap();
        assert!(m.missed_questions(REASK).await.unwrap().is_empty());
        sqlx::query("UPDATE questions SET asked_at = datetime('now', 'localtime', '-2 days')")
            .execute(&m.pool)
            .await
            .unwrap();
        let missed = m.missed_questions(REASK).await.unwrap();
        assert_eq!(missed, [Missed { first_id: q1, book: a.id, page: 2, question: "What is a slot?".into() }]);
        let again = m.ask_question(a.id, 2, 0, "What is a slot?", Some(q1)).await.unwrap();
        m.judge_question(again, true, "Right.").await.unwrap();
        sqlx::query("UPDATE questions SET asked_at = datetime('now', 'localtime', '-2 days')")
            .execute(&m.pool)
            .await
            .unwrap();
        assert!(m.missed_questions(REASK).await.unwrap().is_empty());
        assert_eq!(m.question_tally().await.unwrap(), (3, 2));
        assert_eq!(m.questions(Some(a.id), 10).await.unwrap().len(), 3);
        let _ = std::fs::remove_file(&path);
    }
}
