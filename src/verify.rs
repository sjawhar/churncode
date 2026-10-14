//! `churncode verify`: checks the stored line origins against `git blame --first-parent`.

use std::collections::HashMap;
use std::fmt::Write;
use std::path::PathBuf;

use crate::git::{self, Git};
use crate::patch::is_object_id;
use crate::store;
use crate::track::Origin;
use crate::{Error, track};

/// Which files to check, in which repository and database.
#[derive(Debug, Clone)]
pub struct VerifyOptions {
    pub repo: PathBuf,
    pub db: PathBuf,
    /// Paths from the repository's root.
    pub files: Vec<String>,
    /// The commit to blame at. The database holds origins at its newest commit only, so this
    /// must resolve to that commit; `None` blames at it.
    pub head: Option<String>,
}

/// One file's comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileAgreement {
    pub path: String,
    /// Lines the database tracks in the file.
    pub stored_lines: u64,
    /// Lines `git blame` reports.
    pub blamed_lines: u64,
    /// Lines, position by position, whose stored origin is the commit blame names.
    pub agreeing: u64,
}

impl FileAgreement {
    pub fn agrees(&self) -> bool {
        self.stored_lines == self.blamed_lines && self.agreeing == self.blamed_lines
    }

    /// The larger of the two line counts, which every agreement figure is out of.
    pub fn lines(&self) -> u64 {
        self.stored_lines.max(self.blamed_lines)
    }
}

/// Every checked file's comparison, and the commit blame ran at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verification {
    pub head: String,
    pub files: Vec<FileAgreement>,
}

impl Verification {
    pub fn agrees(&self) -> bool {
        self.files.iter().all(FileAgreement::agrees)
    }

    /// One line per file, then the overall agreement.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let width = self
            .files
            .iter()
            .map(|f| f.path.len())
            .chain([4])
            .max()
            .unwrap_or(4);
        let _ = writeln!(
            out,
            "{:<width$}  {:>8}  {:>8}  {:>8}  AGREEMENT",
            "FILE", "STORED", "BLAMED", "AGREE"
        );
        for f in &self.files {
            let _ = writeln!(
                out,
                "{:<width$}  {:>8}  {:>8}  {:>8}  {}",
                f.path,
                f.stored_lines,
                f.blamed_lines,
                f.agreeing,
                agreement(f.agreeing, f.lines())
            );
        }
        let lines: u64 = self.files.iter().map(FileAgreement::lines).sum();
        let agreeing: u64 = self.files.iter().map(|f| f.agreeing).sum();
        let disagreeing = self.files.iter().filter(|f| !f.agrees()).count();
        let _ = writeln!(
            out,
            "\n{} files, {lines} lines at {}: {agreeing} agree ({}); {disagreeing} files disagree",
            self.files.len(),
            self.head,
            agreement(agreeing, lines)
        );
        out
    }
}

fn agreement(part: u64, whole: u64) -> String {
    if whole == 0 {
        "100.00%".into()
    } else {
        format!("{:.2}%", 100.0 * part as f64 / whole as f64)
    }
}

/// Compares the database's origins of each file with `git blame --first-parent` at the
/// database's newest commit.
pub fn verify(options: &VerifyOptions) -> Result<Verification, Error> {
    let git = Git::open(&options.repo)?;
    let conn = store::open_read_only(&options.db)?;
    let progress = store::progress(&conn)?.ok_or_else(|| {
        Error::Refused(format!(
            "{} holds no history yet; run churncode update first",
            options.db.display()
        ))
    })?;
    if let Some(head) = &options.head {
        let resolved = git.resolve_commit(head)?;
        if resolved != progress.tip {
            return Err(Error::Refused(format!(
                "{head} is {resolved}, but {} holds line origins at its newest commit {} only; \
                 run churncode update --rev {head} first, or verify at {}",
                options.db.display(),
                progress.tip,
                progress.tip
            )));
        }
    }
    let mut origins: HashMap<String, Origin> = store::commit_ids(&conn)?;
    // Blaming the range after the base attributes every older line to the base itself.
    let range = match &progress.settings.base {
        Some(base) => {
            origins.insert(base.clone(), 0);
            format!("{base}..{}", progress.tip)
        }
        None => progress.tip.clone(),
    };
    let mut files = Vec::with_capacity(options.files.len());
    for path in &options.files {
        let stored = store::file_origins(&conn, path.as_bytes())?.unwrap_or_default();
        let mut args: Vec<&str> = vec!["blame"];
        args.extend(git::BLAME_OPTIONS);
        args.extend([range.as_str(), "--", path]);
        let blame = git.output(&args)?;
        let blamed: Vec<Option<Origin>> = blame
            .split(|&b| b == b'\n')
            .filter_map(|line| {
                // Each line of the file has a header "<commit> <old line> <new line>[ <count>]";
                // the file's own text follows a tab, and other headers start with a word.
                let commit = line.split(|&b| b == b' ').next()?;
                (line.len() > commit.len() && is_object_id(commit))
                    .then(|| origins.get(&*String::from_utf8_lossy(commit)).copied())
            })
            .collect();
        let agreeing = stored
            .iter()
            .zip(&blamed)
            .filter(|(stored, blamed)| Some(**stored) == **blamed)
            .count();
        files.push(FileAgreement {
            path: track::display_path(path.as_bytes()),
            stored_lines: stored.len() as u64,
            blamed_lines: blamed.len() as u64,
            agreeing: agreeing as u64,
        });
    }
    Ok(Verification {
        head: progress.tip,
        files,
    })
}
