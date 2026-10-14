//! Forward line tracking: every line of every file carries the commit that added it, from one
//! first-parent commit to the next.

use std::collections::{HashMap, HashSet};

use crate::Error;
use crate::patch::FileDiff;

/// A commit's position in first-parent order, 1 for the oldest commit processed. Lines that were
/// already in the tree at `--base` have origin 0.
pub(crate) type Origin = u32;

/// What one commit did to the lines it touched.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Applied {
    pub lines_added: u64,
    /// How many lines of each origin the commit changed or deleted.
    pub deleted: HashMap<Origin, u64>,
}

/// Each tracked file's lines, as the origin of each line in order.
#[derive(Debug, Default)]
pub(crate) struct State {
    pub files: HashMap<Vec<u8>, Vec<Origin>>,
    /// Paths changed since the state was loaded, which the database must write back.
    pub touched: HashSet<Vec<u8>>,
}

impl State {
    /// Applies one commit's diff against its first parent; the commit's own lines get `origin`.
    pub fn apply(
        &mut self,
        commit: &str,
        origin: Origin,
        files: &[FileDiff],
    ) -> Result<Applied, Error> {
        let mut applied = Applied::default();
        // Every old side comes out before any new side goes in: one commit can rename a to b and
        // b to c, so a path can be one file's old side and another file's new side.
        let olds: Vec<Vec<Origin>> = files
            .iter()
            .map(|file| match &file.old_path {
                Some(path) if !file.gitlink => {
                    self.touched.insert(path.clone());
                    self.files.remove(path).unwrap_or_default()
                }
                _ => Vec::new(),
            })
            .collect();
        let mut news = Vec::with_capacity(files.len());
        for (file, old) in files.iter().zip(olds) {
            if file.gitlink {
                continue;
            }
            let new = if file.binary {
                // git shows no hunks when either side is binary. The old side's lines (if it was
                // text) are all gone; the new side's (if it is text) are all this commit's.
                count(&old, &mut applied.deleted);
                applied.lines_added += u64::from(file.new_text_lines);
                vec![origin; file.new_text_lines as usize]
            } else {
                apply_hunks(commit, file, &old, origin, &mut applied)?
            };
            match &file.new_path {
                Some(path) => news.push((path, new)),
                None if new.is_empty() => {}
                None => {
                    return Err(inconsistent(
                        commit,
                        file,
                        format!("{} lines remain after its deletion", new.len()),
                    ));
                }
            }
        }
        for (path, lines) in news {
            self.touched.insert(path.clone());
            let previous = self.files.insert(path.clone(), lines);
            if previous.is_some_and(|lines| !lines.is_empty()) {
                return Err(Error::Inconsistent(format!(
                    "commit {commit} creates {}, which churncode already tracks",
                    display_path(path)
                )));
            }
        }
        Ok(applied)
    }
}

fn count(lines: &[Origin], deleted: &mut HashMap<Origin, u64>) {
    for &line in lines {
        *deleted.entry(line).or_default() += 1;
    }
}

/// Builds a file's new lines from its old lines and its zero-context hunks.
fn apply_hunks(
    commit: &str,
    file: &FileDiff,
    old: &[Origin],
    origin: Origin,
    applied: &mut Applied,
) -> Result<Vec<Origin>, Error> {
    let added: u64 = file.hunks.iter().map(|h| u64::from(h.new_count)).sum();
    let removed: usize = file.hunks.iter().map(|h| h.old_count as usize).sum();
    let mut new = Vec::with_capacity((old.len() + added as usize).saturating_sub(removed));
    let mut cursor = 0;
    for hunk in &file.hunks {
        // A hunk that removes nothing inserts after line `old_start`; one that removes lines
        // starts at line `old_start`.
        let start = if hunk.old_count == 0 {
            hunk.old_start as usize
        } else {
            hunk.old_start as usize - 1
        };
        let end = start + hunk.old_count as usize;
        if start < cursor || end > old.len() {
            return Err(inconsistent(
                commit,
                file,
                format!(
                    "a hunk removes lines {}..{} of the {} churncode tracks",
                    start + 1,
                    end,
                    old.len()
                ),
            ));
        }
        new.extend_from_slice(&old[cursor..start]);
        count(&old[start..end], &mut applied.deleted);
        let new_start = if hunk.new_count == 0 {
            hunk.new_start as usize
        } else {
            hunk.new_start as usize - 1
        };
        if new.len() != new_start {
            return Err(Error::GitOutput(format!(
                "commit {commit}: the hunks of {} do not line up",
                display_path(file.new_path.as_ref().or(file.old_path.as_ref()).unwrap())
            )));
        }
        new.resize(new.len() + hunk.new_count as usize, origin);
        cursor = end;
    }
    new.extend_from_slice(&old[cursor..]);
    applied.lines_added += added;
    Ok(new)
}

fn inconsistent(commit: &str, file: &FileDiff, what: String) -> Error {
    let path = file.old_path.as_ref().or(file.new_path.as_ref()).unwrap();
    Error::Inconsistent(format!(
        "commit {commit} changes {}, but {what}",
        display_path(path)
    ))
}

pub(crate) fn display_path(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::Hunk;

    fn hunk(old_start: u32, old_count: u32, new_start: u32, new_count: u32) -> Hunk {
        Hunk {
            old_start,
            old_count,
            new_start,
            new_count,
        }
    }

    fn modify(path: &str, hunks: Vec<Hunk>) -> FileDiff {
        FileDiff {
            old_path: Some(path.into()),
            new_path: Some(path.into()),
            hunks,
            ..FileDiff::default()
        }
    }

    fn add(path: &str, lines: u32) -> FileDiff {
        FileDiff {
            new_path: Some(path.into()),
            hunks: vec![hunk(0, 0, 1, lines)],
            ..FileDiff::default()
        }
    }

    fn lines(state: &State, path: &str) -> Vec<Origin> {
        state.files[path.as_bytes()].clone()
    }

    #[test]
    fn modifies_inserts_and_deletes() {
        let mut state = State::default();
        state.apply("c1", 1, &[add("f", 5)]).unwrap();
        // Replace line 2, insert two after line 3, delete line 5.
        let applied = state
            .apply(
                "c2",
                2,
                &[modify(
                    "f",
                    vec![hunk(2, 1, 2, 1), hunk(3, 0, 4, 2), hunk(5, 1, 6, 0)],
                )],
            )
            .unwrap();
        assert_eq!(lines(&state, "f"), vec![1, 2, 1, 2, 2, 1]);
        assert_eq!(applied.lines_added, 3);
        assert_eq!(applied.deleted, HashMap::from([(1, 2)]));
    }

    #[test]
    fn renames_chain_within_one_commit() {
        let mut state = State::default();
        state.apply("c1", 1, &[add("a", 2), add("b", 3)]).unwrap();
        let rename = |from: &str, to: &str| FileDiff {
            old_path: Some(from.into()),
            new_path: Some(to.into()),
            ..FileDiff::default()
        };
        // b -> c listed after a -> b, as git sorts them.
        state
            .apply("c2", 2, &[rename("a", "b"), rename("b", "c")])
            .unwrap();
        assert_eq!(lines(&state, "b"), vec![1, 1]);
        assert_eq!(lines(&state, "c"), vec![1, 1, 1]);
        assert!(!state.files.contains_key(&b"a"[..]));
    }

    #[test]
    fn binary_sides_count_whole_files() {
        let mut state = State::default();
        state.apply("c1", 1, &[add("f", 4)]).unwrap();
        let to_binary = FileDiff {
            binary: true,
            ..modify("f", vec![])
        };
        let applied = state.apply("c2", 2, &[to_binary]).unwrap();
        assert_eq!(applied.deleted, HashMap::from([(1, 4)]));
        assert_eq!(applied.lines_added, 0);
        let to_text = FileDiff {
            binary: true,
            new_text_lines: 3,
            ..modify("f", vec![])
        };
        let applied = state.apply("c3", 3, &[to_text]).unwrap();
        assert_eq!(applied.lines_added, 3);
        assert_eq!(lines(&state, "f"), vec![3, 3, 3]);
    }

    #[test]
    fn gitlinks_are_ignored() {
        let mut state = State::default();
        let gitlink = FileDiff {
            gitlink: true,
            ..add("sub", 1)
        };
        let applied = state.apply("c1", 1, &[gitlink]).unwrap();
        assert_eq!(applied, Applied::default());
        assert!(state.files.is_empty());
    }

    #[test]
    fn refuses_hunks_outside_the_tracked_lines() {
        let mut state = State::default();
        state.apply("c1", 1, &[add("f", 2)]).unwrap();
        let err = state
            .apply("c2", 2, &[modify("f", vec![hunk(3, 1, 3, 1)])])
            .unwrap_err();
        assert!(matches!(err, Error::Inconsistent(_)), "{err}");
        let mut state = State::default();
        state.apply("c1", 1, &[add("f", 2)]).unwrap();
        assert!(state.apply("c2", 2, &[add("f", 1)]).is_err());
    }
}
