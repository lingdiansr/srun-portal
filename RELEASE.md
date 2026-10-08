# 发布与运维

## 当前版本线

仓库当前版本为 `0.1.0`。

在 `1.0.0` 之前，小版本可以包含为适配新 Portal 变体所需的 API 或行为调整；
补丁版本应保持兼容，主要用于修复、验证更新和文档更新。

发布前必须满足：CI 中的格式检查、Clippy、测试和 Linux/macOS/Windows 构建矩阵
全部通过。若发布改变了线路行为，还必须重新执行捕获向量或参考二进制差分检查；
参考二进制和协议规格位于仓库外部。

## 构建目标与产物

支持的发布目标：

- `x86_64-unknown-linux-gnu`
- `x86_64-apple-darwin`
- `x86_64-pc-windows-msvc`

构建当前主机的 release 二进制：

```text
cargo build --release
```

发布包中的二进制建议按版本和目标命名：

```text
srun-portal-v0.1.0-x86_64-unknown-linux-gnu
srun-portal-v0.1.0-x86_64-apple-darwin
srun-portal-v0.1.0-x86_64-pc-windows-msvc.exe
```

每个二进制旁应提供 SHA-256 校验值。仓库 CI 负责验证三个目标的构建；systemd、
launchd 和 Task Scheduler 的实际安装仍需在对应原生系统上检查。

## 安装

将二进制复制到当前用户拥有且位于 `PATH` 的目录。不要以 root 运行：Portal 会话、
配置和凭据都属于当前用户。

没有配置文件时，先带 Portal URL 执行一次交互流程：

```text
srun-portal 'https://net.szu.edu.cn/srun_portal_pc?ac_id=1'
```

首次运行会在选择的系统位置或便携位置创建 `srun-portal.toml`。使用
`--config <path>` 或 `--portable` 可以明确配置位置。使用
`srun-portal config show` 查看有效配置，使用 `config set` 修改配置而无需手写 TOML。

## 升级

1. 执行 `srun-portal service status`，记录当前配置路径。
2. 替换二进制。
3. 如果可执行文件路径或调度计划变化，执行 `srun-portal service install` 重建平台任务。
4. 再次执行 `srun-portal service status` 和一次 `srun-portal reconnect`。

如果已有保存的密码，服务安装器会复用它。正常升级不需要迁移或重写配置文件。

## 卸载

先删除后台重连接任务：

```text
srun-portal service uninstall
```

`service uninstall` 故意保留配置和凭据。如果还要删除已保存的密码，再执行：

```text
srun-portal service forget
```

最后删除二进制；如不再需要，也可以删除用户配置文件和其旁边的
`srun-portal.credentials.toml`。每次命令都会通过 `Config: ...` 输出实际使用的配置路径。

## 凭据和机密信息

- `SRUN_PORTAL_PASSWORD` 会覆盖已保存凭据，仅对当前进程生效。不要把它写入共享的
  进程启动参数或长期服务定义。
- 首选操作系统凭据设施：Secret Service、Windows Credential Manager 或 macOS 登录钥匙串。
- 凭据设施不可用时，回退文件为 `srun-portal.credentials.toml`，权限为 `0600`；
  TOML 配置文件不会保存密码。
- 不要提交配置文件、凭据文件或用户级服务管理器文件。
- `service install` 会先执行 dry run，成功后才保存新输入的密码。

## 验证边界

CI 矩阵提供三个目标平台的编译和 CLI smoke 覆盖。以下检查依赖外部状态，仍需手工执行：

- `cargo run --example live_check -- <portal-url> [username]` 是只读检查，但需要可访问的 Portal。
- 请求差分需要外部参考二进制和记录请求的 stub portal。
- systemd、launchd 和 Task Scheduler 的实际安装必须在对应原生系统上执行。
