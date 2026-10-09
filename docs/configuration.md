# 配置与后台运行

本文说明 `srun-portal.toml`、无交互重连接、平台任务和密码存储。命令行入口
和项目概览见 [README](../README.md)；发布与卸载流程见
[RELEASE.md](../RELEASE.md)。

## 配置文件

客户端最多读取两个同名文件：

| 类型 | 路径 |
|---|---|
| 系统配置 | Linux：`$XDG_CONFIG_HOME/srun-portal/srun-portal.toml`，默认 `~/.config`；macOS：`~/Library/Application Support/srun-portal/srun-portal.toml`；Windows：`%APPDATA%\srun-portal\config\srun-portal.toml` |
| 便携配置 | 可执行文件所在目录的 `srun-portal.toml` |

便携配置按键覆盖系统配置。`--config <path>` 或 `SRUN_PORTAL_CONFIG` 会替代这
两个位置，指定文件成为唯一配置文件；不存在时创建。

首次运行且两个文件都不存在时，交互式 CLI 会询问存储位置；无终端时默认使用
系统位置，`--portable` 会强制使用便携位置。旧版 `~/.srun_portal.json` 只会
被读取并作为首次创建时的 `portal_url` 种子，之后不再写入。

Portal URL 的解析优先级是：命令行位置参数、配置文件中的 `portal_url`、交互式
输入。`config set` / `config unset` 和交互式流程会写入本次解析所使用的文件。
未知 TOML 键会在重写时保留。写配置时，目标文件中的旧键按以下规则迁移：

- 目标文件只要含 `connect_timeout_ms` 或 `read_timeout_ms` 且不含当前
  `timeout_ms`，就把**写入前有效** connect/read 超时（包括默认值和较低优先级
  文件）的较大值写为 `timeout_ms`，因此迁移不会缩短任一有效旧超时；随后删除
  两个旧超时键。显式 `config set timeout_ms` 最后覆盖迁移值；显式
  `config unset timeout_ms` 最后删除它并显露较低层或默认值，不会被迁移重新创建。
- 非空旧 `domain` 会在有效 `username` 存在时折叠为完整 `user@domain` 后删除；
  没有有效账号时保留并延后迁移。空 `domain` 直接删除。
- 目标文件的 `callback` 直接删除，没有替代键；较低优先级文件中的旧
  `callback` 仍按正常分层生效，直到该文件自身被写入。

## 配置键

所有键都是可选的：

```toml
# srun-portal configuration.
portal_url = "https://net.szu.edu.cn/srun_portal_pc?ac_id=1"
username = "testuser@example"
timeout_ms = 10000
reconnect_interval_secs = 300
reconnect_boot_delay_secs = 30
```

| 键 | 含义 |
|---|---|
| `portal_url` | 完整 Portal URL，必须包含 `/srun_portal...` 路径和 `ac_id` 查询键 |
| `username` | 交互式登录和无交互重连接使用的默认账号；域名直接写成 `user@domain` |
| `timeout_ms` | HTTP 连接和读取共用的超时，默认 10000 ms |
| `reconnect_interval_secs` | 后台周期检查间隔；`0` 关闭周期任务 |
| `reconnect_boot_delay_secs` | 启动后的首次检查延迟；`0` 关闭启动任务 |

有效默认值为 10 秒 HTTP 超时、300 秒周期重连接和 30 秒启动延迟。
首次创建的文件会显式写入这些默认值；需要关闭某个后台触发器时将对应键设为
`0`，需要后台任务时无需额外初始化。

交互式输入和配置中的 `username` 都支持 `user@domain`；第一个 `@` 后的内容作为
登录域名。协议发送仍使用 local username 与 `@domain` 两个字段，而凭据以完整有效
身份 `user@domain` 为键。新配置固定使用协议默认 JSONP 回调 `jsonp`；旧版
`callback` 键在其所在文件下一次写配置前仍然生效。

## 配置命令

```text
srun-portal config show
srun-portal config list
srun-portal config set username testuser@szu timeout_ms 10000
srun-portal config set reconnect_interval_secs 300
srun-portal config unset timeout_ms
```

- `show` 显示合并系统配置、便携配置和默认值后的有效结果；旧版键会标为兼容项。
- `list` 只显示当前文件实际保存的键，包括尚未迁移的旧版键。
- `set` 在写入前校验整批键名和值类型；任一错误都不会写文件。
- `unset` 删除指定键，随后立即回到较低优先级文件的值或默认值。旧版键迁移先于
  显式删除，因此 `config unset timeout_ms` 不会重新生成该键。
- 这些命令不会读取、写入或删除密码；密码由 `service forget` 管理。

## 无交互重连接

```text
srun-portal reconnect
```

一次重连接流程会：

1. 读取 Portal 配置页。
2. 查询当前设备是否在线。
3. 仅当设备离线时登录一次。
4. 打印 `Online.`、`Reconnected.` 或可记录到日志的错误信息后退出。

该命令不会提示输入，因此适合交给系统调度器重复执行。
`reconnect` 支持 `--portal-url`、`--username` 和 `--timeout-ms` 作为本次运行的覆盖值，
但仍然不会提示输入，也不会把这些覆盖值写回配置文件。重连接周期和启动延迟只对
`service install` 生效。


## 平台后台任务

```text
srun-portal service install
srun-portal service status
srun-portal service uninstall
srun-portal service forget
```
`service install` 也接受以下 kebab-case 选项；选项值会写入本次使用的配置
文件，供后续的 timer 读取：

```text
srun-portal service install \
  --username testuser@szu \
  --timeout-ms 10000 \
  --reconnect-interval-secs 300 \
  --reconnect-boot-delay-secs 30
```

可用选项为 `--portal-url`、`--username`、`--timeout-ms`、
`--reconnect-interval-secs` 和 `--reconnect-boot-delay-secs`。配置文件中的对应键仍
使用下划线名称，例如 `reconnect_interval_secs`。

交互式执行 `service install` 时，Portal URL、账号、共用超时和两个重连时序都会显示
当前值作为默认值；直接按 Enter 保留默认值。无终端时不会提示，任务使用配置文件中的值。

任务始终属于当前用户：

| 平台 | 机制 | 产物 |
|---|---|---|
| Linux | systemd user service + timer | `$XDG_CONFIG_HOME/systemd/user/srun-portal.{service,timer}` |
| macOS | launchd LaunchAgent | `~/Library/LaunchAgents/com.srun-portal.reconnect.plist` |
| Windows | Task Scheduler | `srun-portal-reconnect` 和 `srun-portal-reconnect-boot` |

默认情况下，安装任务使用 300 秒周期检查和 30 秒启动延迟。两个计划可以独立
关闭：

| `reconnect_interval_secs` | `reconnect_boot_delay_secs` | 安装内容 |
|---:|---:|---|
| 未设置（300） | 未设置（30） | 周期和启动任务 |
| 设置为非零 | `0` | 仅周期任务 |
| `0` | 设置为非零 | 仅启动任务 |
| `0` | `0` | 仅安装服务，不自动触发 |

Linux 的周期 timer 使用 `OnActiveSec=1s` 解决只有 `OnUnitActiveSec=` 时不会
首次触发的问题。两个计划都关闭时不写 timer，也不会修改 login linger。启动任务
需要在登录前运行时，`install` 会报告所需的权限动作，但不会替用户执行特权命令。

## 密码存储

密码来源优先级：

1. `SRUN_PORTAL_PASSWORD` 环境变量。
2. 当前账号在操作系统凭据设施中的条目。
3. 配置目录旁的 `srun-portal.credentials.toml` 回退文件。

支持的凭据设施：

| 平台 | 设施 | 接口 |
|---|---|---|
| Linux | Secret Service | `secret-tool` |
| Windows | Credential Manager | `CredReadW` / `CredWriteW` / `CredDeleteW` |
| macOS | 登录钥匙串 | `security(1)` |

条目使用 `service=srun-portal` 和 `account=<完整有效登录身份>` 标识；例如
`alice@staff` 与 `alice@student` 是严格隔离的两个凭据键。配置文件不保存密码。
没有可用凭据设施时，回退文件权限为 `0600`，并会明确报告回退原因。

受旧版本裸用户名凭据键缺陷影响的升级用户必须重新运行 `service install` 保存
当前完整身份的密码。新版本不会为域名账号回退读取或删除可能属于另一域的裸
`alice` 键，也不会把该密码自动发送到当前 Portal。

`service install` 会先用密码执行 dry run，成功后才保存密码。`service uninstall`
不会删除密码；需要删除时执行 `service forget`。
