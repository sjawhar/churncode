//! Reads the diff of every first-parent commit against its first parent, across worker threads,
//! each feeding a batch of commits to one `git diff-tree --stdin`.

use std::ffi::OsString;
use std::io::{BufRead, Read};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;

use crate::Error;
use crate::git::{self, CommitInfo, Git};
use crate::patch::{CommitPatch, FileDiff, Parser};

/// git treats a blob larger than this as binary; churncode pins git's default so that its own
/// reading of a blob agrees with git's.
pub(crate) const BIG_FILE_THRESHOLD: u64 = 512 * 1024 * 1024;
/// git looks for a NUL byte in this many leading bytes to decide that a blob is binary.
const FIRST_FEW_BYTES: usize = 8000;
/// Batches per worker, so that a worker that drew cheap commits takes another batch.
const BATCHES_PER_WORKER: usize = 4;

/// The diff of each commit against its first parent, in the order of `commits`. Only the first
/// commit may lack a parent, and it is diffed against the empty tree.
pub(crate) fn commit_diffs(
    git: &Git,
    excludes: &[String],
    commits: &[CommitInfo],
    jobs: usize,
) -> Result<Vec<Vec<FileDiff>>, Error> {
    let mut out: Vec<Vec<FileDiff>> = Vec::with_capacity(commits.len());
    let mut rest = commits;
    if let Some(root) = commits.first().filter(|c| c.first_parent.is_none()) {
        out.push(tree_diff(git, excludes, git.empty_tree(), &root.sha)?);
        rest = &commits[1..];
    }
    if let Some(orphan) = rest.iter().find(|c| c.first_parent.is_none()) {
        return Err(Error::GitOutput(format!(
            "commit {} has no parent but is not the oldest commit",
            orphan.sha
        )));
    }
    if rest.is_empty() {
        return Ok(out);
    }
    let jobs = jobs.max(1);
    let batch_size = rest.len().div_ceil(jobs * BATCHES_PER_WORKER);
    let batches: Vec<&[CommitInfo]> = rest.chunks(batch_size).collect();
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let (tx, rx) = mpsc::channel();
    std::thread::scope(|scope| {
        for _ in 0..jobs.min(batches.len()) {
            let tx = tx.clone();
            let (next, failed, batches) = (&next, &failed, &batches);
            scope.spawn(move || {
                while !failed.load(Ordering::Relaxed) {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(batch) = batches.get(i) else { break };
                    let result = batch_diffs(git, excludes, batch);
                    failed.fetch_or(result.is_err(), Ordering::Relaxed);
                    if tx.send((i, result)).is_err() {
                        break;
                    }
                }
            });
        }
    });
    drop(tx);
    let mut results: Vec<Option<Vec<Vec<FileDiff>>>> = batches.iter().map(|_| None).collect();
    for (i, result) in rx {
        results[i] = Some(result?);
    }
    for (batch, result) in batches.iter().zip(results) {
        let diffs = result.ok_or_else(|| {
            Error::GitOutput(format!("no diff was read for commit {}", batch[0].sha))
        })?;
        out.extend(diffs);
    }
    Ok(out)
}

/// The diff from `old` (a tree or commit) to commit `new`, as one commit's files.
pub(crate) fn tree_diff(
    git: &Git,
    excludes: &[String],
    old: &str,
    new: &str,
) -> Result<Vec<FileDiff>, Error> {
    // Two-tree mode prints no commit id, and `--stdin` refuses a line that pairs a commit with the
    // empty tree, so a root commit is diffed on its own.
    let mut args: Vec<OsString> = vec!["diff-tree".into()];
    args.extend(git::PATCH_OPTIONS.iter().map(OsString::from));
    args.push(old.into());
    args.push(new.into());
    args.extend(git::exclude_pathspecs(excludes));
    let mut parser = Parser::default();
    parser.begin_commit(new.to_owned())?;
    let mut running = git.spawn(&args, Vec::new())?;
    running.for_each_line(|line| parser.line(line))?;
    running.finish()?;
    let mut files = parser
        .finish()?
        .pop()
        .map(|(_, files)| files)
        .unwrap_or_default();
    count_text_lines(git, std::slice::from_mut(&mut files))?;
    Ok(files)
}

fn batch_diffs(
    git: &Git,
    excludes: &[String],
    batch: &[CommitInfo],
) -> Result<Vec<Vec<FileDiff>>, Error> {
    // On standard input, "<commit> <parent>" asks for the commit's diff against that parent
    // (the reverse of two-tree mode's argument order).
    let mut input = Vec::with_capacity(batch.len() * 82);
    for commit in batch {
        let parent = commit
            .first_parent
            .as_deref()
            .expect("checked by commit_diffs");
        input.extend_from_slice(commit.sha.as_bytes());
        input.push(b' ');
        input.extend_from_slice(parent.as_bytes());
        input.push(b'\n');
    }
    let mut args: Vec<OsString> = vec!["diff-tree".into(), "--stdin".into()];
    args.extend(git::PATCH_OPTIONS.iter().map(OsString::from));
    args.extend(git::exclude_pathspecs(excludes));
    let mut parser = Parser::default();
    let mut running = git.spawn(&args, input)?;
    running.for_each_line(|line| parser.line(line))?;
    running.finish()?;
    let mut diffs = align(batch, parser.finish()?)?;
    count_text_lines(git, &mut diffs)?;
    Ok(diffs)
}

/// Matches git's output to the commits it was asked for. `diff-tree --stdin` prints nothing at
/// all, not even the commit id, for a commit whose diff is empty (after pathspec exclusion, too),
/// so the output is matched by the printed commit ids, never by position.
fn align(batch: &[CommitInfo], parsed: Vec<CommitPatch>) -> Result<Vec<Vec<FileDiff>>, Error> {
    let mut parsed = parsed.into_iter().peekable();
    let diffs = batch
        .iter()
        .map(
            |commit| match parsed.next_if(|(sha, _)| *sha == commit.sha) {
                Some((_, files)) => files,
                None => Vec::new(),
            },
        )
        .collect();
    match parsed.next() {
        None => Ok(diffs),
        Some((sha, _)) => Err(Error::GitOutput(format!(
            "git diff-tree printed commit {sha}, which was not asked for in that order"
        ))),
    }
}

/// git prints "Binary files differ" when either side of a file is binary. When a binary file
/// that existed before becomes text, its new lines are the commit's own, so their count is read
/// from the new blob with `cat-file`, applying git's own test for binary content.
fn count_text_lines(git: &Git, diffs: &mut [Vec<FileDiff>]) -> Result<(), Error> {
    let mut wanted: Vec<&mut FileDiff> = diffs
        .iter_mut()
        .flatten()
        // A binary file added from nothing is binary on its new side.
        .filter(|f| f.binary && f.old_path.is_some() && f.new_blob.is_some())
        .collect();
    if wanted.is_empty() {
        return Ok(());
    }
    let mut input = Vec::new();
    for file in &wanted {
        input.extend_from_slice(file.new_blob.as_ref().expect("filtered").as_bytes());
        input.push(b'\n');
    }
    let mut running = git.spawn(&["cat-file", "--batch"], input)?;
    let mut header = Vec::new();
    for file in &mut wanted {
        header.clear();
        running.stdout.read_until(b'\n', &mut header)?;
        // "<oid> blob <size>\n<contents>\n"
        let text = String::from_utf8_lossy(&header);
        let size: u64 = match text.trim_end().split(' ').collect::<Vec<_>>()[..] {
            [_, "blob", size] => size.parse().ok(),
            _ => None,
        }
        .ok_or_else(|| Error::GitOutput(format!("unexpected cat-file reply: {}", text.trim())))?;
        file.new_text_lines = text_lines(&mut (&mut running.stdout).take(size), size)?;
        let mut newline = [0u8];
        running.stdout.read_exact(&mut newline)?;
    }
    running.finish()
}

/// The number of lines git sees in a blob read from `blob`, or 0 when git takes it for binary.
/// Reads all `size` bytes either way.
fn text_lines(blob: &mut impl Read, size: u64) -> Result<u32, Error> {
    let mut buf = vec![0u8; 1 << 16];
    let mut seen = 0usize;
    let mut binary = size > BIG_FILE_THRESHOLD;
    let mut newlines = 0u64;
    let mut last = b'\n';
    loop {
        let n = blob.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let chunk = &buf[..n];
        if !binary && seen < FIRST_FEW_BYTES {
            let head = &chunk[..chunk.len().min(FIRST_FEW_BYTES - seen)];
            binary = head.contains(&0);
        }
        seen += n;
        if !binary {
            newlines += chunk.iter().filter(|&&b| b == b'\n').count() as u64;
            last = chunk[n - 1];
        }
    }
    if binary {
        return Ok(0);
    }
    let lines = newlines + u64::from(last != b'\n');
    u32::try_from(lines).map_err(|_| Error::GitOutput(format!("a blob of {lines} lines")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(sha: char) -> CommitInfo {
        CommitInfo {
            sha: sha.to_string().repeat(40),
            first_parent: Some("0".repeat(40)),
            committer_time: 0,
            subject: String::new(),
        }
    }

    #[test]
    fn aligns_by_commit_id() {
        let batch = [commit('a'), commit('b'), commit('c')];
        let file = FileDiff {
            new_path: Some(b"f".to_vec()),
            ..FileDiff::default()
        };
        let parsed = vec![("c".repeat(40), vec![file.clone()])];
        let diffs = align(&batch, parsed).unwrap();
        assert_eq!(diffs, vec![vec![], vec![], vec![file]]);
        let out_of_order = vec![("c".repeat(40), vec![]), ("a".repeat(40), vec![])];
        assert!(align(&batch, out_of_order).is_err());
    }

    #[test]
    fn counts_lines_like_git() {
        let lines = |bytes: &[u8]| text_lines(&mut &bytes[..], bytes.len() as u64).unwrap();
        assert_eq!(lines(b""), 0);
        assert_eq!(lines(b"a\nb\n"), 2);
        assert_eq!(lines(b"a\nb"), 2);
        assert_eq!(lines(b"a\0b\n"), 0);
        let mut late_nul = vec![b'x'; FIRST_FEW_BYTES];
        late_nul.extend_from_slice(b"\0\n");
        assert_eq!(lines(&late_nul), 1);
    }
}
