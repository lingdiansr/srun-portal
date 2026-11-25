# 配置与后台运行

本文说明 `srun-portal.toml`、控制目录、无交互重连接、平台任务和密码存储。
命令行入口和项目概览见 [README](../README.md)；发布与卸载流程见
[RELEASE.md](../RELEASE.md)。

## 配置文件

客户端读取一个 **配置来源**（`ConfigSource`）：按优先级从低到高排列的文件列表，
加上唯一的写入目标和可选的旧版 JSON 文件。

| 类型 | 路径 |
|---|---|
| 系统配置 | Linux：`$XDG_CONFIG_HOME/srun-portal/srun-portal.toml`，默认 `~/.config`；macOS：`~/Library/Application Support/srun-portal/srun-portal.toml`；Windows：`%APPDATA%\srun-portal\config\srun-portal.toml` |
| 便携配置 | 可执行文件所在目录的 `srun-portal.toml` |

便携层位于系统层之后，按键覆盖。若两个路径指向同一物理文件（符号链接或硬链接），
只加载一次（硬链接仅在加载时按身份去重；原子替换后不再保持多路径硬链接关系）。
`--config <path>` 或 `SRUN_PORTAL_CONFIG` 用单个文件替代这个列表。

**读取与初始化是分开的。** `config show`、`config list`、`config validate`、
`reconnect`、`credentials *`、`service status/logs/stop/uninstall` 只读；文件不存在
就使用默认值，绝不创建。只有交互登录、`config set` 和 `service install` 会创建写入
目标；`service install` 在完全没有配置文件时才会询问位置（无终端时选系统位置，
`--portable` 选便携位置）。旧版 `~/.srun_portal.json` 只作为 `portal_url` 的回退读取。

写入是**原子**的：新内容写到同目录临时文件，`sync_all` 后 `rename` 覆盖，Unix 上
再同步父目录。写入前在目标旁的 `.lock` 文件上加排他锁，**锁内重新读取、重新迁移、
再合并**本次变化，因此两个并发 `config set` 不会互相覆盖。失败时磁盘内容保持原样。
若普通配置文件本身是符号链接（例如由 dotfiles 管理），写入解析到链接目标，链接保留。

交互入口的 Portal URL 可用位置参数或 `--portal-url` 指定，两者互斥；任一命令行
形式都高于配置文件中的 `portal_url`，配置文件又高于交互输入。

### 旧键迁移

写配置时，目标文件中的旧键按以下规则迁移：

- 目标文件只要含 `connect_timeout_ms` 或 `read_timeout_ms` 且不含当前
  `timeout_ms`，就把**写入前有效** connect/read 超时的较大值写为 `timeout_ms`；
  随后删除两个旧超时键。显式 `config set timeout_ms` 最后覆盖迁移值；显式
  `config unset timeout_ms` 最后删除它并显露较低层或默认值。
- 非空旧 `domain` 会在有效 `username` 存在时折叠为完整 `user@domain` 后删除；
  没有有效账号时保留并延后迁移。空 `domain` 直接删除。
- 目标文件的 `callback` 直接删除；较低优先级文件中的旧 `callback` 仍按分层生效，
  直到该文件自身被写入。

未知 TOML 键在重写时保留。

## 配置键

```toml
# srun-portal configuration.
portal_url = "https://net.szu.edu.cn/srun_portal_pc?ac_id=1"
username = "testuser@example"
timeout_ms = 10000
reconnect_interval_secs = 300
reconnect_boot_delay_secs = 30
```

| 键 | 含义 | 允许范围 |
|---|---|---|
| `portal_url` | 完整 Portal URL，必须包含 `/srun_portal...` 路径和 `ac_id` 查询键 | http 或 https |
| `username` | 默认账号；域名直接写成 `user@domain` | 非空 |
| `timeout_ms` | HTTP 连接和读取共用的超时 | 1–60000 ms |
| `reconnect_interval_secs` | 后台周期检查间隔；`0` 关闭周期任务 | 0–86340 s |
| `reconnect_boot_delay_secs` | 启动后的首次检查延迟；`0` 关闭启动任务 | 0–86400 s |

有效默认值为 10 秒 HTTP 超时、300 秒周期重连接和 30 秒启动延迟。层次化默认值
与实际写入文件的模板是同一组值。

`config set` 在**任何写盘之前**校验整批更新后的有效配置：非法 URL、空账号、超范围
数值都会让整批被拒绝（退出 2），文件字节不变。修改调度键会提示
`Schedule changed; run service install to apply.` —— 下一次 `reconnect` 会读取新的
认证配置，但原生触发器只在显式重新安装时改变。

## 配置命令

```text
srun-portal config show [--json]
srun-portal config list
srun-portal config set username testuser@szu timeout_ms 10000
srun-portal config unset timeout_ms
srun-portal config validate [--json]
```

- `show` 显示合并后的有效值，以及每个值来自哪个文件（或默认值）。
  `--json` 输出 `{schema_version:1, values, sources, write_path}`，`values` 是原始数值。
- `list` 只显示写入目标实际保存的键，包括尚未迁移的旧版键。
- `set` 整批校验后写入；未知键名直接报错，未知 TOML 键不受影响。
- `unset` 删除指定键并显露较低层或默认值；每行只报告对应键是否实际被删除。
- `validate` 只校验完整有效配置，退出 2 表示配置错误；不写文件、不创建目录。

## 控制目录

安装记录、暂停状态和历史都在同一个每用户目录里，默认是系统配置目录旁的
`service/`（与是否使用便携配置无关），因此一个用户只有一套后台任务：

| 文件 | 内容 |
|---|---|
| `installed.toml` | 安装记录：可执行文件、配置来源、请求周期、原生实际周期、启动延迟、所有者（Unix uid / Windows 令牌 SID） |
| `control.toml` | `version`、`paused`、`installing` |
| `health.json` | 最近一次网络操作的结果，以及最近一次成功的时间 |
| `events.json` | 最近 200 条事件 |
| `credentials.toml` | 文件回退的密码条目 |
| `operation.lock` | 任何可能与 Portal 通信的运行都持有的锁 |

`SRUN_PORTAL_STATE_DIR`（必须是绝对路径）可以把整个目录移到别处，用于隔离部署或
测试；隐藏入口的 `--control-dir` 优先于它，安装记录里保存绝对路径。

`health.json` 与每条事件的字段固定：

```json
{
  "schema_version": 1,
  "started_at_ms": 1794923447000,
  "finished_at_ms": 1794923447054,
  "result": "reconnected",
  "error_kind": null,
  "message": "reconnected",
  "last_success_at_ms": 1794923447054
}
```

`health.json` 的 `result` 取 `online | reconnected | disconnected | failed`。事件额外允许
`paused | busy | installed | uninstalled | resumed | stopped | install_failed`。
`error_kind` 为 `null` 或 `config | credentials | network | authentication | protocol |
timeout | platform`。时间均为 UTC Unix 毫秒。`paused` 与 `busy` 不覆盖最近一次网络结果；
后台入口若启动时已经暂停，则直接退出，不追加事件。

“正在运行”由 `operation.lock` 是否被持有判断，不依赖持久 PID。文件缺失表示默认值
（未安装、未暂停、无历史）；存在但无法解析是明确错误，不会被静默重置。

## 无交互重连接

```text
srun-portal reconnect [--json]
```

1. 取 `operation.lock`；已被占用时报告 `busy`，退出 0，不发任何请求。
2. 在隐藏子进程 `__worker reconnect` 中：读取 Portal 配置页、查询在线状态、
   仅在离线时登录一次。
3. 父进程在 **90 秒**后终止整个 worker 进程树并记录 `timeout`，退出 4。
4. 结果写入 `health.json` 与 `events.json`，`--json` 输出
   `{schema_version, result, error_kind, message}`。

登录请求中的 `ip_already_online_error` 分支（DM 解绑 + 复核 + 重试）最多执行 3 次
登录，之后返回 `Login failed`，不再无界递归。

## 暂停、登出与恢复

```text
srun-portal disconnect [--json]
srun-portal resume [--json]
srun-portal service stop
srun-portal service start
srun-portal service restart
```

- `disconnect` 先原子写入 `paused = true`，再停止原生运行实例、等待
  `operation.lock`（最多 95 秒），最后在同一个 90 秒监督器中登出。离线视为成功。
  暂停写入失败时不会发出登出请求；登出失败仍保持暂停。
- `resume` 拒绝在 `installing = true` 时执行；先原子清除暂停，再执行与
  `reconnect` 完全相同的检查。检查失败不会重新暂停。
- `service stop` 设置暂停并停止在途运行，**保留**任务定义；`service start` 清除
  暂停并立即检查（要求已安装）；`service restart` 是两者组合。
- 所有隐藏的周期/启动入口都尊重暂停：它们快速无网络退出。

`SIGINT` / `SIGTERM` 先终止 worker 再退出（130 / 143），不会留下继续访问 Portal 的
子进程。

## 平台后台任务

```text
srun-portal service install
srun-portal service status [--json]
srun-portal service logs [--json]
srun-portal service uninstall
```

任务执行当前可执行文件的
`__background --control-dir <绝对目录> --trigger interval|startup`，每次重新读取安装
记录中的 `ConfigSource`；入口缺少安装记录报配置错误，`installing`/`paused` 时无网络退出。

| 平台 | 机制 | 产物 |
|---|---|---|
| Linux | systemd user service + timer | `$XDG_CONFIG_HOME/systemd/user/srun-portal.{service,timer}` |
| macOS | 两个 launchd LaunchAgent | `~/Library/LaunchAgents/com.srun-portal.reconnect.{interval,startup}.plist` |
| Windows | 两个 Task Scheduler 任务 | `srun-portal-<SID>-reconnect` 与 `-reconnect-boot` |

Linux：同一个 oneshot service 加一个 timer；只有周期时首次用 `OnActiveSec=1s`，两个
触发器都关闭时不写 timer。单元带 `TimeoutStartSec=100s`、`KillMode=control-group`，
`%` 与 `$` 在 `ExecStart` 中双写转义。`service status --json` 的 `platform_state`
来自 `systemctl --user show`/`list-timers` 的机器可读属性，oneshot 成功后的 `inactive`
不算故障。

macOS：周期与启动是**两个** agent，启动延迟只加在启动 agent 上（周期运行不会重复
睡启动延迟）。安装先 `bootout` 旧 label（含旧版单 agent 的 label），再原子写 plist
并 `bootstrap`。`launchctl print` 的 113 视为未加载，其他失败上报。

Windows：周期按分钟**向上取整**（1–1439 分钟），实际值同时记录在
`installed.toml`/`status`；登录触发器用原生 `LogonTrigger.Delay`（`PTnS`，0 不设置）；
任务 `ExecutionTimeLimit=PT100S`、`MultipleInstancesPolicy=IgnoreNew`，以当前 SID 注册
（InteractiveToken、Limited）。全部通过固定 PowerShell 脚本执行，动态参数走 stdin
JSON，从不拼进脚本。旧任务名 `srun-portal-reconnect(-boot)` 只有在其 Principal 的
SID 等于当前用户时才会被删除，否则保留并报告。

`service status` 只读 `installed.toml`/`control.toml`/`health.json` 加原生查询；
不读密码、不发请求、不创建路径。未安装不是错误（退出 0）；元数据损坏或平台查询
失败退出 3。

## 安装语义

`service install` 是**离线**的：校验配置、保存（或复用）密码、注册任务，全程不访问
Portal。因此“安装成功”只表示任务已注册，不表示密码正确：

```text
Credentials stored; not verified. Run reconnect to check.
```

提交过程：

1. 收集并校验全部本地输入（配置、账号、路径、当前用户、平台参数）。
2. 写 `installing = true`、`paused = true`。
3. 停止旧运行与旧计划，分阶段写入新定义（旧的配置文件与 `installed.toml` 保持到
   新定义全部注册成功）。
4. 写 `installed.toml`，然后 `installing = false` 并恢复安装前的暂停值。

任一步失败：退出 3，保留 `installing = true` 与 `paused = true`，重新输出
`Installation incomplete; background reconnect paused. Rerun service install or service uninstall.`；
`resume`/`start`/`restart` 在此时拒绝。再次 `install` 会先清理已知的 owned 全集再重建；
`uninstall` 无条件停止并删除 owned 全集，全部成功后才删 `installed.toml`，不删除暂停
状态、事件、业务配置或密码。

## 密码存储

密码按 **Portal origin + 完整账号** 为键：`v2:<urlencode(origin)>:<urlencode(account)>`。
同一 origin 下不同 `ac_id` 共用认证域；不同 Portal 的同名账号互不影响。

来源优先级：

1. 非空 `SRUN_PORTAL_PASSWORD`（在任何存储被打开前检查，因此文件损坏/钥匙串锁定时
   仍然可用）。
2. 该键在回退文件中的条目。
3. 系统凭据设施中该键的条目。

文件优先于钥匙串正是为了让“回退写入的新密码”生效，而不被旧钥匙串值覆盖。文件
存在但无法读取是明确错误（`error_kind=credentials`），不会被当作“没有密码”；
文件无条目且钥匙串不可用时报告 `unavailable`。

| 平台 | 设施 | 接口 |
|---|---|---|
| Linux | Secret Service | `secret-tool`，每次调用 5 秒硬期限并回收进程组 |
| Windows | Credential Manager | `CredReadW` / `CredWriteW` / `CredDeleteW` |
| macOS | 登录钥匙串 | `security-framework` 的 `generic_password`/`set_generic_password`/`delete_generic_password`，操作期间关闭钥匙串交互（RAII 恢复），失败直接报错 |

文件位于控制目录的 `credentials.toml`，从创建起就是私有的（Unix `0600`、目录 `0700`；
Windows 仅当前 SID 与 SYSTEM 的受保护 DACL，在写入任何秘密之前设置），原子替换，
拒绝符号链接：

```toml
# srun-portal credentials; keep this file private.
[passwords]
"v2:https%3A%2F%2Fnet.szu.edu.cn:alice%40staff" = "staff-secret"
```

```text
srun-portal credentials set [--store auto|file|keyring]
srun-portal credentials forget [--legacy]
srun-portal credentials status [--json]
```

- `set` 默认 `auto`：先试钥匙串，成功后在**同一把锁内**删除同键文件条目（清理失败
  即报错，不谎称迁移完成）；钥匙串失败则原子写入文件并明确警告。`--store file` 不碰
  钥匙串，`--store keyring` 失败不回退。密码来自环境变量或安全终端提示，命令不发
  Portal 请求。
- `forget` 同时尝试两个后端，任一失败都返回非零；两端都“未找到”是成功。缺少 Portal
  或账号时拒绝，绝不整库删除。
- `forget --legacy` 是唯一清理旧条目的入口：删除当前账号对应的裸 keyring 键、旧版
  配置旁 `srun-portal.credentials.toml` 中无归属的顶层 `password` 与该账号的旧
  `[passwords]` 键。其他账号键与 v2 键都会保留。

旧版条目**不会**被自动绑定到任何 Portal：`credentials status` 报告
`legacy_unscoped`，提示清理后重新保存。这一检测覆盖两个后端——旧版文件里的顶层
`password`／裸账号 `[passwords]` 键，以及**钥匙串中以裸账号存的那条**（v2 之前
`service install` 写入的位置）。v2 键命中时优先报告 `keyring`／`file`，旧条目只在
v2 键未命中时才被报告。存在顶层 `password` 时 `credentials set` 明确
报错 `Legacy unscoped credential file; run credentials forget --legacy first.`，避免产生
读取器拒绝的混合格式。

`service uninstall` 不删除密码；需要删除时执行 `credentials forget`。
