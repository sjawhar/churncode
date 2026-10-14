# churncode

churncode measures code churn on a git branch: for each first-parent commit, how many of the lines
it added a later commit changed or deleted soon after.

## What churn means here

churncode uses GitClear's definition. Churn is the share of the lines a commit adds that a later
commit changes or deletes within a window (14 days by default) after the adding commit.

- Commits are the first-parent commits of the branch you track. A squash-merged pull request is one
  commit, and so is a merge commit: the lines a merge brings in count as the merge's own.
- A line that a later commit modifies counts as churned at that later commit, the same as a deleted
  line. The modified line is a new line of the later commit.
- The window runs from the adding commit's committer time. A line changed by a commit whose
  committer time is at most the window after it is churned; one second later, it is not.

## How it works

churncode tracks lines forward. It reads the diff of every first-parent commit against its first
parent, in order, and keeps, for every line of every file, the commit that added it. When a diff
removes lines, churncode records how many lines of each origin the commit removed. A report then
sums, for each commit, the removals of its lines by commits inside the window.

The diffs come from `git diff-tree` with zero lines of context and rename detection on. churncode
reads only the file headers and `@@` hunk headers, never the content lines, so one pass over the
history costs about as much as `git log -p` with no context. The commits are split across worker
threads, each running one `git diff-tree --stdin`, and the line bookkeeping afterwards is a few
vector splices per hunk.

Tools that work from `git blame` instead (git-of-theseus, for example) blame every file again at
every sample point, so their cost grows with the size of the tree times the number of samples.
churncode reads each commit's diff once.

Everything goes into a SQLite database: the processed commits, the removals aggregated by
(origin commit, removing commit), and every file's line origins at the newest processed commit. A
later `update` checks that the stored newest commit is still an ancestor of the branch and reads
only the commits after it. The window is applied when you run `report`, so one database answers
any window.

## Install

With [mise](https://mise.jdx.dev):

```sh
mise use github:sjawhar/churncode
```

Or download a release tarball from
[the releases page](https://github.com/sjawhar/churncode/releases) (Linux x86_64 and aarch64,
macOS x86_64 and aarch64). Each holds a `churncode-<target>/` directory with the binary and the
license; `SHA256SUMS` lists every tarball's checksum.

Or build it with Cargo:

```sh
cargo install --git https://github.com/sjawhar/churncode
```

churncode runs `git`, which must be version 2.41 or newer.

## Usage

Build the database from a branch's history, leaving out lock files and other generated files:

```sh
churncode update --repo ~/src/project --db project.db --rev main \
  --exclude '**/*.lock' --exclude '**/go.sum' --exclude '**/dist/**'
```

`--exclude` takes a glob in git's pathspec syntax and can be repeated. The excludes are stored in
the database, and so is `--base`: they decide which lines exist at all, so an update that names
different ones is refused, and you build a new database instead. `-j` sets the number of worker
threads (the number of CPUs by default).

Run the same command again later to read the commits added since. If the branch's history was
rewritten so that the database's newest commit is no longer on it, the update is refused and names
both commits.

Report churn for the commits made in a period:

```sh
churncode report --db project.db --since 2026-09-15T00:00:00Z --until 2026-09-29T00:00:00Z
```

```
commits from 2026-09-15T00:00:00Z to 2026-09-29T00:00:00Z, window 14d
newest processed commit 609206fd8c33 at 2026-10-13T08:41:26Z
commits        751
lines added    452327
lines churned  97588
churn          21.57%
final          yes
```

`--since` and `--until` take RFC 3339 times or unix seconds and select commits by committer time
(`--since` inclusive, `--until` exclusive). `--window` takes a duration such as `14d`, `36h`,
`1d12h` or `90s`. `--per-commit` adds one row per commit:

```
COMMIT        COMMITTED             ADDED  CHURNED   CHURN  FINAL  SUBJECT
ec55ea59905d  2026-10-06T01:53:41Z   1171       81    6.9%  no     daemon: readiness gate and capped backoff instead of a boot crash-loop (LEGION-580) (#1817)
1002302bbf74  2026-10-06T02:58:23Z    426       83   19.5%  no     test(pmdoc): assert the guard's own node count and a scaled time ratio, not thresholds race and load break (LEGION-571) (#1808)
```

A commit's figure is final when its committer time plus the window is at or before the newest
processed commit's time: no later update can change it. `--format json` and `--format tsv` print the
same figures for other programs.

Check the stored line origins against `git blame --first-parent`:

```sh
churncode verify --repo ~/src/project --db project.db --files src/main.rs src/lib.rs
```

`verify` prints each file's agreement and exits 1 if any line disagrees. It blames at the
database's newest commit, which is the only commit the database holds line origins for.

## Measurements

On a 32-core machine:

- [sjawhar/legion](https://github.com/sjawhar/legion)'s 2,381 first-parent commits build in about
  1.2 s with 32 workers and 5.7 s with one.
- A private monorepo's 5,265 first-parent commits build in 3.8 s with 32 workers and 30 s with one.
- 20 new commits after a full build take 0.3 to 1.7 s, depending on the machine's load.
- The stored line origins matched `git blame --first-parent` on every line of 60 sampled files
  (140,094 lines).
- Over legion's commits of 2026-09-15 to 2026-09-28, churncode found 21.6% of the added lines
  churned within 14 days, with lock files, `go.sum`, SVG and PNG files, built bundles under `dist/`
  and generated docs excluded. git-of-theseus found 21.3% with the same exclusions, and Apache
  DevLake (with three fixes) 23.2% with none.
- git-of-theseus takes about 100 s on legion and 20 minutes on the monorepo.

## Limitations

- Only first-parent history counts. The commits of a merged branch are not reported one by one;
  their lines are the merge commit's.
- Code moved to another file counts as deleted from the old file and added by the moving commit,
  unless git detects the move as a rename (a file whose content is at least half the same). Code
  moved to another repository counts as deleted.
- git pairs the deleted and added files of one diff into renames across the whole diff, while
  `git blame` looks for each file's source on its own. When one commit deletes and adds several
  similar files, the two can pair them differently, and `verify` reports the lines involved.
- Binary files carry no lines. A file that turns from text into binary has all its lines deleted;
  one that turns from binary into text has all its lines added.
- Submodules are ignored.
- A shallow clone needs history back to the start of the window. Pass the oldest commit as
  `--base`: its tree counts as already there, and its lines belong to no reported commit. A line
  from before the base that a commit in the window changes is not counted against any commit.

## License

Apache-2.0. See [LICENSE](LICENSE).
