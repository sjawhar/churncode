//! `churncode report`: lines added and lines churned per first-parent commit, for any window.

use std::fmt::Write;
use std::path::PathBuf;

use rusqlite::params;
use serde::Serialize;

use crate::Error;
use crate::store;
use crate::time::{format_duration, format_time};

/// Which commits to report, and the churn window.
#[derive(Debug, Clone)]
pub struct ReportOptions {
    pub db: PathBuf,
    /// Commits with a committer time at or after this (unix seconds).
    pub since: Option<i64>,
    /// Commits with a committer time before this (unix seconds).
    pub until: Option<i64>,
    /// A line counts as churned when a commit at most this many seconds after the commit that
    /// added it changes or deletes it.
    pub window: i64,
}

/// One first-parent commit's figures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitChurn {
    pub sha: String,
    pub committer_time: i64,
    pub subject: String,
    pub lines_added: u64,
    /// Lines this commit added that a later commit within the window changed or deleted.
    pub lines_churned: u64,
    /// The commit's time plus the window is at or before the newest processed commit's time, so
    /// a later update cannot change `lines_churned`.
    pub is_final: bool,
}

/// The sums over the reported commits.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Totals {
    pub commits: u64,
    pub lines_added: u64,
    pub lines_churned: u64,
    /// Every reported commit's figure is final.
    pub is_final: bool,
}

impl Totals {
    /// Churned lines as a percentage of added lines; `None` when no lines were added.
    pub fn churn_percent(&self) -> Option<f64> {
        percent(self.lines_churned, self.lines_added)
    }
}

/// The figures for the commits a [`ReportOptions`] selects.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub window: i64,
    /// The newest processed commit and its committer time; `None` for a database that holds only
    /// a base commit.
    pub newest: Option<(String, i64)>,
    pub commits: Vec<CommitChurn>,
    pub totals: Totals,
}

/// How [`Report::render`] prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    Table,
    Json,
    Tsv,
}

/// Reads the figures for the selected commits from the database.
pub fn report(options: &ReportOptions) -> Result<Report, Error> {
    let conn = store::open_read_only(&options.db)?;
    if store::progress(&conn)?.is_none() {
        return Err(Error::Refused(format!(
            "{} holds no history yet; run churncode update first",
            options.db.display()
        )));
    }
    let newest: Option<(String, i64)> = conn
        .query_row(
            "SELECT sha, committer_time FROM commits ORDER BY idx DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            e => Err(e),
        })?;
    let newest_time = newest.as_ref().map_or(i64::MIN, |(_, time)| *time);
    let mut stmt = conn.prepare(
        "SELECT c.sha, c.committer_time, c.subject, c.lines_added,
                (SELECT COALESCE(SUM(d.n), 0)
                   FROM deletions d JOIN commits later ON later.idx = d.deleter_idx
                  WHERE d.origin_idx = c.idx
                    AND later.committer_time <= c.committer_time + ?3)
           FROM commits c
          WHERE c.committer_time >= ?1 AND c.committer_time < ?2
          ORDER BY c.idx",
    )?;
    let rows = stmt.query_map(
        params![
            options.since.unwrap_or(i64::MIN),
            options.until.unwrap_or(i64::MAX),
            options.window
        ],
        |row| {
            let committer_time: i64 = row.get(1)?;
            Ok(CommitChurn {
                sha: row.get(0)?,
                committer_time,
                subject: row.get(2)?,
                lines_added: row.get::<_, i64>(3)? as u64,
                lines_churned: row.get::<_, i64>(4)? as u64,
                is_final: committer_time.saturating_add(options.window) <= newest_time,
            })
        },
    )?;
    let commits = rows.collect::<Result<Vec<_>, _>>()?;
    let totals = Totals {
        commits: commits.len() as u64,
        lines_added: commits.iter().map(|c| c.lines_added).sum(),
        lines_churned: commits.iter().map(|c| c.lines_churned).sum(),
        is_final: commits.iter().all(|c| c.is_final),
    };
    Ok(Report {
        since: options.since,
        until: options.until,
        window: options.window,
        newest,
        commits,
        totals,
    })
}

fn percent(part: u64, whole: u64) -> Option<f64> {
    (whole > 0).then(|| 100.0 * part as f64 / whole as f64)
}

#[derive(Serialize)]
struct JsonReport<'a> {
    since: Option<String>,
    until: Option<String>,
    window: String,
    window_seconds: i64,
    newest_commit: Option<JsonNewest<'a>>,
    totals: JsonTotals,
    #[serde(skip_serializing_if = "Option::is_none")]
    commits: Option<Vec<JsonCommit<'a>>>,
}

#[derive(Serialize)]
struct JsonNewest<'a> {
    sha: &'a str,
    committer_time: String,
}

#[derive(Serialize)]
struct JsonTotals {
    commits: u64,
    lines_added: u64,
    lines_churned: u64,
    churn_percent: Option<f64>,
    #[serde(rename = "final")]
    is_final: bool,
}

#[derive(Serialize)]
struct JsonCommit<'a> {
    sha: &'a str,
    committer_time: String,
    subject: &'a str,
    lines_added: u64,
    lines_churned: u64,
    churn_percent: Option<f64>,
    #[serde(rename = "final")]
    is_final: bool,
}

impl Report {
    /// The report as text, with one row per commit when `per_commit` is set. TSV prints the
    /// commit rows with `per_commit` and the totals row without it.
    pub fn render(&self, format: Format, per_commit: bool) -> String {
        match format {
            Format::Json => self.json(per_commit),
            Format::Tsv => self.tsv(per_commit),
            Format::Table => self.table(per_commit),
        }
    }

    fn json(&self, per_commit: bool) -> String {
        let t = &self.totals;
        let report = JsonReport {
            since: self.since.map(format_time),
            until: self.until.map(format_time),
            window: format_duration(self.window),
            window_seconds: self.window,
            newest_commit: self.newest.as_ref().map(|(sha, time)| JsonNewest {
                sha,
                committer_time: format_time(*time),
            }),
            totals: JsonTotals {
                commits: t.commits,
                lines_added: t.lines_added,
                lines_churned: t.lines_churned,
                churn_percent: t.churn_percent().map(round2),
                is_final: t.is_final,
            },
            commits: per_commit.then(|| {
                self.commits
                    .iter()
                    .map(|c| JsonCommit {
                        sha: &c.sha,
                        committer_time: format_time(c.committer_time),
                        subject: &c.subject,
                        lines_added: c.lines_added,
                        lines_churned: c.lines_churned,
                        churn_percent: percent(c.lines_churned, c.lines_added).map(round2),
                        is_final: c.is_final,
                    })
                    .collect()
            }),
        };
        let mut out = serde_json::to_string_pretty(&report).expect("the report serializes");
        out.push('\n');
        out
    }

    fn tsv(&self, per_commit: bool) -> String {
        let mut out = String::new();
        if per_commit {
            out.push_str("sha\tcommitter_time\tlines_added\tlines_churned\tfinal\tsubject\n");
            for c in &self.commits {
                let _ = writeln!(
                    out,
                    "{}\t{}\t{}\t{}\t{}\t{}",
                    c.sha,
                    format_time(c.committer_time),
                    c.lines_added,
                    c.lines_churned,
                    c.is_final,
                    c.subject.replace(['\t', '\n', '\r'], " ")
                );
            }
        } else {
            let t = &self.totals;
            out.push_str("commits\tlines_added\tlines_churned\tchurn_percent\tfinal\n");
            let _ = writeln!(
                out,
                "{}\t{}\t{}\t{}\t{}",
                t.commits,
                t.lines_added,
                t.lines_churned,
                t.churn_percent()
                    .map_or_else(String::new, |p| format!("{p:.2}")),
                t.is_final
            );
        }
        out
    }

    fn table(&self, per_commit: bool) -> String {
        let mut out = String::new();
        if per_commit && !self.commits.is_empty() {
            let rows: Vec<[String; 6]> = self
                .commits
                .iter()
                .map(|c| {
                    [
                        c.sha.chars().take(12).collect(),
                        format_time(c.committer_time),
                        c.lines_added.to_string(),
                        c.lines_churned.to_string(),
                        percent(c.lines_churned, c.lines_added)
                            .map_or_else(|| "-".into(), |p| format!("{p:.1}%")),
                        if c.is_final { "yes" } else { "no" }.into(),
                    ]
                })
                .collect();
            let header = ["COMMIT", "COMMITTED", "ADDED", "CHURNED", "CHURN", "FINAL"];
            let mut widths = header.map(str::len);
            for row in &rows {
                for (width, cell) in widths.iter_mut().zip(row) {
                    *width = (*width).max(cell.len());
                }
            }
            let line = |out: &mut String, cells: [&str; 6], subject: &str| {
                for (i, (cell, width)) in cells.iter().zip(widths).enumerate() {
                    // Text left-aligned, numbers right-aligned.
                    if (2..=4).contains(&i) {
                        let _ = write!(out, "{cell:>width$}  ");
                    } else {
                        let _ = write!(out, "{cell:<width$}  ");
                    }
                }
                out.push_str(subject);
                out.push('\n');
            };
            line(&mut out, header, "SUBJECT");
            for (row, c) in rows.iter().zip(&self.commits) {
                line(&mut out, row.each_ref().map(String::as_str), &c.subject);
            }
            out.push('\n');
        }
        let range = match (self.since, self.until) {
            (None, None) => "all commits".to_owned(),
            (Some(s), None) => format!("commits from {}", format_time(s)),
            (None, Some(u)) => format!("commits before {}", format_time(u)),
            (Some(s), Some(u)) => format!("commits from {} to {}", format_time(s), format_time(u)),
        };
        let _ = writeln!(out, "{range}, window {}", format_duration(self.window));
        if let Some((sha, time)) = &self.newest {
            let short: String = sha.chars().take(12).collect();
            let _ = writeln!(
                out,
                "newest processed commit {short} at {}",
                format_time(*time)
            );
        }
        let t = &self.totals;
        let _ = writeln!(out, "commits        {}", t.commits);
        let _ = writeln!(out, "lines added    {}", t.lines_added);
        let _ = writeln!(out, "lines churned  {}", t.lines_churned);
        let churn = t
            .churn_percent()
            .map_or_else(|| "-".into(), |p| format!("{p:.2}%"));
        let _ = writeln!(out, "churn          {churn}");
        let _ = writeln!(
            out,
            "final          {}",
            if t.is_final {
                "yes"
            } else {
                "no (some commits' windows reach past the newest processed commit)"
            }
        );
        out
    }
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}
