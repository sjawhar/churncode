//! Code churn of a git branch: the share of the lines each first-parent commit adds that a later
//! commit changes or deletes within a window after it.
//!
//! [`update`] reads the branch's first-parent history into a SQLite database by forward line
//! tracking: every line of every file carries the commit that added it, from one commit's diff to
//! the next, and every change or deletion of a line is recorded against the line's origin.
//! [`report`] reads per-commit figures for any window from that database, and [`verify`] checks
//! the stored line origins against `git blame --first-parent`.

mod diff;
mod error;
mod git;
mod patch;
mod report;
mod store;
mod time;
mod track;
mod update;
mod verify;

pub use error::Error;
pub use report::{CommitChurn, Format, Report, ReportOptions, Totals, report};
pub use time::{format_duration, format_time, parse_duration, parse_time};
pub use update::{UpdateOptions, UpdateSummary, update};
pub use verify::{FileAgreement, Verification, VerifyOptions, verify};
