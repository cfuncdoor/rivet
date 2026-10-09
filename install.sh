#!/usr/bin/env sh
# Install the latest rivet release for Linux (x86_64).
# Usage: curl -fsSL https://raw.githubusercontent.com/<org>/rivet/main/install.sh | sh
set -eu

REPO="${RIVET_REPO:-<org>/rivet}"
VERSION="${RIVET_VERSION:-latest}"
DEST="${DEST:-$HOME/.rivet/bin}"
OS="unknown-linux-musl"
ARCH="$(uname -m)"

case "$ARCH" in
  x86_64|amd64) ARCH="x86_64" ;;
  aarch64|arm64) ARCH="aarch64" ;;
  *) echo "unsupported architecture: $ARCH" >&2; exit 1 ;;
esac

if [ "$VERSION" = "latest" ]; then
  URL="https://github.com/$REPO/releases/latest/download/rivet-$ARCH-$OS.tar.gz"
else
  URL="https://github.com/$REPO/releases/download/$VERSION/rivet-$ARCH-$OS.tar.gz"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM
echo "downloading $URL"
curl -fsSL "$URL" -o "$tmp/rivet.tar.gz" || wget -qO "$tmp/rivet.tar.gz" "$URL"
tar -xzf "$tmp/rivet.tar.gz" -C "$tmp"
mkdir -p "$DEST"
mv "$tmp/rivet" "$DEST/rivet"
chmod +x "$DEST/rivet"
echo "installed rivet to $DEST/rivet"
case ":$PATH:" in
  *":$DEST:"*) ;;
  *) echo "note: $DEST is not on PATH; add: export PATH=\"\$DEST:\$PATH\"" ;;
esac
