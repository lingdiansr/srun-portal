# srun-portal

`srun-portal` 是一个使用 Rust 编写的 Srun Portal 客户端。项目依据从
`portal.rar`（`portal-core@1.9.15`）逆向整理出的协议规格实现；该规格本身
不随仓库发布。

项目的核心契约是**请求等价**：相同输入必须生成与参考客户端逐字节一致的
HTTP 请求。加密、查询参数顺序、JSONP、登录状态机和登出流程均由捕获向量、
stub portal 流程测试和差分验证约束。

后台部分的目标是**可观测、可暂停、可离线部署**：安装不访问 Portal，每次运行
都有硬期限和结构化结果，`disconnect`/`resume` 让“主动断网”成为一等操作。

当前版本：`0.2.0`（Rust 1.89+）

## 快速开始

```text
cargo build --release
./target/release/srun-portal 'https://net.szu.edu.cn/srun_portal_pc?ac_id=1'
cargo run --example live_check -- 'https://net.szu.edu.cn/srun_portal_pc?ac_id=1' [username]
cargo test --all-targets
```

交互入口的 Portal URL 可用位置参数或 `--portal-url` 指定，两者互斥；任一命令行
形式都高于配置文件中的 `portal_url`，配置文件又高于交互输入。`live_check` 只执行
只读请求，不会登录、登出或写入配置。

## CLI

```text
srun-portal [options] [portal-url]
srun-portal [options] reconnect | status | disconnect | resume
srun-portal [options] service install|uninstall|status|start|stop|restart|logs
srun-portal [options] config show|list|set|unset|validate
srun-portal [options] credentials set|forget|status
```

| 命令 | 作用 |
|---|---|
| `reconnect` | 检查当前在线状态，仅在需要时登录一次 |
| `status` | 只读查询：在线/离线与六项账户字段；不登录、不写配置、不读密码 |
| `disconnect` | 先持久暂停后台重连接，再登出（离线视为成功） |
| `resume` | 解除暂停并立即检查 |
| `service install` | 注册后台重连接任务；**不访问 Portal**、不验证密码 |
| `service status` | 安装状态、暂停状态、配置来源、上次结果和平台视图 |
| `service start` / `stop` / `restart` | 暂停/恢复并立即检查，或停止在途运行 |
| `service logs` | 最近 200 条事件（文本或 `--json` 数组） |
| `service uninstall` | 删除任务定义；保留凭据、暂停状态与历史 |
| `config show` | 合并后的有效配置，以及每个值来自哪个文件 |
| `config list` | 当前写入文件实际设置的键 |
| `config set KEY VALUE` | 整批校验后再写入 |
| `config unset KEY` | 删除键，显露较低层或默认值 |
| `config validate` | 校验有效配置（不完整即退出 2） |
| `credentials set` | 保存当前 Portal + 账号的密码（`--store auto|file|keyring`） |
| `credentials forget` | 删除该条密码；`--legacy` 清理旧的无归属条目 |
| `credentials status` | 密码来自环境、文件、钥匙串还是没有 |

选项：

- `--config <path>`：只使用指定配置文件；只有初始化命令会创建它。
- `--portable`：首次创建配置时，放在可执行文件旁边。
- `--json`：`config show|validate`、`credentials status`、`service status|logs`、
  `reconnect|status|disconnect|resume` 的机器可读输出。
- `--version` / `-V`：版本。
- `--help` / `-h`：帮助。

退出码：`0` 成功 / 未安装 / 已暂停 / 忙；`2` 参数或配置错误；`3` 凭据、平台或
持久化错误；`4` 网络、认证、协议或任务总期限失败。

`status` 供 GUI 调用：`--json` 时 stdout 只有一行
`{schema_version, result, account}`，在线/离线都以 `0` 退出、stderr 为空；
查询失败退出 `4`、stdout 为空。字段为 `username`、`ip`、`used_flow_bytes`（字节）、
`used_time_seconds`（秒）、`balance`（面板的 ¥ 数值，不换算）、`product_name`。
它只发配置页和一次 `rad_user_info`，不申请操作锁，也不受后台暂停影响。

## 配置与后台任务

配置文件、配置优先级、旧键迁移算法、所有键的含义、后台调度器和凭据存储方式见
[配置与后台运行](docs/configuration.md)。发布、升级、卸载和安全注意事项见
[发布与运维](RELEASE.md)。

最小配置示例：

```toml
portal_url = "https://net.szu.edu.cn/srun_portal_pc?ac_id=1"
username = "testuser@example"
timeout_ms = 10000
```

后台任务始终以当前用户身份运行，只执行一次性的
`srun-portal __background --control-dir <目录> --trigger interval|startup`；调度
完全交给原生机制（systemd user timer、launchd LaunchAgent、Task Scheduler），
没有常驻 daemon 或 IPC。

密码按 `Portal origin + 完整账号` 隔离，优先保存到操作系统凭据设施，不可用时
回退到控制目录下权限为 `0600`（Windows：仅当前 SID 与 SYSTEM 的 DACL）的
`credentials.toml`；该文件条目优先于钥匙串读取，确保回退后写入的新密码不会被
旧钥匙串值覆盖。`SRUN_PORTAL_PASSWORD` 优先于两者。旧版无归属条目不会被自动
绑定到任何 Portal：`credentials status` 会报告 `legacy_unscoped`，用
`credentials forget --legacy` 清理后重新保存一次即可。

## 代码结构

| 模块 | 职责 |
|---|---|
| `crypto` | 变体 Base64、XXTEA、HMAC-MD5、SHA-1、`{SRBX1}` 信息块 |
| `transport` | URL 编码、JSONP、HTTP 超时、双栈端点 |
| `html` / `portal_config` | 解析 Portal 配置页和 `PortalFlags` |
| `api` | 构造并发送各 Portal API 请求 |
| `runtime` | 在线检查、登录、双栈和登出状态机 |
| `config` / `settings` | 旧 JSON 读取、TOML 分层、`ConfigSource`、只读加载与校验 |
| `persistence` | 原子替换写入、跨进程 `.lock` 文件锁、私有文件创建 |
| `control` | 控制目录：`installed.toml`、`control.toml`、`health.json`、`events.json`、`operation.lock` |
| `process` | 有限期子进程监督：时间上限、输出上限、进程组/Job 回收 |
| `credentials` / `keyring` | v2 凭据键、文件回退与平台凭据设施 |
| `reconnect` / `service` | 类型化的一次性重连接、平台任务计划 |
| `cli` / `cli/supervisor` | 命令分发、监督器（worker + 期限 + 信号）、交互式流程 |
| `sys` | 系统探测和输入处理 |

`src/main.rs` 是命令行入口；`examples/live_check.rs` 是只读实时检查入口。

## 验证状态

```text
cargo test --lib --test flow --test vectors --test background   通过
cargo clippy --all-targets -- -D warnings                       通过
cargo build --release                                           通过
```

`tests/service_native.rs` 是需要真实平台会话的显式验收，默认 `#[ignore]`：

```text
SRUN_PORTAL_NATIVE_TESTS=1 cargo test --test service_native -- --ignored --test-threads=1
```

它要求一个专用一次性账号（无既有 srun-portal 任务、有可用的 user manager /
GUI 会话 / Task Scheduler），注册并跨进程验证 `install → status → stop → start →
restart → uninstall`、双触发器关闭后的残留清理、以及各平台的原生字段（systemd
`ExecMainStatus`、launchd 两个 label 与启动延迟、Task Scheduler 的 SID/限时/分钟
取整）。

协议保真、测试分层、参考二进制差分、真实 Portal 检查以及已知偏差见
[协议保真与验证](docs/protocol.md)。

## 项目边界

已实现：标准 Portal PC 流程、双栈探测、JSONP API、交互登录、DM 登出、带期限与
结构化结果的后台重连接、主动断网/恢复、三类平台任务和多种凭据后端。

明确不在当前协议范围内：

- 网卡绑定（`SO_BINDTODEVICE`）
- 显式认证服务器 IP 覆盖
- dorm `eportal` 协议
- User-Agent 伪装
- captcha 探测
- 常驻重连接 daemon 或本地 IPC
- 设备接口和 `ac_id` 配置覆盖

这些内容不会在协议层之外被隐式加入。扩展边界和 clean-room 说明见
[协议保真与验证](docs/protocol.md)。

## 文档

- [配置与后台运行](docs/configuration.md)
- [协议保真与验证](docs/protocol.md)
- [发布与运维](RELEASE.md)
- [CI 工作流](.github/workflows/ci.yml)
- [Release CD 工作流](.github/workflows/release.yml)

## 许可证

AGPL-3.0-or-later，见 [LICENSE](LICENSE)。
