#!/usr/bin/env bash
# Bootstrap for the memso Claude Code plugin.
# Runs on SessionStart. Downloads the correct memso binary for the current
# platform if not already installed or if the version is outdated.
# Always exits 0 so a failed download does not block the session.
#
# Channel selection (default: stable):
#   MEMSO_CHANNEL=stable  version-pinned releases from GitHub Releases
#   MEMSO_CHANNEL=dev     rolling builds from the 'dev' pre-release
#
# Stable channel versioning:
#   MEMSO_VERSION  - binary release to download.
#   PLUGIN_VERSION - revision of the plugin files (hooks, scripts, manifests).
#   The version file stores the composite MEMSO_VERSION-PLUGIN_VERSION.
#   A PLUGIN_VERSION-only bump updates the version file without re-downloading.
#
# Dev channel versioning:
#   The version file stores the full commit SHA from dev-version.txt.
#   The binary is re-downloaded only when the remote SHA changes.
set -uo pipefail

MEMSO_VERSION="0.2.0"
PLUGIN_VERSION="1"
COMPOSITE="${MEMSO_VERSION}-${PLUGIN_VERSION}"

BINARY="${CLAUDE_PLUGIN_DATA}/memso"
VERSION_FILE="${CLAUDE_PLUGIN_DATA}/version"
REPO="beefsack/memso"
CHANNEL="${MEMSO_CHANNEL:-stable}"

# Allow an explicit binary override - useful in development or for custom builds.
# Set MEMSO_BINARY_OVERRIDE to an absolute path to skip the GitHub download entirely.
if [[ -n "${MEMSO_BINARY_OVERRIDE:-}" ]]; then
  if [[ ! -f "${MEMSO_BINARY_OVERRIDE}" ]]; then
    echo "[memso] MEMSO_BINARY_OVERRIDE is set but '${MEMSO_BINARY_OVERRIDE}' does not exist" >&2
    exit 0
  fi
  echo "[memso bootstrap] Binary ready (override: ${MEMSO_BINARY_OVERRIDE})"
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
    # Windows via Git Bash - required for Claude Code hooks on Windows.
    ARTIFACT="memso-windows-x86_64.zip"
    BINARY="${BINARY}.exe"
    ;;
  *)
    echo "[memso] Unsupported OS: ${OS}" >&2
    exit 0
    ;;
esac

# ---------------------------------------------------------------------------
# Channel-specific freshness check.
# ---------------------------------------------------------------------------
if [[ "${CHANNEL}" == "dev" ]]; then
  DEV_BASE_URL="https://github.com/${REPO}/releases/download/dev"

  REMOTE_SHA="$(curl -fsSL "${DEV_BASE_URL}/dev-version.txt" 2>/dev/null || true)"
  if [[ -z "${REMOTE_SHA}" ]]; then
    echo "[memso] Could not reach dev release - using existing binary if available" >&2
    echo "[memso bootstrap] Binary ready (offline, SHA unknown)"
    exit 0
  fi

  LOCAL_SHA="$(cat "${VERSION_FILE}" 2>/dev/null || echo "")"
  if [[ "${REMOTE_SHA}" == "${LOCAL_SHA}" && -f "${BINARY}" ]]; then
    echo "[memso bootstrap] Binary ready (dev ${REMOTE_SHA:0:7})"
    exit 0
  fi

  DOWNLOAD_URL="${DEV_BASE_URL}/${ARTIFACT}"
  VERSION_TO_WRITE="${REMOTE_SHA}"
  VERSION_LABEL="dev ${REMOTE_SHA:0:7}"

else
  # Stable channel.
  if [[ -f "${VERSION_FILE}" ]]; then
    installed="$(cat "${VERSION_FILE}")"

    if [[ "${installed}" == "${COMPOSITE}" ]]; then
      echo "[memso bootstrap] Binary ready (v${COMPOSITE})"
      exit 0
    fi

    installed_memso="${installed%%-*}"
    if [[ "${installed_memso}" == "${MEMSO_VERSION}" && -f "${BINARY}" ]]; then
      # Only the plugin revision changed - no binary download needed.
      printf '%s' "${COMPOSITE}" > "${VERSION_FILE}"
      echo "[memso bootstrap] Binary ready (v${COMPOSITE})"
      exit 0
    fi
  fi

  DOWNLOAD_URL="https://github.com/${REPO}/releases/download/v${MEMSO_VERSION}/${ARTIFACT}"
  VERSION_TO_WRITE="${COMPOSITE}"
  VERSION_LABEL="v${COMPOSITE}"
fi

# ---------------------------------------------------------------------------
# Download and extract.
# ---------------------------------------------------------------------------
echo "[memso] Downloading ${ARTIFACT} (${VERSION_LABEL})..." >&2

mkdir -p "${CLAUDE_PLUGIN_DATA}"
ARCHIVE="${CLAUDE_PLUGIN_DATA}/${ARTIFACT}"

if ! curl -fsSL "${DOWNLOAD_URL}" -o "${ARCHIVE}"; then
  echo "[memso] Download failed: ${DOWNLOAD_URL}" >&2
  exit 0
fi

case "${ARTIFACT}" in
  *.tar.gz)
    if [[ -f "${BINARY}" ]] && ! mv -f "${BINARY}" "${BINARY}.backup" 2>/dev/null; then
      echo "[memso] Binary is in use - stop all Claude Code sessions to apply the update" >&2
      echo "[memso bootstrap] Update deferred (binary in use)"
      exit 0
    fi
    tar xzf "${ARCHIVE}" -C "${CLAUDE_PLUGIN_DATA}"
    rm -f "${ARCHIVE}"
    chmod +x "${BINARY}"
    ;;
  *.zip)
    if [[ -f "${BINARY}" ]] && ! mv -f "${BINARY}" "${BINARY}.backup" 2>/dev/null; then
      echo "[memso] Binary is in use - stop all Claude Code sessions to apply the update" >&2
      echo "[memso bootstrap] Update deferred (binary in use)"
      exit 0
    fi
    unzip -o "${ARCHIVE}" -d "${CLAUDE_PLUGIN_DATA}"
    rm -f "${ARCHIVE}"
    ;;
esac

printf '%s' "${VERSION_TO_WRITE}" > "${VERSION_FILE}"

echo "[memso] Installed ${VERSION_LABEL} to ${BINARY}" >&2
echo "[memso bootstrap] Downloaded and installed ${VERSION_LABEL} (${OS}/${ARCH})"
