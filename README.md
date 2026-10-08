# srun-portal

`srun-portal` 是一个使用 Rust 编写的 Srun Portal 客户端。项目依据从
`portal.rar`（`portal-core@1.9.15`）逆向整理出的协议规格实现；该规格本身
不随仓库发布。

项目的核心契约是**请求等价**：相同输入必须生成与参考客户端逐字节一致的
HTTP 请求。加密、查询参数顺序、JSONP、登录状态机和登出流程均由捕获向量、
stub portal 流程测试和差分验证约束。

当前版本：`0.1.0`

## 快速开始

```text
cargo build --release
./target/release/srun-portal 'https://net.szu.edu.cn/srun_portal_pc?ac_id=1'
cargo run --example live_check -- 'https://net.szu.edu.cn/srun_portal_pc?ac_id=1' [username]
cargo test --all-targets
```

Portal URL 的解析顺序为：命令行位置参数、配置文件中的 `portal_url`、交互式
输入。`live_check` 只执行只读请求，不会登录、登出或写入配置。

## CLI

```text
srun-portal [options] [portal-url]
srun-portal [options] reconnect
srun-portal [options] service install|uninstall|status|forget
srun-portal [options] config show|list|set|unset
```

常用命令：

| 命令 | 作用 |
|---|---|
| `reconnect` | 检查当前在线状态，仅在需要时登录一次 |
| `service install` | 注册当前用户的后台重连接任务 |
| `service status` | 查看任务、配置和凭据状态 |
| `service uninstall` | 删除后台任务，不删除凭据 |
| `service forget` | 删除保存的密码 |
| `config show` | 显示合并后的有效配置 |
| `config list` | 显示当前文件实际设置的键 |
| `config set KEY VALUE` | 设置一个或多个配置项 |
| `config unset KEY` | 删除配置项，恢复默认值 |

选项：

- `--config <path>`：只使用指定配置文件；文件不存在时创建。
- `--portable`：首次创建配置时，放在可执行文件旁边。
- `--help` / `-h`：显示帮助。

## 配置与后台任务

配置文件、配置优先级、所有键的含义、后台调度器和凭据存储方式见
[配置与后台运行](docs/configuration.md)。发布、升级、卸载和安全注意事项见
[发布与运维](RELEASE.md)。

最小配置示例：

```toml
portal_url = "https://net.szu.edu.cn/srun_portal_pc?ac_id=1"
username = "testuser"
# domain = "@example"
# callback = "jsonp"
# connect_timeout_ms = 5000
# read_timeout_ms = 10000
```

后台任务始终以当前用户身份运行。密码优先保存到操作系统凭据设施；没有可用
凭据设施时，才回退到配置目录旁、权限为 `0600` 的凭据文件。

## 代码结构

| 模块 | 职责 |
|---|---|
| `crypto` | 变体 Base64、XXTEA、HMAC-MD5、SHA-1、`{SRBX1}` 信息块 |
| `transport` | URL 编码、JSONP、HTTP 超时、双栈端点 |
| `html` / `portal_config` | 解析 Portal 配置页和 `PortalFlags` |
| `api` | 构造并发送各 Portal API 请求 |
| `runtime` | 在线检查、登录、双栈和登出状态机 |
| `config` / `settings` | 旧 JSON 读取、TOML 配置发现、分层和首次创建 |
| `credentials` / `keyring` | 凭据设施和 `0600` 文件回退 |
| `reconnect` / `service` | 无交互重连接和平台后台任务 |
| `cli` / `sys` | 交互式命令行、系统探测和输入处理 |

`src/main.rs` 是命令行入口；`examples/live_check.rs` 是只读实时检查入口。

## 验证状态

当前本地基线：

```text
cargo fmt --all -- --check                 通过
cargo clippy --all-targets -- -D warnings  通过
cargo test --all-targets                    153 passed
cargo build --release                       通过
```

仓库 CI 还会在 Linux、macOS、Windows 上执行目标构建和 CLI smoke check。协议
保真、测试分层、参考二进制差分、真实 Portal 检查以及已知偏差见
[协议保真与验证](docs/protocol.md)。

## 项目边界

已实现：标准 Portal PC 流程、双栈探测、JSONP API、交互登录、DM 登出、一次性
后台重连接、三类平台任务和多种凭据后端。

明确不在当前协议范围内：

- 网卡绑定（`SO_BINDTODEVICE`）
- 显式认证服务器 IP 覆盖
- dorm `eportal` 协议
- User-Agent 伪装
- captcha 探测
- 常驻重连接 daemon
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
