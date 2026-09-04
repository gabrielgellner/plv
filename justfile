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

    git add Cargo.toml Cargo.lock CHANGELOG.md
    git commit -m "chore(release): $NEXT"
    git tag -a "$NEXT" -m "Release $NEXT"

    echo ""
    echo "Created commit and tag $NEXT."
    echo "Review with:  git log --oneline -5"
    echo "Then run:     just release"

# Run after `just bump`. Publishes only: the binary on this machine is left
# alone, and `just install` is the step that changes it.
#
# Push the branch and tag, then create the GitLab release
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

    glab release create "$TAG" \
        --name "Release $TAG" \
        --notes "$NOTES"

    echo "Released $TAG to GitLab."
    echo ""
    echo "Your own copy is unchanged. Run: just install"

# Separate from `release` on purpose: publishing to GitLab and replacing the
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
