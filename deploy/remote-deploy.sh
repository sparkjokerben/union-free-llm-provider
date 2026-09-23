#!/bin/sh
# ufp 的服务器端部署脚本：由 GitHub Actions 通过 SSH 调用（root 身份）。
#
# 行为：
#   - 服务器上没有 /etc/init.d/ufp → 首次安装（建用户与目录、装二进制与服务、
#     可选装 nginx 配置），后台密码与证书仍需人工补；
#   - 已有服务 → 不断流升级（新二进制交给 OpenRC 的 upgrade 动作做端口交接）；
#   - 最后一定做健康检查，失败自动回滚到上一版二进制。
#
# 用法：
#   sh remote-deploy.sh <二进制路径>
#
# 环境变量：
#   UFP_DEPLOY_DOMAIN   首次安装时顺便写 nginx 配置用的域名（可选）
#   UFP_DEPLOY_VERSION  部署的版本号，仅用于日志
#   UFP_HEALTH_URL      健康检查地址，默认 http://127.0.0.1:8787/healthz
set -eu

BIN="${1:?用法: remote-deploy.sh <二进制路径>}"
TARGET=/usr/local/bin/ufp
SERVICE=/etc/init.d/ufp
HEALTH="${UFP_HEALTH_URL:-http://127.0.0.1:8787/healthz}"
STAGE=/tmp/ufp-deploy
VERSION="${UFP_DEPLOY_VERSION:-unknown}"

log() { echo "[ufp-deploy] $*"; }

# 服务器上不保证有 curl（Alpine 默认只有 busybox wget）
fetch_health() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsS --max-time 3 "$HEALTH"
  else
    wget -q -O - -T 3 "$HEALTH"
  fi
}

[ "$(id -u)" = "0" ] || { echo "[ufp-deploy] 需要 root 运行" >&2; exit 1; }
[ -f "$BIN" ] || { echo "[ufp-deploy] 找不到二进制：$BIN" >&2; exit 1; }
command -v apk >/dev/null 2>&1 || {
  echo "[ufp-deploy] 这个脚本假设 Alpine Linux（用 apk 装依赖）" >&2
  exit 1
}

FIRST_INSTALL=0
[ -f "$SERVICE" ] || FIRST_INSTALL=1

if [ "$FIRST_INSTALL" = "1" ]; then
  log "首次安装（版本 $VERSION）"
  addgroup -S ufp 2>/dev/null || true
  adduser -S -D -H -G ufp -s /sbin/nologin ufp 2>/dev/null || true
  install -d -o ufp -g ufp -m 0750 /var/lib/ufp /var/log/ufp
  install -m 0755 "$BIN" "$TARGET"
  install -m 0755 "$STAGE/ufp-openrc" "$SERVICE"
  rc-update add ufp default >/dev/null 2>&1 || true

  if [ -n "${UFP_DEPLOY_DOMAIN:-}" ]; then
    log "安装 nginx 配置（域名 $UFP_DEPLOY_DOMAIN）"
    command -v nginx >/dev/null 2>&1 || apk add --no-cache nginx >/dev/null
    mkdir -p /etc/nginx/certs /var/www/acme /etc/nginx/http.d
    sed "s/example.com/$UFP_DEPLOY_DOMAIN/g" "$STAGE/ufp-nginx.conf" \
      > /etc/nginx/http.d/ufp.conf
    nginx -t && (rc-service nginx restart >/dev/null 2>&1 || rc-service nginx start >/dev/null 2>&1 || true)
  else
    log "没给域名：跳过 nginx（之后可手动装 deploy/nginx/ufp.conf）"
  fi

  rc-service ufp start >/dev/null 2>&1 || true
else
  log "升级（版本 $VERSION）：新二进制先接管，旧进程排空"
  if ! cmp -s "$STAGE/ufp-openrc" "$SERVICE"; then
    log "顺带更新 OpenRC 服务脚本"
    install -m 0755 "$STAGE/ufp-openrc" "$SERVICE"
  fi
  install -m 0755 "$BIN" "$TARGET.new"
  if ! UFP_UPGRADE_NO_WAIT=1 rc-service ufp upgrade; then
    log "升级失败，尝试回滚"
    if [ -f "$TARGET.old" ]; then
      install -m 0755 "$TARGET.old" "$TARGET"
    fi
    rc-service ufp restart >/dev/null 2>&1 || true
    exit 1
  fi
fi

# 健康检查：最多等 30 秒
i=0
while [ "$i" -lt 30 ]; do
  if fetch_health > /tmp/ufp-healthz.json 2>/dev/null; then
    log "健康检查通过：$(cat /tmp/ufp-healthz.json)"
    if [ "$FIRST_INSTALL" = "1" ]; then
      cat <<'EOF'
[ufp-deploy] 首次安装完成，还需要你手动做两件事：
  1) 设置后台密码：        ufp set-admin-password
  2) 申请证书并起 nginx：  apk add acme.sh && acme.sh --issue --nginx -d 你的域名
     然后 rc-service nginx start
EOF
    fi
    exit 0
  fi
  i=$((i + 1))
  sleep 1
done

log "健康检查失败"
if [ -f "$TARGET.old" ]; then
  install -m 0755 "$TARGET.old" "$TARGET"
  rc-service ufp restart >/dev/null 2>&1 || true
  log "已回滚到上一版二进制"
else
  log "没有旧二进制可回滚，请手动检查：tail -100 /var/log/ufp/ufp.log"
fi
exit 1
