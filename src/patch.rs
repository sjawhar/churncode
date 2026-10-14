//! Reads the parts of `git diff-tree -p -U0` output that line tracking needs: each file's paths,
//! whether it is binary or a submodule, and its hunk headers. Content lines are skipped.

use crate::Error;

/// One `@@ -a,b +c,d @@` header of a zero-context patch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Hunk {
    /// The first line the hunk removes (1-based), or, when it removes none, the line after which
    /// it inserts (0 inserts at the top).
    pub old_start: u32,
    pub old_count: u32,
    /// The same position on the new side.
    pub new_start: u32,
    pub new_count: u32,
}

/// A full object id, kept inline so that reading one allocates nothing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Oid {
    len: u8,
    hex: [u8; 64],
}

impl Oid {
    pub fn parse(s: &[u8]) -> Option<Oid> {
        if !is_object_id(s) {
            return None;
        }
        let mut hex = [0; 64];
        hex[..s.len()].copy_from_slice(s);
        Some(Oid {
            len: s.len() as u8,
            hex,
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.hex[..usize::from(self.len)]
    }
}

impl std::fmt::Debug for Oid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&String::from_utf8_lossy(self.as_bytes()))
    }
}

/// One file of one commit's diff.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct FileDiff {
    /// The path before the commit; `None` when the commit adds the file.
    pub old_path: Option<Vec<u8>>,
    /// The path after the commit; `None` when the commit deletes the file.
    pub new_path: Option<Vec<u8>>,
    /// git printed "Binary files ... differ" instead of hunks.
    pub binary: bool,
    /// The entry is a submodule commit, not a file.
    pub gitlink: bool,
    pub hunks: Vec<Hunk>,
    /// For a binary diff whose file still exists: the new blob, from the `index` line.
    pub new_blob: Option<Oid>,
    /// For a binary diff: the new side's line count when git reads that side as text on its own
    /// (a file that stops being binary), and 0 otherwise. Filled in after parsing.
    pub new_text_lines: u32,
}

/// One commit's files, under the commit id git printed for them.
pub(crate) type CommitPatch = (String, Vec<FileDiff>);

/// Accumulates commits from patch output fed to it line by line.
#[derive(Default)]
pub(crate) struct Parser {
    commits: Vec<CommitPatch>,
    file: Option<PendingFile>,
}

#[derive(Default)]
struct PendingFile {
    header_paths: Option<(Vec<u8>, Vec<u8>)>,
    rename_from: Option<Vec<u8>>,
    rename_to: Option<Vec<u8>>,
    added: bool,
    deleted: bool,
    binary: bool,
    gitlink: bool,
    hunks: Vec<Hunk>,
    new_blob: Option<Oid>,
}

const GITLINK_MODE: &[u8] = b"160000";

impl Parser {
    /// Starts a commit whose id git does not print (a two-tree `diff-tree`).
    pub fn begin_commit(&mut self, id: String) -> Result<(), Error> {
        self.finish_file()?;
        self.commits.push((id, Vec::new()));
        Ok(())
    }

    /// Feeds one line of output, without its newline.
    pub fn line(&mut self, line: &[u8]) -> Result<(), Error> {
        // Content lines carry a one-character prefix ("+", "-", " ", or "\" before "No newline at
        // end of file"), so none can look like a commit id or a header. The "---" and "+++" file
        // headers start the same way and are not needed either.
        if matches!(line.first(), Some(b'+' | b'-' | b' ' | b'\\')) {
            return Ok(());
        }
        // `diff-tree --stdin` prints each commit's id on a line of its own before its files.
        if is_object_id(line) {
            return self.begin_commit(String::from_utf8_lossy(line).into_owned());
        }
        if let Some(rest) = line.strip_prefix(b"diff --git ") {
            self.finish_file()?;
            if self.commits.is_empty() {
                return Err(unreadable("a file diff before any commit id", line));
            }
            self.file = Some(PendingFile {
                header_paths: parse_diff_git(rest)?,
                ..PendingFile::default()
            });
            return Ok(());
        }
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        if line.starts_with(b"@@ ") {
            file.hunks.push(parse_hunk_header(line)?);
        } else if let Some(rest) = line.strip_prefix(b"rename from ") {
            file.rename_from = Some(path_value(rest)?);
        } else if let Some(rest) = line.strip_prefix(b"rename to ") {
            file.rename_to = Some(path_value(rest)?);
        } else if let Some(mode) = line.strip_prefix(b"new file mode ") {
            file.added = true;
            file.gitlink |= mode == GITLINK_MODE;
        } else if let Some(mode) = line.strip_prefix(b"deleted file mode ") {
            file.deleted = true;
            file.gitlink |= mode == GITLINK_MODE;
        } else if let Some(rest) = line.strip_prefix(b"index ") {
            // "index <old>..<new>", then " <mode>" when the mode did not change.
            let (blobs, mode) = match rest.iter().position(|&b| b == b' ') {
                Some(space) => (&rest[..space], &rest[space + 1..]),
                None => (rest, &b""[..]),
            };
            file.gitlink |= mode == GITLINK_MODE;
            let new = blobs
                .windows(2)
                .position(|w| w == b"..")
                .and_then(|dots| Oid::parse(&blobs[dots + 2..]))
                .ok_or_else(|| unreadable("an index line (run without --full-index?)", line))?;
            file.new_blob = Some(new);
        } else if line.starts_with(b"Binary files ") && line.ends_with(b" differ") {
            file.binary = true;
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<Vec<CommitPatch>, Error> {
        self.finish_file()?;
        Ok(self.commits)
    }

    fn finish_file(&mut self) -> Result<(), Error> {
        let Some(file) = self.file.take() else {
            return Ok(());
        };
        let (commit, files) = self
            .commits
            .last_mut()
            .expect("a file diff is only opened inside a commit");
        let (mut old, mut new) = match file.header_paths {
            Some((old, new)) => (Some(old), Some(new)),
            None => (None, None),
        };
        if file.rename_from.is_some() {
            old = file.rename_from;
        }
        if file.rename_to.is_some() {
            new = file.rename_to;
        }
        let (Some(old), Some(new)) = (old, new) else {
            return Err(Error::GitOutput(format!(
                "commit {commit} has a file diff whose paths could not be read"
            )));
        };
        files.push(FileDiff {
            old_path: (!file.added).then_some(old),
            binary: file.binary,
            gitlink: file.gitlink,
            hunks: file.hunks,
            new_blob: if file.binary && !file.deleted {
                file.new_blob
            } else {
                None
            },
            new_path: (!file.deleted).then_some(new),
            new_text_lines: 0,
        });
        Ok(())
    }
}

/// A full SHA-1 or SHA-256 object id, as git prints it.
pub(crate) fn is_object_id(s: &[u8]) -> bool {
    (s.len() == 40 || s.len() == 64) && s.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn unreadable(what: &str, line: &[u8]) -> Error {
    Error::GitOutput(format!(
        "could not read {what}: {}",
        String::from_utf8_lossy(line)
    ))
}

fn parse_hunk_header(line: &[u8]) -> Result<Hunk, Error> {
    let bad = || unreadable("a hunk header", line);
    let rest = line.strip_prefix(b"@@ -").ok_or_else(bad)?;
    let end = rest.windows(3).position(|w| w == b" @@").ok_or_else(bad)?;
    let ranges = std::str::from_utf8(&rest[..end]).map_err(|_| bad())?;
    let (old, new) = ranges.split_once(" +").ok_or_else(bad)?;
    let (old_start, old_count) = parse_range(old).ok_or_else(bad)?;
    let (new_start, new_count) = parse_range(new).ok_or_else(bad)?;
    Ok(Hunk {
        old_start,
        old_count,
        new_start,
        new_count,
    })
}

/// `start[,count]`, where a missing count means one line.
fn parse_range(s: &str) -> Option<(u32, u32)> {
    match s.split_once(',') {
        Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

/// A file's path before and after a commit.
type PathPair = (Vec<u8>, Vec<u8>);

/// The two paths of a `diff --git a/<old> b/<new>` header, without their prefixes.
///
/// An unquoted header is ambiguous when a path contains " b/". It names one path twice unless the
/// file was renamed, so a header whose halves differ is left unread: a rename carries both paths
/// again in its `rename from` and `rename to` lines.
fn parse_diff_git(rest: &[u8]) -> Result<Option<PathPair>, Error> {
    let bad = || unreadable("a diff header", rest);
    let (old, new) = if rest.first() == Some(&b'"') {
        let (old, after) = take_quoted(rest).ok_or_else(bad)?;
        let after = after.strip_prefix(b" ").ok_or_else(bad)?;
        (old, path_value(after)?)
    } else if let Some(quote) = rest.iter().position(|&b| b == b'"') {
        // git quotes any path containing a quote, so in an unquoted old path the first quote
        // opens the quoted new path.
        let old = rest[..quote].strip_suffix(b" ").ok_or_else(bad)?;
        (old.to_vec(), path_value(&rest[quote..])?)
    } else {
        // "a/" + path + " b/" + path
        let n = rest.len();
        if n < 5 || !(n - 5).is_multiple_of(2) {
            return Ok(None);
        }
        let half = (n - 5) / 2;
        let (old, new) = (&rest[..2 + half], &rest[3 + half..]);
        if rest[2 + half] != b' ' || old[2..] != new[2..] {
            return Ok(None);
        }
        (old.to_vec(), new.to_vec())
    };
    let old = old.strip_prefix(b"a/").ok_or_else(bad)?.to_vec();
    let new = new.strip_prefix(b"b/").ok_or_else(bad)?.to_vec();
    Ok(Some((old, new)))
}

/// A path as git prints it in a header: C-quoted when it holds a quote, a backslash, a control
/// character or (with `core.quotePath`) any non-ASCII byte, and raw otherwise.
fn path_value(s: &[u8]) -> Result<Vec<u8>, Error> {
    if s.first() != Some(&b'"') {
        return Ok(s.to_vec());
    }
    match take_quoted(s) {
        Some((value, [])) => Ok(value),
        _ => Err(unreadable("a quoted path", s)),
    }
}

/// Reads the C-quoted string that `s` starts with; returns its bytes and what follows it.
fn take_quoted(s: &[u8]) -> Option<(Vec<u8>, &[u8])> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 1;
    while let Some(&b) = s.get(i) {
        match b {
            b'"' => return Some((out, &s[i + 1..])),
            b'\\' => {
                let escape = *s.get(i + 1)?;
                if (b'0'..=b'3').contains(&escape) {
                    let digits = s.get(i + 1..i + 4)?;
                    if !digits.iter().all(|d| (b'0'..=b'7').contains(d)) {
                        return None;
                    }
                    out.push((digits[0] - b'0') * 64 + (digits[1] - b'0') * 8 + (digits[2] - b'0'));
                    i += 4;
                } else {
                    out.push(match escape {
                        b'a' => 0x07,
                        b'b' => 0x08,
                        b't' => b'\t',
                        b'n' => b'\n',
                        b'v' => 0x0b,
                        b'f' => 0x0c,
                        b'r' => b'\r',
                        b'"' | b'\\' => escape,
                        _ => return None,
                    });
                    i += 2;
                }
            }
            _ => {
                out.push(b);
                i += 1;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Vec<CommitPatch> {
        let mut parser = Parser::default();
        for line in text.lines() {
            parser.line(line.as_bytes()).unwrap();
        }
        parser.finish().unwrap()
    }

    const C1: &str = "1111111111111111111111111111111111111111";
    const C2: &str = "2222222222222222222222222222222222222222";
    const INDEX: &str = "index 1111111111111111111111111111111111111111..\
                         2222222222222222222222222222222222222222";

    fn hunk(old_start: u32, old_count: u32, new_start: u32, new_count: u32) -> Hunk {
        Hunk {
            old_start,
            old_count,
            new_start,
            new_count,
        }
    }

    #[test]
    fn reads_commits_files_and_hunks() {
        let out = format!(
            "{C1}\n\
             diff --git a/f b/f\n\
             {INDEX} 100644\n\
             --- a/f\n\
             +++ b/f\n\
             @@ -2 +2 @@ a\n\
             -b\n\
             +B\n\
             @@ -3,0 +4,2 @@ c\n\
             +d\n\
             +e\n\
             {C2}\n\
             diff --git a/new b/new\n\
             new file mode 100644\n\
             @@ -0,0 +1 @@\n\
             +diff --git a/not-a-header b/not-a-header\n\
             diff --git a/gone b/gone\n\
             deleted file mode 100644\n\
             @@ -1,2 +0,0 @@\n\
             -{C1}\n\
             -@@ -9 +9 @@\n"
        );
        let commits = parse(&out);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].0, C1);
        assert_eq!(
            commits[0].1,
            vec![FileDiff {
                old_path: Some(b"f".to_vec()),
                new_path: Some(b"f".to_vec()),
                hunks: vec![hunk(2, 1, 2, 1), hunk(3, 0, 4, 2)],
                ..FileDiff::default()
            }]
        );
        assert_eq!(commits[1].0, C2);
        assert_eq!(commits[1].1.len(), 2);
        assert_eq!(commits[1].1[0].old_path, None);
        assert_eq!(commits[1].1[0].new_path.as_deref(), Some(&b"new"[..]));
        assert_eq!(commits[1].1[0].hunks, vec![hunk(0, 0, 1, 1)]);
        assert_eq!(commits[1].1[1].new_path, None);
        assert_eq!(commits[1].1[1].hunks, vec![hunk(1, 2, 0, 0)]);
    }

    #[test]
    fn reads_renames_binaries_and_gitlinks() {
        let out = format!(
            "{C1}\n\
             diff --git a/old name b/x b/new name\n\
             similarity index 90%\n\
             rename from old name\n\
             rename to new name\n\
             @@ -5 +5 @@\n\
             diff --git a/img.png b/img.png\n\
             {INDEX} 100644\n\
             Binary files a/img.png and b/img.png differ\n\
             diff --git a/sub b/sub\n\
             new file mode 160000\n\
             @@ -0,0 +1 @@\n\
             +Subproject commit {C2}\n\
             diff --git a/mod b/mod\n\
             {INDEX} 160000\n\
             @@ -1 +1 @@\n"
        );
        let files = &parse(&out)[0].1;
        assert_eq!(files[0].old_path.as_deref(), Some(&b"old name"[..]));
        assert_eq!(files[0].new_path.as_deref(), Some(&b"new name"[..]));
        assert_eq!(files[0].hunks, vec![hunk(5, 1, 5, 1)]);
        assert!(files[1].binary && !files[1].gitlink);
        assert_eq!(files[1].new_blob, Oid::parse(C2.as_bytes()));
        assert!(files[2].gitlink && files[2].old_path.is_none());
        assert!(files[3].gitlink && !files[3].binary);
        assert_eq!(files[3].new_blob, None);
    }

    #[test]
    fn refuses_abbreviated_index_lines() {
        let mut parser = Parser::default();
        parser.line(C1.as_bytes()).unwrap();
        parser.line(b"diff --git a/f b/f").unwrap();
        assert!(parser.line(b"index 1234567..89abcde 100644").is_err());
    }

    #[test]
    fn reads_quoted_and_ambiguous_paths() {
        let cases: &[(&str, &[u8], &[u8])] = &[
            ("a/x b/x", b"x", b"x"),
            (
                "a/has b/inside b/has b/inside",
                b"has b/inside",
                b"has b/inside",
            ),
            (
                r#""a/\303\274n\303\257.txt" "b/\303\274n\303\257.txt""#,
                "ünï.txt".as_bytes(),
                "ünï.txt".as_bytes(),
            ),
            (r#""a/tab\there" b/plain"#, b"tab\there", b"plain"),
            (r#"a/plain "b/q\"uote""#, b"plain", b"q\"uote"),
            (r#""a/\377" "b/\001""#, b"\xff", b"\x01"),
        ];
        for (header, old, new) in cases {
            let parsed = parse_diff_git(header.as_bytes()).unwrap();
            assert_eq!(parsed, Some((old.to_vec(), new.to_vec())), "{header}");
        }
        assert_eq!(parse_diff_git(b"a/one b/two").unwrap(), None);
    }

    #[test]
    fn a_rename_needs_its_rename_lines_when_the_header_is_ambiguous() {
        let mut parser = Parser::default();
        parser.line(C1.as_bytes()).unwrap();
        parser.line(b"diff --git a/one b/two").unwrap();
        assert!(parser.finish().is_err());
    }

    #[test]
    fn rejects_malformed_hunk_headers() {
        for header in ["@@ -x +1 @@", "@@ -1 +1", "@@ -1,2,3 +1 @@"] {
            assert!(parse_hunk_header(header.as_bytes()).is_err(), "{header}");
        }
        assert_eq!(
            parse_hunk_header(b"@@ -7,3 +9 @@ fn main() {").unwrap(),
            hunk(7, 3, 9, 1)
        );
    }
}
