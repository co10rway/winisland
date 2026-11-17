#!/usr/bin/env bash
# WinIsland 双机同步一键配置脚本
# 用法：在 Git Bash 里执行  bash setup-git-sync.sh
# 作用：自动探测可用的 GitHub 代理端口，配置本仓库 git 走该代理，并建立与远端的跟踪关系。

REPO="$(cd "$(dirname "$0")" && pwd)"
cd "$REPO" || exit 1

echo "==> 仓库目录: $REPO"

# 1) 探测能连通 github.com 的代理端口
#    优先级：v2rayN 默认 10809/10808，其次系统代理环境变量
CANDIDATES=(10809)   # http 代理端口候选
PROXY_FOUND=""

try_proxy() {
  local p="$1"
  local code
  code=$(curl -sS -o /dev/null -w '%{http_code}' --proxy "http://127.0.0.1:$p" \
         --http1.1 --max-time 8 "https://github.com/" 2>/dev/null)
  if [ "$code" = "200" ]; then
    echo "    ✓ 127.0.0.1:$p 可连通 github.com (HTTP 200)"
    return 0
  fi
  echo "    ✗ 127.0.0.1:$p 不通 (HTTP $code)"
  return 1
}

echo "==> 探测可用代理端口..."
for p in "${CANDIDATES[@]}"; do
  if try_proxy "$p"; then PROXY_FOUND="$p"; break; fi
done

# 若候选都不通，尝试从系统环境变量提取端口
if [ -z "$PROXY_FOUND" ] && [ -n "$https_proxy" ]; then
  sysport=$(echo "$https_proxy" | sed -E 's#.*:([0-9]+)/?$#\1#')
  if [ -n "$sysport" ] && try_proxy "$sysport"; then PROXY_FOUND="$sysport"; fi
fi

if [ -z "$PROXY_FOUND" ]; then
  echo "!! 没找到能连通 github.com 的代理端口。"
  echo "   请打开你的代理软件（确认系统代理或 v2rayN 已启动），然后重跑本脚本。"
  echo "   或手动指定：git config http.proxy http://127.0.0.1:<端口>"
  exit 1
fi

# 2) 固化 git 配置（仅对本仓库生效）
echo "==> 配置 git 使用 http://127.0.0.1:$PROXY_FOUND"
git config http.proxy "http://127.0.0.1:$PROXY_FOUND"
git config http.sslBackend openssl

# 3) 确保 remote 存在
if ! git remote get-url origin >/dev/null 2>&1; then
  echo "==> 添加 origin 远端"
  git remote add origin https://github.com/co10rway/winisland.git
fi

# 4) 拉取远端并建立跟踪
echo "==> fetch 远端"
git fetch origin || { echo "!! fetch 失败，检查代理或网络"; exit 1; }

if git show-ref --verify --quiet refs/remotes/origin/main; then
  git branch --set-upstream-to=origin/main main 2>/dev/null || true
fi

echo ""
echo "==> 完成。当前状态："
git status --short --branch
echo ""
echo "日常同步命令："
echo "  拉取： git pull"
echo "  推送： git push"
echo "  查看： git status"
