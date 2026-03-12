#!/bin/sh
set -eu

REPO="lilong7676/codex-pool"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${VERSION:-latest}"
BIN_NAME="codex-pool"

if command -v "$BIN_NAME" >/dev/null 2>&1; then
  exit 0
fi

detect_target() {
  os="$(uname -s)"
  arch="$(uname -m)"

  case "$os" in
    Darwin) os_part="apple-darwin" ;;
    Linux) os_part="unknown-linux-gnu" ;;
    *)
      echo "Unsupported OS: $os" >&2
      exit 1
      ;;
  esac

  case "$arch" in
    arm64|aarch64) arch_part="aarch64" ;;
    x86_64|amd64) arch_part="x86_64" ;;
    *)
      echo "Unsupported architecture: $arch" >&2
      exit 1
      ;;
  esac

  if [ "$os_part" = "unknown-linux-gnu" ] && [ "$arch_part" != "x86_64" ]; then
    echo "Only x86_64 Linux builds are published right now." >&2
    exit 1
  fi

  printf '%s-%s\n' "$arch_part" "$os_part"
}

resolve_url() {
  target="$1"
  archive="${BIN_NAME}-${target}.tar.gz"

  if [ "$VERSION" = "latest" ]; then
    printf 'https://github.com/%s/releases/latest/download/%s\n' "$REPO" "$archive"
  else
    printf 'https://github.com/%s/releases/download/%s/%s\n' "$REPO" "$VERSION" "$archive"
  fi
}

target="$(detect_target)"
url="$(resolve_url "$target")"

tmp_dir="$(mktemp -d)"
cleanup() {
  rm -rf "$tmp_dir"
}
trap cleanup EXIT INT TERM

mkdir -p "$INSTALL_DIR"

echo "Downloading $BIN_NAME for $target"
curl -fsSL "$url" -o "$tmp_dir/${BIN_NAME}.tar.gz"
tar -xzf "$tmp_dir/${BIN_NAME}.tar.gz" -C "$tmp_dir"
install -m 0755 "$tmp_dir/$BIN_NAME" "$INSTALL_DIR/$BIN_NAME"

echo "Installed to $INSTALL_DIR/$BIN_NAME"
