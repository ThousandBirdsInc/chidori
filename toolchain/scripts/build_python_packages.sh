#!/bin/bash
set -euo pipefail

cd ./chidori
maturin develop --features python
pip install pytest pytest-mock pytest-asyncio
pytest -v ./
cd -