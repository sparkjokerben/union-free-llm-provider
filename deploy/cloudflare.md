# Cloudflare 代理 + 在线部署（CI → 网关）

这台服务器是 **IPv6-only**（Vultr 上那种：公网只有 IPv6，v4 是 CGNAT 私网）。这带来两个后果：

1. **GitHub 托管 runner 连不上服务器**：实测 `ubuntu-latest` / `macos-latest` / `windows-latest`
   都没有全局 IPv6 地址与 v6 默认路由（`Network is unreachable`）；
2. **服务器也拉不到 GitHub Release 的资产**：`objects.githubusercontent.com`、
   `release-assets.githubusercontent.com` 在 v6 上不可达（`raw.githubusercontent.com`、`*.github.io` 可达）。

所以部署走 **Cloudflare 代理的 HTTPS**：runner（v4）→ Cloudflare 边缘 → 你的源站（v6）。
Cloudflare 只代理 HTTP/HTTPS，**SSH（22 端口）不走代理**，所以 CI 不再用 SSH，
改成 runner 把发布包 POST 给网关的 `/admin/api/deploy`。

```
git tag v0.1.0 ──▶ Actions 编译(x86_64 + aarch64) ──▶ GitHub Release
                        │
                        └─ POST https://域名/admin/api/deploy  （x-ufp-deploy-token + sha256）
                                    │
                          Cloudflare 边缘（v4）──▶ 源站 nginx（v6）──▶ 网关
                                    │
                          网关验哈希 → 落盘 /var/lib/ufp/incoming/
                                    │
                          sudo ufp-apply-deploy <包> <sha256>（root）
                                    │
                     解包 → 首次安装或不断流升级 → 健康检查 → 失败回滚
```

## 一、DNS 与证书

1. 域名的 NS 交给 Cloudflare，加一条 **AAAA** 记录指向服务器 IPv6，**代理状态开启（橙云）**。
   - 不需要 A 记录（源站没有公网 v4）。
2. 申请 **Cloudflare Origin CA 证书**（免费，有效期 15 年）：
   Cloudflare 面板 → SSL/TLS → Origin Server → Create Certificate →
   Hostnames 填你的域名 → 拿到 certificate 与 private key。
3. 装到服务器：

   ```sh
   mkdir -p /etc/nginx/certs
   # 把面板给的证书与私钥内容贴进这两个文件
   vi /etc/nginx/certs/origin.crt
   vi /etc/nginx/certs/origin.key
   chmod 600 /etc/nginx/certs/origin.key

   # 指向它们（把 deploy/nginx/ufp.conf 里那两行 ssl_certificate* 换掉）
   sed -i 's#/etc/nginx/certs/example.com.crt#/etc/nginx/certs/origin.crt#; \
           s#/etc/nginx/certs/example.com.key#/etc/nginx/certs/origin.key#' \
       /etc/nginx/http.d/ufp.conf
   nginx -t && rc-service nginx restart
   ```

4. Cloudflare 面板 → SSL/TLS → Overview → 模式选 **Full (strict)**。

   用 acme.sh 申请公共证书也可以（`--issue --nginx`，Let's Encrypt 支持 v6 验证），
   只是套了 CF 代理之后没必要多这一层。

## 二、Cloudflare 上要注意的几件事

| 事项 | 说明 |
|---|---|
| **100 秒首字节超时** | CF 免费版对源站有 100 秒首字节限制。网关已经每 15 秒发一次 `ping` 事件（`pingIntervalMs`），长任务不会踩到；**不要把这个值调到 100 秒以上** |
| Bot Fight Mode | 建议关掉（它可能拦 API 请求）；开了 WAF 自定义规则的话，把 `/v1/` 放行 |
| 缓存 | 不要给 `/v1/` 与 `/admin/` 配 Cache Everything，也不要开 Rocket Loader |
| 真实客户端 IP | 走 CF 后 `X-Real-IP` 是 CF 边缘地址，真实 IP 在 `CF-Connecting-IP`（网关优先读它，后台登录限速不受影响） |
| 请求体大小 | CF 免费版单请求最大 100MB，网关自己限 32MB（`UFP_MAX_BODY_BYTES`） |

## 三、在线部署怎么开

1. 服务器上生成部署令牌：

   ```sh
   ufp set-deploy-token      # 打印一次，复制下来
   ```

   （后台「设置 → 在线部署令牌」也能生成/轮换。）

2. 仓库 → Settings → Secrets and variables → Actions，加两个：

   | Secret | 值 |
   |---|---|
   | `DEPLOY_URL` | `https://你的域名`（不要带结尾斜杠） |
   | `DEPLOY_TOKEN` | 上一步的令牌 |

3. 先干跑一次：Actions → **deploy** → Run workflow → 勾上 `dry_run`。
   它只会 `GET /healthz` 验证域名可达与版本号，不推送任何东西。

4. 正式部署：打 tag，或手动跑 **deploy**（默认下发最近一次 Release）。

   ```sh
   git tag v0.1.0 && git push origin v0.1.0
   ```

5. 排查：服务器上 `tail -f /var/lib/ufp/incoming/last-deploy.log`
   （网关收到包 → 触发 → 安装/升级 → 健康检查的完整输出都写在这里）。

## 四、没有网关时怎么装第一版

在线部署依赖「网关已经在跑」，所以**第一次必须手动装**。发布流程会把二进制与辅助文件
推到 **`dist` 分支**（`raw.githubusercontent.com` 由 Fastly 提供，你的服务器在 v6 上实测可达），
所以一条命令就够：

```sh
ssh root@[你的IPv6] \
  'wget -qO- https://raw.githubusercontent.com/sparkjokerben/union-free-llm-provider/main/deploy/bootstrap.sh \
   | UFP_DOMAIN=你的域名 sh'
```

`bootstrap.sh` 会读 `dist` 分支的 `latest.json`、按架构挑二进制、校验 sha256、
拉齐 OpenRC 脚本与 nginx 配置，然后交给 `ufp-apply-deploy` 完成安装
（健康检查失败会自动回滚）。

装完还有两件人工事（脚本结束时会再提醒一次）：

```sh
ssh root@[你的IPv6] 'ufp set-admin-password'      # 后台密码
# 证书按第一节装好 → nginx -t && rc-service nginx start
```

之后打开 `https://你的域名/admin` 确认能进，再回去做第三节的令牌与 Secrets。

> 不想用 bootstrap 也行：本地 `scp` 二进制，再跑 `deploy/install.sh`（见 README 快速开始）。

## 五、SSH 部署（备用）

如果你换了双栈机器、或者给 VPS 加了公网 IPv4，也可以直接从能连 IPv6 的机器 SSH 部署
（CI 里的 runner 不行，原因见文首）：

```sh
tar czf /tmp/deploy.tgz -C upload .        # upload/ 里放 ufp-*、ufp-openrc 等
SHA=$(sha256sum /tmp/deploy.tgz | awk '{print $1}')
scp /tmp/deploy.tgz root@[你的IPv6]:/tmp/
ssh root@[你的IPv6] "/usr/local/bin/ufp-apply-deploy /tmp/deploy.tgz $SHA"
```

`ufp-apply-deploy` 会校验哈希、解包、调用包里的 `remote-deploy.sh` 完成安装或升级，
最后做健康检查，失败自动回滚到 `/usr/local/bin/ufp.old`。
