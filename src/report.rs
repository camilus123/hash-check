//! Verification reports as CSV or plain text.
//!
//! A report covers one verification run: every entry that run looked at,
//! the untracked files found under the scanned roots, and any drives that
//! were not available. Problems come first, intact files last.

use crate::db::{Db, DbResult, Entry, Filter, Scope, Status};
use crate::hasher::to_hex;
use crate::util::{format_unix_nanos, format_unix_secs, group_digits, human_bytes};
use std::io::{self, Write};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Csv,
    Text,
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Format::Csv => "csv",
            Format::Text => "txt",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Format::Csv => "CSV (opens in Excel or LibreOffice)",
            Format::Text => "Plain text",
        }
    }
}

/// Most serious first.
pub const REPORT_ORDER: [Status; 5] = [
    Status::PossiblyCorrupted,
    Status::Unreadable,
    Status::Missing,
    Status::Changed,
    Status::Ok,
];

/// Meta keys a verification run stores for its report.
pub mod meta {
    pub const SCOPE: &str = "run_scope";
    pub const STARTED: &str = "run_started";
    pub const FINISHED: &str = "run_finished";
    pub const CANCELLED: &str = "run_cancelled";
    /// One line per volume: `<volume>\t<entries skipped>`.
    pub const UNAVAILABLE: &str = "run_unavailable";
    pub const RUN_OF_META: &str = "run_meta_for";
}

#[derive(Debug)]
pub enum ReportError {
    Db(crate::db::DbError),
    Io(io::Error),
}

impl std::fmt::Display for ReportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReportError::Db(e) => write!(f, "{e}"),
            ReportError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl From<crate::db::DbError> for ReportError {
    fn from(e: crate::db::DbError) -> Self {
        ReportError::Db(e)
    }
}

impl From<io::Error> for ReportError {
    fn from(e: io::Error) -> Self {
        ReportError::Io(e)
    }
}

struct RunInfo {
    scope: String,
    started: Option<i64>,
    finished: Option<i64>,
    cancelled: bool,
    unavailable: Vec<(String, u64)>,
}

fn run_info(db: &Db, run: i64) -> DbResult<RunInfo> {
    // The descriptive details are kept for the latest run only.
    let current = db
        .meta(meta::RUN_OF_META)?
        .and_then(|v| v.parse::<i64>().ok())
        == Some(run);
    let get =
        |key: &str| -> DbResult<Option<String>> { if current { db.meta(key) } else { Ok(None) } };
    let unavailable = get(meta::UNAVAILABLE)?
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (vol, n) = l.split_once('\t')?;
            Some((vol.to_string(), n.parse().ok()?))
        })
        .collect();
    Ok(RunInfo {
        scope: get(meta::SCOPE)?.unwrap_or_else(|| "unknown".into()),
        started: get(meta::STARTED)?.and_then(|v| v.parse().ok()),
        finished: get(meta::FINISHED)?.and_then(|v| v.parse().ok()),
        cancelled: get(meta::CANCELLED)?.as_deref() == Some("1"),
        unavailable,
    })
}

/// Write the report for verification run `run`. Returns the number of
/// entry rows written.
pub fn write_report(
    db: &Db,
    run: i64,
    format: Format,
    w: &mut dyn Write,
) -> Result<u64, ReportError> {
    let info = run_info(db, run)?;
    let counts = db.status_counts(None, Some(run))?;
    let new_ids = db.new_file_ids(Scope::All, "")?;
    match format {
        Format::Csv => {
            // A byte-order mark makes Excel read the file as UTF-8.
            write!(w, "\u{feff}")?;
            csv_row(
                w,
                &[
                    "status",
                    "path",
                    "size_bytes",
                    "modified",
                    "algorithm",
                    "stored_hash",
                    "last_verified",
                    "detail",
                ],
            )?;
        }
        Format::Text => {
            writeln!(w, "hash-check {} verification report", crate::VERSION)?;
            writeln!(w, "{}", "=".repeat(60))?;
            writeln!(
                w,
                "Run:        #{run}{}",
                if info.cancelled {
                    " (CANCELLED before it finished)"
                } else {
                    ""
                }
            )?;
            writeln!(w, "Scope:      {}", info.scope)?;
            if let Some(t) = info.started {
                writeln!(w, "Started:    {}", format_unix_secs(t))?;
            }
            if let Some(t) = info.finished {
                writeln!(w, "Finished:   {}", format_unix_secs(t))?;
            }
            writeln!(w)?;
            writeln!(w, "Summary")?;
            writeln!(w, "-------")?;
            for s in REPORT_ORDER {
                writeln!(w, "{:<20}{:>12}", s.label(), group_digits(counts.get(s)))?;
            }
            writeln!(
                w,
                "{:<20}{:>12}",
                "New (untracked)",
                group_digits(new_ids.len() as u64)
            )?;
            for (vol, n) in &info.unavailable {
                writeln!(
                    w,
                    "Drive not available: {vol} ({} entries not checked)",
                    group_digits(*n)
                )?;
            }
            writeln!(
                w,
                "{:<20}{:>12}",
                "Checked in total",
                group_digits(counts.total())
            )?;
        }
    }

    let mut rows = 0u64;
    for (vol, n) in &info.unavailable {
        if format == Format::Csv {
            csv_row(
                w,
                &[
                    "Drive not available",
                    vol,
                    "",
                    "",
                    "",
                    "",
                    "",
                    &format!("{n} entries not checked"),
                ],
            )?;
        }
    }
    for status in REPORT_ORDER {
        let ids = db.ids_matching(&Filter {
            status: Some(status),
            run: Some(run),
            ..Default::default()
        })?;
        if ids.is_empty() {
            continue;
        }
        if format == Format::Text {
            writeln!(w)?;
            writeln!(w, "{} ({})", status.label(), group_digits(ids.len() as u64))?;
            writeln!(w, "{}", "-".repeat(60))?;
        }
        for id in ids {
            let Some(e) = db.entry(id)? else { continue };
            entry_row(w, format, &e)?;
            rows += 1;
        }
    }
    if !new_ids.is_empty() {
        if format == Format::Text {
            writeln!(w)?;
            writeln!(
                w,
                "New: under a tracked folder but not in the database ({})",
                group_digits(new_ids.len() as u64)
            )?;
            writeln!(w, "{}", "-".repeat(60))?;
        }
        for id in new_ids {
            let Some(n) = db.new_file(id)? else { continue };
            match format {
                Format::Csv => csv_row(
                    w,
                    &[
                        "New",
                        &n.path,
                        &n.size.to_string(),
                        &format_unix_nanos(n.mtime_ns),
                        "",
                        "",
                        "",
                        "not in the database",
                    ],
                )?,
                Format::Text => writeln!(
                    w,
                    "{}  ({}, modified {})",
                    n.path,
                    human_bytes(n.size),
                    format_unix_nanos(n.mtime_ns)
                )?,
            }
        }
    }
    Ok(rows)
}

fn entry_row(w: &mut dyn Write, format: Format, e: &Entry) -> io::Result<()> {
    let detail = e.last_error.as_deref().unwrap_or("");
    match format {
        Format::Csv => csv_row(
            w,
            &[
                e.status.label(),
                &e.path,
                &e.size.to_string(),
                &format_unix_nanos(e.mtime_ns),
                e.algo.name(),
                &to_hex(&e.hash),
                &format_unix_secs(e.last_verified),
                detail,
            ],
        ),
        Format::Text => {
            writeln!(w, "{}", e.path)?;
            if e.status != Status::Ok {
                writeln!(
                    w,
                    "    stored: {} bytes, modified {}, {} {}",
                    group_digits(e.size),
                    format_unix_nanos(e.mtime_ns),
                    e.algo.name(),
                    to_hex(&e.hash)
                )?;
                if !detail.is_empty() {
                    writeln!(w, "    {detail}")?;
                }
            }
            Ok(())
        }
    }
}

fn csv_row(w: &mut dyn Write, fields: &[&str]) -> io::Result<()> {
    let mut line = String::new();
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            line.push(',');
        }
        line.push_str(&csv_field(f));
    }
    // CRLF line ends, as RFC 4180 and Excel expect.
    write!(w, "{line}\r\n")
}

/// Quote a CSV field when needed. A leading `=`, `+`, `-` or `@` is
/// prefixed with an apostrophe so a spreadsheet does not run a file name
/// as a formula.
pub fn csv_field(f: &str) -> String {
    let risky = f.starts_with(['=', '+', '-', '@', '\t', '\r']);
    let f = if risky {
        format!("'{f}")
    } else {
        f.to_string()
    };
    if f.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", f.replace('"', "\"\""))
    } else {
        f
    }
}

#[cfg(test)]
mod tests {
    use super::csv_field;

    #[test]
    fn csv_quoting() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_field("two\nlines"), "\"two\nlines\"");
        assert_eq!(csv_field("=cmd()"), "'=cmd()");
        assert_eq!(csv_field("-5"), "'-5");
        assert_eq!(csv_field(r"C:\a\b.txt"), r"C:\a\b.txt");
    }
}
