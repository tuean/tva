#!/usr/bin/env bash
# 下载本地 whisper 模型到 ~/Library/Application Support/tva/models/
#
# 用法: ./scripts/download-model.sh [base|small|medium|large-v3-turbo]
#
# 中文效果大致是 small < large-v3-turbo；base 最省资源但错字明显。
set -euo pipefail

SIZE="${1:-small}"
DIR="$HOME/Library/Application Support/tva/models"
mkdir -p "$DIR"

case "$SIZE" in
  base)            FILE="ggml-base.bin";;
  small)           FILE="ggml-small.bin";;
  medium)          FILE="ggml-medium.bin";;
  large-v3-turbo)  FILE="ggml-large-v3-turbo-q5_0.bin";;
  *) echo "未知模型: $SIZE（可选 base | small | medium | large-v3-turbo）"; exit 2;;
esac

URL="https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main/$FILE"
echo "下载 $FILE -> $DIR"
curl -L --progress-bar -o "$DIR/$FILE" "$URL"
echo "完成: $DIR/$FILE"
echo
echo "在配置里启用："
echo "  ~/Library/Application Support/tva/config.toml"
echo "  [asr]"
echo "  engine = \"whisper\""
echo "  model_path = \"$DIR/$FILE\""
