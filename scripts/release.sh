#!/usr/bin/env bash
# Cuts a release: sets the workspace version, commits it on top of `main` as
# `chore: release vX.Y.Z`, runs the gate on that commit, tags it and moves
# `main` to it. The tag and the version can't disagree because both come from
# the one version this computes. `mise run release` runs it; see
# docs/DESIGN.md#releases.
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: mise run release [VERSION] [--push]

  VERSION  X.Y.Z to release (default: the workspace version with its patch
           bumped)
  --push   push main and the tag to origin; without it, print the command
EOF
}

die() {
  echo "release: $*" >&2
  exit 1
}

remote=origin
push=false
version=
for arg in "$@"; do
  case $arg in
    --push) push=true ;;
    -h | --help) usage && exit 0 ;;
    -*) usage >&2 && die "unknown flag $arg" ;;
    *)
      [ -z "$version" ] || die "more than one version given"
      version=$arg
      ;;
  esac
done

semver='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
root=$(jj workspace root)
cd "$root"
# From a jj workspace under .workspaces/ there's no .git here, so git would
# find the main checkout's by walking up; name it instead.
git_dir=$(jj git root)

# The `version` line of `[workspace.package]` in Cargo.toml.
current=$(awk '
  /^\[/ { in_pkg = ($0 == "[workspace.package]") }
  in_pkg && /^version = / { gsub(/^version = "|"$/, ""); print; exit }
' Cargo.toml)
[[ $current =~ $semver ]] || die "can't read an X.Y.Z [workspace.package] version from Cargo.toml (got '$current')"

if [ -z "$version" ]; then
  IFS=. read -r major minor patch <<<"$current"
  version="$major.$minor.$((patch + 1))"
fi
[[ $version =~ $semver ]] || die "version must be X.Y.Z, got '$version'"
tag="v$version"

main=$(jj log --no-graph -r main -T 'commit_id' 2>/dev/null) \
  || die "the main bookmark is missing or conflicted"
clean=$(jj log --no-graph -T 'change_id' \
  -r '@ & empty() & description(exact:"") & children(main) ~ merges()')
[ -n "$clean" ] || die "the working copy must be an empty, undescribed change whose only parent is main (try: jj new main)"

# jj imports git's tags, so this also sees one made with git.
local_tag=$(jj log --no-graph -r "tags(exact:\"$tag\")" -T 'commit_id')
[ -z "$local_tag" ] || die "tag $tag already exists locally"
remote_refs=$(git --git-dir="$git_dir" ls-remote "$remote" "refs/tags/$tag" refs/heads/main) \
  || die "can't list $remote's refs"
[ -z "$(awk -v ref="refs/tags/$tag" '$2 == ref' <<<"$remote_refs")" ] \
  || die "tag $tag already exists on $remote"
# The release is main as the remote has it. That also stops a release from
# stacking on an earlier one whose tag was never pushed.
remote_main=$(awk '$2 == "refs/heads/main" { print $1 }' <<<"$remote_refs")
[ "$remote_main" = "$main" ] \
  || die "main (${main:0:12}) isn't $remote's main (${remote_main:0:12}): push or fetch until they match"

release=
stage=commit
on_exit() {
  local status=$?
  [ "$status" -ne 0 ] || return 0
  local undo="jj tag delete $tag; jj bookmark set main -r $main --allow-backwards; jj abandon $release"
  echo >&2
  case $stage in
    commit)
      [ -n "$release" ] || return 0
      echo "release: failed; change $release holds the release as far as it got." >&2
      echo "Fix and rerun after discarding it with: jj abandon $release" >&2
      ;;
    tag)
      echo "release: failed while tagging and moving main. To undo:" >&2
      echo "  $undo" >&2
      ;;
    pushing_main)
      echo "release: $tag and main are set locally but pushing main failed, so nothing is published. Retry with:" >&2
      echo "  $publish" >&2
      echo "or, if $remote's main moved, undo, fetch and rerun:" >&2
      echo "  $undo" >&2
      ;;
    pushing_tag)
      echo "release: main is pushed but $tag isn't; retry with:" >&2
      echo "  ${push_tag[*]}" >&2
      ;;
  esac
}
trap on_exit EXIT

# A fresh change, so it doesn't inherit the old working copy's author date.
jj new main -m "chore: release $tag"
release=$(jj log --no-graph -r @ -T 'change_id.short()')
awk -v v="$version" '
  /^\[/ { in_pkg = ($0 == "[workspace.package]") }
  in_pkg && !done && /^version = / { $0 = "version = \"" v "\""; done = 1 }
  { print }
' Cargo.toml >Cargo.toml.release
mv Cargo.toml.release Cargo.toml
# Only the workspace members' entries change.
cargo update --workspace --quiet
mise run check

stage=tag
jj new
jj tag set "$tag" -r "$release"
jj bookmark move main --to "$release"

# main first, since pushing the tag is what publishes: one push of both isn't
# atomic, and could publish the tag while the remote rejects main.
push_main=(jj git push --remote "$remote" -b main)
push_tag=(jj git push --remote "$remote" -t "$tag")
publish="${push_main[*]} && ${push_tag[*]}"
if $push; then
  stage=pushing_main
  "${push_main[@]}"
  stage=pushing_tag
  "${push_tag[@]}"
  stage=finished
  echo "Pushed main and $tag to $remote."
else
  stage=finished
  echo
  echo "Tagged $release as $tag and moved main to it. To publish:"
  echo "  $publish"
fi
