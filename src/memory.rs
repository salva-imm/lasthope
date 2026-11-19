//! Memory: everything she knows lives in a local SQLite file (gitignored).
//! The Rust code owns it and injects what is relevant into each prompt; the
//! model is never trusted to remember on its own.
//!
//! - sessions: start, end, summary, the oath sworn, its verdict, focused minutes
//! - messages: every line, saved as it is said (a crash loses nothing), with
//!   FTS5 full-text search kept in sync by triggers
//! - vigils: focused work, timed by the vigil or reported by me, by day
//! - facts: what she has learned about me, each deletable
//! - state: the mission and the streak
//! - settings: small key/value preferences (her voice and pace, the UI key)
//!
//! "Today" is my local day with a 4 am boundary: a session at 00:30 still
//! belongs to the evening before.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};

use crate::rite::PendingOath;
use crate::streak::{Day, Streak};

pub const DEFAULT_MISSION: &str = "Master Rust, EVM internals and networking, \
with at least twenty-five minutes of focused work every day.";

const SCHEMA: &str = r#"
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS sessions (
    id              INTEGER PRIMARY KEY,
    day             TEXT NOT NULL,             -- local day it belongs to (YYYY-MM-DD)
    started_at      TEXT NOT NULL DEFAULT (datetime('now', 'localtime')),
    ended_at        TEXT,                      -- NULL while open (or if it crashed)
    summary         TEXT,
    oath            TEXT,                      -- tomorrow's oath, sworn in this session
    oath_day        TEXT,                      -- the day that oath is for
    oath_kept       INTEGER,                   -- my verdict, given in a later session
    judged_at       TEXT,
    focused_minutes INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS messages (
    id         INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    speaker    TEXT NOT NULL CHECK (speaker IN ('me', 'her')),
    text       TEXT NOT NULL,
    at         TEXT NOT NULL DEFAULT (datetime('now', 'localtime'))
);
CREATE INDEX IF NOT EXISTS messages_session ON messages(session_id, id);

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    text, content = 'messages', content_rowid = 'id', tokenize = 'porter unicode61'
);
CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, text) VALUES ('delete', old.id, old.text);
END;
CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, text) VALUES ('delete', old.id, old.text);
    INSERT INTO messages_fts(rowid, text) VALUES (new.id, new.text);
END;

-- focused work: each completed vigil, or one I reported (done while the
-- program was not running). sessions.focused_minutes counts timed ones only.
CREATE TABLE IF NOT EXISTS vigils (
    id         INTEGER PRIMARY KEY,
    session_id INTEGER REFERENCES sessions(id) ON DELETE SET NULL,
    day        TEXT NOT NULL,
    minutes    INTEGER NOT NULL CHECK (minutes > 0),
    source     TEXT NOT NULL CHECK (source IN ('timer', 'told')),
    at         TEXT NOT NULL DEFAULT (datetime('now', 'localtime'))
);
CREATE INDEX IF NOT EXISTS vigils_day ON vigils(day);

CREATE TABLE IF NOT EXISTS facts (
    id         INTEGER PRIMARY KEY,
    text       TEXT NOT NULL,
    learned_at TEXT NOT NULL DEFAULT (datetime('now', 'localtime')),
    session_id INTEGER REFERENCES sessions(id) ON DELETE SET NULL
);

CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS state (
    id           INTEGER PRIMARY KEY CHECK (id = 1),
    mission      TEXT NOT NULL,
    streak_count INTEGER NOT NULL DEFAULT 0,
    streak_last  TEXT,                         -- day of the last judged oath
    speaks_first INTEGER NOT NULL DEFAULT 1
);
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Speaker {
    Me,
    Her,
}

impl Speaker {
    fn as_str(self) -> &'static str {
        match self {
            Speaker::Me => "me",
            Speaker::Her => "her",
        }
    }

    fn parse(s: &str) -> Self {
        if s == "her" { Speaker::Her } else { Speaker::Me }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Line {
    pub speaker: Speaker,
    pub text: String,
    /// Local time, "YYYY-MM-DD HH:MM:SS".
    pub at: String,
}

/// One session as the archive lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SessionInfo {
    pub id: i64,
    pub day: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub summary: Option<String>,
    /// The oath sworn in this session (for the next day) and my verdict on it.
    pub oath: Option<String>,
    pub oath_day: Option<String>,
    pub oath_kept: Option<bool>,
    /// Focused work recorded during this session (timed or reported).
    pub focused_minutes: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Fact {
    pub id: i64,
    pub text: String,
}

/// A past line found by full-text search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub session: i64,
    pub day: Day,
    pub speaker: Speaker,
    pub text: String,
}

/// An oath already sworn for a day still ahead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpcomingOath {
    pub text: String,
    pub day: Day,
}

/// Where a vigil's minutes come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VigilSource {
    /// The timer ran to the end.
    Timer,
    /// I said I did it.
    Told,
}

/// Focused work on one day.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Focus {
    pub minutes: u32,
    pub vigils: u32,
}

#[derive(Clone)]
pub struct Memory {
    pool: SqlitePool,
}

impl Memory {
    pub async fn open(path: &Path) -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .with_context(|| format!("opening {}", path.display()))?;
        sqlx::raw_sql(SCHEMA).execute(&pool).await.context("creating the schema")?;
        sqlx::query("INSERT OR IGNORE INTO state (id, mission) VALUES (1, ?1)")
            .bind(DEFAULT_MISSION)
            .execute(&pool)
            .await?;
        Ok(Self { pool })
    }

    /// My local day (4 am boundary).
    pub async fn today(&self) -> Result<Day> {
        let s: String = sqlx::query_scalar("SELECT date('now', 'localtime', '-4 hours')")
            .fetch_one(&self.pool)
            .await?;
        Day::parse(&s).ok_or_else(|| anyhow!("bad date from SQLite: {s}"))
    }

    /// The local hour of day (0-23), for greetings.
    pub async fn local_hour(&self) -> Result<u32> {
        let h: String = sqlx::query_scalar("SELECT strftime('%H', 'now', 'localtime')")
            .fetch_one(&self.pool)
            .await?;
        Ok(h.parse()?)
    }

    // --- sessions -------------------------------------------------------

    pub async fn start_session(&self) -> Result<i64> {
        let r = sqlx::query("INSERT INTO sessions (day) VALUES (date('now', 'localtime', '-4 hours'))")
            .execute(&self.pool)
            .await?;
        Ok(r.last_insert_rowid())
    }

    /// Sessions never closed (the app crashed or was killed), except `current`.
    pub async fn unfinished_sessions(&self, current: Option<i64>) -> Result<Vec<i64>> {
        Ok(sqlx::query_scalar("SELECT id FROM sessions WHERE ended_at IS NULL AND id IS NOT ?1 ORDER BY id")
            .bind(current)
            .fetch_all(&self.pool)
            .await?)
    }

    /// Ended sessions with something I said but no summary (the summary call
    /// failed), most recent first.
    pub async fn sessions_missing_summary(&self, limit: u32) -> Result<Vec<i64>> {
        Ok(sqlx::query_scalar(
            "SELECT s.id FROM sessions s WHERE s.ended_at IS NOT NULL AND s.summary IS NULL
               AND EXISTS (SELECT 1 FROM messages m WHERE m.session_id = s.id AND m.speaker = 'me')
             ORDER BY s.id DESC LIMIT ?1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Close a session. A crashed one ends at its last message.
    pub async fn finish_session(&self, session: i64, summary: Option<&str>, crashed: bool) -> Result<()> {
        let sql = if crashed {
            "UPDATE sessions SET summary = COALESCE(?2, summary), ended_at = COALESCE(
                 (SELECT max(at) FROM messages WHERE session_id = ?1), started_at) WHERE id = ?1"
        } else {
            "UPDATE sessions SET summary = COALESCE(?2, summary),
                 ended_at = datetime('now', 'localtime') WHERE id = ?1"
        };
        sqlx::query(sql).bind(session).bind(summary).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn session_day(&self, session: i64) -> Result<Day> {
        let s: String = sqlx::query_scalar("SELECT day FROM sessions WHERE id = ?1")
            .bind(session)
            .fetch_one(&self.pool)
            .await?;
        Day::parse(&s).ok_or_else(|| anyhow!("bad session day {s}"))
    }

    /// The most recent earlier session: its day and summary.
    pub async fn last_session(&self, before: i64) -> Result<Option<(Day, Option<String>)>> {
        let row: Option<(String, Option<String>)> =
            sqlx::query_as("SELECT day, summary FROM sessions WHERE id < ?1 ORDER BY id DESC LIMIT 1")
                .bind(before)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|(d, s)| Day::parse(&d).map(|d| (d, s))))
    }

    // --- messages -------------------------------------------------------

    pub async fn add_message(&self, session: i64, speaker: Speaker, text: &str) -> Result<()> {
        sqlx::query("INSERT INTO messages (session_id, speaker, text) VALUES (?1, ?2, ?3)")
            .bind(session)
            .bind(speaker.as_str())
            .bind(text)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn session_lines(&self, session: i64) -> Result<Vec<Line>> {
        let rows: Vec<(String, String, String)> =
            sqlx::query_as("SELECT speaker, text, at FROM messages WHERE session_id = ?1 ORDER BY id")
                .bind(session)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows
            .into_iter()
            .map(|(s, text, at)| Line { speaker: Speaker::parse(&s), text, at })
            .collect())
    }

    /// Full-text search over everything said, best matches first, skipping
    /// `exclude` (usually the current session). `query` is free text.
    pub async fn search(&self, query: &str, limit: u32, exclude: Option<i64>) -> Result<Vec<Hit>> {
        let Some(fts) = fts_query(query) else { return Ok(vec![]) };
        let rows: Vec<(i64, String, String, String)> = sqlx::query_as(
            "SELECT m.session_id, s.day, m.speaker, m.text
             FROM messages_fts
             JOIN messages m ON m.id = messages_fts.rowid
             JOIN sessions s ON s.id = m.session_id
             WHERE messages_fts MATCH ?1 AND m.session_id IS NOT ?2
             ORDER BY bm25(messages_fts) LIMIT ?3",
        )
        .bind(fts)
        .bind(exclude)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(session, day, speaker, text)| {
                Some(Hit { session, day: Day::parse(&day)?, speaker: Speaker::parse(&speaker), text })
            })
            .collect())
    }

    // --- oaths and the streak -------------------------------------------

    /// Swear tomorrow's oath in `session`. Returns the day it is for.
    pub async fn set_oath(&self, session: i64, text: &str) -> Result<Day> {
        let day = self.session_day(session).await?.next();
        sqlx::query("UPDATE sessions SET oath = ?2, oath_day = ?3, oath_kept = NULL WHERE id = ?1")
            .bind(session)
            .bind(text)
            .bind(day.to_string())
            .execute(&self.pool)
            .await?;
        Ok(day)
    }

    /// The latest oath sworn before `current`, if it is due (its day has come)
    /// and not yet judged. Older unjudged oaths have lapsed.
    pub async fn pending_oath(&self, today: Day, current: i64) -> Result<Option<PendingOath>> {
        let row: Option<(i64, String, String, Option<i64>)> = sqlx::query_as(
            "SELECT id, oath, oath_day, oath_kept FROM sessions
             WHERE oath IS NOT NULL AND id != ?1 ORDER BY id DESC LIMIT 1",
        )
        .bind(current)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(session, text, day, kept)| {
            let day = Day::parse(&day)?;
            (kept.is_none() && day <= today).then_some(PendingOath { session, text, day })
        }))
    }

    /// The latest oath for a day after `today` (sworn tonight or earlier today).
    pub async fn upcoming_oath(&self, today: Day) -> Result<Option<UpcomingOath>> {
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT oath, oath_day FROM sessions WHERE oath IS NOT NULL AND oath_day > ?1
             ORDER BY id DESC LIMIT 1",
        )
        .bind(today.to_string())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(text, day)| Some(UpcomingOath { text, day: Day::parse(&day)? })))
    }

    /// Record my verdict on the oath sworn in `oath_session` and update the
    /// streak from it and the oath's day. One verdict per oath.
    pub async fn record_verdict(&self, oath_session: i64, kept: bool) -> Result<Streak> {
        let mut tx = self.pool.begin().await?;
        let row: Option<(Option<String>, Option<i64>)> =
            sqlx::query_as("SELECT oath_day, oath_kept FROM sessions WHERE id = ?1")
                .bind(oath_session)
                .fetch_optional(&mut *tx)
                .await?;
        let (Some(day), None) = row.unwrap_or((None, Some(0))) else {
            bail!("session {oath_session} has no oath awaiting a verdict");
        };
        let oath_day = Day::parse(&day).ok_or_else(|| anyhow!("bad oath day {day}"))?;
        sqlx::query("UPDATE sessions SET oath_kept = ?2, judged_at = datetime('now', 'localtime') WHERE id = ?1")
            .bind(oath_session)
            .bind(kept)
            .execute(&mut *tx)
            .await?;
        let (count, last): (i64, Option<String>) =
            sqlx::query_as("SELECT streak_count, streak_last FROM state WHERE id = 1")
                .fetch_one(&mut *tx)
                .await?;
        let before = Streak { count: count as u32, last: last.as_deref().and_then(Day::parse) };
        let after = before.apply_verdict(oath_day, kept);
        sqlx::query("UPDATE state SET streak_count = ?1, streak_last = ?2 WHERE id = 1")
            .bind(after.count as i64)
            .bind(after.last.map(|d| d.to_string()))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(after)
    }

    pub async fn streak(&self) -> Result<Streak> {
        let (count, last): (i64, Option<String>) =
            sqlx::query_as("SELECT streak_count, streak_last FROM state WHERE id = 1")
                .fetch_one(&self.pool)
                .await?;
        Ok(Streak { count: count as u32, last: last.as_deref().and_then(Day::parse) })
    }

    // --- vigils -----------------------------------------------------------

    /// Record focused work on `day`. Timed vigils also count on the session.
    /// `session` 0: between sessions (the rite is closed).
    pub async fn add_vigil(&self, session: i64, day: Day, minutes: u32, source: VigilSource) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO vigils (session_id, day, minutes, source) VALUES (?1, ?2, ?3, ?4)")
            .bind((session != 0).then_some(session))
            .bind(day.to_string())
            .bind(minutes)
            .bind(match source {
                VigilSource::Timer => "timer",
                VigilSource::Told => "told",
            })
            .execute(&mut *tx)
            .await?;
        if source == VigilSource::Timer {
            sqlx::query("UPDATE sessions SET focused_minutes = focused_minutes + ?2 WHERE id = ?1")
                .bind(session)
                .bind(minutes)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn focus_on(&self, day: Day) -> Result<Focus> {
        let (minutes, vigils): (i64, i64) =
            sqlx::query_as("SELECT COALESCE(sum(minutes), 0), count(*) FROM vigils WHERE day = ?1")
                .bind(day.to_string())
                .fetch_one(&self.pool)
                .await?;
        Ok(Focus { minutes: minutes as u32, vigils: vigils as u32 })
    }

    // --- mission, settings ----------------------------------------------

    pub async fn mission(&self) -> Result<String> {
        Ok(sqlx::query_scalar("SELECT mission FROM state WHERE id = 1").fetch_one(&self.pool).await?)
    }

    pub async fn set_mission(&self, mission: &str) -> Result<()> {
        sqlx::query("UPDATE state SET mission = ?1 WHERE id = 1").bind(mission).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn speaks_first(&self) -> Result<bool> {
        Ok(sqlx::query_scalar("SELECT speaks_first FROM state WHERE id = 1").fetch_one(&self.pool).await?)
    }

    pub async fn set_speaks_first(&self, on: bool) -> Result<()> {
        sqlx::query("UPDATE state SET speaks_first = ?1 WHERE id = 1").bind(on).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT value FROM settings WHERE key = ?1")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?)
    }

    pub async fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query("INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = ?2")
            .bind(key)
            .bind(value)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // --- the archive ------------------------------------------------------

    /// All sessions, newest first.
    pub async fn sessions(&self) -> Result<Vec<SessionInfo>> {
        let rows: Vec<SessionRow> = sqlx::query_as(
            "SELECT s.id, s.day, s.started_at, s.ended_at, s.summary, s.oath, s.oath_day, s.oath_kept,
                    (SELECT COALESCE(sum(v.minutes), 0) FROM vigils v WHERE v.session_id = s.id)
             FROM sessions s ORDER BY s.id DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(SessionInfo::from).collect())
    }

    pub async fn session_info(&self, id: i64) -> Result<Option<SessionInfo>> {
        let row: Option<SessionRow> = sqlx::query_as(
            "SELECT s.id, s.day, s.started_at, s.ended_at, s.summary, s.oath, s.oath_day, s.oath_kept,
                    (SELECT COALESCE(sum(v.minutes), 0) FROM vigils v WHERE v.session_id = s.id)
             FROM sessions s WHERE s.id = ?1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(SessionInfo::from))
    }

    /// Facts first learned in `session`.
    pub async fn facts_of_session(&self, session: i64) -> Result<Vec<Fact>> {
        let rows: Vec<(i64, String)> = sqlx::query_as("SELECT id, text FROM facts WHERE session_id = ?1 ORDER BY id")
            .bind(session)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(|(id, text)| Fact { id, text }).collect())
    }

    /// Burn a night: its words (and their search index) and the facts learned
    /// in it are gone. Its focused minutes stay counted on their day, and the
    /// chain of seals is not rewritten. Returns false if there was no such
    /// session.
    pub async fn delete_session(&self, session: i64) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM facts WHERE session_id = ?1").bind(session).execute(&mut *tx).await?;
        let r = sqlx::query("DELETE FROM sessions WHERE id = ?1").bind(session).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(r.rows_affected() > 0)
    }

    // --- facts ----------------------------------------------------------

    pub async fn facts(&self) -> Result<Vec<Fact>> {
        let rows: Vec<(i64, String)> =
            sqlx::query_as("SELECT id, text FROM facts ORDER BY id").fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(|(id, text)| Fact { id, text }).collect())
    }

    /// Add facts learned in `session`, skipping ones already known
    /// (case-insensitive). Returns how many were new.
    pub async fn add_facts(&self, session: i64, facts: &[String]) -> Result<usize> {
        let mut known: Vec<String> =
            self.facts().await?.into_iter().map(|f| normalize_fact(&f.text)).collect();
        let mut added = 0;
        for fact in facts {
            let norm = normalize_fact(fact);
            if norm.is_empty() || known.contains(&norm) {
                continue;
            }
            sqlx::query("INSERT INTO facts (text, session_id) VALUES (?1, ?2)")
                .bind(fact.trim())
                .bind(session)
                .execute(&self.pool)
                .await?;
            known.push(norm);
            added += 1;
        }
        Ok(added)
    }

    /// Strike a fact out. Returns false if there was no such fact.
    pub async fn forget_fact(&self, id: i64) -> Result<bool> {
        let r = sqlx::query("DELETE FROM facts WHERE id = ?1").bind(id).execute(&self.pool).await?;
        Ok(r.rows_affected() > 0)
    }
}

type SessionRow = (i64, String, String, Option<String>, Option<String>, Option<String>, Option<String>, Option<bool>, i64);

impl From<SessionRow> for SessionInfo {
    fn from(r: SessionRow) -> Self {
        let (id, day, started_at, ended_at, summary, oath, oath_day, oath_kept, minutes) = r;
        Self { id, day, started_at, ended_at, summary, oath, oath_day, oath_kept, focused_minutes: minutes as u32 }
    }
}

fn normalize_fact(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Turn free text into a safe FTS5 query: the meaningful words, each quoted,
/// joined with OR (bm25 ranks lines matching more of them first).
fn fts_query(text: &str) -> Option<String> {
    const STOP: &[&str] = &[
        "the", "and", "for", "you", "are", "was", "not", "but", "how", "can", "did", "has", "had",
        "her", "his", "she", "him", "its", "our", "out", "who", "why", "all", "any", "yes",
        "about", "after", "again", "also", "because", "been", "before", "being", "could", "does",
        "doing", "from", "have", "just", "know", "like", "make", "more", "much", "only", "really",
        "should", "some", "than", "that", "their", "them", "then", "there", "these", "they",
        "thing", "this", "today", "tonight", "very", "want", "what", "when", "where", "which",
        "while", "will", "with", "would", "your", "yours",
    ];
    let mut words: Vec<String> = Vec::new();
    for w in text.to_lowercase().split(|c: char| !c.is_alphanumeric()) {
        if w.chars().count() >= 3 && !STOP.contains(&w) && !words.iter().any(|x| x == w) {
            words.push(w.to_string());
        }
    }
    words.truncate(12);
    (!words.is_empty()).then(|| {
        words.iter().map(|w| format!("\"{w}\"")).collect::<Vec<_>>().join(" OR ")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    async fn temp_memory() -> (Memory, std::path::PathBuf) {
        static N: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "lasthope-test-{}-{}.db",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        (Memory::open(&path).await.unwrap(), path)
    }

    fn cleanup(path: std::path::PathBuf) {
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
        }
    }

    #[test]
    fn fts_query_is_safe_and_meaningful() {
        assert_eq!(
            fts_query("What about the EVM's storage \"slots\"? AND/OR NEAR(x)").unwrap(),
            "\"evm\" OR \"storage\" OR \"slots\" OR \"near\""
        );
        assert_eq!(fts_query("a an it"), None);
    }

    #[tokio::test]
    async fn session_lifecycle_messages_and_search() {
        let (m, path) = temp_memory().await;
        assert_eq!(m.mission().await.unwrap(), DEFAULT_MISSION);

        let s1 = m.start_session().await.unwrap();
        m.add_message(s1, Speaker::Me, "Tonight I study the EVM storage layout.").await.unwrap();
        m.add_message(s1, Speaker::Her, "Then begin the vigil.").await.unwrap();
        assert_eq!(m.unfinished_sessions(None).await.unwrap(), [s1]);

        // a crash: the next run finds s1 unfinished and closes it
        let s2 = m.start_session().await.unwrap();
        assert_eq!(m.unfinished_sessions(Some(s2)).await.unwrap(), [s1]);
        m.finish_session(s1, None, true).await.unwrap();
        assert_eq!(m.sessions_missing_summary(5).await.unwrap(), [s1]);
        m.finish_session(s1, Some("They studied storage."), true).await.unwrap();
        assert!(m.sessions_missing_summary(5).await.unwrap().is_empty());
        assert!(m.unfinished_sessions(Some(s2)).await.unwrap().is_empty());
        let (_, summary) = m.last_session(s2).await.unwrap().unwrap();
        assert_eq!(summary.as_deref(), Some("They studied storage."));

        let lines = m.session_lines(s1).await.unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].speaker, Speaker::Her);

        // search: stemmed ("layouts" finds "layout"), excludes the current session
        m.add_message(s2, Speaker::Me, "storage again").await.unwrap();
        let hits = m.search("how do storage layouts work", 5, Some(s2)).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session, s1);
        assert!(m.search("zebra", 5, None).await.unwrap().is_empty());
        cleanup(path);
    }

    #[tokio::test]
    async fn oath_verdict_and_streak_use_real_dates() {
        let (m, path) = temp_memory().await;
        let today = m.today().await.unwrap();

        let s1 = m.start_session().await.unwrap();
        let day = m.set_oath(s1, "Read one chapter.").await.unwrap();
        assert_eq!(day, today.next());
        // not due yet: it is tomorrow's oath
        let s2 = m.start_session().await.unwrap();
        assert_eq!(m.pending_oath(today, s2).await.unwrap(), None);
        assert_eq!(m.upcoming_oath(today).await.unwrap().unwrap().text, "Read one chapter.");
        // tomorrow it is due
        let pending = m.pending_oath(today.next(), s2).await.unwrap().unwrap();
        assert_eq!(pending.session, s1);

        let streak = m.record_verdict(s1, true).await.unwrap();
        assert_eq!(streak, Streak { count: 1, last: Some(day) });
        assert_eq!(m.streak().await.unwrap(), streak);
        // one verdict per oath, and none for sessions without one
        assert!(m.record_verdict(s1, false).await.is_err());
        assert!(m.record_verdict(s2, true).await.is_err());
        assert_eq!(m.pending_oath(today.next(), s2).await.unwrap(), None);
        cleanup(path);
    }

    #[tokio::test]
    async fn vigils_count_by_day() {
        let (m, path) = temp_memory().await;
        let today = m.today().await.unwrap();
        let yesterday = Day(today.0 - 1);
        let s = m.start_session().await.unwrap();
        assert_eq!(m.focus_on(today).await.unwrap(), Focus::default());
        m.add_vigil(s, today, 25, VigilSource::Timer).await.unwrap();
        m.add_vigil(s, today, 40, VigilSource::Told).await.unwrap();
        m.add_vigil(s, yesterday, 50, VigilSource::Told).await.unwrap();
        assert_eq!(m.focus_on(today).await.unwrap(), Focus { minutes: 65, vigils: 2 });
        assert_eq!(m.focus_on(yesterday).await.unwrap(), Focus { minutes: 50, vigils: 1 });
        let on_session: i64 = sqlx::query_scalar("SELECT focused_minutes FROM sessions WHERE id = ?1")
            .bind(s)
            .fetch_one(&m.pool)
            .await
            .unwrap();
        assert_eq!(on_session, 25); // timed vigils only
        cleanup(path);
    }

    #[tokio::test]
    async fn the_archive_lists_and_burns_nights() {
        let (m, path) = temp_memory().await;
        let today = m.today().await.unwrap();
        let s1 = m.start_session().await.unwrap();
        m.add_message(s1, Speaker::Me, "The storage layout is hard.").await.unwrap();
        m.add_facts(s1, &["They study the EVM.".into()]).await.unwrap();
        m.set_oath(s1, "Read chapter nine.").await.unwrap();
        m.add_vigil(s1, today, 25, VigilSource::Timer).await.unwrap();
        m.finish_session(s1, Some("They studied storage."), false).await.unwrap();
        let s2 = m.start_session().await.unwrap();

        let all = m.sessions().await.unwrap();
        assert_eq!(all.iter().map(|s| s.id).collect::<Vec<_>>(), [s2, s1]);
        let one = m.session_info(s1).await.unwrap().unwrap();
        assert_eq!(one.summary.as_deref(), Some("They studied storage."));
        assert_eq!(one.oath.as_deref(), Some("Read chapter nine."));
        assert_eq!(one.oath_kept, None);
        assert_eq!(one.focused_minutes, 25);
        assert!(all[0].ended_at.is_none());
        assert_eq!(m.facts_of_session(s1).await.unwrap().len(), 1);

        assert!(m.delete_session(s1).await.unwrap());
        assert!(!m.delete_session(s1).await.unwrap());
        assert!(m.session_info(s1).await.unwrap().is_none());
        assert!(m.facts().await.unwrap().is_empty());
        assert!(m.search("storage layout", 5, None).await.unwrap().is_empty());
        assert_eq!(m.focus_on(today).await.unwrap().minutes, 25); // the work still counts

        m.set_setting("voice", "5").await.unwrap();
        m.set_setting("voice", "7").await.unwrap();
        assert_eq!(m.setting("voice").await.unwrap().as_deref(), Some("7"));
        assert_eq!(m.setting("nope").await.unwrap(), None);
        m.set_speaks_first(false).await.unwrap();
        assert!(!m.speaks_first().await.unwrap());
        cleanup(path);
    }

    #[tokio::test]
    async fn facts_are_deduplicated_and_deletable() {
        let (m, path) = temp_memory().await;
        let s = m.start_session().await.unwrap();
        let added = m
            .add_facts(s, &["They train at the gym on Mondays.".into(), "They live in Berlin.".into()])
            .await
            .unwrap();
        assert_eq!(added, 2);
        let added = m.add_facts(s, &["they train at the gym on mondays".into()]).await.unwrap();
        assert_eq!(added, 0);
        let facts = m.facts().await.unwrap();
        assert!(m.forget_fact(facts[0].id).await.unwrap());
        assert!(!m.forget_fact(facts[0].id).await.unwrap());
        assert_eq!(m.facts().await.unwrap().len(), 1);
        cleanup(path);
    }
}
