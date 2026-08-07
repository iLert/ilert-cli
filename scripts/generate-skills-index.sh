#!/usr/bin/env bash
#
# Regenerate skills/index.json from the SKILL.md frontmatter in skills/*/.
#
#   scripts/generate-skills-index.sh          rewrite skills/index.json
#   scripts/generate-skills-index.sh --check   exit non-zero if it is out of date
#
# The manifest is committed (and compiled into the binary) rather than derived at
# runtime, so `ilert skills list` needs no network and no GitHub API quota. CI
# runs --check so the two can never drift.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
skills_dir="$repo_root/skills"
index_file="$skills_dir/index.json"

check_only=0
if [ "${1:-}" = "--check" ]; then
  check_only=1
elif [ "$#" -gt 0 ]; then
  echo "usage: $(basename "$0") [--check]" >&2
  exit 64
fi

die() {
  echo "error: $*" >&2
  exit 1
}

# Escape a value for embedding in a JSON string. Frontmatter is single-line, so
# backslash and double quote are the only characters that need handling.
json_escape() {
  printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'
}

# Read one key out of the YAML frontmatter block at the top of a file.
frontmatter_field() {
  awk -v want="$2" '
    NR == 1 { if ($0 != "---") exit 1; next }
    $0 == "---" { exit }
    {
      i = index($0, ":")
      if (i == 0) next
      key = substr($0, 1, i - 1)
      val = substr($0, i + 1)
      gsub(/^[ \t]+|[ \t]+$/, "", key)
      gsub(/^[ \t]+|[ \t]+$/, "", val)
      if (key == want) { print val; exit }
    }
  ' "$1"
}

[ -d "$skills_dir" ] || die "no skills/ directory at $skills_dir"

entries=""
count=0

# LC_ALL=C keeps the ordering stable across machines.
while IFS= read -r skill_file; do
  dir="$(basename "$(dirname "$skill_file")")"

  name="$(frontmatter_field "$skill_file" name || true)"
  description="$(frontmatter_field "$skill_file" description || true)"

  [ -n "$name" ] || die "$skill_file: frontmatter is missing a 'name'"
  [ -n "$description" ] || die "$skill_file: frontmatter is missing a 'description'"

  case "$name" in
    *[!a-z0-9-]* | -* | *-) die "$skill_file: name '$name' must be lowercase letters, digits and hyphens" ;;
  esac

  [ "$name" = "$dir" ] || die "$skill_file: name '$name' does not match its directory '$dir'"

  if [ -n "$entries" ]; then
    entries="$entries,"$'\n'
  fi
  entries="$entries    {
      \"name\": \"$(json_escape "$name")\",
      \"description\": \"$(json_escape "$description")\",
      \"path\": \"$(json_escape "$dir/SKILL.md")\"
    }"
  count=$((count + 1))
done < <(LC_ALL=C find "$skills_dir" -mindepth 2 -maxdepth 2 -name SKILL.md | LC_ALL=C sort)

[ "$count" -gt 0 ] || die "found no skills/*/SKILL.md files"

generated="{
  \"version\": 1,
  \"skills\": [
$entries
  ]
}"

if [ "$check_only" -eq 1 ]; then
  [ -f "$index_file" ] || die "skills/index.json is missing — run $(basename "$0")"
  if ! printf '%s\n' "$generated" | diff -u "$index_file" - >/dev/null; then
    echo "error: skills/index.json is out of date; run scripts/generate-skills-index.sh" >&2
    printf '%s\n' "$generated" | diff -u "$index_file" - || true
    exit 1
  fi
  echo "skills/index.json is up to date ($count skills)"
else
  printf '%s\n' "$generated" > "$index_file"
  echo "wrote skills/index.json ($count skills)"
fi
