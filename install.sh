#!/usr/bin/env bash
set -euo pipefail

REPO="MorseWayne/claude-proxy"
INSTALL_DIR="${CLP_INSTALL_DIR:-$HOME/.local/bin}"
SERVICE_BINARY=""
SERVICE_WAS_RUNNING=0

detect_platform() {
    local arch os
    arch="$(uname -m)"
    os="$(uname -s)"

    case "$os" in
        Linux)
            case "$arch" in
                x86_64)  echo "x86_64-unknown-linux-musl" ;;
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

find_existing_binary() {
    if [[ -x "${INSTALL_DIR}/claude-proxy" ]]; then
        echo "${INSTALL_DIR}/claude-proxy"
    elif command -v claude-proxy >/dev/null 2>&1; then
        command -v claude-proxy
    fi
}

daemon_is_running() {
    local binary="$1"
    local status

    if [[ -z "$binary" ]]; then
        return 1
    fi

    status="$("$binary" server status 2>/dev/null || true)"
    grep -q "claude-proxy is running" <<<"$status"
}

confirm_continue() {
    local answer

    if [[ ! -r /dev/tty ]]; then
        echo "No interactive terminal is available to confirm stopping claude-proxy." >&2
        return 1
    fi

    printf "Continue? [y/N] " >/dev/tty
    IFS= read -r answer </dev/tty || return 1

    case "$answer" in
        y|Y|yes|YES|Yes) return 0 ;;
        *) return 1 ;;
    esac
}

prepare_existing_service() {
    SERVICE_BINARY="$(find_existing_binary || true)"

    if daemon_is_running "$SERVICE_BINARY"; then
        echo "A running claude-proxy daemon was detected."
        echo "The installer will stop it before replacing the binary and restart it afterward."
        if ! confirm_continue; then
            echo "Installation cancelled."
            exit 1
        fi
        SERVICE_WAS_RUNNING=1
    fi
}

stop_existing_service() {
    if [[ "$SERVICE_WAS_RUNNING" -ne 1 ]]; then
        return
    fi

    if daemon_is_running "$SERVICE_BINARY"; then
        echo "Stopping existing claude-proxy daemon..."
        "$SERVICE_BINARY" server stop
    else
        echo "claude-proxy daemon is no longer running."
    fi

    if daemon_is_running "$SERVICE_BINARY"; then
        echo "Failed to stop the existing claude-proxy daemon." >&2
        exit 1
    fi
}

restart_existing_service() {
    if [[ "$SERVICE_WAS_RUNNING" -ne 1 ]]; then
        return
    fi

    echo "Restarting claude-proxy daemon..."
    "${INSTALL_DIR}/claude-proxy" server start --daemon
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

    prepare_existing_service

    echo "Downloading claude-proxy for ${platform}..."
    echo "  URL: ${url}"

    TMP_DIR="$(mktemp -d)"

    curl -fsSL "$url" -o "${TMP_DIR}/${archive}"
    tar xzf "${TMP_DIR}/${archive}" -C "$TMP_DIR"

    stop_existing_service

    mkdir -p "$INSTALL_DIR"
    mv "${TMP_DIR}/claude-proxy" "${INSTALL_DIR}/claude-proxy"
    chmod +x "${INSTALL_DIR}/claude-proxy"

    restart_existing_service

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
