//! Running git. Every invocation names the options its output depends on, so neither a user's git
//! configuration (diff algorithm, rename limit, path quoting, prefixes, colour, ignored blame
//! revisions) nor the repository's `.gitattributes` (binary, diff drivers, textconv) can change
//! what churncode reads: which lines exist is decided by the history's content alone.

use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::thread::JoinHandle;

use crate::Error;

/// The options of every patch churncode reads, from `diff-tree` here and, for `verify`, from
/// `blame`'s own diffs in [`BLAME_OPTIONS`]: zero context, full blob ids, renames found without a
/// limit, and git's default diff algorithm and indent heuristic named explicitly.
pub(crate) const PATCH_OPTIONS: &[&str] = &[
    "-r",
    "-p",
    "-U0",
    "-M",
    "-l0",
    "--full-index",
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--no-relative",
    "--src-prefix=a/",
    "--dst-prefix=b/",
    "--diff-algorithm=myers",
    "--indent-heuristic",
];

pub(crate) const BLAME_OPTIONS: &[&str] = &[
    "--first-parent",
    "--porcelain",
    "--no-textconv",
    "--diff-algorithm=myers",
    "--indent-heuristic",
    "--no-ignore-revs-file",
];

/// `--attr-source`, which keeps the repository's `.gitattributes` out of every diff.
const MIN_GIT: (u32, u32) = (2, 41);

/// A repository on disk.
#[derive(Clone, Debug)]
pub(crate) struct Git {
    repo: PathBuf,
    empty_tree: String,
}

/// One commit of the first-parent history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommitInfo {
    pub sha: String,
    /// `None` for a root commit, and for the oldest commit of a shallow clone.
    pub first_parent: Option<String>,
    pub committer_time: i64,
    pub subject: String,
}

impl Git {
    /// Checks that git is new enough and that `repo` is a repository.
    pub fn open(repo: &Path) -> Result<Git, Error> {
        check_version()?;
        let mut git = Git {
            repo: repo.to_path_buf(),
            empty_tree: String::new(),
        };
        let mut cmd = git.base_command();
        cmd.args(["rev-parse", "--git-dir"]);
        let out = cmd.output().map_err(Error::GitSpawn)?;
        if !out.status.success() {
            return Err(Error::Refused(format!(
                "{} is not a git repository",
                repo.display()
            )));
        }
        // The empty tree's id depends on the repository's object format.
        let mut cmd = git.base_command();
        cmd.args(["hash-object", "-t", "tree", "--stdin"]);
        git.empty_tree = String::from_utf8_lossy(&run(cmd, &["hash-object"])?)
            .trim()
            .to_owned();
        Ok(git)
    }

    pub fn empty_tree(&self) -> &str {
        &self.empty_tree
    }

    fn base_command(&self) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(&self.repo)
            // Paths in patch headers C-quoted, so that any byte is readable.
            .args(["-c", "core.quotePath=true"])
            // Binary detection as git's defaults decide it (see diff::BIG_FILE_THRESHOLD).
            .args(["-c", "core.bigFileThreshold=512m"])
            .args(["-c", "core.attributesFile=/dev/null"])
            .env("GIT_ATTR_NOSYSTEM", "1");
        cmd
    }

    /// A `git` command in the repository, reading attributes from the empty tree only.
    fn command<S: AsRef<OsStr>>(&self, args: &[S]) -> Command {
        let mut cmd = self.base_command();
        cmd.arg(format!("--attr-source={}", self.empty_tree))
            .args(args);
        cmd
    }

    /// Runs git and returns its standard output; a non-zero exit is an error carrying stderr.
    pub fn output<S: AsRef<OsStr>>(&self, args: &[S]) -> Result<Vec<u8>, Error> {
        run(self.command(args), args)
    }

    /// Resolves a revision to a full commit id.
    pub fn resolve_commit(&self, rev: &str) -> Result<String, Error> {
        let spec = format!("{rev}^{{commit}}");
        let out = self
            .command(&[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &spec,
            ])
            .output()
            .map_err(Error::GitSpawn)?;
        if !out.status.success() {
            return Err(Error::Refused(format!(
                "{rev:?} does not name a commit in {}",
                self.repo.display()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    /// Whether `commit` is in the repository.
    pub fn has_commit(&self, commit: &str) -> Result<bool, Error> {
        let status = self
            .command(&["cat-file", "-e", &format!("{commit}^{{commit}}")])
            .stderr(Stdio::null())
            .status()
            .map_err(Error::GitSpawn)?;
        Ok(status.success())
    }

    /// Whether `ancestor` is `descendant` or one of its ancestors.
    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool, Error> {
        let args = ["merge-base", "--is-ancestor", ancestor, descendant];
        let out = self.command(&args).output().map_err(Error::GitSpawn)?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(failed(&args, &out.stderr)),
        }
    }

    /// Whether the commit object names a parent. A shallow clone's oldest commit does, though git
    /// lists it without one.
    pub fn records_parent(&self, commit: &str) -> Result<bool, Error> {
        let object = self.output(&["cat-file", "commit", commit])?;
        Ok(object
            .split(|&b| b == b'\n')
            .take_while(|line| !line.is_empty())
            .any(|line| line.starts_with(b"parent ")))
    }

    /// The first-parent commits in `range` (a revision or `a..b`), oldest first.
    pub fn first_parent_commits(&self, range: &str) -> Result<Vec<CommitInfo>, Error> {
        let out = self.output(&[
            "rev-list",
            "--first-parent",
            "--reverse",
            "--no-commit-header",
            "--format=%H%x00%P%x00%ct%x00%s",
            "--end-of-options",
            range,
        ])?;
        let mut commits = Vec::new();
        for line in out.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
            let unreadable = || {
                Error::GitOutput(format!(
                    "unreadable rev-list line: {}",
                    String::from_utf8_lossy(line)
                ))
            };
            let mut fields = line.splitn(4, |&b| b == 0);
            let (Some(sha), Some(parents), Some(time), Some(subject)) =
                (fields.next(), fields.next(), fields.next(), fields.next())
            else {
                return Err(unreadable());
            };
            let committer_time = std::str::from_utf8(time)
                .ok()
                .and_then(|t| t.parse().ok())
                .ok_or_else(unreadable)?;
            let first_parent = parents
                .split(|&b| b == b' ')
                .next()
                .filter(|p| !p.is_empty())
                .map(|p| String::from_utf8_lossy(p).into_owned());
            commits.push(CommitInfo {
                sha: String::from_utf8_lossy(sha).into_owned(),
                first_parent,
                committer_time,
                subject: String::from_utf8_lossy(subject).into_owned(),
            });
        }
        Ok(commits)
    }

    /// Starts git with piped standard streams, writing `input` to it from another thread.
    pub fn spawn<S: AsRef<OsStr>>(&self, args: &[S], input: Vec<u8>) -> Result<Running, Error> {
        let mut cmd = self.command(args);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(Error::GitSpawn)?;
        let mut stdin = child.stdin.take().expect("stdin is piped");
        let writer = std::thread::spawn(move || stdin.write_all(&input));
        let mut stderr = child.stderr.take().expect("stderr is piped");
        let stderr = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf);
            buf
        });
        let stdout =
            BufReader::with_capacity(1 << 16, child.stdout.take().expect("stdout is piped"));
        Ok(Running {
            child,
            stdout,
            writer: Some(writer),
            stderr: Some(stderr),
            args: describe(args),
        })
    }
}

/// A git process whose output is being read.
pub(crate) struct Running {
    child: Child,
    pub stdout: BufReader<ChildStdout>,
    writer: Option<JoinHandle<std::io::Result<()>>>,
    stderr: Option<JoinHandle<Vec<u8>>>,
    args: String,
}

impl Running {
    /// Reads output lines (without their newline) to the end, handing each to `each`.
    pub fn for_each_line(
        &mut self,
        mut each: impl FnMut(&[u8]) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let mut line = Vec::with_capacity(256);
        loop {
            line.clear();
            if self.stdout.read_until(b'\n', &mut line)? == 0 {
                return Ok(());
            }
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            each(&line)?;
        }
    }

    /// Waits for git to exit; a non-zero exit is an error carrying stderr.
    pub fn finish(mut self) -> Result<(), Error> {
        // Drain anything unread so that git never blocks on a full pipe.
        std::io::copy(&mut self.stdout, &mut std::io::sink())?;
        let status = self.child.wait()?;
        let stderr = self.stderr.take().expect("finish runs once").join();
        let written = self.writer.take().expect("finish runs once").join();
        if !status.success() {
            return Err(Error::Git {
                args: self.args,
                stderr: String::from_utf8_lossy(&stderr.unwrap_or_default())
                    .trim()
                    .to_owned(),
            });
        }
        match written {
            Ok(result) => Ok(result?),
            Err(_) => Err(Error::GitOutput(format!(
                "writing to git {} panicked",
                self.args
            ))),
        }
    }
}

fn run<S: AsRef<OsStr>>(mut cmd: Command, args: &[S]) -> Result<Vec<u8>, Error> {
    let out = cmd.output().map_err(Error::GitSpawn)?;
    if !out.status.success() {
        return Err(failed(args, &out.stderr));
    }
    Ok(out.stdout)
}

fn describe<S: AsRef<OsStr>>(args: &[S]) -> String {
    args.iter()
        .map(|a| a.as_ref().to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

fn failed<S: AsRef<OsStr>>(args: &[S], stderr: &[u8]) -> Error {
    Error::Git {
        args: describe(args),
        stderr: String::from_utf8_lossy(stderr).trim().to_owned(),
    }
}

fn check_version() -> Result<(), Error> {
    let out = Command::new("git")
        .arg("version")
        .output()
        .map_err(Error::GitSpawn)?;
    let text = String::from_utf8_lossy(&out.stdout);
    // "git version 2.43.0", or "git version 2.39.5 (Apple Git-154)".
    let version = text.split_whitespace().nth(2).unwrap_or_default();
    let mut parts = version.split('.').map(|p| p.parse::<u32>().ok());
    match (parts.next().flatten(), parts.next().flatten()) {
        (Some(major), Some(minor)) if (major, minor) >= MIN_GIT => Ok(()),
        (Some(_), Some(_)) => Err(Error::Refused(format!(
            "churncode needs git {}.{} or newer; found {}",
            MIN_GIT.0,
            MIN_GIT.1,
            text.trim()
        ))),
        _ => Err(Error::GitOutput(format!(
            "unreadable git version: {}",
            text.trim()
        ))),
    }
}

/// Each exclude glob as the negative pathspec git takes, after a `--` when there are any.
pub(crate) fn exclude_pathspecs(excludes: &[String]) -> Vec<OsString> {
    if excludes.is_empty() {
        return Vec::new();
    }
    std::iter::once(OsString::from("--"))
        .chain(
            excludes
                .iter()
                .map(|e| format!(":(exclude,glob){e}").into()),
        )
        .collect()
}
