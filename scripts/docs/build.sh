#!/usr/bin/env bash
# Builds the documentation site into docs/book/html, and beside the HTML:
#
#   llms.txt       every page, with a link to its markdown and the first
#                  sentence of its introduction (https://llmstxt.org)
#   llms-full.txt  every page in one file, in table-of-contents order
#   <page>.md      each page's markdown, includes expanded
#
# All three come from mdBook's markdown renderer, in the same build as the
# HTML, so they can't disagree with it.
#
# SITE_URL (optional): the site's address, such as https://docs.example.com/.
# With it, llms.txt links are absolute, as the convention prefers; without
# it they are relative to llms.txt, which is fine where it is served.
#
# Needs mdbook (the version in .github/workflows/ci.yml's docs job) and git.
set -euo pipefail
root="$(git rev-parse --show-toplevel)"
docs="$root/docs"
html="$docs/book/html"
markdown="$docs/book/markdown"

rm -rf "$docs/book"
mdbook build "$docs"

base="${SITE_URL:-}"
if [ -n "$base" ]; then
  base="${base%/}/"
fi

# The table of contents, in order, as "section|title|path" lines. The
# introduction comes before any section, so its section is "-". (Not a tab:
# read collapses empty tab-separated fields.)
pages="$(awk '
  BEGIN { section = "-" }
  /^# / && NR > 1 { section = substr($0, 3); next }
  match($0, /\[[^]]*\]\([^)]*\.md\)/) {
    link = substr($0, RSTART, RLENGTH)
    title = link; sub(/^\[/, "", title); sub(/\]\(.*$/, "", title)
    path = link; sub(/^.*\]\(/, "", path); sub(/\)$/, "", path)
    printf "%s|%s|%s\n", section, title, path
  }
' "$docs/src/SUMMARY.md")"

# The first sentence of a page's first paragraph after its title.
describe() {
  awk '
    !started && /^# / { started = 1; next }
    !started { next }
    /^[[:space:]]*$/ { if (text != "") exit; next }
    /^(<|```|\||>|- |[0-9]+\. )/ { if (text != "") exit; next }
    { text = (text == "" ? $0 : text " " $0) }
    END {
      if (match(text, /[.!?]( |$)/)) text = substr(text, 1, RSTART)
      print text
    }
  ' "$1"
}

intro_path="$(printf '%s\n' "$pages" | awk -F'|' '$1 == "-" { print $3; exit }')"
{
  echo "# gitforge-mcp"
  echo
  echo "> $(describe "$markdown/$intro_path")"
  echo
  echo "Each link is the page's markdown. llms-full.txt holds every page in one file."
  current="none"
  while IFS='|' read -r section title path; do
    # The llms.txt convention: links under "Optional" can be skipped when
    # context is short. The development pages are those.
    [ "$section" = "Development" ] && section="Optional"
    [ "$section" = "-" ] && section="Introduction"
    if [ "$section" != "$current" ]; then
      echo
      echo "## $section"
      echo
      current="$section"
    fi
    echo "- [$title]($base$path): $(describe "$markdown/$path")"
  done <<< "$pages"
} > "$html/llms.txt"

{
  first=1
  while IFS='|' read -r _ _ path; do
    [ "$first" = 1 ] || printf '\n\n'
    first=0
    echo "<!-- page: $base$path -->"
    echo
    cat "$markdown/$path"
  done <<< "$pages"
} > "$html/llms-full.txt"

# Each page's markdown beside its HTML: guide/safety.md next to
# guide/safety.html.
(cd "$markdown" && find . -name '*.md' -print) | while read -r file; do
  mkdir -p "$(dirname "$html/$file")"
  cp "$markdown/$file" "$html/$file"
done

echo "built $html: $(printf '%s\n' "$pages" | wc -l | tr -d ' ') pages, llms.txt, llms-full.txt" >&2
