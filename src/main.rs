use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use churncode::{
    Format, ReportOptions, UpdateOptions, VerifyOptions, parse_duration, parse_time, report,
    update, verify,
};

/// Per-commit code churn of a git branch: the share of the lines each first-parent commit adds
/// that a later commit changes or deletes within a window after it.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build the database from the branch's first-parent history, or extend it with the commits
    /// after its newest.
    Update {
        /// The git repository.
        #[arg(long)]
        repo: PathBuf,
        /// The database file, created when it does not exist.
        #[arg(long)]
        db: PathBuf,
        /// The branch (or any revision) whose first-parent history is read.
        #[arg(long, default_value = "HEAD")]
        rev: String,
        /// A glob of paths whose lines are not counted, as a git `:(exclude,glob)` pathspec
        /// (`**/*.lock`). Repeatable. Stored in the database: every later update must name the
        /// same ones.
        #[arg(long = "exclude", value_name = "PATHSPEC")]
        excludes: Vec<String>,
        /// A commit whose tree counts as already there: history starts after it, which lets a
        /// shallow clone be tracked from its oldest commit. Stored in the database.
        #[arg(long, value_name = "SHA")]
        base: Option<String>,
        /// Worker threads, each running one `git diff-tree`. Defaults to the number of CPUs.
        #[arg(short = 'j', long = "jobs", value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
        jobs: Option<u32>,
    },
    /// Print lines added and lines churned, in total and per first-parent commit.
    Report {
        /// The database `churncode update` wrote.
        #[arg(long)]
        db: PathBuf,
        /// Only commits with a committer time at or after this (RFC 3339 or unix seconds).
        #[arg(long, value_parser = parse_time)]
        since: Option<i64>,
        /// Only commits with a committer time before this (RFC 3339 or unix seconds).
        #[arg(long, value_parser = parse_time)]
        until: Option<i64>,
        /// A line is churned when a commit at most this long after the one that added it changes
        /// or deletes it (14d, 36h, 1d12h, 90s).
        #[arg(long, default_value = "14d", value_parser = parse_duration)]
        window: i64,
        /// table for reading, json and tsv for other programs.
        #[arg(long, value_enum, default_value_t = Format::Table)]
        format: Format,
        /// One row per first-parent commit: sha, committer time, subject, lines added, lines
        /// churned, and whether the figure is final (its window has ended). table and json print
        /// the totals too; tsv prints the commit rows only.
        #[arg(long)]
        per_commit: bool,
    },
    /// Compare the stored line origins with `git blame --first-parent`; exits 1 on any
    /// disagreement.
    Verify {
        /// The git repository.
        #[arg(long)]
        repo: PathBuf,
        /// The database `churncode update` wrote.
        #[arg(long)]
        db: PathBuf,
        /// Files to check, as paths from the repository's root.
        #[arg(long, num_args = 1.., required = true, value_name = "PATH")]
        files: Vec<String>,
        /// The commit to blame at, which must be the database's newest commit (the default).
        #[arg(long, value_name = "REV")]
        head: Option<String>,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        // A reader that stopped early (`churncode report | head`) wants no more output.
        Err(churncode::Error::Io(err)) if err.kind() == std::io::ErrorKind::BrokenPipe => {
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("churncode: {err}");
            ExitCode::from(2)
        }
    }
}

fn print(text: &str) -> Result<(), churncode::Error> {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(text.as_bytes())?;
    stdout.flush()?;
    Ok(())
}

fn run(cli: Cli) -> Result<ExitCode, churncode::Error> {
    match cli.command {
        Command::Update {
            repo,
            db,
            rev,
            excludes,
            base,
            jobs,
        } => {
            let jobs = jobs.map_or_else(
                || std::thread::available_parallelism().map_or(1, |n| n.get()),
                |j| j as usize,
            );
            let summary = update(&UpdateOptions {
                repo,
                db,
                rev,
                excludes,
                base,
                jobs,
            })?;
            if summary.processed == 0 {
                print(&format!(
                    "up to date at {} ({} commits)\n",
                    summary.tip, summary.total_commits
                ))?;
            } else {
                print(&format!(
                    "processed {} commits with {jobs} workers in {:.2}s (diff {:.2}s, apply \
                     {:.2}s, write {:.2}s); {} commits up to {}\n",
                    summary.processed,
                    summary.total_time.as_secs_f64(),
                    summary.diff_time.as_secs_f64(),
                    summary.apply_time.as_secs_f64(),
                    summary.write_time.as_secs_f64(),
                    summary.total_commits,
                    summary.tip
                ))?;
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Report {
            db,
            since,
            until,
            window,
            format,
            per_commit,
        } => {
            let report = report(&ReportOptions {
                db,
                since,
                until,
                window,
            })?;
            print(&report.render(format, per_commit))?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Verify {
            repo,
            db,
            files,
            head,
        } => {
            let verification = verify(&VerifyOptions {
                repo,
                db,
                files,
                head,
            })?;
            print(&verification.render())?;
            Ok(if verification.agrees() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
    }
}
