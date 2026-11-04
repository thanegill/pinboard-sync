#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<EOF
usage: $0 [--no-push] X.Y.Z

Opens the bump PR for release X.Y.Z. On a new release/vX.Y.Z branch from origin/master
it sets the version in Cargo.toml and Cargo.lock, moves CHANGELOG.md's [Unreleased]
items under "## [X.Y.Z] - <today>", points the link references at the new tag, commits,
pushes the branch and opens the PR. With --no-push it stops after the commit. It never
pushes master or a tag: after the PR merges, you tag its commit (see release.yml).
EOF
  exit 2
}

fail() {
  echo "prepare-release: $*" >&2
  exit 1
}

push=true
version=
for arg in "$@"; do
  case $arg in
    --no-push) push=false ;;
    -*) usage ;;
    *)
      [[ -z $version ]] || usage
      version=$arg
      ;;
  esac
done
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || usage

cd "$(git rev-parse --show-toplevel)"
[[ -z $(git status --porcelain) ]] || fail "the working tree has changes; commit or set them aside first"

git fetch origin master
if git rev-parse --quiet --verify "refs/tags/v$version" > /dev/null; then
  fail "a v$version tag already exists locally"
fi

current=$(git show origin/master:Cargo.toml | awk -F'"' '
  /^\[/ { in_package = ($0 == "[package]") }
  in_package && /^version = "/ { print $2; exit }
')
[[ -n $current ]] || fail "found no version in Cargo.toml's [package] table"
newest=$(printf '%s\n' "$current" "$version" | sort -V | tail -n1)
[[ $version != "$current" && $newest == "$version" ]] ||
  fail "$version is not newer than master's version, $current"

changelog=$(git show origin/master:CHANGELOG.md)
if [[ -n $(awk -v heading="## [$version]" 'index($0, heading) == 1' <<<"$changelog") ]]; then
  fail "CHANGELOG.md already has a [$version] section"
fi
unreleased=$(awk '
  $0 == "## [Unreleased]" { grab = 1; next }
  grab && (/^## \[/ || /^\[[^][]+\]: /) { exit }
  grab { print }
' <<<"$changelog" | sed -e '/./,$!d')
[[ $unreleased =~ [^[:space:]] ]] || fail "CHANGELOG.md's [Unreleased] section is empty: nothing to release"

branch="release/v$version"
git switch --no-track -c "$branch" origin/master

write_back() {
  local edited
  edited=$(mktemp)
  cat > "$edited"
  cat "$edited" > "$1"
  rm "$edited"
}

awk -v version="$version" '
  /^\[/ { in_package = ($0 == "[package]") }
  in_package && !done && /^version = "/ { $0 = "version = \"" version "\""; done = 1 }
  { print }
' Cargo.toml | write_back Cargo.toml
awk -v version="$version" '
  after_name && /^version = "/ { $0 = "version = \"" version "\"" }
  { after_name = ($0 == "name = \"pinboard-sync\""); print }
' Cargo.lock | write_back Cargo.lock
awk -v version="$version" -v date="$(date -u +%Y-%m-%d)" '
  $0 == "## [Unreleased]" { print; print ""; print "## [" version "] - " date; next }
  /^\[Unreleased\]: .*\/compare\// {
    base = substr($0, length("[Unreleased]: ") + 1)
    base = substr(base, 1, index(base, "/compare/") - 1)
    print "[Unreleased]: " base "/compare/v" version "...HEAD"
    print "[" version "]: " base "/releases/tag/v" version
    next
  }
  { print }
' CHANGELOG.md | write_back CHANGELOG.md

git diff --quiet Cargo.lock && fail "found no pinboard-sync entry to update in Cargo.lock"
git diff --stat

git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -m "chore: release v$version"

tag_steps="After it merges, tag the merge commit on master to start the release workflow:

    git switch master && git pull --ff-only
    git tag v$version && git push origin v$version"

if [[ $push != true ]]; then
  echo "Committed on $branch; not pushed (--no-push). To open the PR:"
  echo "    git push --no-follow-tags -u origin $branch && gh pr create --base master --fill"
  echo
  echo "$tag_steps"
  exit 0
fi

git push --no-follow-tags -u origin "$branch"
gh pr create --base master --head "$branch" --title "chore: release v$version" --body "Bumps the version to $version and moves the changelog's [Unreleased] items under [$version].

$tag_steps

## Release notes

$unreleased"
echo
echo "$tag_steps"
