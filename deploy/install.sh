#!/bin/sh
# ufp 在 Alpine 上的一键安装脚本（在服务器上以 root 运行）
#
#   sh install.sh /path/to/ufp x86_64   # 二进制来自 GitHub Release
#
# 做完这些事：
#   1) 建 ufp 用户与目录（/var/lib/ufp、/var/log/ufp）
#   2) 装二进制到 /usr/local/bin/ufp
#   3) 装 OpenRC 服务并设为自启
#   4) 装 nginx（证书用 acme.sh 自己申请，见 deploy/nginx/ufp.conf 的注释）
set -eu

BINARY="${1:-./ufp}"
DOMAIN="${DOMAIN:-}"

if [ ! -f "$BINARY" ]; then
    echo "找不到二进制：$BINARY（用法：sh install.sh /path/to/ufp）" >&2
    exit 1
fi
if [ "$(id -u)" -ne 0 ]; then
    echo "请用 root 运行" >&2
    exit 1
fi

echo "== 建用户与目录"
addgroup -S ufp 2>/dev/null || true
adduser -S -D -H -G ufp -s /sbin/nologin ufp 2>/dev/null || true
install -d -o ufp -g ufp -m 0750 /var/lib/ufp
install -d -o ufp -g ufp -m 0750 /var/log/ufp

echo "== 安装二进制"
install -m 0755 "$BINARY" /usr/local/bin/ufp
/usr/local/bin/ufp version

echo "== 安装部署脚本与 sudoers（在线部署用）"
install -d -o ufp -g ufp -m 0750 /var/lib/ufp/incoming
install -m 0755 "$(dirname "$0")/ufp-apply-deploy" /usr/local/bin/ufp-apply-deploy
printf 'ufp ALL=(root) NOPASSWD: /usr/local/bin/ufp-apply-deploy\n' > /etc/sudoers.d/ufp-deploy
chmod 0440 /etc/sudoers.d/ufp-deploy
command -v sudo >/dev/null 2>&1 || apk add --no-cache sudo >/dev/null

echo "== 安装 OpenRC 服务"
install -m 0755 "$(dirname "$0")/openrc/ufp" /etc/init.d/ufp
rc-update add ufp default >/dev/null 2>&1 || true

echo "== 安装 nginx"
apk add --no-cache nginx >/dev/null
mkdir -p /etc/nginx/certs /var/www/acme
if [ -n "$DOMAIN" ]; then
    sed "s/example.com/$DOMAIN/g" "$(dirname "$0")/nginx/ufp.conf" > /etc/nginx/http.d/ufp.conf
    echo "已写入 /etc/nginx/http.d/ufp.conf（域名：$DOMAIN）"
else
    echo "未设置 DOMAIN，nginx 配置未安装；可手动执行："
    echo "  DOMAIN=你的域名 sh install.sh $BINARY"
fi

echo "== 设置后台密码"
printf '请输入后台管理密码（至少 8 位）：'
/usr/local/bin/ufp set-admin-password

echo
echo "完成。接下来："
echo "  1) 申请证书：acme.sh --issue --nginx -d \$DOMAIN （或 --standalone）"
echo "  2) nginx -t && rc-service nginx start"
echo "  3) rc-service ufp start && tail -f /var/log/ufp/ufp.log"
echo "  4) 浏览器打开 https://\$DOMAIN/admin 添加渠道、key 与条目"
