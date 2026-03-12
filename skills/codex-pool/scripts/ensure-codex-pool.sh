#!/bin/sh
set -eu

REPO="lilong7676/codex-pool"
if [ "${INSTALL_DIR+x}" = "x" ]; then
  INSTALL_DIR_WAS_SET=1
else
  INSTALL_DIR_WAS_SET=0
  INSTALL_DIR="$HOME/.local/bin"
fi
VERSION="${VERSION:-v0.1.2}"
BIN_NAME="codex-pool"
APPROVAL="${CODEX_POOL_INSTALL_APPROVED:-}"
TARGET_VERSION="${VERSION#v}"
ACTION="Install"
CURRENT_BIN=""
CURRENT_VERSION=""

if CURRENT_BIN="$(command -v "$BIN_NAME" 2>/dev/null)"; then
  CURRENT_VERSION="$("$CURRENT_BIN" --version 2>/dev/null | awk '{print $NF}' | sed 's/^v//')"
  if [ "$INSTALL_DIR_WAS_SET" -eq 0 ]; then
    INSTALL_DIR="$(dirname "$CURRENT_BIN")"
  fi
  if [ -n "$CURRENT_VERSION" ] && [ "$CURRENT_VERSION" = "$TARGET_VERSION" ]; then
    exit 0
  fi
  ACTION="Update"
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
  printf 'https://github.com/%s/releases/download/%s/%s\n' "$REPO" "$VERSION" "$archive"
}

target="$(detect_target)"
url="$(resolve_url "$target")"
archive="${BIN_NAME}-${target}.tar.gz"
checksum_url="${url}.sha256"

tmp_dir="$(mktemp -d)"
cleanup() {
  rm -rf "$tmp_dir"
}
trap cleanup EXIT INT TERM

confirm_install() {
  if [ "$ACTION" = "Update" ] && [ -n "$CURRENT_VERSION" ]; then
    prompt="Update ${BIN_NAME} from ${CURRENT_VERSION} to ${VERSION} in ${INSTALL_DIR}/${BIN_NAME}? This will download release assets and write to disk. [y/N] "
  elif [ "$ACTION" = "Update" ]; then
    prompt="Update ${BIN_NAME} to ${VERSION} in ${INSTALL_DIR}/${BIN_NAME}? This will download release assets and write to disk. [y/N] "
  else
    prompt="Install ${BIN_NAME} ${VERSION} from https://github.com/${REPO}/releases into ${INSTALL_DIR}/${BIN_NAME}? This will download release assets and write to disk. [y/N] "
  fi

  if [ "$APPROVAL" = "1" ]; then
    return 0
  fi

  if [ -t 0 ] && [ -t 1 ] && [ -r /dev/tty ]; then
    printf '%s' "$prompt" > /dev/tty
    read ans < /dev/tty || ans="n"
    case "$ans" in
      y|Y|yes|YES)
        return 0
        ;;
    esac
  fi

  echo "Installation cancelled. Re-run after explicit confirmation." >&2
  exit 1
}

verify_archive() {
  archive_path="$1"
  checksum_path="$2"

  if command -v sha256sum >/dev/null 2>&1; then
    (cd "$tmp_dir" && sha256sum -c "$(basename "$checksum_path")")
    return 0
  fi

  if command -v shasum >/dev/null 2>&1; then
    (cd "$tmp_dir" && shasum -a 256 -c "$(basename "$checksum_path")")
    return 0
  fi

  echo "Missing checksum verifier: need sha256sum or shasum." >&2
  exit 1
}

action_label() {
  case "$ACTION" in
    Update) printf 'Updating' ;;
    *) printf 'Installing' ;;
  esac
}

confirm_install
mkdir -p "$INSTALL_DIR"

echo "$(action_label) $BIN_NAME for $target from GitHub Releases"
curl -fsSL "$url" -o "$tmp_dir/$archive"
curl -fsSL "$checksum_url" -o "$tmp_dir/${archive}.sha256"
verify_archive "$tmp_dir/$archive" "$tmp_dir/${archive}.sha256"
tar -xzf "$tmp_dir/$archive" -C "$tmp_dir"
install -m 0755 "$tmp_dir/$BIN_NAME" "$INSTALL_DIR/$BIN_NAME"

echo "Installed to $INSTALL_DIR/$BIN_NAME"
