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
#   sh remote-deploy.sh <舞台目录>
#
# 舞台目录里应有：ufp、ufp-openrc、ufp-nginx.conf（由 ufp-apply-deploy 从发布包里解开）。
#
# 环境变量：
#   UFP_DEPLOY_DOMAIN   首次安装时顺便写 nginx 配置用的域名（可选）
#   UFP_DEPLOY_VERSION  部署的版本号，仅用于日志
#   UFP_HEALTH_URL      健康检查地址，默认 http://127.0.0.1:8787/healthz
set -eu

STAGE="${1:?用法: remote-deploy.sh <舞台目录>}"
# 发布包里两个架构都带着（CI 推给服务器时没法预先探测架构），这里按本机挑
case "$(uname -m)" in
  x86_64)  BIN="$STAGE/ufp-x86_64-unknown-linux-musl" ;;
  aarch64) BIN="$STAGE/ufp-aarch64-unknown-linux-musl" ;;
  *)       BIN="" ;;
esac
[ -n "$BIN" ] && [ -f "$BIN" ] || BIN="$STAGE/ufp"
TARGET=/usr/local/bin/ufp
APPLY=/usr/local/bin/ufp-apply-deploy
SERVICE=/etc/init.d/ufp
SUDOERS=/etc/sudoers.d/ufp-deploy
HEALTH="${UFP_HEALTH_URL:-http://127.0.0.1:8787/healthz}"
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
[ -f "$BIN" ] || { echo "[ufp-deploy] 找不到适合 $(uname -m) 的二进制（舞台目录：$STAGE）" >&2; exit 1; }
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
  # 网关收部署包的目录（它以 ufp 用户运行，只能写自己的目录）
  install -d -o ufp -g ufp -m 0750 /var/lib/ufp/incoming
  install -m 0755 "$BIN" "$TARGET"
  install -m 0755 "$STAGE/ufp-apply-deploy" "$APPLY"
  install -m 0755 "$STAGE/ufp-openrc" "$SERVICE"
  # 允许 ufp 用户只免密运行这一个脚本（在线部署用）
  command -v sudo >/dev/null 2>&1 || apk add --no-cache sudo >/dev/null 2>&1 || true
  printf 'ufp ALL=(root) NOPASSWD: %s\n' "$APPLY" > "$SUDOERS"
  chmod 0440 "$SUDOERS"
  rc-update add ufp default >/dev/null 2>&1 || true

  if [ -n "${UFP_DEPLOY_DOMAIN:-}" ]; then
    log "安装 nginx 配置（域名 $UFP_DEPLOY_DOMAIN）"
    command -v nginx >/dev/null 2>&1 || apk add --no-cache nginx >/dev/null
    mkdir -p /etc/nginx/certs /var/www/acme /etc/nginx/http.d
    # 没有证书时先放一张自签占位证书：nginx 能立刻起来、链路可以本地验证；
    # 接了 Cloudflare 之后要换成 Origin CA 证书（否则 Full (strict) 会握手失败）。
    if [ ! -f "/etc/nginx/certs/$UFP_DEPLOY_DOMAIN.crt" ]; then
      command -v openssl >/dev/null 2>&1 || apk add --no-cache openssl >/dev/null 2>&1 || true
      if openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
           -keyout "/etc/nginx/certs/$UFP_DEPLOY_DOMAIN.key" \
           -out "/etc/nginx/certs/$UFP_DEPLOY_DOMAIN.crt" \
           -subj "/CN=$UFP_DEPLOY_DOMAIN" >/dev/null 2>&1; then
        chmod 600 "/etc/nginx/certs/$UFP_DEPLOY_DOMAIN.key"
        log "已生成自签占位证书；接 Cloudflare 前换成 Origin CA 证书"
      else
        log "自签证书生成失败（缺 openssl），请自行放证书到 /etc/nginx/certs/"
      fi
    fi
    sed "s/example.com/$UFP_DEPLOY_DOMAIN/g" "$STAGE/ufp-nginx.conf" \
      > /etc/nginx/http.d/ufp.conf
    if nginx -t >/dev/null 2>&1; then
      rc-service nginx restart >/dev/null 2>&1 || rc-service nginx start >/dev/null 2>&1 || true
      log "nginx 已载入配置"
    else
      log "nginx 配置已写入 /etc/nginx/http.d/ufp.conf，但校验没过（通常是没有证书）"
      log "把证书放到 /etc/nginx/certs/ 后再执行：nginx -t && rc-service nginx start"
    fi
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
  if [ -f "$STAGE/ufp-apply-deploy" ] && ! cmp -s "$STAGE/ufp-apply-deploy" "$APPLY"; then
    install -m 0755 "$STAGE/ufp-apply-deploy" "$APPLY"
  fi
  # 已有 nginx 配置：把新模板同步过去（域名从现有配置里读，别被 example.com 覆盖）。
  # 校验不过就回滚旧文件，绝不把 nginx 搞挂。
  NGINX_CONF=/etc/nginx/http.d/ufp.conf
  if [ -f "$NGINX_CONF" ] && [ -f "$STAGE/ufp-nginx.conf" ]; then
    CUR_DOMAIN=$(sed -n 's/^[[:space:]]*server_name[[:space:]]\+\([^;]*\);.*/\1/p' "$NGINX_CONF" | head -1)
    TMP=$(mktemp)
    sed "s/example.com/$CUR_DOMAIN/g" "$STAGE/ufp-nginx.conf" > "$TMP"
    if ! cmp -s "$TMP" "$NGINX_CONF"; then
      cp -a "$NGINX_CONF" "$TMP.bak"
      mv "$TMP" "$NGINX_CONF"
      if nginx -t >/dev/null 2>&1; then
        log "已更新 nginx 配置（域名 $CUR_DOMAIN）"
        rc-service nginx reload >/dev/null 2>&1 || rc-service nginx restart >/dev/null 2>&1 || true
      else
        log "新 nginx 配置校验没过，回滚到旧文件"
        mv "$TMP.bak" "$NGINX_CONF"
      fi
      rm -f "$TMP.bak"
    fi
    rm -f "$TMP"
  fi
  install -m 0755 "$BIN" "$TARGET.new"
  # 换二进制 + 重启（旧进程排空在途请求，最多 30 秒，见 OpenRC 脚本里的 UFP_DRAIN_SECONDS）
  install -m 0755 "$TARGET.new" "$TARGET"
  if ! rc-service ufp restart; then
    log "升级失败，尝试回滚"
    if [ -f "$TARGET.old" ]; then
      install -m 0755 "$TARGET.old" "$TARGET"
    fi
    rc-service ufp restart >/dev/null 2>&1 || true
    exit 1
  fi
fi

# 健康检查：最多等 90 秒（升级时旧进程可能还在排空在途请求）
i=0
while [ "$i" -lt 90 ]; do
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
