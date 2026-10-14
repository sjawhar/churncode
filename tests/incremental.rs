//! Extending a database, and the updates a database refuses.

mod common;

use churncode::{Format, ReportOptions, UpdateOptions};
use common::*;

/// A history of `n` commits that edit, add, rename and delete files.
fn history(repo: &Repo, from: usize, n: usize) {
    for i in from..from + n {
        let time = T0 + i as i64 * 7 * HOUR;
        repo.write(
            &format!("f{}.txt", i % 4),
            lines(&format!("c{i}-"), 3 + i % 5),
        );
        if i % 3 == 0 {
            repo.write(
                &format!("dir/new{i}.txt"),
                lines(&format!("n{i}-"), i % 7 + 1),
            );
        }
        if i % 5 == 4 {
            let gone = format!("dir/new{}.txt", i - 4 - (i - 4) % 3);
            if repo.path().join(&gone).exists() {
                repo.rename(&gone, &format!("moved/{i}.txt"));
            }
        }
        repo.commit(&format!("commit {i}"), time);
    }
}

fn render(db: &std::path::Path) -> String {
    churncode::report(&ReportOptions {
        db: db.to_path_buf(),
        since: None,
        until: None,
        window: WINDOW,
    })
    .unwrap()
    .render(Format::Json, true)
}

#[test]
fn an_incremental_update_equals_a_fresh_build() {
    let repo = Repo::new();
    history(&repo, 0, 12);
    let db = Db::new();
    let incremental = db.named("incremental.db");
    let first = repo.update(&incremental);
    assert_eq!(first.processed, 12);

    history(&repo, 12, 9);
    let second = repo.update(&incremental);
    assert_eq!(second.processed, 9);
    assert_eq!(second.total_commits, 21);
    // Nothing new: nothing to do.
    assert_eq!(repo.update(&incremental).processed, 0);

    // A side branch merged in, then more commits, in one more update.
    repo.git(&["checkout", "-q", "-b", "side"]);
    repo.write("f1.txt", lines("side", 4));
    repo.commit("side", T0 + 200 * HOUR);
    repo.git(&["checkout", "-q", "main"]);
    repo.merge("side", "merge side", T0 + 201 * HOUR);
    history(&repo, 30, 3);
    assert_eq!(repo.update(&incremental).processed, 4);

    let fresh = db.named("fresh.db");
    repo.update(&fresh);
    assert_eq!(render(&incremental), render(&fresh));
    let verification = repo.verify(&incremental, repo.files());
    assert!(verification.agrees(), "{}", verification.render());
}

#[test]
fn a_rewritten_history_is_refused() {
    let repo = Repo::new();
    history(&repo, 0, 5);
    let db = Db::new();
    repo.update(&db.path());
    let old_tip = repo.head();

    repo.git(&["reset", "-q", "--hard", "HEAD~2"]);
    repo.write("other.txt", "rewritten\n");
    let new_tip = repo.commit("rewritten", T0 + 100 * HOUR);
    let err = churncode::update(&repo.update_options(&db.path()))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains(&old_tip) && err.contains(&new_tip) && err.contains("rewritten"),
        "{err}"
    );
    // The refused update changed nothing.
    let commits = commits(&db.path(), WINDOW);
    assert_eq!(commits.len(), 5);
    assert_eq!(commits.last().unwrap().sha, old_tip);
}

#[test]
fn a_tip_that_left_the_first_parent_history_is_refused() {
    let repo = Repo::new();
    history(&repo, 0, 3);
    let db = Db::new();
    repo.update(&db.path());
    let old_tip = repo.head();
    // main moves to a new line of history that merges the old one as a side branch.
    repo.git(&["checkout", "-q", "-b", "other", "HEAD~2"]);
    repo.write("x.txt", "x\n");
    repo.commit("other", T0 + 50 * HOUR);
    repo.merge("main", "merge old main", T0 + 51 * HOUR);
    repo.git(&["branch", "-q", "-f", "main", "other"]);
    let err = churncode::update(&repo.update_options(&db.path()))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains(&old_tip) && err.contains("first-parent"),
        "{err}"
    );
}

#[test]
fn other_excludes_or_base_need_a_new_database() {
    let repo = Repo::new();
    history(&repo, 0, 4);
    let base = repo.git(&["rev-parse", "HEAD~2"]).trim().to_owned();
    let db = Db::new();
    let options = UpdateOptions {
        excludes: vec!["**/*.lock".into(), "docs/**".into()],
        ..repo.update_options(&db.path())
    };
    churncode::update(&options).unwrap();
    history(&repo, 4, 1);

    // The same excludes in another order and repeated are the same choice.
    churncode::update(&UpdateOptions {
        excludes: vec!["docs/**".into(), "**/*.lock".into(), "docs/**".into()],
        ..options.clone()
    })
    .unwrap();

    for (what, other) in [
        (
            "fewer excludes",
            UpdateOptions {
                excludes: vec!["**/*.lock".into()],
                ..options.clone()
            },
        ),
        (
            "a base",
            UpdateOptions {
                base: Some(base.clone()),
                ..options.clone()
            },
        ),
    ] {
        let err = churncode::update(&other).unwrap_err().to_string();
        assert!(
            err.contains("new database") && err.contains("--exclude '**/*.lock'"),
            "{what}: {err}"
        );
    }

    // A database built with a base refuses an update without it.
    let based = db.named("based.db");
    let with_base = UpdateOptions {
        db: based.clone(),
        base: Some(base.clone()),
        excludes: Vec::new(),
        ..options.clone()
    };
    churncode::update(&with_base).unwrap();
    let err = churncode::update(&UpdateOptions {
        base: None,
        ..with_base
    })
    .unwrap_err()
    .to_string();
    assert!(
        err.contains(&format!("--base {base}")) && err.contains("new database"),
        "{err}"
    );
}

#[test]
fn verify_reports_a_stale_head_and_a_disagreement() {
    let repo = Repo::new();
    history(&repo, 0, 3);
    let db = Db::new();
    // The database does not track f1.txt, which blame does.
    churncode::update(&UpdateOptions {
        excludes: vec!["f1.txt".into()],
        ..repo.update_options(&db.path())
    })
    .unwrap();
    let tip = repo.head();
    history(&repo, 3, 1);
    // The database holds origins at its own newest commit only.
    let err = churncode::verify(&churncode::VerifyOptions {
        repo: repo.path().to_path_buf(),
        db: db.path(),
        files: vec!["f0.txt".into()],
        head: Some("main".into()),
    })
    .unwrap_err()
    .to_string();
    assert!(err.contains(&tip), "{err}");
    // verify blames at the database's newest commit, whatever is checked out.
    let verification = repo.verify(&db.path(), vec!["f0.txt".into(), "f1.txt".into()]);
    assert_eq!(verification.head, tip);
    assert!(verification.files[0].agrees());
    assert!(!verification.files[1].agrees());
    assert_eq!(verification.files[1].stored_lines, 0);
    assert!(!verification.agrees());
}
