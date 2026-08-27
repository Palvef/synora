# Synora 配置参考

主配置文件默认在 `/etc/synora/synora.toml`（可用 `-c` 指定）。TOML 格式，
支持 `include`（glob）、`${VAR}` 环境变量展开、SIGHUP / `synora reload` 热重载。

## 顶层

| 配置项 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `include` | `string[]` | — | 引入其他配置文件（glob 通配，相对当前文件目录解析；循环引入会报错） |
| `version` | `int` | 1 | 配置格式版本 |

## `[daemon]`

| 配置项 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `log_dir` | `string` | `/var/log/synora` | 运行日志目录（每 job 一个子目录，`current.log` + 按日归档）。pid 文件在 `/run/synora/`，不在 log_dir |
| `default_proxy` | `string` | — | 探测/工具流量的默认出口代理名（探测走代理，镜像同步默认本机直连） |

### `[daemon.db]`

| 配置项 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `kind` | `"sqlite"`/`"postgres"` | `"sqlite"` | 数据库后端（不可热更） |
| `path` | `string` | — | SQLite 文件路径 |
| `url` | `string` | — | PostgreSQL 连接串（`kind = "postgres"` 时必填） |

## `[api]`

| 配置项 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `listen` | `string` | `127.0.0.1:8100` | 监听地址（不可热更） |
| `synora_json_path` | `string` | `/synora.json` | 自研状态 JSON 输出路径 |
| `tunasync_json_path` | `string` | `/tunasync.json` | tunasync/mirror-web 兼容 JSON 输出路径（裸数组格式） |
| `status_format` | `"synora"`/`"tunasync"`/`"both"` | `"both"` | 选择输出哪种状态 JSON（API 输出格式可选） |

### `[api.tls]`（可选，启用 HTTPS）

| 配置项 | 类型 | 说明 |
|---|---|---|
| `cert` | `string` | TLS 证书（PEM）路径 |
| `key` | `string` | TLS 私钥路径 |
| `client_ca` | `string` | 可选：mTLS 客户端 CA（worker 需要客户端证书） |

### `[[api.tokens]]`（可多个）

| 配置项 | 类型 | 说明 |
|---|---|---|
| `name` | `string` | token 名称（worker 默认用它当 worker id，可被 `[worker] name` 覆盖） |
| `token` | `string` | Bearer token 值 |
| `role` | `"admin"`/`"operator"`/`"viewer"` | 权限组 |
| `permissions` | `string[]` | 附加权限键：`jobs.read`/`jobs.write`/`runs.manage`/`workers.read`/`workers.write`/`logs.read` |

## `[[jobs]]`（每个镜像一个，可多个）

| 配置项 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `name` | `string` | 必填 | 任务名（唯一） |
| `enabled` | `bool` | `true` | 是否启用 |
| `worker` | `string` | — | 指定 worker id 或 worker 组标签（不填 = 按 `resources` 标签自动选） |
| `provider` | `"rsync"`/`"two-stage-rsync"`/`"script"`/`"docker"`/`"git"`/`"http"` | 必填 | 同步方式 |
| `upstream` | `string` | 按 provider | 上游地址（rsync://…、http(s)://…、git 地址、本地路径） |
| `storage` | `string` | 必填 | 仓库存储路径 |
| `mirror_subdir` | `string` | — | tunasync 同名字段：实际存储 = `<storage>/<mirror_subdir>` |
| `schedule` | `"cron"`/`"daily"`/`"weekly"`/`"interval"`/`"startup"`/`"manual"` | 必填 | 调度方式（manual = 手动触发） |
| `cron` | `string` | — | `schedule = "cron"` 时：6 字段 cron 表达式（秒 分 时 日 月 周） |
| `at` | `string` | — | `daily`/`weekly` 的触发时间 `HH:MM` |
| `weekday` | `string` | — | `weekly` 的星期（mon/tue/…） |
| `every` | `string` | — | `interval` 的间隔：`30m`/`6h`/`1d`（不漂移：从固定锚点算） |
| `timezone` | `string` | `UTC` | 任务时区（IANA 名，如 `Asia/Shanghai`；DST 正确） |
| `misfire_policy` | `"skip"`/`"run-immediately"`/`"run-next"` | `"skip"` | 机器离线错过调度点时的策略 |
| `timeout` | `int`/`string` | 不配置 | 可选的单次任务强制停止超时：秒数或 `1m`/`1h`/`1d`；由 Manager 随任务下发给 Worker，未配置时一直等待任务自然完成 |
| `retry` | `int` | 3 | 失败重试次数 |
| `retry_delay` | `string` | `30s` | 首次重试等待 |
| `retry_backoff` | `float` | 2.0 | 退避倍数（封顶 24h） |
| `success_exit_codes` | `int[]` | `[23, 24]` | 视为成功的退出码（tunasync 约定：0 恒成功，23/24 部分错误不失败） |
| `fail_on_match` | `string` | — | 输出匹配该正则即判失败（即使退出码 0） |
| `max_concurrency` | `int` | 1 | 同任务最大并发运行数 |
| `on_worker_lost` | `"retry"`/`"fail"` | `"retry"` | worker 失联（租约过期 → LOST）后的处理 |
| `statistics` | `"provider"`/`"filesystem"` | `"provider"` | 仓库大小统计来源 |
| `resources` | `string[]` | — | 需求标签（匹配 worker labels） |
| `priority` | `int` | 0 | 队列优先级 |
| `proxy` | `string` | — | 该任务使用的代理组名（**镜像同步默认本机直连**） |
| `egress` | `string` | — | 出口地址组名（bind 源地址） |
| `family` | `"ipv4"`/`"ipv6"`/`"any"` | `"any"` | 连接地址族（rsync `--ipv4/--ipv6`） |
| `depends_on` | `string[]` | — | 依赖任务：任一失败则本任务 SKIPPED |
| `memory_limit` | `int`/`string` | — | cgroup v2 内存上限（字节、`512M` 或裸 MB） |
| `cpu_limit` | `float` | — | cgroup v2 CPU 配额（核数，如 2.0） |

### rsync 参数（与 tunasync 一致）

默认 argv：`rsync -aH --delete --delete-delay --delay-updates --safe-links --timeout=120 --stats`；
上游为 `rsync://` 时追加 `--contimeout=120`。

### two-stage-rsync（与 tunasync 的 two-stage-rsync provider 一致）

两遍同步：**阶段 1** 用 profile 过滤器先同步一小部分可发布子集（`-aH --no-o --no-g
--safe-links --stats` + profile 过滤规则，必须退出码 0，失败即任务失败）；**阶段 2**
完整同步（追加 `--delete --delete-after --delay-updates` + job 的 `options`，
`success_exit_codes` 判定适用）。profile 规则取自 tunasync（ftpsync 参考实现）。

| 字段 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `stage1_profile` | `"debian"`/`"debian-oldstyle"` | `"debian"` | 阶段 1 的子集过滤 profile |

| 配置项 | 类型 | 说明 |
|---|---|---|
| `options` | `string[]` | 追加的 rsync 参数（每个元素一个参数） |
| `exclude` | `string[]` | rsync：`--exclude=PATTERN`；HTTP：不遍历、不下载且不删除的根目录相对路径前缀（可带首尾 `/`） |

### script provider

Worker 上始终在 `synora-scripts` 容器内执行（`[worker] scripts_image`，默认 `synora-scripts:latest`）。配置仍是 `provider = "script"`。脚本读 `SYNORA_JOB` / `SYNORA_UPSTREAM` / `SYNORA_STORAGE` / `SYNORA_API`。需要固定镜像 argv 的任务（如 `github-release`、`rubygems`）改用 `provider = "docker"` + `docker_command`。

| 配置项 | 类型 | 说明 |
|---|---|---|
| `command` | `string` | 脚本/命令路径。注入 `SYNORA_JOB/UPSTREAM/STORAGE/LOG_DIR/RUN_ID/API`；输出 `SYNORA_SIZE=123`（字节）、`SYNORA_STATUS=success`、`SYNORA_MESSAGE=…` |

脚本和容器的退出码是最终依据：只有退出码为 0 且没有报告失败状态时才算成功。`SYNORA_STATUS=success` 不能覆盖非零退出码；`SYNORA_STATUS=failed` 则可以将退出码为 0 的运行标记为失败。`success_exit_codes` 仅用于 rsync provider 显式接受部分传输退出码。

### docker provider

| 配置项 | 类型 | 说明 |
|---|---|---|
| `image` | `string` | 镜像（tunasync-scripts 风格：镜像 entrypoint 读环境变量执行） |
| `docker_command` | `string[]` | 可选：容器内命令 argv（空 = 镜像自身 entrypoint） |
| `env` | `string[]` | `"K=V"` 环境变量 |
| `volumes` | `string[]` | `"host:container"` 挂载（storage 默认挂到 `/data`；用户显式挂 `/data` 时不重复挂） |
| `keep_container` | `bool` | `false` 默认 `--rm` |
| `docker_network` | `string` | docker `--network`; `host` 用于绕过 docker0 NAT |

### git provider

Worker 上始终与 script 共用 `synora-scripts` 容器。配置仍是 `provider = "git"`。

| 配置项 | 类型 | 说明 |
|---|---|---|
| `branch` | `string` | 单分支 checkout 模式；不填 = `git clone --mirror` 全引用镜像（更新走 `remote update --prune`） |

### http provider（tsumugu 式目录镜像）

| 配置项 | 类型 | 说明 |
|---|---|---|
| `parser` | `"nginx"`/`"apache"`/`"caddy"`/`"s3"`/`"directory-listing"`/`"fallback"` | 目录列表解析器 |
| `delete` | `bool` | 上游消失的本地文件是否删除 |
| `threads` | `int` | 目录索引请求和文件下载的并发数（tunasync `TUNASYNC_TSUMUGU_THREADS`；默认 5，0 视为 1，最多 64 个并发索引请求） |

行为：按大小+时间判断是否下载、**单文件失败/超时跳过不退出**、**本地软链接不覆盖不写穿**
（tsumugu 语义：同步时忽略软链接）、fancyindex `@` 后缀标记的软链接条目镜像为本地软链接
（列表不提供指向目标时，链接目标为该条目名；幂等，已有同指向软链接则跳过）、
`delete=true` 时多余软链接随文件一起清理、连接超时 30s、空闲读取超时 120s、`.partial` 原子改名；
规划日志每 10 秒记录目录数、条目数、待下载数、索引速度和并发状态；传输日志实时记录下载文件、目标路径、文件大小、耗时、单文件速度以及当前/平均总吞吐。

### `[jobs.hooks]`（可选）

| 配置项 | 类型 | 说明 |
|---|---|---|
| `before_sync` / `after_sync` / `on_success` / `on_failure` | `string` | 钩子命令（注入 `SYNORA_JOB` 等环境变量） |

### `[jobs.safety]`（删除/缩小保护）

| 配置项 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `max_delete_files` | `int` | — | 一次同步最多删除文件数（rsync 转 `--max-delete=N` 提前中止；超出 → 任务失败） |
| `max_delete_ratio` | `float` | — | 删除文件占原文件数比例上限 |
| `max_size_drop_ratio` | `float` | — | 仓库大小跌幅上限（如 0.3 = 缩小超过 30% 判失败） |

### `[jobs.snapshot]`（默认关闭，逐 job 开启）

| 配置项 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `policy` | `"never"`/`"before-sync"`/`"after-success"`/`"before-and-after"`/`"manual"` | `"never"` | 快照策略（仅 ZFS/Btrfs 存储） |

### `[jobs.verify]`（可选）

| 配置项 | 类型 | 说明 |
|---|---|---|
| `mode` | `"path"`/`"size"`/`"command"` | 校验方式 |
| `paths` | `string[]` | `path` 模式校验的文件（存在即可） |
| `min_size` | `int` | `size` 模式的最小字节数 |
| `command` | `string` | `command` 模式校验命令（退出码 0 = 通过） |

## `[proxy.<name>]`（代理，TUI 可注册/添加）

| 配置项 | 类型 | 说明 |
|---|---|---|
| `type` | `"http"`/`"socks5h"`/`"command"`/`"direct"` | 代理类型（CF One/WARP 本地端口 = `socks5h`） |
| `url` | `string` | 代理地址（`http://…` 或 `socks5h://…`） |
| `healthcheck` | `string` | 健康检查 URL |
| `timeout` | `string` | 超时（默认 10s） |
| `expose` | `string` | 暴露本地监听（如 `127.0.0.1:4000` 供其他程序使用） |

## `[proxy_groups.<name>]`

| 配置项 | 类型 | 说明 |
|---|---|---|
| `proxies` | `string[]` | 成员代理名 |
| `strategy` | `"fixed"`/`"failover"`/`"round-robin"`/`"random"` | 选择策略 |

## `[[egress]]` / `[egress_groups.<name>]`（出口地址）

| 配置项 | 类型 | 说明 |
|---|---|---|
| `name` | `string` | 出口名 |
| `address` | `string` | bind 源 IP |
| `probe` | `string` | TCP 探测目标（如 `1.1.1.1:443`） |

## `[storage.<name>]`（可选，不配则 job 的 storage 直接当目录用）

| 配置项 | 类型 | 说明 |
|---|---|---|
| `kind` | `"dir"`/`"zfs"`/`"btrfs"` | 后端 |
| `mountpoint` | `string` | 挂载点（与 job 的 storage 匹配） |
| `auto_create` | `bool` | 不存在时自动创建 |
| `require_empty` | `bool` | 要求为空 |
| `pool` | `string` | zfs 池 |
| `dataset` | `string` | zfs dataset |
| `zfs_options` | 表或字符串 | **zfs create 的 `-o` 参数**（由配置指定）。表形式：`zfs_options = { recordsize = "1M", atime = "off" }`；字符串形式：`zfs_options = "-o recordsize=1M -o xattr=off"`。创建 dataset 时逐一转为 `zfs create -o k=v` 追加执行 |

## `[cgroup]`（可选）

| 配置项 | 类型 | 说明 |
|---|---|---|
| `base_path` | `string` | cgroup 基路径（v2） |

## `[notification]`（可选 webhook）

| 配置项 | 类型 | 说明 |
|---|---|---|
| `webhook_url` | `string` | 失败通知地址（连续失败去重，恢复发 RECOVERED） |
| `alert_after_failures` | `int` | 连续失败 N 次才告警 |

## `[snapshot_retention]`（全局快照保留，可选）

| 配置项 | 类型 | 说明 |
|---|---|---|
| `keep_last` / `keep_daily` / `keep_weekly` / `keep_monthly` | `int` | 各粒度保留数 |

## `[groups.<name>]`（任务组）

| 配置项 | 类型 | 说明 |
|---|---|---|
| `jobs` | `string[]` | 组成员（`synora run-group <name>` 批量触发） |

## `min_free_bytes`（可选）

同步前检查存储剩余空间，低于该值任务 BLOCKED_STORAGE。

## `[worker]`（worker 配置文件专用段）

| 配置项 | 类型 | 说明 |
|---|---|---|
| `name` | `string` | worker 名字（默认用 token 名） |
| `hostname` | `string` | 上报给 manager 的主机名（默认取系统 hostname） |
| `manager` | `string` | manager URL |
| `token` | `string` | API token |
| `labels` | `string[]` | 标签（被 job 的 `worker`/`resources` 匹配） |
| `max_concurrency` | `int` | 最大并发运行数 |
| `ca_cert` | `string` | 可选：校验 manager TLS 的 CA |
| `log_dir` | `string` | 日志目录 |
| `scripts_image` | `string` | git/script 使用的容器镜像，默认 `synora-scripts:latest` |
