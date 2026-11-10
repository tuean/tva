#!/usr/bin/env bash
# 开发时直接跑（权限会记在终端 App 上）。
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
exec cargo run -p tva-app "$@"
