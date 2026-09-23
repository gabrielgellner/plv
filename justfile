# plv release workflow
#
# `just --list` shows a recipe's *last* comment line and drops the rest, so
# every recipe here puts its detail above and its one-line summary at the
# bottom of the block. Written the other way round, the listing quotes a
# fragment of the explanation instead of saying what the recipe does.

# List available recipes
default:
    @just --list

# Preview changelog for unreleased commits
changelog-preview:
    git-cliff --unreleased

# Regenerate the full CHANGELOG.md from all history
changelog-full:
    git-cliff -o CHANGELOG.md
    @echo "CHANGELOG.md updated"

# Print the next version that bump would produce (requires at least one vX.Y.Z tag)
next-version:
    @git-cliff --bumped-version

# For the very first release pass the version explicitly: `just bump v0.1.0`.
# Otherwise omit it and git-cliff calculates the bump from the commit types
# since the last tag — pass one by hand when that answer is wrong, as it is
# for a release of polish committed under `fix:`.
#
# Nothing is pushed. Review with `git log --oneline -5`, then `just release`.
#
# Update Cargo.toml + CHANGELOG.md, commit and tag — without pushing
bump version="":
    #!/usr/bin/env bash
    set -euo pipefail

    if [ -n "{{version}}" ]; then
        NEXT="{{version}}"
        [[ "$NEXT" == v* ]] || NEXT="v$NEXT"
    else
        NEXT=$(git-cliff --bumped-version) || {
            echo ""
            echo "ERROR: could not calculate next version (no tags found?)."
            echo "For the first release run:  just bump 0.1.0"
            exit 1
        }
    fi

    VER="${NEXT#v}"
    echo "Bumping to $NEXT"

    # Update version in Cargo.toml and refresh Cargo.lock
    cargo set-version "$VER"

    # Write full changelog (--tag sets the version for unreleased commits)
    git-cliff --tag "$NEXT" -o CHANGELOG.md

    # The README's install example pins a version, because the checksum step
    # needs a concrete one to verify against. It has to move with the bump or
    # the docs describe the release before last. The line is checked for first:
    # a substitution that quietly matched nothing would leave exactly the
    # staleness this is here to prevent, and it would not show up until someone
    # followed the instructions.
    if ! grep -qE '^VER=[0-9]+\.[0-9]+\.[0-9]+$' README.md; then
        echo "ERROR: no VER=x.y.z line found in README.md."
        echo "The install example moved or changed shape; update this recipe."
        exit 1
    fi
    # Written without sed -i, whose in-place flag takes an argument on BSD and
    # not on GNU, so the one spelling would break on the other's machine.
    TMP=$(mktemp)
    sed -E "s/^VER=[0-9]+\.[0-9]+\.[0-9]+/VER=$VER/" README.md > "$TMP"
    mv "$TMP" README.md

    git add Cargo.toml Cargo.lock CHANGELOG.md README.md
    git commit -m "chore(release): $NEXT"
    git tag -a "$NEXT" -m "Release $NEXT"

    echo ""
    echo "Created commit and tag $NEXT."
    echo "Review with:  git log --oneline -5"
    echo "Then run:     just release"

# Run after `just bump`. Publishes only: the binary on this machine is left
# alone, and `just install` is the step that changes it.
#
# The release is created here rather than by the workflow, because git-cliff
# writes better notes than --generate-notes can: it groups by commit type and
# reads the bodies. Pushing the tag starts the binary build, so the two race —
# whichever arrives second edits the notes instead of failing on a release
# that is already there.
#
# Push the branch and tag, then create the GitHub release
release:
    #!/usr/bin/env bash
    set -euo pipefail

    TAG=$(git describe --tags --abbrev=0)

    git push origin HEAD
    git push origin "$TAG"

    # Release notes = this tag's section from the changelog, from the first
    # group heading on. The version heading is dropped: the release is already
    # titled for its version, so repeating it inside the notes is noise.
    NOTES=$(git-cliff --latest --strip all | awk 'f || /^###/ { f = 1; print }')

    gh release create "$TAG" --title "Release $TAG" --notes "$NOTES" 2>/dev/null \
        || gh release edit "$TAG" --title "Release $TAG" --notes "$NOTES"

    echo "Released $TAG to GitHub."
    echo ""
    echo "Binaries are building: gh run watch"
    echo "To put this version on crates.io: just publish"
    echo "Your own copy is unchanged. Run: just install"

# Deliberately not part of `release`, and deliberately last. A GitHub release
# can be deleted and cut again; a crates.io version cannot. `cargo yank` only
# stops new dependents from resolving it — the version, and the code in it,
# stay public forever. So this is its own decision, made after the release it
# publishes has been looked at.
#
# The version is read from the tag rather than passed in, and checked against
# Cargo.toml, because publishing a version other than the one just tagged is
# the mistake worth spending a check on.
#
# Publish the current tag to crates.io (irreversible)
publish:
    #!/usr/bin/env bash
    set -euo pipefail

    TAG=$(git describe --tags --exact-match 2>/dev/null) || {
        echo "ERROR: HEAD is not exactly on a tag. Run 'just bump' first."
        exit 1
    }
    VER="${TAG#v}"
    MANIFEST=$(cargo metadata --no-deps --format-version 1 \
        | python3 -c 'import json,sys; print(json.load(sys.stdin)["packages"][0]["version"])')

    if [ "$VER" != "$MANIFEST" ]; then
        echo "ERROR: tag is $TAG but Cargo.toml says $MANIFEST."
        exit 1
    fi

    if [ -n "$(git status --porcelain)" ]; then
        echo "ERROR: working tree is dirty. Commit or stash first."
        exit 1
    fi

    echo "About to publish plv $VER to crates.io. This cannot be undone."
    read -p "Type the version to confirm: " CONFIRM
    [ "$CONFIRM" = "$VER" ] || { echo "Aborted."; exit 1; }

    cargo publish --locked

    echo ""
    echo "Published plv $VER: https://crates.io/crates/plv/$VER"

# Separate from `release` on purpose: publishing to GitHub and replacing the
# binary on this machine are different decisions, and a release cut from a
# branch you are not running should not silently change what `plv` means in
# your shell.
#
# Install the working tree to ~/.cargo/bin and say what landed
install:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo install --path .
    echo ""
    echo "Now on PATH: $(plv --version) at $(command -v plv)"
