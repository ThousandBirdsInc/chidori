#!/bin/bash

# Working directory
working_directory="chidori"

# Node versions to test
node_versions=(16 18 19 20)

# System target
os="macos-11"
target="arm64-apple-darwin"

# Get the release version from the last git tag
release_version=$(git describe --tags $(git rev-list --tags --max-count=1))

# Get release version from args
if [ $# -eq 1 ]; then
  release_version=$1
fi

# Set the release version as environment variable
export RELEASE_VERSION=$release_version

# Iterate over Node versions
for node_version in "${node_versions[@]}"; do

  # Use nvm to switch to desired Node version
  . ~/.nvm/nvm.sh
  nvm install $node_version
  nvm use $node_version

  # Install dependencies
  cd $working_directory
  yarn install

  # Tweak package.json
  python3 -c "import os; import json; p = json.load(open('package.json')); p['version'] = os.environ['RELEASE_VERSION']; json.dump(p, open('package.json', 'w'), indent=2, ensure_ascii=False);"

  # Build
  yarn run build-release

  # Package the asset
  npx node-pre-gyp package

  # Upload to Github releases
  gh release upload $RELEASE_VERSION $(find ./build -name *.tar.gz) --clobber

done

# Publish to npm
npm publish --access public
