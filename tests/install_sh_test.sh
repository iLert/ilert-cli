#!/usr/bin/env bash
#
# Tests for the helper functions in install.sh.
#
# install.sh is sourced with ILERT_INSTALL_SH_LIB_ONLY=1, which stops it before
# it downloads or installs anything, so every case here exercises the real
# functions rather than a copy of them. `gh` is replaced by a stub on PATH to
# drive the attestation cases, since the outcomes that matter (no gh, an old
# gh, a failing attestation) cannot be produced on demand with the real one.
#
# Run with: bash tests/install_sh_test.sh

set -uo pipefail

REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
INSTALL_SH="${REPO_ROOT}/install.sh"

WORK_DIR=$(mktemp -d)
trap 'rm -rf -- "$WORK_DIR"' EXIT

PASSED=0
FAILED=0

# Report one assertion. Kept explicit rather than pulling in a framework: this
# file has to run from a bare shell on any machine that can run the installer.
check() {
  local name="$1"
  local expected="$2"
  local actual="$3"

  if [ "$expected" = "$actual" ]; then
    PASSED=$((PASSED + 1))
    printf '  ok   %s\n' "$name"
  else
    FAILED=$((FAILED + 1))
    printf '  FAIL %s\n' "$name"
    printf '         expected: %s\n' "$expected"
    printf '         actual:   %s\n' "$actual"
  fi
}

check_contains() {
  local name="$1"
  local needle="$2"
  local haystack="$3"

  if printf '%s' "$haystack" | grep -qF -- "$needle"; then
    PASSED=$((PASSED + 1))
    printf '  ok   %s\n' "$name"
  else
    FAILED=$((FAILED + 1))
    printf '  FAIL %s\n' "$name"
    printf '         expected output to contain: %s\n' "$needle"
    printf '         actual output: %s\n' "$haystack"
  fi
}

# Build a directory holding a `gh` stub with the given behaviour, to be placed
# at the front of PATH.
#
#   verify_asset_supported : whether `gh release verify-asset --help` succeeds
#   authenticated          : whether `gh auth status` succeeds
#   verify_exit            : exit status of the actual verify-asset call
make_gh_stub() {
  local dir="$1"
  local verify_asset_supported="$2"
  local authenticated="$3"
  local verify_exit="$4"

  mkdir -p "$dir"
  cat > "${dir}/gh" <<STUB
#!/usr/bin/env bash
if [ "\$1" = "auth" ] && [ "\$2" = "status" ]; then
  [ "${authenticated}" = "yes" ] && exit 0 || exit 1
fi
if [ "\$1" = "release" ] && [ "\$2" = "verify-asset" ]; then
  if [ "\$3" = "--help" ]; then
    [ "${verify_asset_supported}" = "yes" ] && exit 0 || exit 1
  fi
  echo "gh stub: verify-asset \$3 \$4"
  exit ${verify_exit}
fi
exit 1
STUB
  chmod 755 "${dir}/gh"
}

# Run verify_attestation in a subshell with a controlled PATH, capturing both
# its output and its exit status. `exit 1` inside the function has to terminate
# the installer, so it is checked as a process exit and not a return value.
run_verify_attestation() {
  local path_prefix="$1"
  local require="$2"
  local target_file="$3"

  local out
  out=$(
    PATH="${path_prefix}:${MINIMAL_PATH}" \
    ILERT_REQUIRE_ATTESTATION="$require" \
    ILERT_INSTALL_SH_LIB_ONLY=1 \
    bash -c '
      . "$1" || exit 99
      VERSION="1.2.3"
      verify_attestation "$2"
      echo "REACHED_INSTALL"
    ' _ "$INSTALL_SH" "$target_file" 2>&1
  )
  local rc=$?
  printf '%s\n__rc=%s' "$out" "$rc"
}

# A PATH with the ordinary tools but deliberately without gh, so the "no gh"
# case is real rather than simulated.
MINIMAL_PATH="${WORK_DIR}/bin"
mkdir -p "$MINIMAL_PATH"
for tool in bash awk grep sed cat chmod mktemp uname printf command sha256sum shasum openssl; do
  tool_path=$(command -v "$tool" 2>/dev/null) || continue
  ln -sf "$tool_path" "${MINIMAL_PATH}/${tool}" 2>/dev/null || true
done
if command -v gh >/dev/null 2>&1; then
  # Guard against the host's gh leaking in through a symlink above.
  rm -f "${MINIMAL_PATH}/gh"
fi

ASSET_FILE="${WORK_DIR}/ilert_linux"
printf 'binary payload' > "$ASSET_FILE"

echo "verify_attestation"

# 1. gh is not installed at all: opportunistic, so it reports and continues.
result=$(run_verify_attestation "${WORK_DIR}/empty" "0" "$ASSET_FILE")
check "no gh: continues to install" "0" "${result##*__rc=}"
check_contains "no gh: says why it skipped" "the GitHub CLI (gh) is not installed" "$result"
check_contains "no gh: reaches installation" "REACHED_INSTALL" "$result"

# 2. gh exists but predates `gh release verify-asset`.
make_gh_stub "${WORK_DIR}/old" "no" "yes" "0"
result=$(run_verify_attestation "${WORK_DIR}/old" "0" "$ASSET_FILE")
check "old gh: continues to install" "0" "${result##*__rc=}"
check_contains "old gh: names the missing command" "no 'gh release verify-asset' command" "$result"
check_contains "old gh: reaches installation" "REACHED_INSTALL" "$result"

# 3. gh present but not logged in.
make_gh_stub "${WORK_DIR}/anon" "yes" "no" "0"
result=$(run_verify_attestation "${WORK_DIR}/anon" "0" "$ASSET_FILE")
check "unauthenticated gh: continues to install" "0" "${result##*__rc=}"
check_contains "unauthenticated gh: says why" "gh is not authenticated against github.com" "$result"

# 4. Attestation verifies.
make_gh_stub "${WORK_DIR}/good" "yes" "yes" "0"
result=$(run_verify_attestation "${WORK_DIR}/good" "0" "$ASSET_FILE")
check "successful attestation: continues to install" "0" "${result##*__rc=}"
check_contains "successful attestation: announces the check" "Verifying release attestation" "$result"
check_contains "successful attestation: reaches installation" "REACHED_INSTALL" "$result"

# 5. Attestation runs and fails: never fall back to installing anyway.
make_gh_stub "${WORK_DIR}/bad" "yes" "yes" "1"
result=$(run_verify_attestation "${WORK_DIR}/bad" "0" "$ASSET_FILE")
check "failed attestation: aborts" "1" "${result##*__rc=}"
check_contains "failed attestation: names the asset" "ilert_linux" "$result"
if printf '%s' "$result" | grep -qF "REACHED_INSTALL"; then
  FAILED=$((FAILED + 1))
  printf '  FAIL failed attestation: must not reach installation\n'
else
  PASSED=$((PASSED + 1))
  printf '  ok   failed attestation: does not reach installation\n'
fi

# 6. Required mode turns an unavailable check into a hard failure.
result=$(run_verify_attestation "${WORK_DIR}/empty" "1" "$ASSET_FILE")
check "required mode without gh: aborts" "1" "${result##*__rc=}"
check_contains "required mode without gh: explains" "ILERT_REQUIRE_ATTESTATION=1" "$result"
if printf '%s' "$result" | grep -qF "REACHED_INSTALL"; then
  FAILED=$((FAILED + 1))
  printf '  FAIL required mode without gh: must not reach installation\n'
else
  PASSED=$((PASSED + 1))
  printf '  ok   required mode without gh: does not reach installation\n'
fi

# Required mode is satisfied when the check can actually run.
result=$(run_verify_attestation "${WORK_DIR}/good" "1" "$ASSET_FILE")
check "required mode with working gh: installs" "0" "${result##*__rc=}"

echo
echo "verify_checksum"

SUMS_DIR="${WORK_DIR}/sums"
mkdir -p "$SUMS_DIR"
printf 'arm32 payload' > "${SUMS_DIR}/ilert_arm"
printf 'arm64 payload' > "${SUMS_DIR}/ilert_arm64"
printf 'linux payload' > "${SUMS_DIR}/ilert_linux"
printf 'tampered'      > "${SUMS_DIR}/corrupt"
(
  cd "$SUMS_DIR"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum ilert_arm ilert_arm64 ilert_linux > SHA256SUMS
  else
    shasum -a 256 ilert_arm ilert_arm64 ilert_linux > SHA256SUMS
  fi
)

run_verify_checksum() {
  local file="$1"
  local asset="$2"

  local out
  out=$(
    ILERT_INSTALL_SH_LIB_ONLY=1 bash -c '
      . "$1" || exit 99
      VERSION="1.2.3"
      verify_checksum "$2" "$3" "$4"
      echo "CHECKSUM_OK"
    ' _ "$INSTALL_SH" "$file" "$asset" "${SUMS_DIR}/SHA256SUMS" 2>&1
  )
  local rc=$?
  printf '%s\n__rc=%s' "$out" "$rc"
}

result=$(run_verify_checksum "${SUMS_DIR}/ilert_linux" "ilert_linux")
check "matching checksum: passes" "0" "${result##*__rc=}"

# `ilert_arm` is a prefix of `ilert_arm64`, so a sloppy match would let one
# asset be verified against the other's digest.
result=$(run_verify_checksum "${SUMS_DIR}/ilert_arm64" "ilert_arm64")
check "arm64 matched against its own line" "0" "${result##*__rc=}"
result=$(run_verify_checksum "${SUMS_DIR}/ilert_arm" "ilert_arm")
check "arm matched against its own line" "0" "${result##*__rc=}"
result=$(run_verify_checksum "${SUMS_DIR}/ilert_arm64" "ilert_arm")
check "arm64 bytes under the arm name: rejected" "1" "${result##*__rc=}"

result=$(run_verify_checksum "${SUMS_DIR}/corrupt" "ilert_linux")
check "corrupted download: rejected" "1" "${result##*__rc=}"
check_contains "corrupted download: shows both digests" "expected" "$result"

result=$(run_verify_checksum "${SUMS_DIR}/ilert_linux" "ilert_mac")
check "asset absent from SHA256SUMS: rejected" "1" "${result##*__rc=}"

# ---------------------------------------------------------------------------

echo
echo "resolve_install_target"

# The target `ilert update` asks for wins over the script's own guess, because
# the guess can name a different ilert than the one being replaced.
run_resolve_target() {
  ILERT_INSTALL_SH_LIB_ONLY=1 ILERT_INSTALL_URI="$1" bash -c '
    . "$1" || exit 99
    resolve_install_target "$2"
  ' _ "$INSTALL_SH" "/usr/local/bin/ilert" 2>&1
}

result=$(run_resolve_target "/opt/custom/ilert")
check "explicit target is used verbatim" "/opt/custom/ilert" "$result"

# Unset, not empty: the fallback path has to keep working for a first install.
result=$(ILERT_INSTALL_SH_LIB_ONLY=1 bash -c '
  . "$1" || exit 99
  PATH="/usr/bin:/bin"
  HOME="/nonexistent-home"
  resolve_install_target "$2"
' _ "$INSTALL_SH" "/usr/local/bin/ilert" 2>&1)
check "no explicit target falls back to the usual choice" "/usr/local/bin/ilert" "$result"

result=$(run_resolve_target "relative/ilert"; printf '__rc=%s' "$?")
check "a relative target is refused" "1" "${result##*__rc=}"
check_contains "a relative target says why" "absolute path" "$result"

result=$(run_resolve_target "$WORK_DIR"; printf '__rc=%s' "$?")
check "a directory target is refused" "1" "${result##*__rc=}"
check_contains "a directory target says why" "is a directory" "$result"

# ---------------------------------------------------------------------------

echo
echo "install_binary"

INSTALL_DIR="${WORK_DIR}/install"
mkdir -p "$INSTALL_DIR"

run_install_binary() {
  ILERT_INSTALL_SH_LIB_ONLY=1 bash -c '
    . "$1" || exit 99
    install_binary "$2" "$3"
  ' _ "$INSTALL_SH" "$1" "$2" 2>&1
}

printf 'new binary' > "${WORK_DIR}/new-binary"
printf 'old binary' > "${INSTALL_DIR}/ilert"
chmod 644 "${INSTALL_DIR}/ilert"

run_install_binary "${WORK_DIR}/new-binary" "${INSTALL_DIR}/ilert" >/dev/null
check "replaces the existing binary" "new binary" "$(cat "${INSTALL_DIR}/ilert")"

# The mode is set on the staged file before the swap, so what lands at the
# destination is executable the moment it is reachable — there is no second
# step that can fail and leave it unrunnable.
check "installed binary is executable" "yes" "$([ -x "${INSTALL_DIR}/ilert" ] && echo yes || echo no)"

# Nothing may be left beside the destination: a stray `ilert.update.<pid>` in
# /usr/local/bin is both confusing and, being executable, worth avoiding.
leftovers=$(find "$INSTALL_DIR" -name 'ilert.update.*' | wc -l | tr -d ' ')
check "no staged file is left behind" "0" "$leftovers"

# The source is left alone, so the caller can still verify or reuse it.
check "the downloaded file is not consumed" "new binary" "$(cat "${WORK_DIR}/new-binary")"

echo
printf '%s passed, %s failed\n' "$PASSED" "$FAILED"
[ "$FAILED" -eq 0 ]
