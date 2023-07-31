#!/bin/bash

GIT_ROOT=$(git rev-parse --show-toplevel)


# Check for the correct number of arguments
if [ "$#" -ne 1 ]; then
  echo "Usage: $0 <new_version>"
  exit 1
fi

# New version number from the command line arguments
NEW_VERSION="$1"

# Hardcoded paths
CARGO_TOML_PATHS=(
  "${GIT_ROOT}/prompt-graph-core/Cargo.toml"
  "${GIT_ROOT}/prompt-graph-exec/Cargo.toml"
  "${GIT_ROOT}/chidori/Cargo.toml"
  "${GIT_ROOT}/prompt-graph-ui/Cargo.toml"
)

# Loop through the paths and update the version numbers
for CARGO_TOML_PATH in "${CARGO_TOML_PATHS[@]}"; do
  # Update all version strings that match the pattern
  sed -i "s/\(prompt-graph-core.*version = \"\)\(\^\?[0-9]*\.[0-9]*\.[0-9]*\"\)/\1^$NEW_VERSION\"/g" "$CARGO_TOML_PATH"
  sed -i "s/\(prompt-graph-exec.*version = \"\)\(\^\?[0-9]*\.[0-9]*\.[0-9]*\"\)/\1^$NEW_VERSION\"/g" "$CARGO_TOML_PATH"
  sed -i "s/\(chidori.*version = \"\)\(\^\?[0-9]*\.[0-9]*\.[0-9]*\"\)/\1^$NEW_VERSION\"/g" "$CARGO_TOML_PATH"
  echo "Versions updated to ^$NEW_VERSION in $CARGO_TOML_PATH"
done
