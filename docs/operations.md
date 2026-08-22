# Synora 操作手册

日常操作都在这里：如何注册 CF One / WARP、如何添加代理、TUI 怎么用、
常用命令对照表、systemd 部署步骤。

## 1. 注册 CF One / WARP（加入 manager）

### 自动注册（推荐）

1. 在本机安装 Cloudflare WARP / CF One 客户端并启动（它会在本机监听一个
   本地 SOCKS5 端口，常见 40000）。
2. 运行 TUI：`synora tui -c /etc/synora/synora.toml`。
3. **TUI 启动时会自动探测本机常见本地代理端口**（40000/40001/1080/10808/
   7890/7891/2080/8899），发现 WARP 端口后自动写入配置 `[proxy.cf-warp]`
   并调用 reload 加入 manager——无需任何手动操作。
4. 也可以进 Proxies 面板（F3）按 `w` 手动触发一次注册。

> **远端 worker 要用 manager 的代理出口时，`[proxy.<name>]` 的 `expose`
> 必须监听 LAN 地址**（如 `0.0.0.0:4000`，配 `expose_auth` 用户名:密码做
> 认证）；只听 127.0.0.1 的话其他机器的 worker 连不上。

注册完成后 `curl -H "Authorization: Bearer <token>" http://<manager>/api/v1/proxies`
可以看到 cf-warp 的延迟（latency_ms）、出口 IP（egress_ip）、健康状态。

> 探测是每 30s 一轮：延迟 = 通过代理请求探测页的耗时；出口 IP = 代理对外
> 显示的地址（探测页回显）。`expose` 可以把本地 WARP 端点转发暴露成
> 其他地址供机器上的程序使用（如 `0.0.0.0:4000`）。

### 手动注册（等效）

在 `/etc/synora/synora.toml` 里加一段，然后 reload：

```toml
[proxy.cf-warp]
type = "socks5h"
url = "socks5h://127.0.0.1:40000"     # WARP 本地端口
healthcheck = "https://cloudflare.com/cdn-cgi/trace"
expose = "127.0.0.1:4000"              # 可选：暴露端口
```

```sh
synora reload -c /etc/synora/synora.toml   # 或 systemctl reload synora-manager
```

## 2. 添加代理（http / socks5h）

**TUI**：F3 进 Proxies 面板 → 按 `a` → 依次输入：名字 → 地址
（`http://…` 或 `socks5h://…`）→ 可选暴露端口 → 回车即写入配置并热重载。

**配置文件**（与 TUI 等价）：

```toml
[proxy.<名字>]
type = "http"            # 或 "socks5h"
url = "http://1.2.3.4:8080"
healthcheck = "http://www.gnu.org/"
timeout = "10s"
expose = "127.0.0.1:8081"   # 可选
```

多个代理可以编组：

```toml
[proxy_groups.default]
proxies = ["cf-warp", "backup"]
strategy = "failover"    # fixed / failover / round-robin / random
```

## 3. 镜像同步默认直连（重要）

**镜像同步默认使用本机网络直连**，不走任何代理——代理只用于探测与工具
流量。要指定出口行为，在 job 里配：

```toml
[[jobs]]
name = "example"
# ... provider/upstream/storage ...
family = "ipv4"            # ipv4 / ipv6 / any（默认 any）
egress = "eth0-out"        # 指定 bind 源地址（对应 [[egress]] 段）
proxy = "default"          # 显式指定才走代理组（一般不需要）
```

## 4. TUI 操作键位

| 键 | 作用 | 所在面板 |
|---|---|---|
| F1 / F2 / F3 / F5 | 任务 / Worker / 代理 / 日志 | 全局 |
| ↑ ↓ | 选择 | Jobs/Workers |
| Enter | 查看任务详情（最近运行/耗时/大小/错误信息） | Jobs |
| Esc | 返回任务列表 | 任务详情 |
| r / s | 触发运行 / 停止 | Jobs、任务详情 |
| / | 搜索过滤任务（按名字） | Jobs |
| n | 新建任务（表单：名字 → provider（1-5 选择或手输）→ upstream → storage → schedule（1-4 选择或手输）→ 回车创建并热重载） | Jobs |
| a | 添加代理（表单：名字 → 地址 → 可选暴露端口） | Proxies |
| w | 注册 CF One / WARP（自动探测本地端口） | Proxies |
| e | 打开配置文件编辑器 | Proxies |
| F6 | 配置文件编辑器（整个 /etc/synora 配置：主配置、jobs/*.toml、worker.toml）——方向键移动光标、直接输入编辑、Tab 切换文件、S 保存（manager 配置自动 reload；worker.toml 保存后重启 worker 生效）、Esc 退出 | 全局 |
| q / F10 | 退出 | 全局 |

底部提示栏只显示当前面板可用的按键。

## 5. 常用命令对照

| 操作 | 命令 |
|---|---|
| 校验配置 | `synora check -c /etc/synora/synora.toml`（报错带 文件:行号） |
| 单机运行 | `synora start -c /etc/synora/synora.toml` |
| 分布式 manager | `synora-manager -c /etc/synora/synora.toml`（systemd 部署） |
| worker | `synora-worker -c /etc/synora/worker.toml`（每台机器一个，可 `[worker] name` 起名） |
| 手动触发一个任务 | `synora run <job>` / `synora job start <job>` / `synora job start -f <job>`（-f 先停再启）或 API `POST /api/v1/jobs/<job>/run` |

> **手动触发 = 实时开始**：手动 run 的任务带高优先级，直接插到排队任务前，
> worker 空闲时 2 秒一次心跳领取，基本是即点即跑，不需要等积压队列。
| 重启任务 | `synora job restart <job>`（停 + 启） |
| 触发一组任务 | `synora run-group <group> -c ...` |
| 停止运行中的任务 | `synora stop <job>` 或 API `POST /api/v1/jobs/<job>/stop` |
| 热重载 | `synora reload`（走 manager API；pid 在 `/run/synora/`）或 `POST /api/v1/reload`（**变更的 job 自动补跑一次**） |
| 查看状态 | `synora status -c ...` |
| 查看日志 | `synora logs <job> --lines 100`（API：`GET /api/v1/jobs/<job>/logs?tail=100`） |
| 任务历史 | API `GET /api/v1/jobs/<job>/history` |
| 快照列表 | `synora snapshot list <job>` |
| 快照回滚 | `synora snapshot rollback <job> <snapshot>` |
| worker 列表 | `synora worker list --manager http://<manager> --token <token>` |
| worker 停接新任务 | `synora worker retire <id> --manager ... --token ...` |
| TUI | `synora tui -c /etc/synora/synora.toml`（自动加载 /etc/synora 配置） |

所有命令同时支持 `--` 风格：`synora --check`、`synora --status`、
`synora --run <job>`、`synora --stop <job>`、`synora --logs <job>`、
`synora --reload` 与子命令形式完全等价。

## 6. systemd 部署（生产）

```sh
# 安装二进制
install -m 0755 synora synora-manager synora-worker synora-tui /usr/local/bin/

# 配置
mkdir -p /etc/synora/jobs /var/lib/synora /var/log/synora
#   /etc/synora/synora.toml   — manager 配置（jobs include 进来）
#   /etc/synora/worker.toml   — worker 配置（[worker] 段）

# systemd（unit 在 deploy/systemd/）
cp deploy/systemd/synora-manager.service /etc/systemd/system/
cp deploy/systemd/synora-worker.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now synora-manager
systemctl enable --now synora-worker   # 每台执行机器

# 日常
systemctl reload synora-manager        # SIGHUP → 热重载配置
journalctl -u synora-manager -f        # 看服务日志
tail -f /var/log/synora/<job>/current.log   # 看某镜像同步日志
```

## 7. 与 tunasync 并行（迁移期）

Synora 与 tunasync 可以同时在一台机器上运行：

- tunasync worker 照常跑它自己的调度；
- synora worker 以另一个进程注册到 synora manager；
- 同一存储目录不要同时让两者写：建议先只把 tunasync 上 **failed 的、
  没有 interval 的（manual）作业**接到 synora 试跑，确认正常后再逐步切换；
- 迁移工具：`python3 scripts/tunasync2synora.py <workers.conf> -o <out>`
  生成的配置用 `synora check` 校验后放入 `/etc/synora/jobs/`。

## 7.5 使用 ZFS 存储时 mirror_dir / storage 的写法

ZFS 后端按「池/数据集」创建并挂载，镜像目录直接落在数据集挂载点之下：

```toml
[storage.mirror]
kind = "zfs"
pool = "data"                    # zpool 名（已有的池）
dataset = "mirror"               # dataset 名（不存在时自动创建）
mountpoint = "/datas"            # 该数据集的挂载点（zfs set mountpoint）
auto_create = true               # dataset 不存在时执行 zfs create
zfs_options = "-o recordsize=1M -o xattr=off -o atime=off -o setuid=off \
               -o exec=off -o devices=off -o sync=disabled \
               -o secondarycache=metadata -o redundant_metadata=most"
```

对应的 job：

```toml
[[jobs]]
name = "debian-security"
storage = "/datas/debian-security"   # = mountpoint + 镜像目录名
# 或者用 tunasync 的写法：
# storage = "/datas"                # mountpoint
# mirror_subdir = "debian-security" # 实际目录 = /datas/debian-security
```

规则：

- `storage` 写**完整路径**（挂载点 + 镜像目录名）；或 `storage` 写挂载点、
  `mirror_subdir` 写镜像目录名，两者等价。
- dataset 不存在且 `auto_create = true` 时，首次运行会执行
  `zfs create <zfs_options> <pool>/<dataset>`，再 `zfs set
  mountpoint=<mountpoint>`；`auto_create = false` 且 dataset 不存在则任务失败。
- 快照在 dataset 级：`zfs snapshot <pool>/<dataset>@synora-<时间戳>`，
  回滚用 `synora snapshot rollback <job> <快照名>`。
- 多个镜像可共用一个 dataset（都在 mountpoint 下各自建目录），也可以
  每镜像一个 dataset（为每个镜像单独配一个 `[storage.xxx]` 段）。
- 多 Worker 时每个节点用不同的 `[storage.<name>]`。ZFS HDD 节点用
  `[storage.mirror]`（`/datas`），NVMe/web 节点用 `[storage.nvme]`
  （`/data`）。Job 里写 `storage_name = "nvme"` 并 `worker = "worker-nvme"`，
  避免两个节点都叫 `mirror` 时任务写到错误的池。

## 8. 监控

- Prometheus 抓取 `<manager>:<port>/metrics`——**该端点免认证**，只应暴露在
  内网（listen 默认 127.0.0.1；若配 0.0.0.0 请用防火墙限制来源）；
  指标名见 README。
- Grafana 导入 `deploy/grafana-synora.json`（uid synora-mirror）：
  「所有任务状态」表（状态/是否成功/失败/重试/耗时/传输量/仓库大小/
  下次运行/上次同步）+ 带宽曲线 + Worker 状态与负载 + CPU/内存。
- 注意：Prometheus 会保留 `job`/`instance` 标签，任务名在查询里是
  `exported_job`（面板已处理）。
