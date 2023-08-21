#!/bin/bash
set -euo pipefail

rootpath=$(git rev-parse --show-toplevel)/toolchain
projectpath=${rootpath}/prompt-graph-ui
protoc --plugin=${projectpath}/node_modules/.bin/protoc-gen-ts_proto --ts_proto_opt=esModuleInterop=true --ts_proto_out=${projectpath}/src/protobufs  ${rootpath}/prompt-graph-core/protobufs/DSL_v1.proto --proto_path=${rootpath}/prompt-graph-core/protobufs
