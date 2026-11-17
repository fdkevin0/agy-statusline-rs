#!/usr/bin/env sh
set -eu

REPO="fdkevin0/agy-statusline-rs"
BIN_NAME="agy-statusline"
INSTALL_DIR="${AGY_INSTALL_DIR:-${HOME}/.local/bin}"

err() {
  printf 'Error: %s\n' "$1" >&2
  exit 1
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || err "Required command not found: $1"
}

detect_artifact() {
  os="$(uname -s)"
  arch="$(uname -m)"

  case "${os}" in
    Linux)
      case "${arch}" in
        x86_64 | amd64) printf '%s\n' "${BIN_NAME}-linux-x64.tar.gz" ;;
        aarch64 | arm64) printf '%s\n' "${BIN_NAME}-linux-arm64.tar.gz" ;;
        *) err "Unsupported Linux architecture: ${arch} (try: cargo install ${BIN_NAME})" ;;
      esac
      ;;
    Darwin)
      case "${arch}" in
        arm64 | aarch64) printf '%s\n' "${BIN_NAME}-macos-arm64.tar.gz" ;;
        x86_64) printf '%s\n' "${BIN_NAME}-macos-x64.tar.gz" ;;
        *) err "Unsupported macOS architecture: ${arch} (try: cargo install ${BIN_NAME})" ;;
      esac
      ;;
    *)
      err "Unsupported OS: ${os} (try: cargo install ${BIN_NAME})"
      ;;
  esac
}

compute_sha256() {
  file="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "${file}" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "${file}" | awk '{print $1}'
  elif command -v openssl >/dev/null 2>&1; then
    openssl dgst -sha256 "${file}" | awk '{print $NF}'
  else
    err "No SHA256 tool found (need sha256sum, shasum, or openssl)"
  fi
}

main() {
  need_cmd curl
  need_cmd tar
  need_cmd awk
  need_cmd mktemp

  artifact="$(detect_artifact)"
  if [ -n "${VERSION:-}" ]; then
    base_url="https://github.com/${REPO}/releases/download/${VERSION}"
  else
    base_url="https://github.com/${REPO}/releases/latest/download"
  fi

  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "${tmp_dir}"' EXIT INT TERM

  printf 'Downloading %s...\n' "${artifact}"
  curl -fsSL "${base_url}/${artifact}" -o "${tmp_dir}/${artifact}"
  curl -fsSL "${base_url}/SHA256SUMS.txt" -o "${tmp_dir}/SHA256SUMS.txt"

  expected_sha="$(awk -v f="${artifact}" '$2 == f || $2 == ("./" f) {print $1; exit}' "${tmp_dir}/SHA256SUMS.txt")"
  [ -n "${expected_sha}" ] || err "Checksum entry for ${artifact} not found in SHA256SUMS.txt"

  actual_sha="$(compute_sha256 "${tmp_dir}/${artifact}")"
  if [ "${expected_sha}" != "${actual_sha}" ]; then
    err "Checksum mismatch for ${artifact}: expected ${expected_sha}, got ${actual_sha}"
  fi

  tar -xzf "${tmp_dir}/${artifact}" -C "${tmp_dir}"
  [ -f "${tmp_dir}/${BIN_NAME}" ] || err "Extracted archive does not contain ${BIN_NAME}"

  mkdir -p "${INSTALL_DIR}"
  if command -v install >/dev/null 2>&1; then
    install -m 755 "${tmp_dir}/${BIN_NAME}" "${INSTALL_DIR}/${BIN_NAME}"
  else
    cp "${tmp_dir}/${BIN_NAME}" "${INSTALL_DIR}/${BIN_NAME}"
    chmod 755 "${INSTALL_DIR}/${BIN_NAME}"
  fi

  if [ "$(uname -s)" = "Darwin" ] && command -v xattr >/dev/null 2>&1; then
    xattr -d com.apple.quarantine "${INSTALL_DIR}/${BIN_NAME}" >/dev/null 2>&1 || true
  fi

  printf 'Installed %s to %s/%s\n' "${BIN_NAME}" "${INSTALL_DIR}" "${BIN_NAME}"

  case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
      printf '\nAdd %s to your PATH:\n  export PATH="%s:$PATH"\n' "${INSTALL_DIR}" "${INSTALL_DIR}"
      ;;
  esac

  printf '\nNext steps:\n  %s config      # interactive layout configurator\n  %s apply --yes # register with AGY\n' "${BIN_NAME}" "${BIN_NAME}"
}

main "$@"
