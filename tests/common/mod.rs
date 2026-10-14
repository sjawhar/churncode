//! Throwaway git repositories with scripted commits, for the integration tests.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use churncode::{
    CommitChurn, ReportOptions, UpdateOptions, UpdateSummary, Verification, VerifyOptions,
};

/// 2023-11-14T22:13:20Z, the time of the first scripted commit.
pub const T0: i64 = 1_700_000_000;
pub const HOUR: i64 = 3_600;
pub const DAY: i64 = 86_400;
pub const WINDOW: i64 = 14 * DAY;

/// A repository in a temporary directory, removed when dropped.
pub struct Repo {
    dir: tempfile::TempDir,
}

impl Repo {
    pub fn new() -> Repo {
        let repo = Repo {
            dir: tempfile::tempdir().unwrap(),
        };
        repo.git(&["init", "-q", "-b", "main"]);
        repo
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Runs git in the repository, isolated from the user's configuration, and returns its
    /// standard output; panics when git fails.
    pub fn git(&self, args: &[&str]) -> String {
        git_in(self.path(), args, &[])
    }

    fn file(&self, path: &str) -> PathBuf {
        let file = self.path().join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        file
    }

    pub fn write(&self, path: &str, content: impl AsRef<[u8]>) {
        std::fs::write(self.file(path), content).unwrap();
    }

    pub fn read(&self, path: &str) -> Vec<u8> {
        std::fs::read(self.path().join(path)).unwrap()
    }

    pub fn remove(&self, path: &str) {
        std::fs::remove_file(self.path().join(path)).unwrap();
    }

    pub fn rename(&self, from: &str, to: &str) {
        std::fs::rename(self.path().join(from), self.file(to)).unwrap();
    }

    /// Commits every change in the work tree at `time` (unix seconds) and returns the commit id.
    pub fn commit(&self, message: &str, time: i64) -> String {
        self.git(&["add", "-A"]);
        let date = format!("@{time} +0000");
        git_in(
            self.path(),
            &["commit", "-q", "--allow-empty", "-m", message],
            &[("GIT_AUTHOR_DATE", &date), ("GIT_COMMITTER_DATE", &date)],
        );
        self.head()
    }

    /// Merges `branch` into the current branch with a merge commit at `time`.
    pub fn merge(&self, branch: &str, message: &str, time: i64) -> String {
        let date = format!("@{time} +0000");
        git_in(
            self.path(),
            &["merge", "-q", "--no-ff", "-m", message, branch],
            &[("GIT_AUTHOR_DATE", &date), ("GIT_COMMITTER_DATE", &date)],
        );
        self.head()
    }

    pub fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"]).trim().to_owned()
    }

    /// Every tracked path at HEAD.
    pub fn files(&self) -> Vec<String> {
        self.git(&["-c", "core.quotePath=false", "ls-files"])
            .lines()
            .map(str::to_owned)
            .collect()
    }

    pub fn update_options(&self, db: &Path) -> UpdateOptions {
        UpdateOptions {
            repo: self.path().to_path_buf(),
            db: db.to_path_buf(),
            rev: "main".into(),
            excludes: Vec::new(),
            base: None,
            jobs: 3,
        }
    }

    /// Builds or extends `db` from main's history.
    pub fn update(&self, db: &Path) -> UpdateSummary {
        churncode::update(&self.update_options(db)).unwrap()
    }

    /// Compares `db`'s origins of every file at HEAD with `git blame`.
    pub fn verify(&self, db: &Path, files: Vec<String>) -> Verification {
        churncode::verify(&VerifyOptions {
            repo: self.path().to_path_buf(),
            db: db.to_path_buf(),
            files,
            head: None,
        })
        .unwrap()
    }
}

pub fn git_in(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .envs(env.iter().copied())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// `n` lines `<prefix>1` to `<prefix><n>`, each ending in a newline.
pub fn lines(prefix: &str, n: usize) -> String {
    (1..=n).map(|i| format!("{prefix}{i}\n")).collect()
}

/// Every commit of `db`, with the given window.
pub fn commits(db: &Path, window: i64) -> Vec<CommitChurn> {
    churncode::report(&ReportOptions {
        db: db.to_path_buf(),
        since: None,
        until: None,
        window,
    })
    .unwrap()
    .commits
}

/// Each commit's subject, lines added and lines churned within `window`.
pub fn figures(db: &Path, window: i64) -> Vec<(String, u64, u64)> {
    commits(db, window)
        .into_iter()
        .map(|c| (c.subject, c.lines_added, c.lines_churned))
        .collect()
}

pub fn expected(rows: &[(&str, u64, u64)]) -> Vec<(String, u64, u64)> {
    rows.iter()
        .map(|&(subject, added, churned)| (subject.to_owned(), added, churned))
        .collect()
}

/// A database path in its own temporary directory.
pub struct Db {
    dir: tempfile::TempDir,
}

impl Db {
    pub fn new() -> Db {
        Db {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.path().join("churn.db")
    }

    pub fn named(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
}
