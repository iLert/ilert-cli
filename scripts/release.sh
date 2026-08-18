#!/usr/bin/env bash
#
# Cuts a release: tags the current commit, opens the draft release and
# dispatches release-binaries.yml to fill it with the cross compiled binaries.
#
# Publishing stays manual on purpose — see the comment at the top of
# .github/workflows/release-binaries.yml. The command to do it is printed at
# the end, once the draft is complete.
#
#   ./scripts/release.sh            # version from Cargo.toml
#   ./scripts/release.sh 0.3.1      # explicit version
#   ./scripts/release.sh --no-watch # dispatch and return immediately
#
set -euo pipefail
if [ -z "${DEBUG:-}" ]; then
  set +o xtrace
else
  set -o xtrace
fi

BRANCH="master"
WORKFLOW="release-binaries.yml"

die() {
  echo "error: $*" >&2
  exit 1
}

VERSION=""
WATCH=1
for arg in "$@"; do
  case "$arg" in
    --no-watch) WATCH=0 ;;
    -h|--help)
      sed -n '2,/^set -/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    -*) die "unknown option: $arg" ;;
    *)
      [ -z "$VERSION" ] || die "version given twice: $VERSION and $arg"
      VERSION="$arg"
      ;;
  esac
done

cd "$(dirname "$0")/.."

command -v gh >/dev/null || die "gh is not installed — https://cli.github.com"
gh auth status >/dev/null 2>&1 || die "gh is not authenticated — run 'gh auth login'"

CARGO_VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
[ -n "$CARGO_VERSION" ] || die "could not read the version from Cargo.toml"

if [ -z "$VERSION" ]; then
  VERSION="$CARGO_VERSION"
elif [ "$VERSION" != "$CARGO_VERSION" ]; then
  # The binary reports the Cargo version, so a tag that disagrees with it ships
  # a binary that misreports itself.
  die "version '${VERSION}' does not match Cargo.toml ('${CARGO_VERSION}') — bump Cargo.toml first"
fi

# Preflight. Everything here is cheap to check and expensive to get wrong: a tag
# cannot be moved once the workflow has resolved it, and a published release
# cannot receive assets at all.
current_branch=$(git rev-parse --abbrev-ref HEAD)
[ "$current_branch" = "$BRANCH" ] || die "on branch '${current_branch}', releases are cut from '${BRANCH}'"

[ -z "$(git status --porcelain)" ] || die "working tree is dirty — commit or stash first"

git fetch --quiet origin "$BRANCH"
local_sha=$(git rev-parse HEAD)
remote_sha=$(git rev-parse "origin/${BRANCH}")
[ "$local_sha" = "$remote_sha" ] || die "HEAD (${local_sha:0:12}) differs from origin/${BRANCH} (${remote_sha:0:12}) — push or pull first"

git rev-parse -q --verify "refs/tags/${VERSION}" >/dev/null &&
  die "tag '${VERSION}' already exists locally"
[ -z "$(git ls-remote --tags origin "refs/tags/${VERSION}")" ] ||
  die "tag '${VERSION}' already exists on origin"
gh release view "$VERSION" >/dev/null 2>&1 &&
  die "release '${VERSION}' already exists"

echo "Releasing ${VERSION} from ${BRANCH} at ${local_sha:0:12}"
echo

echo "==> Tagging"
git tag -a "$VERSION" -m "ilert-cli ${VERSION}"
git push origin "$VERSION"

echo "==> Creating the draft release"
# --notes "" keeps this non-interactive; notes are written by hand in the draft
# before publishing.
gh release create "$VERSION" \
  --draft \
  --prerelease=false \
  --verify-tag \
  --title "$VERSION" \
  --notes ""

echo "==> Dispatching ${WORKFLOW}"
gh workflow run "$WORKFLOW" --ref "$BRANCH" -f tag="$VERSION"

if [ "$WATCH" -eq 0 ]; then
  echo
  echo "Watch it with: gh run watch \$(gh run list --workflow=${WORKFLOW} --limit 1 --json databaseId --jq '.[0].databaseId')"
  echo "Once it succeeds and the draft looks right: gh release edit ${VERSION} --draft=false"
  exit 0
fi

# The dispatch returns before the run is queued, so the id has to be polled for.
echo -n "Waiting for the run to appear"
run_id=""
for _ in $(seq 1 30); do
  run_id=$(gh run list --workflow="$WORKFLOW" --branch "$BRANCH" --event workflow_dispatch \
    --limit 1 --json databaseId --jq '.[0].databaseId // empty')
  [ -n "$run_id" ] && break
  echo -n "."
  sleep 2
done
echo

if [ -z "$run_id" ]; then
  echo "Could not find the run — check https://github.com/$(gh repo view --json nameWithOwner --jq .nameWithOwner)/actions"
  exit 1
fi

gh run watch "$run_id" --exit-status || die "the release-binaries run failed — inspect it with 'gh run view ${run_id} --log-failed'"

echo
echo "Draft release ${VERSION} is filled. Review it, write the release notes, then publish:"
echo
echo "  gh release view ${VERSION} --web"
echo "  gh release edit ${VERSION} --draft=false"
echo
echo "Publishing triggers docker-release.yml and freezes the assets."
