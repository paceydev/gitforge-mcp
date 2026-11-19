#!/bin/sh
# Install a versioned Linux release after verifying its SHA-256 checksum.
set -eu

repository=https://github.com/paceydev/gitforge-mcp
version=${GITFORGE_VERSION:-0.2.0}
install_dir=${GITFORGE_INSTALL_DIR:-"$HOME/.local/bin"}
case "$version" in
  ''|*[!0-9A-Za-z.+-]*) echo 'Invalid GITFORGE_VERSION' >&2; exit 1 ;;
esac
[ "$(uname -s)" = Linux ] || { echo 'This installer supports Linux. See the release downloads for macOS and Windows.' >&2; exit 1; }
case "$(uname -m)" in
  x86_64|amd64) target=x86_64-unknown-linux-gnu ;;
  aarch64|arm64) target=aarch64-unknown-linux-gnu ;;
  *) echo 'Unsupported CPU architecture' >&2; exit 1 ;;
esac
for tool in curl sha256sum tar mktemp; do
  command -v "$tool" >/dev/null 2>&1 || { echo "Required command missing: $tool" >&2; exit 1; }
done
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT HUP INT TERM
name=gitforge-mcp-$version-$target
archive=$name.tar.gz
base=$repository/releases/download/v$version
curl --proto '=https' --tlsv1.2 -fsSL "$base/$archive" -o "$work/$archive"
curl --proto '=https' --tlsv1.2 -fsSL "$base/SHA256SUMS" -o "$work/SHA256SUMS"
awk -v file="$archive" '$2 == file { print; found++ } END { if (found != 1) exit 1 }' "$work/SHA256SUMS" > "$work/checksum"
(cd "$work" && sha256sum --check checksum)
tar -xzf "$work/$archive" -C "$work"
reported=$("$work/$name/gitforge-mcp" --version)
[ "$reported" = "gitforge-mcp $version" ] || { echo "Unexpected executable version: $reported" >&2; exit 1; }
mkdir -p "$install_dir"
staged=$(mktemp "$install_dir/.gitforge-mcp.XXXXXX")
trap 'rm -rf "$work"; rm -f "$staged"' EXIT HUP INT TERM
cp "$work/$name/gitforge-mcp" "$staged"
chmod 755 "$staged"
mv -f "$staged" "$install_dir/gitforge-mcp"
printf 'Installed %s/gitforge-mcp\nEnsure this directory is on your PATH.\n' "$install_dir"
