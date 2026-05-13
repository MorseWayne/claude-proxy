#!/usr/bin/env bash
set -euo pipefail

REPO="MorseWayne/claude-proxy"
INSTALL_DIR="${CLP_INSTALL_DIR:-$HOME/.local/bin}"

detect_platform() {
    local arch os
    arch="$(uname -m)"
    os="$(uname -s)"

    case "$os" in
        Linux)
            case "$arch" in
                x86_64)  echo "x86_64-unknown-linux-gnu" ;;
                aarch64) echo "aarch64-unknown-linux-gnu" ;;
                *)       echo "Unsupported architecture: $arch" >&2; exit 1 ;;
            esac
            ;;
        Darwin)
            case "$arch" in
                x86_64)  echo "x86_64-apple-darwin" ;;
                arm64)   echo "aarch64-apple-darwin" ;;
                *)       echo "Unsupported architecture: $arch" >&2; exit 1 ;;
            esac
            ;;
        *)
            echo "Unsupported OS: $os. Download manually from GitHub Releases." >&2
            exit 1
            ;;
    esac
}

TMP_DIR=""
cleanup() {
    if [[ -n "${TMP_DIR:-}" ]]; then
        rm -rf "$TMP_DIR"
    fi
}
trap cleanup EXIT

main() {
    local platform
    platform="$(detect_platform)"
    local archive="claude-proxy-${platform}.tar.gz"
    local url="https://github.com/${REPO}/releases/latest/download/${archive}"

    echo "Downloading claude-proxy for ${platform}..."
    echo "  URL: ${url}"

    TMP_DIR="$(mktemp -d)"

    curl -fsSL "$url" -o "${TMP_DIR}/${archive}"
    tar xzf "${TMP_DIR}/${archive}" -C "$TMP_DIR"

    mkdir -p "$INSTALL_DIR"
    mv "${TMP_DIR}/claude-proxy" "${INSTALL_DIR}/claude-proxy"
    chmod +x "${INSTALL_DIR}/claude-proxy"

    echo ""
    echo "Installed to ${INSTALL_DIR}/claude-proxy"
    echo ""

    # Check if INSTALL_DIR is in PATH
    if ! echo "$PATH" | tr ':' '\n' | grep -qx "$INSTALL_DIR"; then
        echo "NOTE: ${INSTALL_DIR} is not in your PATH."
        echo "Add it with:"
        echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
        echo ""
    fi

    "${INSTALL_DIR}/claude-proxy" --version
}

main "$@"
