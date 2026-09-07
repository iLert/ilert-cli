#!/usr/bin/env bash
set -e
if [ -z "${DEBUG}" ]; then
  set +o xtrace
else
  set -o xtrace
fi

REPO="iLert/ilert-cli"

curl_fetch_headers() {
  curl --silent --show-error --location --head --connect-timeout 10 --max-time 30 "$1"
}

curl_download_file() {
  local url="$1"
  local output_path="$2"
  local download_connect_timeout=10
  local download_max_time=600
  local download_retry_count=2
  local download_retry_delay=2
  local download_retry_max_time=620

  if [ -t 2 ]; then
    curl --show-error --location --fail --connect-timeout "$download_connect_timeout" --max-time "$download_max_time" --retry "$download_retry_count" --retry-delay "$download_retry_delay" --retry-max-time "$download_retry_max_time" "$url" --output "$output_path"
  else
    curl --silent --show-error --location --fail --connect-timeout "$download_connect_timeout" --max-time "$download_max_time" --retry "$download_retry_count" --retry-delay "$download_retry_delay" --retry-max-time "$download_retry_max_time" "$url" --output "$output_path"
  fi
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  elif command -v openssl >/dev/null 2>&1; then
    openssl dgst -sha256 "$1" | awk '{print $NF}'
  else
    return 1
  fi
}

# Check a downloaded file against the SHA256SUMS asset published alongside it.
#
# This step is mandatory. The binary and the sums file come from the same host
# over the same transport, so on its own it is not a defence against a
# compromised release — it catches a truncated, corrupted or proxy-mangled
# download, and it fails loudly if the release ever serves bytes that differ
# from the ones it published. `verify_attestation` adds GitHub's own signature
# over the release contents, when it can run.
verify_checksum() {
  local file="$1"
  local asset="$2"
  local sums_file="$3"

  # Matched on the whole field: a prefix match would let the `ilert_arm64` line
  # satisfy a request for `ilert_arm`. The `*name` form is what sha256sum
  # writes for a file it read in binary mode.
  local expected
  expected=$(awk -v want="$asset" '$2 == want || $2 == "*" want { print $1; exit }' "$sums_file")
  if [ -z "$expected" ]; then
    echo "Release ${VERSION} publishes no checksum for '${asset}'; refusing to install an unverified binary."
    exit 1
  fi

  local actual
  if ! actual=$(sha256_of "$file"); then
    echo "Found no sha256 tool (sha256sum, shasum or openssl); cannot verify the download."
    exit 1
  fi

  if [ "$actual" != "$expected" ]; then
    echo "Checksum mismatch for '${asset}':"
    echo "  expected ${expected}"
    echo "  actual   ${actual}"
    echo "The download is corrupt or has been tampered with. Not installing."
    exit 1
  fi
}

# Check the downloaded asset against GitHub's immutable-release attestation.
#
# An immutable release is signed by GitHub, and `gh release verify-asset` checks
# the file against that signature. What that establishes is membership: these
# exact bytes are the ones this release carries, attested by GitHub rather than
# by a sums file the same host served a moment earlier. It says nothing about
# how the binary was built — not the workflow, not the commit, not the runner.
#
# This check is opportunistic: an old or unauthenticated gh means it cannot
# run, and the mandatory checksum has already passed, so installation continues
# with a note. Set ILERT_REQUIRE_ATTESTATION=1 to make an unavailable check
# fatal too. A check that runs and *fails* is always fatal — there is no
# falling back from that.
verify_attestation() {
  local file="$1"
  local unavailable=""

  if ! command -v gh >/dev/null 2>&1; then
    unavailable="the GitHub CLI (gh) is not installed"
  elif ! gh release verify-asset --help >/dev/null 2>&1; then
    unavailable="this version of gh has no 'gh release verify-asset' command"
  elif ! gh auth status --hostname github.com >/dev/null 2>&1; then
    unavailable="gh is not authenticated against github.com"
  fi

  if [ -n "$unavailable" ]; then
    if [ "${ILERT_REQUIRE_ATTESTATION:-0}" = "1" ]; then
      echo "ILERT_REQUIRE_ATTESTATION=1, but ${unavailable}. Not installing."
      exit 1
    fi
    echo "Skipping release attestation: ${unavailable}."
    echo "The published SHA256 checksum already matched. Set ILERT_REQUIRE_ATTESTATION=1 to require attestation."
    return 0
  fi

  echo "Verifying release attestation.."
  if ! gh release verify-asset "$VERSION" "$file" --repo "$REPO"; then
    echo "Release attestation verification failed for '${file##*/}'. Not installing."
    exit 1
  fi
}

# Prompt user to run a command with sudo; show exact command first.
#
# Takes the command as separate arguments and runs it directly. It used to take
# one string and `eval` it, which meant a quote or a `;` anywhere in a path we
# derived from the environment (TMPDIR, HOME) became a command run as root.
run_with_sudo_prompt() {
  local reason="$1"
  shift
  echo ""
  if [ -n "$reason" ]; then
    echo "$reason"
  fi
  echo "This usually needs administrator privileges."
  echo "The installer can continue by running:"
  # %q so what is shown is what will run, even for an awkward path.
  printf '  sudo'
  printf ' %q' "$@"
  printf '\n'
  read -r -p "Run with sudo? [y/N] " answer </dev/tty
  case "$answer" in
    [yY]|[yY][eE][sS])
      sudo "$@"
      ;;
    *)
      echo "Aborted."
      exit 1
      ;;
  esac
}

# Whether a failed install attempt is something sudo could actually fix.
#
# Everything the swap below does — creating the install directory, writing the
# staged file, renaming it over the destination — needs write *and* search
# permission on the closest directory that already exists, and nothing else. So
# that is what is asked, rather than pattern-matching an error message that
# changes with the platform and the locale.
#
# Offering sudo for a failure it cannot fix is worse than not offering it: a
# fresh macOS has no /usr/local/bin, the resulting "No such file or directory"
# was reported to the user as a permissions problem, and typing a password
# produced the identical error a second time.
install_needs_privileges() {
  local dir="$1"

  # `dirname /` is `/`, which is the only way this terminates when nothing on
  # the path exists. Written as an `if` rather than `[ ... ] && break`, which
  # under `set -e` fails the whole list on the common iteration and takes the
  # script with it.
  while [ ! -e "$dir" ]; do
    local parent
    parent=$(dirname -- "$dir")
    if [ "$parent" = "$dir" ]; then
      break
    fi
    dir="$parent"
  done

  # Not a directory at all (a regular file sitting at /usr/local/bin, say) is
  # also nothing sudo can help with, and the real error says so far better than
  # a password prompt would.
  #
  # Write permission alone is not enough to create an entry in a directory: the
  # search bit has to be there too. A mode-0600 ancestor is writable by `test -w`
  # and still refuses `mkdir` with "Permission denied" — a refusal sudo does fix,
  # so checking only -w would send the user away with a bare error instead.
  if [ ! -d "$dir" ]; then
    return 1
  fi
  [ ! -w "$dir" ] || [ ! -x "$dir" ]
}

# Put the new binary in place.
#
# The last step has to be a rename within one filesystem for the replacement to
# be atomic, so the new binary is staged *beside* the destination rather than
# moved in from the temp dir: /tmp is usually a different filesystem, which
# silently turns `mv` into a copy — and a copy can be interrupted half-written,
# over the binary that is running.
#
# The mode is set on the staged file, before anything can reach it by the real
# name. Setting it afterwards made the install two steps that could each fail
# separately: a chmod that failed after the move left the destination replaced
# but not executable, and the caller was told the install had failed.
#
# After this function returns, either the old binary or the new one is at
# `install_uri`, complete and executable. Never a partial file, never a file
# without +x.
install_binary() {
  local tmp_file="$1"
  local install_uri="$2"
  local staged="${install_uri}.update.$$"
  local install_dir
  install_dir=$(dirname -- "$install_uri")

  # A directory where the binary belongs is not a previous installation, and
  # `mv -f` will not replace it: it moves the staged file *inside* it and
  # reports success. That left an `ilert.update.<pid>` buried in a directory
  # named `ilert`, an install that claimed to have worked, and nothing runnable
  # on PATH. `resolve_install_target` rejects this for an explicit
  # ILERT_INSTALL_URI; the default paths reach here without that check.
  if [ -d "$install_uri" ]; then
    echo "'${install_uri}' is a directory, not the ilert binary."
    echo "Remove it and run the installer again."
    exit 1
  fi

  # `mkdir -p` is part of the swap so that a missing install directory is
  # created by whichever attempt gets that far — including the sudo one, which
  # is the case that matters, since a user who cannot write /usr/local/bin
  # cannot create it either.
  #
  # `sh -c` with the paths passed as positional parameters and never
  # interpolated into the script text, so the code that runs — as root, on the
  # sudo path — is this fixed string whatever the paths happen to contain.
  local swap='mkdir -p -- "$4" && cp -- "$1" "$2" && chmod -- 755 "$2" && mv -f -- "$2" "$3"'

  # The first attempt's stderr is kept rather than discarded: it is the only
  # account of why the install failed, and it is what the user is shown when
  # sudo is not the answer.
  local err
  if err=$(LC_ALL=C sh -c "$swap" sh "$tmp_file" "$staged" "$install_uri" "$install_dir" 2>&1); then
    return 0
  fi

  # A staged file can survive a failure between the copy and the rename. It is
  # removed by its full name, and a failure to remove it is not fatal — the
  # sudo path below may be about to overwrite it anyway.
  rm -f -- "$staged" 2>/dev/null || true

  if ! install_needs_privileges "$install_dir"; then
    echo "Failed to install to '${install_uri}'."
    if [ -n "$err" ]; then
      echo "  ${err}"
    fi
    exit 1
  fi

  if run_with_sudo_prompt "Cannot write to '${install_uri}' with current user permissions." \
    sh -c "$swap" sh "$tmp_file" "$staged" "$install_uri" "$install_dir"; then
    return 0
  fi

  # The privileged attempt can leave a root-owned staged file behind the same
  # way the unprivileged one can, and that one the user cannot delete without
  # sudo of their own. `sudo -n` so a cleanup never turns into a second,
  # unexplained password prompt; if the credentials have already expired the
  # file simply stays, which is what would have happened anyway.
  rm -f -- "$staged" 2>/dev/null || sudo -n rm -f -- "$staged" 2>/dev/null || true
  echo "Failed to install to '${install_uri}' even with administrator privileges."
  exit 1
}

# Prefer ~/.local/bin if it exists in $PATH, creating it if needed
resolve_install_uri() {
  local fallback="$1"
  local local_bin="${HOME}/.local/bin"

  if echo "$PATH" | tr ':' '\n' | grep -qx "$local_bin"; then
    mkdir -p "$local_bin"
    echo "$local_bin/ilert"
  else
    echo "$fallback"
  fi
}

# Which binary this run replaces.
#
# `ilert update` sets ILERT_INSTALL_URI to the path of the executable that is
# actually running, because the guesses below are guesses: a machine can hold
# more than one ilert, and picking a different one would let an update report
# success while the binary the caller invoked stayed exactly as it was.
#
# The path must be absolute — a relative one would resolve against whatever
# directory this script happens to run in — and must not name a directory,
# which would turn the install into a file created *inside* it.
resolve_install_target() {
  local fallback="$1"
  local requested="${ILERT_INSTALL_URI:-}"

  if [ -z "$requested" ]; then
    resolve_install_uri "$fallback"
    return 0
  fi

  case "$requested" in
    /*) ;;
    *)
      echo "ILERT_INSTALL_URI must be an absolute path, got '${requested}'." >&2
      return 1
      ;;
  esac

  if [ -d "$requested" ]; then
    echo "ILERT_INSTALL_URI '${requested}' is a directory, not a file." >&2
    return 1
  fi

  echo "$requested"
}

# Everything above is a function definition; everything below installs. The
# test suite sources this file with ILERT_INSTALL_SH_LIB_ONLY=1 to exercise the
# helpers without downloading or installing anything.
if [ -n "${ILERT_INSTALL_SH_LIB_ONLY:-}" ]; then
  return 0 2>/dev/null || exit 0
fi

VERSION=$(curl_fetch_headers "https://github.com/${REPO}/releases/latest" | grep -i '^location:' | sed 's|.*/||' | tr -d '\r\n')
if [ -z "$VERSION" ]; then
  echo "Failed to determine latest release version."
  exit 1
fi
echo "Installing ilert-cli version ${VERSION}"

# Pick the release asset for this machine.
#
# `uname -m` reports `aarch64` (some kernels: `arm64`) on 64-bit ARM, which does
# not begin with "arm" — so a prefix test for "arm" alone silently handed every
# Graviton, Ampere and 64-bit Raspberry Pi OS host the x86_64 binary. The
# architectures are matched exactly, and an unrecognised one now stops rather
# than falling through to a binary that cannot run.
case "$(uname -s)" in
  Darwin)
    # `ilert_mac` is a universal binary, so Apple Silicon and Intel Macs take
    # the same asset; the loader selects the matching slice.
    PLATFORM_LABEL="MacOS"
    ASSET="ilert_mac"
    INSTALL_URI=$(resolve_install_target "/usr/local/bin/ilert") || exit 1
    ;;
  Linux*)
    case "$(uname -m)" in
      aarch64|arm64)
        PLATFORM_LABEL="Linux ARM64"
        ASSET="ilert_arm64"
        ;;
      armv6l|armv7l|arm)
        PLATFORM_LABEL="Linux ARM"
        ASSET="ilert_arm"
        ;;
      x86_64|amd64)
        PLATFORM_LABEL="Linux"
        ASSET="ilert_linux"
        ;;
      *)
        echo "Unsupported architecture '$(uname -m)', please install manually."
        exit 1
        ;;
    esac
    INSTALL_URI=$(resolve_install_target "/usr/bin/ilert") || exit 1
    ;;
  *)
    echo "Unsupported platform '$(uname -s)', please install manually."
    exit 1
    ;;
esac

TEMP_DIR=$(mktemp -d)
# Kept under the asset's real name: `gh release verify-asset` matches the local
# file against the release asset of the same basename, so downloading to a
# generic "ilert" would leave it with nothing to compare against.
TEMP_FILE="${TEMP_DIR}/${ASSET}"
SUMS_FILE="${TEMP_DIR}/SHA256SUMS"
# The two files this script creates are named explicitly rather than cleaned up
# with `rm -rf "$TEMP_DIR"`, so a surprising value in TEMP_DIR cannot turn into
# a recursive delete.
trap 'rm -f -- "$TEMP_FILE" "$SUMS_FILE"; rmdir "$TEMP_DIR" 2>/dev/null || true' EXIT

BASE_URL="https://github.com/${REPO}/releases/download/${VERSION}"

echo "[${PLATFORM_LABEL}] Downloading binary.. please be patient."
if ! curl_download_file "${BASE_URL}/${ASSET}" "$TEMP_FILE"; then
  echo "Download failed or timed out. Please check your network connection and try again."
  exit 1
fi

echo "Verifying checksum.."
if ! curl_download_file "${BASE_URL}/SHA256SUMS" "$SUMS_FILE"; then
  echo "Release ${VERSION} publishes no SHA256SUMS file; refusing to install an unverified binary."
  exit 1
fi
verify_checksum "$TEMP_FILE" "$ASSET" "$SUMS_FILE"

verify_attestation "$TEMP_FILE"

install_binary "$TEMP_FILE" "$INSTALL_URI"
echo "Done"

# Run the binary that was just installed, by its full path.
#
# This is the line that proves the bytes we wrote actually execute on this
# machine, so it has to run *that* file. A bare `ilert` would go through PATH
# and could easily be a different installation — which would let an update that
# replaced /usr/local/bin/ilert report the version of ~/.local/bin/ilert.
#
# `ilert update` re-runs this script, and the full help dumped over the caller's
# terminal is the wrong ending for an update, so that path prints the version
# the binary now reports instead.
if [ -n "${ILERT_UPDATE:-}" ]; then
  "$INSTALL_URI" version
else
  "$INSTALL_URI" --help
fi
