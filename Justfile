# STYX development tasks

list:
    just --list

# Generate railroad diagrams from EBNF grammar
ebnf:
    ./scripts/generate-grammar-diagrams.sh

# Sync tree-sitter query files to zed-styx extension
sync-queries:
    cp crates/tree-sitter-styx/queries/highlights.scm editors/zed-styx/languages/styx/
    cp crates/tree-sitter-styx/queries/brackets.scm editors/zed-styx/languages/styx/
    cp crates/tree-sitter-styx/queries/indents.scm editors/zed-styx/languages/styx/
    cp crates/tree-sitter-styx/queries/injections.scm editors/zed-styx/languages/styx/
    @echo "Query files synced to zed-styx"

# Sync grammar revision in zed-styx extension.toml to latest commit
sync-grammar:
    #!/usr/bin/env bash
    set -euo pipefail

    # Check for uncommitted changes in tree-sitter-styx
    if ! git diff --quiet crates/tree-sitter-styx || ! git diff --cached --quiet crates/tree-sitter-styx; then
        echo "Error: crates/tree-sitter-styx has uncommitted changes. Commit them first."
        exit 1
    fi

    # Get latest commit touching tree-sitter-styx
    latest=$(git log -1 --format=%H -- crates/tree-sitter-styx)

    # Get current rev from extension.toml
    current=$(grep '^rev = ' editors/zed-styx/extension.toml | sed 's/rev = "\(.*\)"/\1/')

    if [ "$latest" = "$current" ]; then
        echo "Grammar rev already up to date: $current"
    else
        echo "Updating grammar rev: $current -> $latest"
        sed "s/^rev = \".*\"/rev = \"$latest\"/" editors/zed-styx/extension.toml > editors/zed-styx/extension.toml.tmp
        mv editors/zed-styx/extension.toml.tmp editors/zed-styx/extension.toml
    fi
