#!/bin/bash
set -euo pipefail

cd ./chidori
npm run build
npm run test-js
cd -