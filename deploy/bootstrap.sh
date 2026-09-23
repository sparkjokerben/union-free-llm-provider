#!/bin/sh
# ufp 在服务器上的一键安装 / 升级（从 dist 分支拉取，绕开 IPv6-only 拉不到 Release 资产的问题）。
#
# 用法（在服务器上以 root 执行）：
#
#   wget -qO- https://raw.githubusercontent.com/<owner>/<repo>/main/deploy/bootstrap.sh \
#     | UFP_DOMAIN=你的域名 sh
#
# 它会：
#   1) 从 dist 分支读 latest.json（Fastly，IPv6 可达）；
#   2) 按 uname -m 挑二进制，校验 sha256；
#   3) 拉取 ufp-apply-deploy / remote-deploy.sh / OpenRC 脚本 / nginx 配置；
#   4) 打成一个包交给 ufp-apply-deploy 完成安装或升级（含健康检查与回滚）。
#
# 可用环境变量：
#   UFP_DOMAIN   首次安装时写 nginx 配置的域名（可选）
#   UFP_REPO     仓库，默认 sparkjokerben/union-free-llm-provider
#   UFP_BASE     自定义 dist 基地址（默认 raw.githubusercontent.com/<repo>/dist）
set -eu

REPO="${UFP_REPO:-sparkjokerben/union-free-llm-provider}"
# 二进制与清单走 dist 分支（发布产物）；部署脚本走 main（改脚本不必重新打 tag）
BASE="${UFP_BASE:-https://raw.githubusercontent.com/$REPO/dist}"
SCRIPTS_BASE="${UFP_SCRIPTS_BASE:-https://raw.githubusercontent.com/$REPO/main/deploy}"
DOMAIN="${UFP_DOMAIN:-}"
WORK=$(mktemp -d /tmp/ufp-boot.XXXXXX)
trap 'rm -rf "$WORK"' EXIT INT TERM

log() { echo "[ufp-bootstrap] $*"; }

fetch() {
  # $1 = url, $2 = 输出文件
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --max-time 60 "$1" -o "$2"
  else
    wget -q -O "$2" "$1"
  fi
}

[ "$(id -u)" = "0" ] || { echo "[ufp-bootstrap] 需要 root 运行" >&2; exit 1; }
command -v apk >/dev/null 2>&1 || { echo "[ufp-bootstrap] 只支持 Alpine（apk）" >&2; exit 1; }

# 1) 版本清单
log "读取 $BASE/latest.json"
fetch "$BASE/latest.json" "$WORK/latest.json" || {
  echo "[ufp-bootstrap] 拉不到 latest.json：先确认仓库已经打过 tag（dist 分支由发布流程生成）" >&2
  exit 1
}
TAG=$(sed -n 's/.*"tag"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$WORK/latest.json" | head -1)
[ -n "$TAG" ] || { echo "[ufp-bootstrap] latest.json 里没有 tag" >&2; exit 1; }
log "目标版本：$TAG"

# 2) 按架构挑二进制
case "$(uname -m)" in
  x86_64)  ARCH=x86_64-unknown-linux-musl ;;
  aarch64) ARCH=aarch64-unknown-linux-musl ;;
  *) echo "[ufp-bootstrap] 不支持的架构：$(uname -m)" >&2; exit 1 ;;
esac
BIN="ufp-$ARCH"
log "下载 $BIN"
fetch "$BASE/binaries/$BIN" "$WORK/$BIN"

# 3) 校验 sha256（清单里按文件名索引）
EXPECT=$(python3 - "$WORK/latest.json" "$BIN" <<'PY' 2>/dev/null || true
import json, sys
data = json.load(open(sys.argv[1]))
print(data["files"].get(sys.argv[2], {}).get("sha256", ""))
PY
)
if [ -z "$EXPECT" ]; then
  # 没有 python3 就退化成从 JSON 里粗糙地 grep
  EXPECT=$(grep -A2 "\"$BIN\"" "$WORK/latest.json" | sed -n 's/.*"sha256"[[:space:]]*:[[:space:]]*"\([0-9a-f]*\)".*/\1/p' | head -1)
fi
ACTUAL=$(sha256sum "$WORK/$BIN" | awk '{print $1}')
if [ -n "$EXPECT" ] && [ "$EXPECT" != "$ACTUAL" ]; then
  echo "[ufp-bootstrap] sha256 不匹配：期望 $EXPECT 实际 $ACTUAL" >&2
  exit 1
fi
[ -n "$EXPECT" ] && log "sha256 校验通过"
mv "$WORK/$BIN" "$WORK/ufp-$ARCH"

# 4) 拉齐其余文件（脚本以 main 为准，二进制必须用发布产物）
for f in ufp-apply-deploy remote-deploy.sh; do
  fetch "$SCRIPTS_BASE/$f" "$WORK/$f"
done
fetch "$SCRIPTS_BASE/openrc/ufp" "$WORK/ufp-openrc"
fetch "$SCRIPTS_BASE/nginx/ufp.conf" "$WORK/ufp-nginx.conf"
chmod +x "$WORK/ufp-"* "$WORK/remote-deploy.sh"

# 5) 打成一个包，交给 apply 脚本（它会自己更新自己）
log "打包并应用"
tar czf "$WORK/deploy.tgz" -C "$WORK" \
  "ufp-$ARCH" ufp-apply-deploy remote-deploy.sh ufp-openrc ufp-nginx.conf
install -m 0755 "$WORK/ufp-apply-deploy" /usr/local/bin/ufp-apply-deploy
UFP_DEPLOY_VERSION="$TAG" UFP_DEPLOY_DOMAIN="$DOMAIN" \
  /usr/local/bin/ufp-apply-deploy "$WORK/deploy.tgz" "$(sha256sum "$WORK/deploy.tgz" | awk '{print $1}')"
