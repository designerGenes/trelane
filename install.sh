#!/usr/bin/env bash
# install.sh -- install trelane to ~/.local/bin
#
# Usage:
#   ./install.sh                  # build and install current version
#   ./install.sh --bump-version   # bump patch version, then install
#   ./install.sh --set-version 0.2.0   # set explicit version, then install
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

CARGO_TOML="$SCRIPT_DIR/Cargo.toml"
INSTALL_DIR="$HOME/.local/bin"

# ----------------------------------------------------------------- arg parsing

BUMP=false
SET_VERSION=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --bump-version)
            BUMP=true
            shift
            ;;
        --set-version)
            SET_VERSION="$2"
            shift 2
            ;;
        *)
            echo "Unknown option: $1" >&2
            echo "Usage: $0 [--bump-version] [--set-version X.Y.Z]" >&2
            exit 1
            ;;
    esac
done

# ----------------------------------------------------------- version handling

if [[ "$BUMP" == "true" && -n "$SET_VERSION" ]]; then
    echo "Error: --bump-version and --set-version cannot be used together." >&2
    exit 1
fi

if [[ "$BUMP" == "true" ]]; then
    CURRENT=$(grep '^version' "$CARGO_TOML" | head -1 | sed 's/.*"\(.*\)".*/\1/')
    IFS='.' read -r MAJOR MINOR PATCH <<< "$CURRENT"
    PATCH=$((PATCH + 1))
    NEW_VERSION="${MAJOR}.${MINOR}.${PATCH}"
    echo "Bumping version: $CURRENT -> $NEW_VERSION"
    sed -i.bak "s/^version = \"$CURRENT\"/version = \"$NEW_VERSION\"/" "$CARGO_TOML"
    rm -f "$CARGO_TOML.bak"
fi

if [[ -n "$SET_VERSION" ]]; then
    CURRENT=$(grep '^version' "$CARGO_TOML" | head -1 | sed 's/.*"\(.*\)".*/\1/')
    echo "Setting version: $CURRENT -> $SET_VERSION"
    sed -i.bak "s/^version = \"$CURRENT\"/version = \"$SET_VERSION\"/" "$CARGO_TOML"
    rm -f "$CARGO_TOML.bak"
fi

# --------------------------------------------------------------- build + install

echo "Building trelane..."
cargo build --release

echo "Installing binary to $INSTALL_DIR..."
mkdir -p "$INSTALL_DIR"
cp "$SCRIPT_DIR/target/release/trelane" "$INSTALL_DIR/trelane"

# Add ~/.local/bin to PATH in shell rc if not present
if [[ ":$PATH:" != *":$HOME/.local/bin:"* ]]; then
    for rcfile in "$HOME/.zshrc" "$HOME/.bashrc"; do
        if [[ -f "$rcfile" ]] && ! grep -q '.local/bin' "$rcfile" 2>/dev/null; then
            echo '' >> "$rcfile"
            echo '# Added by trelane installer' >> "$rcfile"
            echo 'export PATH="$HOME/.local/bin:$PATH"' >> "$rcfile"
            echo "Added ~/.local/bin to PATH in $rcfile"
        fi
    done
fi

echo ""
echo "trelane installed successfully."
echo "  Binary: $INSTALL_DIR/trelane"
INSTALLED_VERSION=$("$INSTALL_DIR/trelane" --version 2>/dev/null || echo "unknown")
echo "  Version: $INSTALLED_VERSION"
if [[ ":$PATH:" != *":$HOME/.local/bin:"* ]]; then
    echo ""
    echo "NOTE: ~/.local/bin is not in your PATH."
    echo "  Restart your shell, or run: export PATH=\"\$HOME/.local/bin:\$PATH\""
fi
