# Synora

Synora 是一个用 Rust 编写的**镜像同步引擎**：统一管理「什么时候同步、在哪台机器同步、
用什么工具同步、同步到哪、如何记录与恢复」。设计参考
[tunasync](https://github.com/tuna/tunasync)（清华大学 TUNA）、
[Yuki](https://github.com/ustclug/yuki)（中国科学技术大学 LUG）与
[tsumugu](https://github.com/taoky/tsumugu)，并采用了它们在生产环境中的成熟做法：

- **tunasync**：Manager + Worker 架构、rsync 参数约定（`success_exit_codes` 23/24、
  `--safe-links --timeout=120` 等）、脚本环境变量兼容（`TUNASYNC_*`）、
  manager/worker 之间明文或证书加密、`mirror_subdir`、tunasync.json 兼容输出、
  感谢 TUNA 的 tunasync / tunasync-scripts
- **Yuki**：SQLite 存储、`yukictl reload` 式热重载、每仓库 cron 调度
- **tsumugu**：HTTP 目录镜像的容错语义（单文件失败跳过、忽略软链接、30s 单请求超时）

> 本项目为 tunasync / Yuki / tsumugu 的再实现与演进，在此向上述项目的作者与维护者
> 致以感谢。

## 特性

- **不漂移调度**：cron / 每日 / 每周 / 间隔 / 启动 / 手动；间隔从固定锚点计算，
  重启不漂移；任务级时区（DST 正确）；离线错过调度点按 misfire 策略处理
- **Manager + N Worker**：worker 拉取模型（心跳即领任务，NAT 友好）；
  租约 + 收割器保证**任务永不永久卡死**（LOST → 按 `on_worker_lost` 重派）
- **六种同步方式**：rsync（参数与 tunasync 一致）、two-stage-rsync（tunasync 两遍同步：
  stage1 子集先发布 + stage2 全量）、script / git（worker 上始终在 `synora-scripts` 容器里跑，`SYNORA_*` 环境变量）、
  docker、git / script（worker 上默认跑在 `synora-scripts` 容器里）、HTTP 目录镜像（tsumugu 式）
- **删除/缩小保护**：`max_delete_files` / `max_delete_ratio` / `max_size_drop_ratio`
  同步前后校验，异常直接判失败
- **存储**：普通目录 / ZFS（dataset 创建参数由配置指定）/ Btrfs；快照 + 保留策略
  （默认关闭，逐 job 开启）；剩余空间保护
- **代理与出口**：http / socks5h / command / direct；延迟 + 出口 IP 探测；
  默认 Cloudflare 出口；镜像同步默认本机直连，可指定 ipv4/ipv6 与 bind 地址
- **cgroup v2 资源限制**（memory.max / cpu.max）与 Docker 资源限制
- **热重载**：SIGHUP / `synora reload` / API；**变更的 job 自动补跑一次**
- **TUI 控制台**：任务列表（搜索过滤）、任务详情（最近运行/耗时/大小/错误）、
  日志跟随选中任务、Worker 面板、代理面板（**自动注册 CF One / WARP**、手动添加代理）、
  新建任务（provider/调度可选可手输）
- **REST API**：Bearer token + RBAC；明文或 TLS/mTLS（tunasync 模式：manager 可选
  `ssl_cert/ssl_key`，worker 配 `ca_cert`）
- **监控**：Prometheus 指标 + Grafana 面板（任务状态/成功与否/失败/重试/耗时/带宽/
  传输量/仓库大小/下次运行/Worker 状态与负载/CPU/内存）
- **迁移工具**：`tunasync2synora.py`（tunasync workers.conf）、`yuki2synora.py`
  （Yuki repo YAML）一键迁移
- **tunasync.json 兼容**：状态 JSON 与 mirror-web 前端直接对接

## 快速开始

```sh
# 校验配置（报错带 文件:行号）
synora check -c /etc/synora/synora.toml

# 单机模式：调度器 + 执行器 + 指标端点一体
synora start -c /etc/synora/synora.toml

# 分布式模式（生产推荐）
synora-manager -c /etc/synora/synora.toml     # 本机 manager
synora-worker  -c /etc/synora/worker.toml     # 每台机器一个 worker

# 操作
synora tui -c /etc/synora/synora.toml         # 控制台
synora run <job> / synora run-group <group>   # 手动触发（实时开始，不排队）
synora stop <job> / synora reload             # 取消 / 热重载
synora status / synora logs <job>             # 状态 / 日志
synora snapshot list|rollback <job> ...       # 快照
synora worker list --manager URL --token T    # worker 管理
```

systemd（`deploy/systemd/`）：`synora-manager.service` 与 `synora-worker.service`，
装到 `/etc/systemd/system/` 后 `systemctl enable --now synora-manager`。

## 配置

逐项解析见 [docs/config-reference.md](docs/config-reference.md)；生产示例见
[examples/production.toml](examples/production.toml)；日常操作（注册 CF One /
WARP、添加代理、TUI 键位、命令对照、systemd 部署、与 tunasync 并行）见
[docs/operations.md](docs/operations.md)。

## 指标（Prometheus）

`synora_job_status`（0-11 状态映射）、`synora_job_runs_total`、
`synora_job_failures_total`、`synora_job_retries_total`、
`synora_job_bytes_transferred_total`、`synora_job_duration_seconds`、
`synora_job_memory_bytes`、`synora_job_cpu_usage_seconds_total`、
`synora_repository_size_bytes`、`synora_job_next_run_timestamp`、
`synora_worker_status`（0 离线/1 在线/2 停接/3 维护）、`synora_worker_jobs_running`。

Grafana 面板模板：`deploy/grafana-synora.json`（导入后选择 Prometheus 数据源）。

## REST API

所有端点位于 `/api/v1`，`Authorization: Bearer <token>`；角色 admin / operator /
viewer，权限键 `jobs.read` / `jobs.write` / `runs.manage` / `workers.read` /
`workers.write` / `logs.read`。

| 方法 | 路径 | 用途 |
|---|---|---|
| POST | `/workers/register` | worker 注册（可带 `name`） |
| POST | `/workers/{id}/heartbeat` | 心跳 + 续租 + 领任务 |
| POST | `/runs/{id}/claim` | 原子认领（`?worker=`） |
| POST | `/runs/{id}/complete` | 回写结果 |
| POST | `/workers/{id}/retire`（兼容 `/drain`） / DELETE `/workers/{id}` | 停接新任务 / 注销 |
| GET | `/jobs` | 任务列表 |
| POST | `/jobs/{name}/run` / `/jobs/{name}/stop` | 触发 / 取消 |
| GET | `/jobs/{name}/history` / `/jobs/{name}/logs?tail=` | 历史 / 日志 |
| GET | `/workers` / `/proxies` | worker / 代理（延迟+出口 IP） |
| POST | `/reload` | 热重载 |
| GET | `/metrics`（免认证） | Prometheus 指标 |

**免认证端点**——`/metrics`、`/healthz` 与两个状态 JSON
（`synora_json_path` / `tunasync_json_path`，路径可配，空串=关闭）按设计不
要求 token，供 Prometheus 抓取和 mirror-web 前端直接读取。它们只暴露聚合
指标与镜像状态，不含任务定义与运行日志；若需要更严格的边界，保持监听在
回环地址或用防火墙限制来源。

## 构建与开发

```sh
CGO_ENABLED=0 cargo build --workspace
cargo test --workspace
cargo clippy --workspace
```

GitHub Actions：push/PR 自动 CI；`VERSION` 文件变更时自动构建 Linux
x86_64/aarch64 并发布 Release。
