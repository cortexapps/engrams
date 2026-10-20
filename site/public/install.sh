#!/bin/sh
# Install the engrams CLI from a GitHub release.
#
#   curl -fsSL https://engramsfactory.com/install.sh | sh
#
# Environment:
#   ENGRAMS_CHANNEL      stable (default) or nightly. A nightly is the newest
#                        prerelease, versioned <next>-rc.<n>.
#   ENGRAMS_VERSION      an exact version, for example 0.11.0 or 0.12.0-rc.3.
#                        Overrides the channel.
#   ENGRAMS_INSTALL_DIR  where to put the binary. Default: /usr/local/bin when
#                        it is writable, else ~/.local/bin.
#
# The script downloads the binary for this machine and its SHA256SUMS from
# the release, checks the checksum, and copies the binary to the install
# directory as `engrams`. It needs curl or wget, and sha256sum or shasum.
set -eu

REPO="cortexapps/engrams"
API="https://api.github.com/repos/$REPO"
DOWNLOAD="https://github.com/$REPO/releases/download"

say() { printf 'engrams install: %s\n' "$*" >&2; }
die() { say "$*"; exit 1; }

need() {
  command -v "$1" > /dev/null 2>&1
}

fetch() {
  # fetch <url> <file>
  if need curl; then
    curl -fsSL --retry 3 -o "$2" "$1"
  elif need wget; then
    wget -q -O "$2" "$1"
  else
    die "curl or wget is needed."
  fi
}

# The release asset for this machine.
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux) os=linux ;;
  Darwin) os=darwin ;;
  *) die "no build for $os. Windows is not supported; use WSL." ;;
esac
case "$arch" in
  x86_64 | amd64) arch=x64 ;;
  aarch64 | arm64) arch=arm64 ;;
  *) die "no build for $arch." ;;
esac
asset="engrams-$os-$arch"
case "$asset" in
  engrams-linux-x64 | engrams-linux-arm64 | engrams-darwin-arm64) ;;
  *) die "no build for $os $arch. Build it from the repository: cargo build --profile cli -p engram-cli" ;;
esac

# The release: an exact version, else the newest of the channel.
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
channel="${ENGRAMS_CHANNEL:-stable}"
if [ -n "${ENGRAMS_VERSION:-}" ]; then
  tag="v${ENGRAMS_VERSION#v}"
else
  case "$channel" in
    stable)
      # The redirect target of releases/latest is the newest stable release;
      # GitHub never marks a prerelease as latest.
      fetch "$API/releases/latest" "$tmp/release.json"
      tag="$(sed -n 's/^.*"tag_name": *"\([^"]*\)".*$/\1/p' "$tmp/release.json" | head -n 1)"
      ;;
    nightly)
      # The list is newest first; the first rc tag is the newest nightly.
      fetch "$API/releases?per_page=30" "$tmp/releases.json"
      tag="$(tr ',' '\n' < "$tmp/releases.json" \
        | sed -n 's/^.*"tag_name": *"\(v[0-9][^"]*-rc\.[0-9][0-9]*\)".*$/\1/p' | head -n 1)"
      ;;
    *) die "ENGRAMS_CHANNEL must be stable or nightly, not '$channel'." ;;
  esac
fi
[ -n "$tag" ] || die "could not find a $channel release."
version="${tag#v}"

say "downloading $asset $version"
fetch "$DOWNLOAD/$tag/$asset" "$tmp/$asset" || die "no release $tag with the asset $asset."
fetch "$DOWNLOAD/$tag/SHA256SUMS" "$tmp/SHA256SUMS"

# Check the checksum.
want="$(awk -v a="$asset" '$2 == a { print $1 }' "$tmp/SHA256SUMS")"
[ -n "$want" ] || die "SHA256SUMS of $tag has no entry for $asset."
if need sha256sum; then
  have="$(sha256sum "$tmp/$asset" | awk '{ print $1 }')"
elif need shasum; then
  have="$(shasum -a 256 "$tmp/$asset" | awk '{ print $1 }')"
else
  die "sha256sum or shasum is needed."
fi
[ "$have" = "$want" ] || die "checksum mismatch for $asset: got $have, want $want."

# Install.
if [ -n "${ENGRAMS_INSTALL_DIR:-}" ]; then
  dir="$ENGRAMS_INSTALL_DIR"
elif [ -d /usr/local/bin ] && [ -w /usr/local/bin ]; then
  dir=/usr/local/bin
else
  dir="$HOME/.local/bin"
fi
mkdir -p "$dir"
chmod +x "$tmp/$asset"
# A copy and a rename: a running `engrams` keeps its old inode.
cp "$tmp/$asset" "$dir/engrams.tmp.$$"
mv -f "$dir/engrams.tmp.$$" "$dir/engrams"
say "installed $("$dir/engrams" --version) to $dir/engrams"

case ":$PATH:" in
  *":$dir:"*) ;;
  *) say "add $dir to your PATH, for example: export PATH=\"$dir:\$PATH\"" ;;
esac
