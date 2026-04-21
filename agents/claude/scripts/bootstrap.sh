#!/usr/bin/env bash
# Bootstrap for the tyto Claude Code plugin.
# Runs on SessionStart. Downloads the correct tyto binary for the current
# platform if not already installed or if the version is outdated.
# Always exits 0 so a failed download does not block the session.
#
# Channel selection (default: stable):
#   TYTO_CHANNEL=stable  version-pinned releases from GitHub Releases
#   TYTO_CHANNEL=dev     rolling builds from the 'dev' pre-release
#
# Stable channel versioning:
#   TYTO_VERSION  - binary release to download.
#   PLUGIN_VERSION - revision of the plugin files (hooks, scripts, manifests).
#   The version file stores the composite TYTO_VERSION-PLUGIN_VERSION.
#   A PLUGIN_VERSION-only bump updates the version file without re-downloading.
#
# Dev channel versioning:
#   The version file stores the full commit SHA from dev-version.txt.
#   The binary is re-downloaded only when the remote SHA changes.
set -uo pipefail

TYTO_VERSION="0.5.0"
PLUGIN_VERSION="1"
COMPOSITE="${TYTO_VERSION}-${PLUGIN_VERSION}"

BINARY="${CLAUDE_PLUGIN_DATA}/tyto"
VERSION_FILE="${CLAUDE_PLUGIN_DATA}/version"
REPO="tyto-ai/tyto"
CHANNEL="${TYTO_CHANNEL:-stable}"

# Allow overriding the GitHub releases base URL - used in E2E tests to point at a
# local mock HTTP server instead of hitting GitHub. Stable channel builds URLs as:
#   ${RELEASES_BASE}/v${TYTO_VERSION}/${ARTIFACT}
# Dev channel builds:
#   ${RELEASES_BASE}/dev/dev-version.txt
#   ${RELEASES_BASE}/dev/${ARTIFACT}
RELEASES_BASE="${TYTO_RELEASES_BASE:-https://github.com/${REPO}/releases/download}"

# ---------------------------------------------------------------------------
# File logging - mirrors all output to a persistent log for troubleshooting.
# Check ${CLAUDE_PLUGIN_DATA}/bootstrap.log if something goes wrong.
# ---------------------------------------------------------------------------
mkdir -p "${CLAUDE_PLUGIN_DATA}" 2>/dev/null || true
LOG_FILE="${CLAUDE_PLUGIN_DATA}/bootstrap.log"

# log: print to stdout (-> additionalContext) AND append to log file.
log() {
  echo "$@"
  echo "$(date '+%Y-%m-%d %H:%M:%S') $*" >> "${LOG_FILE}" 2>/dev/null || true
}

# log_err: print to stderr AND append to log file.
log_err() {
  echo "$@" >&2
  echo "$(date '+%Y-%m-%d %H:%M:%S') [err] $*" >> "${LOG_FILE}" 2>/dev/null || true
}

{
  echo ""
  echo "=== $(date '+%Y-%m-%d %H:%M:%S') bootstrap start ==="
  echo "channel: ${CHANNEL}, version: ${TYTO_VERSION}-${PLUGIN_VERSION}"
  echo "plugin_data: ${CLAUDE_PLUGIN_DATA}"
  echo "os: $(uname -s 2>/dev/null || echo unknown), arch: $(uname -m 2>/dev/null || echo unknown)"
  echo "version_file: $(cat "${VERSION_FILE}" 2>/dev/null || echo '(none)')"
} >> "${LOG_FILE}" 2>/dev/null || true

# Allow an explicit binary override - useful in development or for custom builds.
# Set TYTO_BINARY_OVERRIDE to an absolute path to skip the GitHub download entirely.
if [[ -n "${TYTO_BINARY_OVERRIDE:-}" ]]; then
  if [[ ! -f "${TYTO_BINARY_OVERRIDE}" ]]; then
    log_err "[tyto] TYTO_BINARY_OVERRIDE is set but '${TYTO_BINARY_OVERRIDE}' does not exist"
    exit 0
  fi
  log "[tyto bootstrap] Binary ready (override: ${TYTO_BINARY_OVERRIDE})"
  exit 0
fi

# Force re-download regardless of installed version.
# Set TYTO_FORCE_UPDATE=1 to bypass the version check and always download.
# Useful for troubleshooting stuck or corrupted installs.
FORCE_UPDATE="${TYTO_FORCE_UPDATE:-0}"

# Detect OS and architecture.
OS="$(uname -s)"
ARCH="$(uname -m)"

case "${OS}" in
  Linux)
    case "${ARCH}" in
      x86_64)          ARTIFACT="tyto-linux-x86_64.tar.gz" ;;
      aarch64 | arm64) ARTIFACT="tyto-linux-aarch64.tar.gz" ;;
      *) log_err "[tyto] Unsupported architecture: ${ARCH}"; exit 0 ;;
    esac
    ;;
  Darwin)
    case "${ARCH}" in
      arm64)  ARTIFACT="tyto-macos-aarch64.tar.gz" ;;
      x86_64) ARTIFACT="tyto-macos-x86_64.tar.gz" ;;
      *) log_err "[tyto] Unsupported architecture: macOS ${ARCH}"; exit 0 ;;
    esac
    ;;
  MINGW* | MSYS* | CYGWIN*)
    # Windows via Git Bash - required for Claude Code hooks on Windows.
    ARTIFACT="tyto-windows-x86_64.zip"
    BINARY="${BINARY}.exe"
    ;;
  *)
    log_err "[tyto] Unsupported OS: ${OS}"
    exit 0
    ;;
esac

# ---------------------------------------------------------------------------
# Channel-specific freshness check.
# ---------------------------------------------------------------------------
if [[ "${CHANNEL}" == "dev" ]]; then
  DEV_BASE_URL="${RELEASES_BASE}/dev"

  REMOTE_SHA="$(curl -fsSL "${DEV_BASE_URL}/dev-version.txt" 2>/dev/null || true)"
  if [[ -z "${REMOTE_SHA}" ]]; then
    log_err "[tyto] Could not reach dev release - using existing binary if available"
    log "[tyto bootstrap] Binary ready (offline, SHA unknown)"
    exit 0
  fi

  LOCAL_SHA="$(cat "${VERSION_FILE}" 2>/dev/null || echo "")"
  if [[ "${FORCE_UPDATE}" != "1" && "${REMOTE_SHA}" == "${LOCAL_SHA}" && -f "${BINARY}" ]]; then
    log "[tyto bootstrap] Binary ready (dev ${REMOTE_SHA:0:7})"
    exit 0
  fi

  DOWNLOAD_URL="${DEV_BASE_URL}/${ARTIFACT}"
  VERSION_TO_WRITE="${REMOTE_SHA}"
  VERSION_LABEL="dev ${REMOTE_SHA:0:7}"

else
  # Stable channel.
  if [[ "${FORCE_UPDATE}" != "1" && -f "${VERSION_FILE}" ]]; then
    installed="$(cat "${VERSION_FILE}")"

    if [[ "${installed}" == "${COMPOSITE}" ]]; then
      log "[tyto bootstrap] Binary ready (v${COMPOSITE})"
      exit 0
    fi

    installed_tyto="${installed%%-*}"
    if [[ "${installed_tyto}" == "${TYTO_VERSION}" && -f "${BINARY}" ]]; then
      # Only the plugin revision changed - no binary download needed.
      printf '%s' "${COMPOSITE}" > "${VERSION_FILE}"
      log "[tyto bootstrap] Binary ready (v${COMPOSITE})"
      exit 0
    fi
  fi

  DOWNLOAD_URL="${RELEASES_BASE}/v${TYTO_VERSION}/${ARTIFACT}"
  VERSION_TO_WRITE="${COMPOSITE}"
  VERSION_LABEL="v${COMPOSITE}"
fi

# ---------------------------------------------------------------------------
# Download and extract.
# ---------------------------------------------------------------------------
log_err "[tyto] Downloading ${ARTIFACT} (${VERSION_LABEL})..."

mkdir -p "${CLAUDE_PLUGIN_DATA}"
ARCHIVE="${CLAUDE_PLUGIN_DATA}/${ARTIFACT}"

if ! curl -fsSL "${DOWNLOAD_URL}" -o "${ARCHIVE}"; then
  log_err "[tyto] Download failed: ${DOWNLOAD_URL}"
  exit 0
fi

case "${ARTIFACT}" in
  *.tar.gz)
    if [[ -f "${BINARY}" ]] && ! mv -f "${BINARY}" "${BINARY}.backup" 2>/dev/null; then
      log_err "[tyto] Binary is in use - stop all Claude Code sessions to apply the update"
      log "[tyto bootstrap] Update deferred (binary in use)"
      exit 0
    fi
    tar xzf "${ARCHIVE}" -C "${CLAUDE_PLUGIN_DATA}"
    rm -f "${ARCHIVE}"
    chmod +x "${BINARY}"
    ;;
  *.zip)
    if [[ -f "${BINARY}" ]] && ! mv -f "${BINARY}" "${BINARY}.backup" 2>/dev/null; then
      log_err "[tyto] Binary is in use - stop all Claude Code sessions to apply the update"
      log "[tyto bootstrap] Update deferred (binary in use)"
      exit 0
    fi
    unzip -o "${ARCHIVE}" -d "${CLAUDE_PLUGIN_DATA}"
    rm -f "${ARCHIVE}"
    ;;
esac

printf '%s' "${VERSION_TO_WRITE}" > "${VERSION_FILE}"

log_err "[tyto] Installed ${VERSION_LABEL} to ${BINARY}"
log "[tyto bootstrap] Downloaded and installed ${VERSION_LABEL} (${OS}/${ARCH})"
log "[tyto bootstrap] First-run note: tyto will download a ~22MB embedding model on first use. Memory tools will be unavailable for up to ~1 minute while the model loads. The agent will inform you when this is happening."
