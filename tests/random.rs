//! Random edit histories: every line's stored origin must be the commit `git blame
//! --first-parent` names.

mod common;

use common::*;

/// SplitMix64: a small, seedable generator, so that a failing seed can be replayed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A number in `0..n`.
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

/// Lines that repeat often (braces, blank lines, indentation), so that the diff has ambiguous
/// alignments to choose between, as real code does.
const COMMON: &[&str] = &["", "}", "    }", "{", "    return x;", "end", "x", "y"];

fn random_line(rng: &mut Rng) -> String {
    if rng.chance(40) {
        (*rng.pick(COMMON)).to_owned()
    } else {
        format!("line {}", rng.below(50))
    }
}

/// Random insertions, deletions and replacements in a file's lines.
fn edit(rng: &mut Rng, lines: &mut Vec<String>) {
    for _ in 0..1 + rng.below(4) {
        let at = rng.below(lines.len() + 1);
        match rng.below(3) {
            0 => {
                for _ in 0..1 + rng.below(5) {
                    lines.insert(at, random_line(rng));
                }
            }
            1 if !lines.is_empty() => {
                let start = at.min(lines.len() - 1);
                let end = (start + 1 + rng.below(4)).min(lines.len());
                lines.drain(start..end);
            }
            _ if at < lines.len() => lines[at] = random_line(rng),
            _ => lines.push(random_line(rng)),
        }
    }
}

fn read_lines(repo: &Repo, path: &str) -> Vec<String> {
    String::from_utf8(repo.read(path))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn write_lines(repo: &Repo, path: &str, lines: &[String], newline_at_end: bool) {
    let mut text = lines.join("\n");
    if newline_at_end && !lines.is_empty() {
        text.push('\n');
    }
    repo.write(path, text);
}

/// Committed files whose name starts with `prefix`; every change is committed before the next.
fn files_with(repo: &Repo, prefix: &str) -> Vec<String> {
    repo.files()
        .into_iter()
        .filter(|f| f.rsplit('/').next().unwrap().starts_with(prefix))
        .collect()
}

/// One random change to the files whose names start with `prefix`. With `moves` off it only adds
/// and edits files. git pairs the deleted and added files of one diff into renames across the
/// whole diff, while `git blame` looks for each file's source on its own, so a diff that deletes
/// and adds several similar files can pair them differently (the README's limitations). A main
/// commit makes one change, so it never deletes and adds more than one file; a side branch's
/// commits reach the first-parent history as one merge diff, so they never delete or rename.
fn random_change(rng: &mut Rng, repo: &Repo, prefix: &str, counter: &mut usize, moves: bool) {
    let files = files_with(repo, prefix);
    let dirs = ["", "src/", "src/deep/", "docs/"];
    *counter += 1;
    match rng.below(10) {
        // Add a file.
        0 | 1 => {
            let path = format!("{}{prefix}{}.txt", rng.pick(&dirs), *counter);
            let lines: Vec<String> = (0..1 + rng.below(30)).map(|_| random_line(rng)).collect();
            write_lines(repo, &path, &lines, rng.chance(90));
        }
        // Delete a file.
        2 if moves && files.len() > 2 => repo.remove(rng.pick(&files)),
        // Rename a file, sometimes with edits.
        3 if moves && !files.is_empty() => {
            let from = rng.pick(&files).clone();
            let to = format!("{}{prefix}{}.txt", rng.pick(&dirs), *counter);
            let mut lines = read_lines(repo, &from);
            repo.remove(&from);
            if rng.chance(50) {
                edit(rng, &mut lines);
            }
            write_lines(repo, &to, &lines, true);
        }
        // Edit one to three files.
        _ if !files.is_empty() => {
            for _ in 0..1 + rng.below(3) {
                let path = rng.pick(&files);
                let mut lines = read_lines(repo, path);
                edit(rng, &mut lines);
                write_lines(repo, path, &lines, rng.chance(95));
            }
        }
        _ => {
            let path = format!("{prefix}{}.txt", *counter);
            write_lines(repo, &path, &[random_line(rng)], true);
        }
    }
}

fn random_history(seed: u64, commits: usize) -> Repo {
    let mut rng = Rng(seed);
    let repo = Repo::new();
    let mut counter = 0;
    let mut time = T0;
    for i in 0..3 {
        let lines: Vec<String> = (0..10 + rng.below(20))
            .map(|_| random_line(&mut rng))
            .collect();
        write_lines(&repo, &format!("m{i}.txt"), &lines, true);
    }
    repo.commit("root", time);
    let mut side = 0;
    for i in 0..commits {
        time += 1 + rng.below(6 * HOUR as usize) as i64;
        if rng.chance(15) {
            // A side branch, merged back with its commits off the first-parent history. The
            // side touches only its own files and main only its own, so the merge is clean.
            side += 1;
            let branch = format!("side{side}");
            repo.git(&["checkout", "-q", "-b", &branch]);
            for j in 0..1 + rng.below(3) {
                random_change(&mut rng, &repo, "s", &mut counter, false);
                repo.commit(&format!("{branch} commit {j}"), time);
                time += 1 + rng.below(HOUR as usize) as i64;
            }
            repo.git(&["checkout", "-q", "main"]);
            if rng.chance(50) {
                random_change(&mut rng, &repo, "m", &mut counter, true);
                repo.commit(&format!("commit {i} beside {branch}"), time);
                time += 1;
            }
            repo.merge(&branch, &format!("merge {branch}"), time);
        } else {
            let prefix = if rng.chance(80) { "m" } else { "s" };
            random_change(&mut rng, &repo, prefix, &mut counter, true);
            repo.commit(&format!("commit {i}"), time);
        }
    }
    repo
}

#[test]
fn every_stored_origin_matches_blame() {
    let mut lines = 0;
    for seed in 1..=12 {
        let repo = random_history(seed, 50);
        let db = Db::new();
        repo.update(&db.path());
        let verification = repo.verify(&db.path(), repo.files());
        assert!(
            verification.agrees(),
            "seed {seed} in {}:\n{}",
            repo.path().display(),
            verification.render()
        );
        lines += verification.files.iter().map(|f| f.lines()).sum::<u64>();
        // The figures do not depend on how the commits were split among workers.
        let one_job = Db::new();
        churncode::update(&churncode::UpdateOptions {
            jobs: 1,
            ..repo.update_options(&one_job.path())
        })
        .unwrap();
        assert_eq!(
            figures(&one_job.path(), WINDOW),
            figures(&db.path(), WINDOW),
            "seed {seed}"
        );
    }
    assert!(lines > 2_000, "only {lines} lines were compared");
}
