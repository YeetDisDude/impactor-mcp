#!/usr/bin/env sh
# Installs the Impactor MCP server (impactor-mcp) and registers it with Claude Code /
# Claude Desktop when present. Any other MCP client can point at the installed binary.
# Works alongside the stock Impactor app - nothing about the app is modified.
#
# Run straight from the network (nothing to clone):
#   curl -fsSL https://raw.githubusercontent.com/YeetDisDude/impactor-mcp/main/install.sh | sh
#   curl -fsSL .../install.sh | sh -s -- --auto-refresh
#
# Options: --no-claude-code --no-desktop --auto-refresh --from-source --uninstall
set -eu

REPO="YeetDisDude/impactor-mcp"
SERVER_NAME="impactor"
INSTALL_DIR="${IMPACTOR_MCP_INSTALL_DIR:-$HOME/.local/bin}"
BIN_PATH="$INSTALL_DIR/impactor-mcp"

# A local checkout has Cargo.toml next to this script; when piped into `sh`, $0 is the shell.
LOCAL_ROOT=""
case "$0" in
    */install.sh|install.sh)
        dir="$(cd "$(dirname "$0")" && pwd)"
        [ -f "$dir/Cargo.toml" ] && LOCAL_ROOT="$dir"
        ;;
esac

OS="$(uname -s)"
case "$OS" in
    Darwin) PLATFORM=macos; DESKTOP_CONFIG="$HOME/Library/Application Support/Claude/claude_desktop_config.json" ;;
    Linux) PLATFORM=linux; DESKTOP_CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}/Claude/claude_desktop_config.json" ;;
    *) echo "Unsupported OS: $OS (use install.ps1 on Windows)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
    arm64|aarch64) ARCH=aarch64 ;;
    *) ARCH=x86_64 ;;
esac

NO_CLAUDE_CODE=0; NO_DESKTOP=0; AUTO_REFRESH=0; FROM_SOURCE=0; UNINSTALL=0
for arg in "$@"; do
    case "$arg" in
        --no-claude-code) NO_CLAUDE_CODE=1 ;;
        --no-desktop) NO_DESKTOP=1 ;;
        --auto-refresh) AUTO_REFRESH=1 ;;
        --from-source) FROM_SOURCE=1 ;;
        --uninstall) UNINSTALL=1 ;;
        -h|--help) echo "Options: --no-claude-code --no-desktop --auto-refresh --from-source --uninstall"; exit 0 ;;
        *) echo "unknown option: $arg" >&2; exit 2 ;;
    esac
done

step() { printf '\033[36m==> %s\033[0m\n' "$1"; }
warn() { printf '\033[33mwarning: %s\033[0m\n' "$1" >&2; }

# Edits claude_desktop_config.json with python3 (preinstalled on macOS and most
# Linux distros). $1 = "add" | "remove", $2 = server name (default $SERVER_NAME)
desktop_config() {
    if ! command -v python3 >/dev/null 2>&1; then
        warn "python3 not found; add this to $DESKTOP_CONFIG under \"mcpServers\":"
        echo "  \"$SERVER_NAME\": { \"command\": \"$BIN_PATH\", \"args\": [] }"
        return 0
    fi
    python3 - "$1" "$DESKTOP_CONFIG" "${2:-$SERVER_NAME}" "$BIN_PATH" "$AUTO_REFRESH" <<'PY'
import json, os, shutil, sys
action, path, name, binary, auto = sys.argv[1:6]
config = {}
if os.path.exists(path):
    shutil.copyfile(path, path + ".bak")
    with open(path, encoding="utf-8") as f:
        text = f.read().strip()
        config = json.loads(text) if text else {}
servers = config.setdefault("mcpServers", {})
if action == "add":
    entry = {"command": binary, "args": []}
    if auto == "1":
        entry["env"] = {"IMPACTOR_MCP_AUTO_REFRESH": "1"}
    servers[name] = entry
else:
    servers.pop(name, None)
os.makedirs(os.path.dirname(path), exist_ok=True)
with open(path, "w", encoding="utf-8") as f:
    json.dump(config, f, indent=2)
PY
}

download() { # url dest
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$1" -o "$2"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO "$2" "$1"
    else
        return 1
    fi
}

install_prebuilt() {
    url="https://github.com/$REPO/releases/latest/download/impactor-mcp-$PLATFORM-$ARCH"
    step "Downloading prebuilt binary ($url)"
    tmp="$(mktemp)"
    if download "$url" "$tmp"; then
        mv -f "$tmp" "$BIN_PATH"
        chmod +x "$BIN_PATH"
        return 0
    fi
    rm -f "$tmp"
    warn "No prebuilt binary available; building from source instead."
    return 1
}

install_from_source() {
    [ -d "$HOME/.cargo/bin" ] && PATH="$HOME/.cargo/bin:$PATH"
    if ! command -v cargo >/dev/null 2>&1; then
        step "Rust not found; installing rustup (https://rustup.rs)"
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
        PATH="$HOME/.cargo/bin:$PATH"
    fi
    step "Building impactor-mcp (release). The first build downloads dependencies and takes a few minutes..."
    if [ -n "$LOCAL_ROOT" ]; then
        (cd "$LOCAL_ROOT" && cargo build --release --locked)
        cp -f "$LOCAL_ROOT/target/release/impactor-mcp" "$BIN_PATH"
    else
        root="$(mktemp -d)"
        cargo install --locked --git "https://github.com/$REPO" impactor-mcp --root "$root"
        cp -f "$root/bin/impactor-mcp" "$BIN_PATH"
        rm -rf "$root"
    fi
    chmod +x "$BIN_PATH"
}

# Earlier releases were named `plume-impactor` / `plumemcp`; clean those up.
remove_legacy() {
    if command -v claude >/dev/null 2>&1; then
        claude mcp remove --scope user plume-impactor >/dev/null 2>&1 || true
    fi
    if [ -f "$DESKTOP_CONFIG" ] && grep -q '"plume-impactor"' "$DESKTOP_CONFIG" 2>/dev/null; then
        desktop_config remove plume-impactor
    fi
    rm -f "$INSTALL_DIR/plumemcp"
}

# --- Uninstall ---------------------------------------------------------------
if [ "$UNINSTALL" = 1 ]; then
    remove_legacy
    if command -v claude >/dev/null 2>&1; then
        claude mcp remove --scope user "$SERVER_NAME" >/dev/null 2>&1 || true
    fi
    if [ -f "$DESKTOP_CONFIG" ]; then desktop_config remove; fi
    rm -f "$BIN_PATH"
    step "Uninstalled. (Impactor accounts in its config directory were left untouched.)"
    exit 0
fi

# --- 1. Get the binary -------------------------------------------------------
mkdir -p "$INSTALL_DIR"
# Prefer the prebuilt binary (no compiling); build only when asked or when none exists.
if [ "$FROM_SOURCE" = 1 ] || ! install_prebuilt; then
    install_from_source
fi
step "Installed $BIN_PATH"
remove_legacy

# --- 2. Register -------------------------------------------------------------
if [ "$NO_CLAUDE_CODE" = 0 ]; then
    if command -v claude >/dev/null 2>&1; then
        step "Registering with Claude Code (user scope)"
        claude mcp remove --scope user "$SERVER_NAME" >/dev/null 2>&1 || true
        if [ "$AUTO_REFRESH" = 1 ]; then
            claude mcp add --scope user -e IMPACTOR_MCP_AUTO_REFRESH=1 "$SERVER_NAME" -- "$BIN_PATH"
        else
            claude mcp add --scope user "$SERVER_NAME" -- "$BIN_PATH"
        fi
    else
        warn "Claude Code CLI not found; skipped. Later run: claude mcp add --scope user $SERVER_NAME -- \"$BIN_PATH\""
    fi
fi

if [ "$NO_DESKTOP" = 0 ]; then
    step "Registering with Claude Desktop ($DESKTOP_CONFIG)"
    desktop_config add
    echo "    Restart Claude Desktop to load the server."
fi

# --- 3. Prerequisite check ---------------------------------------------------
if [ "$PLATFORM" = linux ] && ! command -v usbmuxd >/dev/null 2>&1; then
    warn "usbmuxd not found. Install it (e.g. 'sudo apt install usbmuxd') so the server can talk to iPhones/iPads."
fi

step "Done. Other MCP clients: use command \"$BIN_PATH\" (stdio). Try asking your agent: \"list my connected iOS devices\"."
