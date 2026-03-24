# Contributing

## Prerequisites

- [Rust](https://rustup.rs/) (stable toolchain)
- [just](https://github.com/casey/just) — task runner (`cargo install just` or `brew install just`)
- [git-cliff](https://git-cliff.org/) — changelog generator (`cargo install git-cliff` or `brew install git-cliff`)
- [glab](https://gitlab.com/gitlab-org/cli) — GitLab CLI, needed only for publishing releases (`brew install glab`)

## Development

```bash
cargo build                        # debug build
cargo run -- path/to/file.csv      # run with a file
cargo run -- path/to/file.parquet
cargo test                         # run tests
cargo clippy                       # lint — must be clean before committing
cargo build --release              # optimised build (LTO + strip)
```

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

`chore(release):` commits are generated automatically by `just bump` and are excluded from the changelog.

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

This pushes the branch and tag to GitLab and creates a GitLab release with the changelog section for this version as the release notes.

### Update the changelog without cutting a release

```bash
just changelog-full    # rewrites CHANGELOG.md from all history
```
