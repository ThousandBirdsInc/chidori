#!/bin/bash

INITIAL_DIR=$(pwd)
GIT_ROOT=$(git rev-parse --show-toplevel)

# Node versions to test
node_versions=(16 18 19 20)

# Get the release version from the last git tag
RELEASE_VERSION=$(python3 $GIT_ROOT/toolchain/scripts/get_target_version.py)

# Iterate over Node versions
for node_version in "${node_versions[@]}"; do

  # Use nvm to switch to desired Node version
  . ~/.nvm/nvm.sh
  nvm install $node_version
  nvm use $node_version

  # Install dependencies
  cd "$GIT_ROOT/toolchain/chidori" || exit
  yarn install

  # Tweak package.json
  python3 -c "import os; import json; p = json.load(open('package.json')); p['version'] = os.environ['RELEASE_VERSION']; json.dump(p, open('package.json', 'w'), indent=2, ensure_ascii=False);"

  # Build
  yarn run build-release

  # Package the asset
  npx node-pre-gyp package

  # Upload to Github releases
  gh release upload "$RELEASE_VERSION" "$(find $GIT_ROOT/toolchain/chidori/build -name chidori-$RELEASE_VERSION-*.tar.gz)" --clobber

done

cd "$INITIAL_DIR" || exit