//! Exact per-commit figures on scripted histories.

mod common;

use common::*;

#[test]
fn modified_and_deleted_lines_count_against_the_commit_that_added_them() {
    let repo = Repo::new();
    repo.write("f.txt", lines("a", 10));
    repo.commit("add f", T0);
    // Rewrite lines 2 and 3 and delete line 10.
    let mut f: Vec<String> = lines("a", 9).lines().map(str::to_owned).collect();
    f[1] = "b2".into();
    f[2] = "b3".into();
    repo.write("f.txt", f.join("\n") + "\n");
    repo.commit("edit f", T0 + HOUR);
    // Delete lines 1 and 2 (a1 from the first commit, b2 from the second).
    repo.write("f.txt", f[2..].join("\n") + "\n");
    repo.commit("trim f", T0 + 2 * HOUR);
    repo.remove("f.txt");
    repo.write("g.txt", lines("g", 3));
    repo.commit("replace f with g", T0 + 3 * HOUR);

    let db = Db::new();
    repo.update(&db.path());
    assert_eq!(
        figures(&db.path(), WINDOW),
        expected(&[
            ("add f", 10, 10),
            ("edit f", 2, 2),
            ("trim f", 0, 0),
            ("replace f with g", 3, 0),
        ])
    );
}

#[test]
fn a_renamed_file_keeps_its_lines_origins() {
    let repo = Repo::new();
    repo.write("a.txt", lines("line", 20));
    repo.commit("add a", T0);
    // Rename with two lines rewritten: similar enough for git to see a rename.
    let mut a: Vec<String> = lines("line", 20).lines().map(str::to_owned).collect();
    a[4] = "renamed 5".into();
    a[9] = "renamed 10".into();
    repo.remove("a.txt");
    repo.write("dir/b.txt", a.join("\n") + "\n");
    repo.commit("rename a to dir/b", T0 + HOUR);
    // One line from each commit.
    a[0] = "edited 1".into();
    a[4] = "edited 5".into();
    repo.write("dir/b.txt", a.join("\n") + "\n");
    repo.commit("edit b", T0 + 2 * HOUR);

    let db = Db::new();
    repo.update(&db.path());
    assert_eq!(
        figures(&db.path(), WINDOW),
        expected(&[
            ("add a", 20, 3),
            ("rename a to dir/b", 2, 1),
            ("edit b", 2, 0),
        ])
    );
    assert!(repo.verify(&db.path(), repo.files()).agrees());
}

#[test]
fn binary_files_carry_no_lines() {
    let repo = Repo::new();
    repo.write("t.txt", lines("t", 5));
    repo.write("bin.dat", b"\x00\x01binary\n\x02\n");
    repo.commit("add text and binary", T0);
    repo.write("bin.dat", b"\x00\x01changed binary\n");
    repo.commit("change binary", T0 + HOUR);
    // A text file becoming binary: all of its lines are deleted.
    repo.write("t.txt", b"now\x00binary\n");
    repo.commit("text to binary", T0 + 2 * HOUR);
    // A binary file becoming text: all of its lines are added.
    repo.write("bin.dat", lines("b", 3));
    repo.commit("binary to text", T0 + 3 * HOUR);
    repo.write("bin.dat", "b1\nB2\nb3\n");
    repo.commit("edit former binary", T0 + 4 * HOUR);

    let db = Db::new();
    repo.update(&db.path());
    assert_eq!(
        figures(&db.path(), WINDOW),
        expected(&[
            ("add text and binary", 5, 5),
            ("change binary", 0, 0),
            ("text to binary", 0, 0),
            ("binary to text", 3, 1),
            ("edit former binary", 1, 0),
        ])
    );
    assert!(repo.verify(&db.path(), vec!["bin.dat".into()]).agrees());
}

#[test]
fn a_commit_touching_only_excluded_paths_adds_nothing() {
    let repo = Repo::new();
    repo.write("src.txt", lines("s", 4));
    repo.write("Cargo.lock", lines("lock", 100));
    repo.commit("add src and lock", T0);
    repo.write("Cargo.lock", lines("relock", 100));
    repo.commit("relock", T0 + HOUR);
    repo.write("src.txt", "S1\ns2\ns3\ns4\n");
    repo.commit("edit src 1", T0 + 2 * HOUR);
    repo.write("nested/deep/x.lock", lines("x", 7));
    repo.commit("add nested lock", T0 + 3 * HOUR);
    repo.write("src.txt", "S1\nS2\ns3\ns4\n");
    repo.commit("edit src 2", T0 + 4 * HOUR);
    repo.write("Cargo.lock", lines("lock", 3));
    repo.commit("relock again", T0 + 5 * HOUR);
    // One job puts two commits in a batch (so an empty diff starts one batch and is all of
    // another), several put one in each.
    for jobs in 1..=3 {
        let db = Db::new();
        let options = churncode::UpdateOptions {
            excludes: vec!["**/*.lock".into()],
            jobs,
            ..repo.update_options(&db.path())
        };
        churncode::update(&options).unwrap();
        assert_eq!(
            figures(&db.path(), WINDOW),
            expected(&[
                ("add src and lock", 4, 2),
                ("relock", 0, 0),
                ("edit src 1", 1, 0),
                ("add nested lock", 0, 0),
                ("edit src 2", 1, 0),
                ("relock again", 0, 0),
            ]),
            "{jobs} jobs"
        );
    }

    // Without the exclude, the lock files count like any other.
    let db = Db::new();
    repo.update(&db.path());
    assert_eq!(
        figures(&db.path(), WINDOW),
        expected(&[
            ("add src and lock", 104, 102),
            ("relock", 100, 100),
            ("edit src 1", 1, 0),
            ("add nested lock", 7, 0),
            ("edit src 2", 1, 0),
            ("relock again", 3, 0),
        ])
    );
}

#[test]
fn a_merge_is_one_first_parent_commit() {
    let repo = Repo::new();
    repo.write("f.txt", lines("f", 6));
    repo.commit("add f", T0);
    repo.git(&["checkout", "-q", "-b", "side"]);
    repo.write("f.txt", "side1\nf2\nf3\nf4\nf5\nf6\n");
    repo.write("g.txt", lines("g", 3));
    repo.commit("side 1", T0 + HOUR);
    repo.write("f.txt", "side1\nside2\nf3\nf4\nf5\nf6\n");
    repo.commit("side 2", T0 + 2 * HOUR);
    repo.git(&["checkout", "-q", "main"]);
    repo.write("h.txt", lines("h", 2));
    repo.commit("main 1", T0 + 3 * HOUR);
    repo.merge("side", "merge side", T0 + 4 * HOUR);
    repo.write("g.txt", "G1\ng2\ng3\n");
    repo.commit("edit g", T0 + 5 * HOUR);

    let db = Db::new();
    repo.update(&db.path());
    // The side branch's commits are not first-parent commits: their lines are the merge's.
    assert_eq!(
        figures(&db.path(), WINDOW),
        expected(&[
            ("add f", 6, 2),
            ("main 1", 2, 0),
            ("merge side", 5, 1),
            ("edit g", 1, 0),
        ])
    );
    assert!(repo.verify(&db.path(), repo.files()).agrees());
}

#[test]
fn the_window_includes_its_last_second_only() {
    let repo = Repo::new();
    repo.write("f.txt", lines("f", 4));
    repo.commit("add f", T0);
    repo.write("f.txt", "f2\nf3\nf4\n");
    repo.commit("at the window's end", T0 + WINDOW);
    repo.write("f.txt", "f3\nf4\n");
    repo.commit("one second past it", T0 + WINDOW + 1);

    let db = Db::new();
    repo.update(&db.path());
    assert_eq!(
        figures(&db.path(), WINDOW),
        expected(&[
            ("add f", 4, 1),
            ("at the window's end", 0, 0),
            ("one second past it", 0, 0),
        ])
    );
    assert_eq!(figures(&db.path(), WINDOW + 1)[0], ("add f".into(), 4, 2));
    assert_eq!(figures(&db.path(), WINDOW - 1)[0], ("add f".into(), 4, 0));

    // A figure is final once its window ends at or before the newest commit's time.
    let finals: Vec<bool> = commits(&db.path(), WINDOW)
        .iter()
        .map(|c| c.is_final)
        .collect();
    assert_eq!(finals, vec![true, false, false]);
    let finals: Vec<bool> = commits(&db.path(), 1).iter().map(|c| c.is_final).collect();
    assert_eq!(finals, vec![true, true, false]);
}

#[test]
fn since_and_until_select_by_committer_time() {
    let repo = Repo::new();
    for i in 0..4 {
        repo.write(&format!("f{i}.txt"), lines("x", i + 1));
        repo.commit(&format!("c{i}"), T0 + i as i64 * DAY);
    }
    let db = Db::new();
    repo.update(&db.path());
    let report = churncode::report(&churncode::ReportOptions {
        db: db.path(),
        since: Some(T0 + DAY),
        until: Some(T0 + 3 * DAY),
        window: WINDOW,
    })
    .unwrap();
    let subjects: Vec<&str> = report.commits.iter().map(|c| c.subject.as_str()).collect();
    assert_eq!(subjects, ["c1", "c2"]);
    assert_eq!(report.totals.lines_added, 5);
    assert_eq!(report.totals.commits, 2);
}

#[test]
fn base_counts_the_tree_as_already_there() {
    let repo = Repo::new();
    repo.write("f.txt", lines("f", 5));
    repo.write("img.bin", b"\x00\x00\n");
    repo.commit("before base", T0);
    repo.write("f.txt", "f1\nf2\nf3\nf4\nf5\nnew6\n");
    let base = repo.commit("base", T0 + HOUR);
    repo.write("f.txt", "F1\nf2\nf3\nf4\nf5\nNEW6\n");
    repo.commit("after base", T0 + 2 * HOUR);

    let db = Db::new();
    churncode::update(&churncode::UpdateOptions {
        base: Some(base),
        ..repo.update_options(&db.path())
    })
    .unwrap();
    // Lines that were there at the base belong to no reported commit.
    assert_eq!(
        figures(&db.path(), WINDOW),
        expected(&[("after base", 2, 0)])
    );
    assert!(repo.verify(&db.path(), vec!["f.txt".into()]).agrees());
}

#[test]
fn a_shallow_clone_needs_a_base() {
    let origin = Repo::new();
    for i in 0..4 {
        origin.write("f.txt", lines(&format!("v{i}-"), 3));
        origin.commit(&format!("c{i}"), T0 + i as i64 * HOUR);
    }
    let parent = tempfile::tempdir().unwrap();
    let url = format!("file://{}", origin.path().display());
    git_in(
        parent.path(),
        &[
            "clone", "-q", "--depth", "2", "--branch", "main", &url, "clone",
        ],
        &[],
    );
    let clone = parent.path().join("clone");
    let oldest = git_in(&clone, &["rev-list", "--max-parents=0", "HEAD"], &[])
        .trim()
        .to_owned();
    let db = Db::new();
    let options = churncode::UpdateOptions {
        repo: clone.clone(),
        ..origin.update_options(&db.path())
    };
    let err = churncode::update(&options).unwrap_err().to_string();
    assert!(
        err.contains("shallow clone") && err.contains(&format!("--base {oldest}")),
        "{err}"
    );
    churncode::update(&churncode::UpdateOptions {
        base: Some(oldest),
        ..options
    })
    .unwrap();
    assert_eq!(figures(&db.path(), WINDOW), expected(&[("c3", 3, 0)]));
}
