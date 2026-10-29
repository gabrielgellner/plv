# Contributing

## Prerequisites

- [Rust](https://rustup.rs/) (stable toolchain)
- [cargo-edit](https://github.com/killercup/cargo-edit) — adds `cargo set-version` (`cargo install cargo-edit`)
- [just](https://github.com/casey/just) — task runner (`cargo install just` or `brew install just`)
- [git-cliff](https://git-cliff.org/) — changelog generator (`cargo install git-cliff` or `brew install git-cliff`)
- [gh](https://cli.github.com/) — GitHub CLI, needed only for publishing releases (`brew install gh`)

## Development

```bash
cargo build                        # debug build
cargo run -- path/to/file.csv      # run with a file
cargo run -- path/to/file.jsonl    # or .ndjson, .parquet, a .ducklake catalog
cargo run -- samples/log.jsonl     # a small log to try things on
cargo test                         # run tests
cargo fmt                          # format — CI runs `cargo fmt --check`
cargo clippy --all-targets         # lint — CI fails on any warning
cargo build --release              # optimised build (LTO + strip)
```

## Pull requests and CI

`main` is protected: every change reaches it through a pull request, and it
cannot be force-pushed or deleted. Work on a branch named for its type
(`fix/…`, `feat/…`, `docs/…`, `chore/…`); a branch for an issue is made with
`gh issue develop <n> --checkout --name fix/<n>-…`, which links the two, and
`Closes #<n>` in the pull request closes the issue when it merges.

Every pull request, and every push to `main`, runs the `ci` workflow
(`.github/workflows/ci.yml`). Its `check` job reports on the pull request; it
is not a required check yet, so look for green before merging:

```bash
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

Run the same three before pushing and CI holds no surprises. `--locked` checks
what the committed `Cargo.lock` describes; if it fails there, the lock file is
out of date with `Cargo.toml` and needs committing. The toolchain is the latest
stable, so a new Rust release can bring a new lint that fails a branch which
did not cause it: fix it there, in its own `chore:` commit.

Merge with a merge commit rather than a squash or a rebase. Its subject follows
the branch's type, `fix: merge fix/1-parquet-dtypes — what it does`, and the
changelog skips it in favour of the branch's own commits, which is why their
types matter (below).

## Commit style

This project uses [Conventional Commits](https://www.conventionalcommits.org/). The type prefix determines how `just bump` calculates the next version:

| Prefix | Example | Version effect |
|---|---|---|
| `feat:` | `feat(ui): add search bar` | bumps minor |
| `fix:` | `fix: correct row offset calculation` | bumps patch |
| `perf:` | `perf: cache column widths` | bumps patch |
| `refactor:` | `refactor(store): simplify fetch` | bumps patch |
| `docs:` | `docs: update README` | bumps patch |
| `chore:` | `chore: update dependencies` | bumps patch |
| `feat!:` or `BREAKING CHANGE:` in body | | bumps major |

`chore(release):` commits are generated automatically by `just bump` and are excluded from the changelog. Merge commits are excluded too, so the changelog entry comes from the branch's own commit — give that one the type you want the release notes to read.

**Polish is `feat:`, not `fix:`.** Reserve `fix:` for something that was actually broken. A change to what a key does, what the screen shows, or what the user can now reach is a feature even when it reads like correcting a wart — it is what the user will notice in the release. v0.12.0 shipped `-` and `s` gaining a column cursor and the sort-indicator rework as four `fix:` commits, so the notes had no **Features** heading and `just bump` proposed a patch when the release plainly was not one.

## Release workflow

All release steps run through `just`. Run `just` with no arguments to list available recipes.

### Preview what would go into the next release

```bash
just changelog-preview
```

### Cut a release

```bash
just bump          # auto-calculates version from commits since last tag
just bump 1.2.0    # or specify explicitly (useful for the first release)
```

`just bump` will:
1. Calculate the next version from conventional commits (`feat` → minor, `fix`/`perf`/etc. → patch, breaking → major).
2. Update `version` in `Cargo.toml` and refresh `Cargo.lock`.
3. Regenerate `CHANGELOG.md`.
4. Create a `chore(release): vX.Y.Z` commit.
5. Create an annotated git tag `vX.Y.Z`.

Nothing is pushed yet. Review the result:

```bash
git log --oneline -5
git show --stat HEAD
```

If something looks wrong, undo with:

```bash
git tag -d vX.Y.Z
git reset --soft HEAD~1
```

### Publish

Once satisfied:

```bash
just release
```

Cut a release from an up-to-date `main` with every pull request for it merged.
The `chore(release)` commit is pushed straight to `main` rather than through a
pull request: branch protection lets an admin bypass it, and the release
depends on that, since the tag has to point at the commit that names its
version.

This pushes the branch and tag to GitHub and creates a GitHub release with the
changelog section for this version as the release notes.

Pushing the tag also starts the `release` workflow, which builds binaries for
macOS arm64 and Linux x86_64/arm64 and attaches them to that release along with
a `SHA256SUMS` file. Watch it with `gh run watch`. The release exists as soon as
`just release` returns; the binaries arrive when the builds finish, which takes
a while because each one compiles the bundled DuckDB.

### Publish to crates.io

Separate from `just release`, and deliberately so: a GitHub release can be
deleted and cut again, while a crates.io version is permanent — `cargo yank`
only stops new dependents from resolving it.

```bash
just publish
```

It refuses unless `HEAD` is exactly on a tag, the tag matches the version in
`Cargo.toml` and the working tree is clean, then asks you to type the version
back before it publishes.

### Update your own copy

Publishing does not touch the binary on your machine — `plv` in your shell stays
whatever you last installed, which is easy to miss for several releases running.

```bash
just install       # cargo install --path ., then prints what landed
```

It is deliberately a separate step: cutting a release from a branch you are not
running should not silently change what `plv` means in your shell.

### Update the changelog without cutting a release

```bash
just changelog-full    # rewrites CHANGELOG.md from all history
```
