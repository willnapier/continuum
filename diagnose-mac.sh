#!/usr/bin/env bash
# Diagnostic script for Mac Claude setup issues
# Run this on your Mac to diagnose why 'claude' doesn't work

echo "=== Claude Diagnostic Report ==="
echo ""

# System info
echo "1. System Info:"
echo "   OS: $(uname -s)"
echo "   Architecture: $(uname -m)"
echo "   Shell: $SHELL"
echo ""

# Check PATH
echo "2. PATH:"
echo "   $PATH" | tr ':' '\n' | grep -E "local/bin|homebrew" || echo "   No .local/bin or homebrew in PATH"
echo ""

# Check if claude exists anywhere
echo "3. Finding 'claude' command:"
which -a claude 2>/dev/null || echo "   'which claude' found nothing"
echo ""

# Check common installation locations
echo "4. Checking common Claude locations:"
for path in \
    "/usr/bin/claude" \
    "/usr/local/bin/claude" \
    "/opt/homebrew/bin/claude" \
    "$HOME/.local/bin/claude" \
    "$HOME/.local/bin/continuum-claude"; do
    if [ -e "$path" ]; then
        echo "   ✓ Found: $path"
        ls -la "$path"
    else
        echo "   ✗ Missing: $path"
    fi
done
echo ""

# Check npm global packages
echo "5. npm global packages:"
if command -v npm &> /dev/null; then
    npm list -g --depth=0 2>/dev/null | grep claude || echo "   No claude package found"
    echo "   npm prefix: $(npm config get prefix)"
else
    echo "   npm not found"
fi
echo ""

# Check Node.js installation
echo "6. Node.js:"
if command -v node &> /dev/null; then
    echo "   Version: $(node --version)"
    echo "   Location: $(which node)"
else
    echo "   Node.js not installed"
fi
echo ""

# Check if continuum exists
echo "7. Continuum setup:"
if [ -d "$HOME/Assistants/projects/continuum" ]; then
    echo "   ✓ Continuum project exists"
    if [ -f "$HOME/Assistants/projects/continuum/target/release/continuum-claude" ]; then
        echo "   ✓ continuum-claude binary built"
    else
        echo "   ✗ continuum-claude binary not built"
        echo "     Run: cd ~/Assistants/projects/continuum && cargo build --release"
    fi
else
    echo "   ✗ Continuum project not found at ~/Assistants/projects/continuum"
fi
echo ""

# Check shell config
echo "8. Shell configuration:"
if [ -f "$HOME/.zshrc" ]; then
    echo "   .zshrc exists"
    if grep -q "claude" "$HOME/.zshrc"; then
        echo "   Contains 'claude':"
        grep "claude" "$HOME/.zshrc" | head -3
    else
        echo "   No claude alias/function found"
    fi
else
    echo "   No .zshrc file"
fi

if [ -f "$HOME/.bashrc" ]; then
    echo "   .bashrc exists"
    if grep -q "claude" "$HOME/.bashrc"; then
        echo "   Contains 'claude':"
        grep "claude" "$HOME/.bashrc" | head -3
    else
        echo "   No claude alias/function found"
    fi
else
    echo "   No .bashrc file"
fi
echo ""

# Check Nushell
echo "9. Nushell setup:"
if [ -f "$HOME/.config/nushell/config.nu" ]; then
    echo "   config.nu exists"
    if grep -q "def claude" "$HOME/.config/nushell/config.nu"; then
        echo "   ✓ claude function defined"
    else
        echo "   ✗ No claude function found"
    fi
else
    echo "   No Nushell config found"
fi
echo ""

# Test type/alias command
echo "10. What 'claude' resolves to:"
type claude 2>&1 || echo "    'claude' command not found"
echo ""

# Check if Rust/cargo installed
echo "11. Rust/Cargo:"
if command -v cargo &> /dev/null; then
    echo "   ✓ cargo $(cargo --version)"
else
    echo "   ✗ Cargo not installed (needed to build continuum-claude)"
fi
echo ""

echo "=== End Diagnostic Report ==="
echo ""
echo "Please share this output to diagnose the issue."
