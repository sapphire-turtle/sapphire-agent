#!/bin/sh
# Install the headless edition of sapphire-agent (the CLI, the app server and
# the MCP server in one binary, for servers and other machines without a GUI)
# from this repository's GitHub releases. The desktop app ships as an installer
# on the same releases. sapphire-bridge, which the app syncs through, is not
# installed here: sapphire-framework has a script for it.
#
#   curl -fsSL https://raw.githubusercontent.com/sapphire-turtle/sapphire-agent/main/install/install.sh | sh
#
# Run with --help for the options. Linux (x86_64, aarch64) and macOS (aarch64);
# Windows has install.ps1.
#
# Each download is checked against the SHA-256 GitHub records for it and, when
# minisign is on PATH, against the organization's signature before it is
# installed. Files go to ~/.local/bin, where the `update` command of each
# binary recognises a release install and can replace it later. A binary
# installed by cargo or a package manager is left alone, as `update` would.
#
# Made from sapphire-turtle/.github's templates/install/install.sh: keep the
# two in step, and change only the comment above and the tables below.
set -eu

# Every application this script installs: name, repository, command, and the
# prefix of its release tags. The command's release file is
# `<command>-headless-<os>-<arch>`. Keep in step with install.ps1 and with the
# repository's release-plz.toml. With one row, the name may be left out.
APPS="
agent           sapphire-agent     sapphire-agent            v
"
# Applications installed by another repository's script: name, and that
# repository.
MOVED="
"
OWNER="sapphire-turtle"

# The organization's minisign public key (sapphire_framework_update::UPDATE_PUBKEY),
# which signs every release file.
PUBKEY="RWTryBeqOT+n9BzfWt2B3inpQylvPAJYvSy5tBmOEAauzdqmbUMqayhq"
PUBKEY_ID="F4A73F39AA17C8EB"

usage() {
  cat <<EOF
Usage: install.sh [options] [<app>...]

Installs each <app>'s headless edition from its latest release. For the
desktop app, download its installer from the releases instead.

Apps:
$(printf '%s\n' "$APPS" | awk 'NF { printf "  %s\n", $1 }')

Options:
  --dir <dir>            Install into <dir> (default: ~/.local/bin)
  --require-signature    Fail, rather than warn, when minisign is not on PATH to
                         check the signature
  -h, --help             Show this help
EOF
}

say() { printf '%s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
err() { printf 'error: %s\n' "$*" >&2; exit 1; }

INSTALL_DIR="${HOME}/.local/bin"
REQUIRE_SIGNATURE=0
REQUESTED=""

while [ $# -gt 0 ]; do
  case "$1" in
    --dir) [ $# -ge 2 ] || err "--dir needs a directory"; INSTALL_DIR="$2"; shift 2 ;;
    --require-signature) REQUIRE_SIGNATURE=1; shift ;;
    # The bridge is no longer installed with the applications; the option is
    # accepted so that older command lines still run.
    --no-bridge) shift ;;
    -h|--help) usage; exit 0 ;;
    -*) err "unknown option: $1 (see --help)" ;;
    *) REQUESTED="$REQUESTED $1"; shift ;;
  esac
done

if [ -z "$REQUESTED" ]; then
  if [ "$(printf '%s\n' "$APPS" | awk 'NF' | wc -l)" -eq 1 ]; then
    REQUESTED=$(printf '%s\n' "$APPS" | awk 'NF { print $1 }')
  else
    usage >&2
    exit 2
  fi
fi
for app in $REQUESTED; do
  repo=$(printf '%s\n' "$MOVED" | awk -v app="$app" '$1 == app { print $2 }')
  if [ -n "$repo" ]; then
    err "$app is installed by its own repository's script now:
  curl -fsSL https://raw.githubusercontent.com/${OWNER}/${repo}/main/install/install.sh | sh"
  fi
  case "$app" in
    *-desktop)
      repo=$(printf '%s\n' "$APPS" | awk -v app="${app%-desktop}" '$1 == app { print $2 }')
      [ -z "$repo" ] || err "$app is not installed by this script: download the desktop app's installer from https://github.com/${OWNER}/${repo}/releases/latest"
      ;;
  esac
  printf '%s\n' "$APPS" | awk -v app="$app" '$1 == app { found = 1 } END { exit !found }' \
    || err "unknown app: $app (see --help)"
done

command -v curl >/dev/null 2>&1 || err "curl is required"

case "$(uname -s)" in
  Linux*)  OS="linux" ;;
  Darwin*) OS="macos" ;;
  *) err "unsupported OS: $(uname -s) (on Windows, use install.ps1)" ;;
esac
case "$(uname -m)" in
  x86_64|amd64)  ARCH="x86_64" ;;
  aarch64|arm64) ARCH="aarch64" ;;
  *) err "unsupported architecture: $(uname -m)" ;;
esac
if [ "$OS" = "macos" ] && [ "$ARCH" = "x86_64" ]; then
  err "no release is built for Intel macOS; build from source with cargo"
fi

if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | awk '{ print $1 }'; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | awk '{ print $1 }'; }
else
  err "sha256sum or shasum is required"
fi
MINISIGN=$(command -v minisign 2>/dev/null || true)
if [ -z "$MINISIGN" ]; then
  if [ "$REQUIRE_SIGNATURE" -eq 1 ]; then
    err "minisign is required to check the signature (--require-signature): install it from https://jedisct1.github.io/minisign/"
  fi
  warn "minisign is not on PATH: downloads are checked against their SHA-256 only. Install minisign (https://jedisct1.github.io/minisign/) to check the signature as well."
fi

# app_field <app> <column>: one column of the app's row in APPS.
app_field() {
  printf '%s\n' "$APPS" | awk -v app="$1" -v col="$2" '$1 == app { print $col }'
}

# github_api <path>: a GitHub API response. Unauthenticated requests share a
# low per-address rate limit (CI runners hit it); GITHUB_TOKEN or GH_TOKEN, when
# set, lifts it.
github_api() {
  token="${GITHUB_TOKEN:-${GH_TOKEN:-}}"
  if [ -n "$token" ]; then
    curl -fsSL -H "Authorization: Bearer $token" "https://api.github.com/$1"
  else
    curl -fsSL "https://api.github.com/$1"
  fi
}

# latest_version <repo> <tag prefix>: the highest x.y.z among the repository's
# releases tagged <prefix>x.y.z. Not `releases/latest`: a repository that tags
# several binaries has one "latest" among all of them. Pre-release versions
# (x.y.z-…) are skipped, as `update` skips them.
latest_version() {
  releases=$(github_api "repos/${OWNER}/$1/releases?per_page=100") \
    || err "could not list the releases of ${OWNER}/$1 (rate-limited? set GITHUB_TOKEN)"
  printf '%s\n' "$releases" \
    | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' \
    | awk -v prefix="$2" '
        index($0, prefix) == 1 {
          v = substr($0, length(prefix) + 1)
          if (v ~ /^[0-9]+\.[0-9]+\.[0-9]+$/) print v
        }' \
    | sort -t . -k 1,1n -k 2,2n -k 3,3n \
    | tail -n 1
}

# asset_digest <release JSON> <file>: the SHA-256 GitHub recorded for the
# release's <file>, empty if it has none, or `missing` if there is no such file.
# The API's JSON has one field per line, and an asset's "digest" follows its
# "name" (the uploader between them has neither).
asset_digest() {
  printf '%s\n' "$1" | awk -v name="$2" '
    /^ *"name": / {
      current = $0
      sub(/^ *"name": *"/, "", current); sub(/",? *$/, "", current)
      if (current == name) found = 1
    }
    current == name && /^ *"digest": *"sha256:/ {
      digest = $0
      sub(/^ *"digest": *"sha256:/, "", digest); sub(/",? *$/, "", digest)
    }
    END { if (!found) print "missing"; else print digest }'
}

# verify <file> <asset name> <release URL> <release JSON>: check the download
# against its SHA-256 and the organization's signature, or exit.
verify() {
  want=$(asset_digest "$4" "$2")
  [ -n "$want" ] || err "$2 has no SHA-256 recorded on GitHub: not installed"
  got=$(sha256 "$1")
  [ "$got" = "$want" ] || err "$2 does not match its SHA-256 (expected $want, got $got): not installed"

  [ "$(asset_digest "$4" "$2.sig")" != missing ] || err "$2 is not signed: not installed"
  [ -n "$MINISIGN" ] || return 0

  # The .sig file is a minisign signature, base64-encoded as cargo-packager
  # writes it.
  SIG=$(mktemp "${TMPDIR:-/tmp}/sapphire-install.XXXXXX")
  sig=$(curl -fsSL "$3/$2.sig") || err "could not download $3/$2.sig"
  printf '%s' "$sig" | base64 -d > "$SIG" 2>/dev/null || err "$2.sig is not a signature cargo-packager writes"
  # -Q prints the trusted comment alone: timestamp:<n>\tfile:<name>.
  if ! comment=$("$MINISIGN" -V -Q -P "$PUBKEY" -m "$1" -x "$SIG" 2>&1); then
    err "$2 does not carry a valid signature from the organization's key ($PUBKEY_ID): not installed"
  fi
  rm -f "$SIG"
  SIG=""
  signed=$(printf '%s\n' "$comment" | tr '\t' '\n' | sed -n 's/^file://p')
  [ "$signed" = "$2" ] || err "the signature on $2 was made for ${signed:-another file}: not installed"
  say "  Signature verified ($PUBKEY_ID)."
}

# install_method <path>: how the binary at <path> was installed, judged by
# where it is, as sapphire-framework-update's `install_method` does: cargo,
# package-manager or release. Only a release install may be replaced.
install_method() {
  cargo_bin="${CARGO_HOME:-$HOME/.cargo}/bin"
  case "$1" in
    "$cargo_bin"/*) echo cargo ;;
    /usr/local/*) echo release ;;
    /usr/*|/bin/*|/sbin/*|/nix/*|/opt/homebrew/*|/home/linuxbrew/*|/snap/*) echo package-manager ;;
    *) echo release ;;
  esac
}

# desktop_install <binary>: where the application's desktop package is
# installed, or nothing. The package carries the same CLI and app server and
# updates them itself: a second copy here would run a second server.
desktop_install() {
  case "$OS" in
    macos)
      # sapphire-journal -> Sapphire Journal.app
      name=$(printf '%s\n' "$1" | awk -F- -v OFS=' ' '{ for (i = 1; i <= NF; i++) $i = toupper(substr($i, 1, 1)) substr($i, 2); print }')
      for dir in "$HOME/Applications" /Applications; do
        if [ -d "$dir/$name.app" ]; then echo "$dir/$name.app"; return 0; fi
      done
      ;;
    linux)
      # The package links (or wraps) its AppImage into ~/.local/bin.
      link="$HOME/.local/bin/$1"
      if [ -L "$link" ]; then
        target=$(readlink "$link")
        case "$target" in *.AppImage) echo "$target"; return 0 ;; esac
      elif [ -f "$link" ] && [ "$(head -c 2 "$link")" = '#!' ] && grep -q '\.AppImage' "$link"; then
        echo "$link"
        return 0
      fi
      ;;
  esac
}

INSTALLED=""

# The download in flight and its signature, removed if the script stops before
# the download is renamed into place.
TMP=""
SIG=""
trap 'rm -f "$TMP" "$SIG"' EXIT
trap 'exit 130' INT TERM

# install_app <app>
install_app() {
  repo=$(app_field "$1" 2)
  base=$(app_field "$1" 3)
  prefix=$(app_field "$1" 4)

  desktop=$(desktop_install "$base")
  if [ -n "$desktop" ]; then
    say "Skipping $base: the desktop app at $desktop already includes it and keeps it up to date."
    return 0
  fi

  # The binary is installed under its command's name, which is also the name
  # the application's own commands use for it.
  existing=$(command -v "$base" 2>/dev/null || true)
  if [ -n "$existing" ]; then
    method=$(install_method "$existing")
    if [ "$method" != release ]; then
      say "Leaving $existing alone: it was installed by $method. Update it the same way."
      return 0
    fi
  fi

  version=$(latest_version "$repo" "$prefix")
  [ -n "$version" ] || err "$repo has no ${prefix}x.y.z release of $base yet"
  tag="${prefix}${version}"
  release_json=$(github_api "repos/${OWNER}/${repo}/releases/tags/${tag}") \
    || err "could not read the release $tag of ${OWNER}/$repo (rate-limited? set GITHUB_TOKEN)"
  release="https://github.com/${OWNER}/${repo}/releases/download/${tag}"
  asset="${base}-headless-${OS}-${ARCH}"
  [ "$(asset_digest "$release_json" "$asset")" != missing ] \
    || err "$repo $tag has no $base for $OS-$ARCH"
  dest="${INSTALL_DIR}/${base}"

  say "Installing $base $version ($OS-$ARCH) to $INSTALL_DIR"
  mkdir -p "$INSTALL_DIR"
  # Download beside the destination and rename it into place: the rename is
  # atomic, so a running copy (the bridge, typically) keeps its old file and
  # nothing ever sees a half-written executable.
  TMP=$(mktemp "${INSTALL_DIR}/.${base}.XXXXXX")
  curl -fsSL "${release}/${asset}" -o "$TMP" || err "could not download ${release}/${asset}"
  verify "$TMP" "$asset" "$release" "$release_json"
  chmod 755 "$TMP"
  mv -f "$TMP" "$dest"
  TMP=""

  if [ -n "$existing" ] && [ "$existing" != "$dest" ]; then
    say "Note: $existing comes first on PATH and was not replaced; remove it, or install"
    say "      with --dir $(dirname "$existing") to replace it."
  fi
  INSTALLED="$INSTALLED $base"
}

for app in $REQUESTED; do
  install_app "$app"
done

[ -n "$INSTALLED" ] || exit 0
say ""
say "Installed:$INSTALLED"

case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *)
    say ""
    say "$INSTALL_DIR is not on PATH. Add it in your shell profile:"
    say "  export PATH=\"$INSTALL_DIR:\$PATH\""
    ;;
esac

case "$INSTALLED" in
  *" sapphire-bridge"*)
    say ""
    say "Run the bridge as a service, so it starts with your session:"
    say "  sapphire-bridge service install"
    say "If the service was already running, restart it to run the new version."
    ;;
  *)
    # The applications sync through the host's bridge, which is installed on
    # its own.
    if ! command -v sapphire-bridge >/dev/null 2>&1 && [ ! -x "$INSTALL_DIR/sapphire-bridge" ]; then
      say ""
      say "To sync, install sapphire-bridge on this host as well:"
      say "  curl -fsSL https://raw.githubusercontent.com/${OWNER}/sapphire-framework/main/install/install.sh | sh -s -- bridge"
    fi
    ;;
esac
