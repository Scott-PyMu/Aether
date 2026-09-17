#!/usr/bin/env bash
# M1-06 macOS 同步盘样本生成 / 清理（DoD2；Gate 1 证据辅助脚本）。
#
# 样本目录（设计 A4 macOS 两类 + 本地对照）：
#   ~/Library/CloudStorage/Dropbox
#   ~/Library/CloudStorage/GoogleDrive
#   ~/Library/CloudStorage/OneDrive
#   ~/Library/Mobile Documents（iCloud 容器）
#   ~/Library/Mobile Documents/com~apple~CloudDocs（iCloud Drive 根）
#   ~/AetherTest/local（本地对照，必须放行）
#
# 用法：
#   bash scripts/test/macos_sync_samples.sh --json    # 生成并输出 JSON 清单（CI/归档）
#   bash scripts/test/macos_sync_samples.sh --clean   # 清理（仅空目录；rmdir 不删非空）
#
# 约束：只创建/删除目录，不写入数据文件；不触碰任何已有内容的目录。
set -eu

MODE_JSON=0
MODE_CLEAN=0
for arg in "$@"; do
  case "$arg" in
    --json) MODE_JSON=1 ;;
    --clean) MODE_CLEAN=1 ;;
    *)
      echo "未知参数：$arg（支持 --json / --clean）" >&2
      exit 2
      ;;
  esac
done

: "${HOME:?缺少 HOME，无法定位样本目录}"

cloudstorage="${HOME}/Library/CloudStorage"
icloud="${HOME}/Library/Mobile Documents"
icloud_drive="${icloud}/com~apple~CloudDocs"
local_control="${HOME}/AetherTest/local"

if [ "${MODE_CLEAN}" = "1" ]; then
  # rmdir 仅删除空目录：任何已存在内容的目录（含真实同步盘）不会被触碰。
  rmdir "${local_control}" 2>/dev/null || true
  rmdir "${HOME}/AetherTest" 2>/dev/null || true
  rmdir "${cloudstorage}/Dropbox" 2>/dev/null || true
  rmdir "${cloudstorage}/GoogleDrive" 2>/dev/null || true
  rmdir "${cloudstorage}/OneDrive" 2>/dev/null || true
  rmdir "${icloud_drive}" 2>/dev/null || true
  rmdir "${icloud}" 2>/dev/null || true
  rmdir "${cloudstorage}" 2>/dev/null || true
  echo "[macos-sync-samples] 清理完成（仅删除空目录）"
  exit 0
fi

mkdir -p "${cloudstorage}/Dropbox" "${cloudstorage}/GoogleDrive" "${cloudstorage}/OneDrive"
mkdir -p "${icloud_drive}"
mkdir -p "${local_control}"

if [ "${MODE_JSON}" = "1" ]; then
  cat <<JSON
{
  "platform": "macos",
  "home": "${HOME}",
  "samples": [
    { "id": "cloudstorage.dropbox", "path": "${cloudstorage}/Dropbox", "kind": "sync_hit" },
    { "id": "cloudstorage.googledrive", "path": "${cloudstorage}/GoogleDrive", "kind": "sync_hit" },
    { "id": "cloudstorage.onedrive", "path": "${cloudstorage}/OneDrive", "kind": "sync_hit" },
    { "id": "icloud.mobile-documents", "path": "${icloud}", "kind": "sync_hit_degraded" },
    { "id": "icloud.cloud-docs", "path": "${icloud_drive}", "kind": "sync_hit_degraded" },
    { "id": "local.control", "path": "${local_control}", "kind": "local_allow" }
  ]
}
JSON
else
  echo "[macos-sync-samples] 样本目录："
  echo "  ${cloudstorage}/Dropbox"
  echo "  ${cloudstorage}/GoogleDrive"
  echo "  ${cloudstorage}/OneDrive"
  echo "  ${icloud}"
  echo "  ${icloud_drive}"
  echo "  ${local_control}"
fi
