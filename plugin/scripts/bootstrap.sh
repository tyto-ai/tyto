#!/usr/bin/env bash
# Bootstrap script for the memso Claude Code plugin.
# Runs on SessionStart. Downloads the correct memso binary for the current
# platform if not already installed or if the version is outdated.
# Always exits 0 so a failed download does not block the session.
#
# Versioning:
#   MEMSO_VERSION - the memso binary release to download from GitHub Releases.
#                   Bump when cutting a new memso release.
#   PLUGIN_VERSION - revision of the plugin files (hooks, scripts, manifests).
#                    Bump for plugin-only fixes that don't require a new binary.
#   The version file stores the composite MEMSO_VERSION-PLUGIN_VERSION.
#   A PLUGIN_VERSION-only bump updates the version file without re-downloading.
set -uo pipefail

MEMSO_VERSION="0.1.0"
PLUGIN_VERSION="1"
COMPOSITE="${MEMSO_VERSION}-${PLUGIN_VERSION}"

BINARY="${CLAUDE_PLUGIN_DATA}/memso"
VERSION_FILE="${CLAUDE_PLUGIN_DATA}/version"
REPO="beefsack/memso"

# Allow an explicit binary override - useful in development or for custom builds.
# Set MEMSO_BINARY_OVERRIDE to an absolute path to skip the GitHub download entirely.
if [[ -n "${MEMSO_BINARY_OVERRIDE:-}" ]]; then
  if [[ ! -f "${MEMSO_BINARY_OVERRIDE}" ]]; then
    echo "[memso] MEMSO_BINARY_OVERRIDE is set but '${MEMSO_BINARY_OVERRIDE}' does not exist" >&2
    exit 0
  fi
  mkdir -p "${CLAUDE_PLUGIN_DATA}"
  cp -f "${MEMSO_BINARY_OVERRIDE}" "${BINARY}"
  chmod +x "${BINARY}"
  printf '%s' "${COMPOSITE}" > "${VERSION_FILE}"
  exit 0
fi

# Detect OS and architecture.
OS="$(uname -s)"
ARCH="$(uname -m)"

case "${OS}" in
  Linux)
    case "${ARCH}" in
      x86_64)          ARTIFACT="memso-linux-x86_64.tar.gz" ;;
      aarch64 | arm64) ARTIFACT="memso-linux-aarch64.tar.gz" ;;
      *) echo "[memso] Unsupported architecture: ${ARCH}" >&2; exit 0 ;;
    esac
    ;;
  Darwin)
    case "${ARCH}" in
      arm64)  ARTIFACT="memso-macos-aarch64.tar.gz" ;;
      x86_64) ARTIFACT="memso-macos-x86_64.tar.gz" ;;
      *) echo "[memso] Unsupported architecture: macOS ${ARCH}" >&2; exit 0 ;;
    esac
    ;;
  MINGW* | MSYS* | CYGWIN*)
    # Windows via Git Bash - Git Bash is required for Claude Code hooks on Windows
    ARTIFACT="memso-windows-x86_64.zip"
    BINARY="${BINARY}.exe"
    ;;
  *)
    echo "[memso] Unsupported OS: ${OS}" >&2
    exit 0
    ;;
esac

# Check what is currently installed.
if [[ -f "${VERSION_FILE}" ]]; then
  installed="$(cat "${VERSION_FILE}")"

  if [[ "${installed}" == "${COMPOSITE}" ]]; then
    # Already up to date.
    exit 0
  fi

  installed_memso="${installed%%-*}"
  if [[ "${installed_memso}" == "${MEMSO_VERSION}" && -f "${BINARY}" ]]; then
    # Only the plugin revision changed - no binary download needed.
    printf '%s' "${COMPOSITE}" > "${VERSION_FILE}"
    exit 0
  fi
fi

# Download and extract the memso archive.
URL="https://github.com/${REPO}/releases/download/v${MEMSO_VERSION}/${ARTIFACT}"
echo "[memso] Downloading ${ARTIFACT} v${MEMSO_VERSION}..." >&2

mkdir -p "${CLAUDE_PLUGIN_DATA}"
ARCHIVE="${CLAUDE_PLUGIN_DATA}/${ARTIFACT}"

if ! curl -fsSL "${URL}" -o "${ARCHIVE}"; then
  echo "[memso] Download failed: ${URL}" >&2
  exit 0
fi

case "${ARTIFACT}" in
  *.tar.gz)
    tar xzf "${ARCHIVE}" -C "${CLAUDE_PLUGIN_DATA}"
    rm -f "${ARCHIVE}"
    chmod +x "${BINARY}"
    ;;
  *.zip)
    unzip -o "${ARCHIVE}" -d "${CLAUDE_PLUGIN_DATA}"
    rm -f "${ARCHIVE}"
    ;;
esac

printf '%s' "${COMPOSITE}" > "${VERSION_FILE}"

echo "[memso] Installed v${COMPOSITE} to ${BINARY}" >&2
