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

`network` 必须手动指定为 `office` 或 `dorm`。首次运行或配置文件缺少该项时，
交互式流程会询问并写回。非交互 `reconnect`/后台任务在缺少 `network` 但已有有效
`portal_url` 时复用旧配置；只有两者都缺失时才失败。显式 `portal_url` 优先于
`network`。`/srun_portal...` URL 选择办公区 Srun；根路径 URL 选择 Dr.COM 4.0
EPortal，例如宿舍区 `http://172.30.255.42/`。`config set` / `config unset` 和
交互式流程会写入本次解析所使用的文件。未知 TOML 键会在重写时保留。

## 配置键

所有键都是可选的：

```toml
# srun-portal configuration.
network = "office"
username = "testuser"
portal_url = "https://net.szu.edu.cn/srun_portal_pc?ac_id=1" # optional override
domain = "@example"
callback = "jsonp"
connect_timeout_ms = 5000
read_timeout_ms = 10000
reconnect_interval_secs = 300
reconnect_boot_delay_secs = 30
```

宿舍区配置只需替换：

```toml
portal_url = "http://172.30.255.42/"
username = "testuser"
```

Dr.COM 登录会从根页面读取终端 IPv4，从 EPortal 配置读取端口和账号后缀，
并使用 `user_account`, `user_password`, `wlan_user_ip` 等字段。密码仍由交互输入、
`SRUN_PORTAL_PASSWORD` 或现有凭据设施提供，配置文件不保存密码。

| `network` | 手动网络模式：`office` 办公区，`dorm` 宿舍区 |
| `portal_url` | 显式 Srun URL（含 `/srun_portal...` 和 `ac_id`），或 Dr.COM 根 URL；优先级高于 `network` |
| `username` | 交互式登录和无交互重连接使用的默认账号 |
| `domain` | Srun 账号没有 `@` 时追加的域名后缀；Dr.COM 仅使用账号中显式输入的 `@` 后缀 |
| `connect_timeout_ms` | HTTP 连接超时，默认 5000 ms |
| `read_timeout_ms` | HTTP 读取超时，默认 10000 ms |
| `reconnect_interval_secs` | 后台周期检查间隔；`0` 关闭周期任务 |
| `reconnect_boot_delay_secs` | 启动后的首次检查延迟；`0` 关闭启动任务 |

有效默认值为 5 秒连接超时、10 秒读取超时、300 秒周期重连接和 30 秒启动延迟。
首次创建的文件会显式写入默认配置，其中后台任务键写为 `0`，避免首次配置
完成前自动运行；需要后台任务时通过 `config set` 打开。

`domain` 的默认值是空字符串。Portal 页面中的 `#domain` 是域名菜单，不会自动
成为账号后缀。交互式输入支持 `user@domain`，第一个 `@` 后的内容作为域名参数。

## 配置命令

```text
srun-portal config show
srun-portal config list
srun-portal config set username testuser
srun-portal config set domain szu reconnect_interval_secs 300
srun-portal config unset domain
```

- `show` 显示合并系统配置、便携配置和默认值后的有效结果。
- `list` 只显示当前文件实际保存的键。
- `set` 在写入前校验键名和值类型；错误不会破坏文件。
- `unset` 删除指定键，随后回到默认值或较低优先级文件的值。
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

## 平台后台任务

```text
srun-portal service install
srun-portal service status
srun-portal service uninstall
srun-portal service forget
```

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

条目使用 `service=srun-portal` 和 `account=<username>` 标识。配置文件不保存
密码。没有可用凭据设施时，回退文件权限为 `0600`，并会明确报告回退原因。

`service install` 会先用密码执行 dry run，成功后才保存密码。`service uninstall`
不会删除密码；需要删除时执行 `service forget`。
