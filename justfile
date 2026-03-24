# plv release workflow

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

# Bump version: update Cargo.toml + CHANGELOG.md, commit, tag.
# For the very first release pass the version explicitly: just bump v0.1.0
# Subsequent releases can omit it and git-cliff will calculate the bump automatically.
# Does NOT push — review with `git log --oneline -5`, then run `just release`.
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

# Push the current branch + latest tag, then create the GitLab release.
# Run after `just bump`.
release:
    #!/usr/bin/env bash
    set -euo pipefail

    TAG=$(git describe --tags --abbrev=0)

    git push origin HEAD
    git push origin "$TAG"

    # Release notes = this tag's section from the changelog (header/footer stripped)
    NOTES=$(git-cliff --latest --strip all)

    glab release create "$TAG" \
        --name "Release $TAG" \
        --notes "$NOTES"

    echo "Released $TAG to GitLab."
