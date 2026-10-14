//! `churncode update`: reads the first-parent history into a database, or extends one with the
//! commits after its newest.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use rusqlite::TransactionBehavior;

use crate::git::{self, CommitInfo, Git};
use crate::store::{self, CommitRow, Settings};
use crate::track::{Applied, Origin, State};
use crate::{Error, diff};

/// What to read, from where, into which database.
#[derive(Debug, Clone)]
pub struct UpdateOptions {
    pub repo: PathBuf,
    pub db: PathBuf,
    /// The branch (or any revision) whose first-parent history is read.
    pub rev: String,
    /// Globs of paths whose lines are not counted, as git `:(exclude,glob)` pathspecs.
    pub excludes: Vec<String>,
    /// A commit whose tree counts as already there (origin 0): history starts after it.
    pub base: Option<String>,
    /// Worker threads, each running one `git diff-tree --stdin`.
    pub jobs: usize,
}

/// What an update did.
#[derive(Debug, Clone)]
pub struct UpdateSummary {
    /// Commits this update read.
    pub processed: usize,
    /// The newest commit the database holds (the base commit when it holds none).
    pub tip: String,
    /// Commits the database holds.
    pub total_commits: u32,
    pub diff_time: Duration,
    pub apply_time: Duration,
    pub write_time: Duration,
    pub total_time: Duration,
}

/// Builds the database, or extends it with the first-parent commits after its newest.
pub fn update(options: &UpdateOptions) -> Result<UpdateSummary, Error> {
    let started = Instant::now();
    let git = Git::open(&options.repo)?;
    let target = git.resolve_commit(&options.rev)?;
    let mut excludes = options.excludes.clone();
    excludes.sort();
    excludes.dedup();
    let base = options
        .base
        .as_deref()
        .map(|b| git.resolve_commit(b))
        .transpose()?;
    let settings = Settings { excludes, base };

    let mut conn = store::open_for_update(&options.db)?;
    // Taking the write lock first keeps a concurrent update from reading the same progress.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (mut state, commits, first_idx) = match store::progress(&tx)? {
        Some(progress) => {
            if progress.settings != settings {
                return Err(Error::Refused(format!(
                    "{} was built with {}, and this update names {}. Excludes and --base decide \
                     which lines are counted, so a database holds one choice of them: use a new \
                     database for different ones.",
                    options.db.display(),
                    describe(&progress.settings),
                    describe(&settings),
                )));
            }
            let commits = commits_after(&git, &progress.tip, &target, &options.rev)?;
            if commits.is_empty() {
                return Ok(UpdateSummary {
                    processed: 0,
                    tip: progress.tip,
                    total_commits: progress.tip_idx,
                    diff_time: Duration::ZERO,
                    apply_time: Duration::ZERO,
                    write_time: Duration::ZERO,
                    total_time: started.elapsed(),
                });
            }
            (store::load_state(&tx)?, commits, progress.tip_idx + 1)
        }
        None => {
            let (state, commits) = start(&git, &settings, &target, &options.rev)?;
            (state, commits, 1)
        }
    };

    let clock = Instant::now();
    let diffs = diff::commit_diffs(&git, &settings.excludes, &commits, options.jobs)?;
    let diff_time = clock.elapsed();

    let clock = Instant::now();
    let applied = commits
        .iter()
        .zip(&diffs)
        .zip(first_idx..)
        .map(|((commit, files), idx)| state.apply(&commit.sha, idx, files))
        .collect::<Result<Vec<Applied>, Error>>()?;
    drop(diffs);
    let apply_time = clock.elapsed();

    let clock = Instant::now();
    let rows: Vec<CommitRow> = commits
        .iter()
        .zip(&applied)
        .zip(first_idx..)
        .map(|((commit, applied), idx)| CommitRow {
            idx,
            sha: &commit.sha,
            committer_time: commit.committer_time,
            subject: &commit.subject,
            applied,
        })
        .collect();
    let tip_idx = first_idx + commits.len() as Origin - 1;
    let tip = match commits.last() {
        Some(commit) => commit.sha.clone(),
        // A new database whose base is the target itself.
        None => target,
    };
    store::append(&tx, &settings, &rows, &state, &tip, tip_idx)?;
    tx.commit()?;
    let write_time = clock.elapsed();

    Ok(UpdateSummary {
        processed: commits.len(),
        tip,
        total_commits: tip_idx,
        diff_time,
        apply_time,
        write_time,
        total_time: started.elapsed(),
    })
}

/// The state and commits of a new database.
fn start(
    git: &Git,
    settings: &Settings,
    target: &str,
    rev: &str,
) -> Result<(State, Vec<CommitInfo>), Error> {
    let Some(base) = &settings.base else {
        let commits = git.first_parent_commits(target)?;
        let oldest = commits.first().expect("rev-list lists the target itself");
        if oldest.first_parent.is_none() && git.records_parent(&oldest.sha)? {
            return Err(Error::Refused(format!(
                "{} is the oldest commit of a shallow clone, and the history before it is \
                 missing. Pass --base {} to count its tree as already there, or fetch more \
                 history.",
                oldest.sha, oldest.sha
            )));
        }
        return Ok((State::default(), commits));
    };
    let commits = git.first_parent_commits(&format!("{base}..{target}"))?;
    let follows_base = match commits.first() {
        Some(oldest) => oldest.first_parent.as_deref() == Some(base),
        None => base == target,
    };
    if !follows_base {
        return Err(Error::Refused(format!(
            "--base {base} is not on the first-parent history of {rev} ({target})"
        )));
    }
    Ok((base_state(git, base, &settings.excludes)?, commits))
}

/// The commits after the database's newest, refusing a history that no longer contains it.
fn commits_after(git: &Git, tip: &str, target: &str, rev: &str) -> Result<Vec<CommitInfo>, Error> {
    if tip == target {
        return Ok(Vec::new());
    }
    let rewritten = || {
        Error::Refused(format!(
            "the database's newest commit {tip} is not an ancestor of {rev} ({target}): the \
             history was rewritten. Build a new database."
        ))
    };
    if !git.has_commit(tip)? || !git.is_ancestor(tip, target)? {
        return Err(rewritten());
    }
    let commits = git.first_parent_commits(&format!("{tip}..{target}"))?;
    match commits.first() {
        Some(oldest) if oldest.first_parent.as_deref() == Some(tip) => Ok(commits),
        Some(_) => Err(Error::Refused(format!(
            "the database's newest commit {tip} is an ancestor of {rev} ({target}) but not on \
             its first-parent history: a merge made the branch's previous history a side branch. \
             Build a new database."
        ))),
        None => Err(rewritten()),
    }
}

/// Every text file at `base`, with all its lines of origin 0.
fn base_state(git: &Git, base: &str, excludes: &[String]) -> Result<State, Error> {
    let mut args: Vec<OsString> = [
        "diff-tree",
        "-r",
        "-z",
        "--numstat",
        "--no-renames",
        "--ignore-submodules=all",
        "--no-ext-diff",
        "--no-textconv",
        "--no-relative",
        git.empty_tree(),
        base,
    ]
    .iter()
    .map(OsString::from)
    .collect();
    args.extend(git::exclude_pathspecs(excludes));
    let out = git.output(&args)?;
    let mut state = State::default();
    // "<added>\t<deleted>\t<path>\0" per file; a binary file's counts are "-".
    for entry in out.split(|&b| b == 0).filter(|e| !e.is_empty()) {
        let mut fields = entry.splitn(3, |&b| b == b'\t');
        let (Some(added), Some(_), Some(path)) = (fields.next(), fields.next(), fields.next())
        else {
            return Err(Error::GitOutput(format!(
                "unreadable numstat entry: {}",
                String::from_utf8_lossy(entry)
            )));
        };
        if added == b"-" {
            continue;
        }
        let lines: usize = std::str::from_utf8(added)
            .ok()
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| {
                Error::GitOutput(format!(
                    "unreadable numstat entry: {}",
                    String::from_utf8_lossy(entry)
                ))
            })?;
        if lines > 0 {
            state.touched.insert(path.to_vec());
            state.files.insert(path.to_vec(), vec![0; lines]);
        }
    }
    Ok(state)
}

fn describe(settings: &Settings) -> String {
    let excludes = if settings.excludes.is_empty() {
        "no --exclude".to_owned()
    } else {
        settings
            .excludes
            .iter()
            .map(|e| format!("--exclude '{e}'"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    match &settings.base {
        Some(base) => format!("{excludes} and --base {base}"),
        None => format!("{excludes} and no --base"),
    }
}
