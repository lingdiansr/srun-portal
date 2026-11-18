# 发布与运维

## 当前版本线

仓库当前版本为 `0.1.0`；最低 Rust 版本为 **1.89**（`File::lock` 的稳定版本）。

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

每个二进制旁应提供 SHA-256 校验值。仓库 CI 验证三个目标的构建与隔离回归；
systemd、launchd 和 Task Scheduler 的原生安装由 `tests/service_native.rs` 在具备
真实会话的专用账号上执行（见下文“验证边界”）。

## 安装

将二进制复制到当前用户拥有且位于 `PATH` 的目录。不要以 root 运行：Portal 会话、
配置、控制目录和凭据都属于当前用户。

部署流程是**离线可用**的：`service install` 不访问 Portal，只在本地校验配置、
保存凭据并注册任务。先准备配置与凭据，再安装：

```text
srun-portal config set portal_url 'https://net.szu.edu.cn/srun_portal_pc?ac_id=1' \
    username testuser@szu
srun-portal credentials set          # 从 SRUN_PORTAL_PASSWORD 或安全提示读取
srun-portal service install
srun-portal reconnect                # 真正验证凭据
```

`service install` 结束后会明确输出 `Credentials stored; not verified. Run reconnect to check.`：
安装成功不表示密码正确。没有配置文件时，安装会询问存储位置（无终端时选系统位置，
`--portable` 选便携位置）。

控制目录默认是系统配置目录旁的 `service/`；可用 `SRUN_PORTAL_STATE_DIR`（绝对路径）
整体迁移，便于隔离部署与测试。

## 发布命令

发布工作流位于 [`.github/workflows/release.yml`](.github/workflows/release.yml)。
它只响应 `v*` 标签，分别在 Linux、macOS 和 Windows runner 上构建二进制，
生成 SHA-256 文件，然后使用 `GITHUB_TOKEN` 创建 GitHub Release 并上传产物。

```text
git tag -a v0.1.0 -m "srun-portal v0.1.0"
git push origin v0.1.0
```

## 升级

1. 执行 `srun-portal service status --json`，记录 `config_sources` 与 `platform_state`。
2. 替换二进制。
3. 如果可执行文件路径或调度计划变化，执行 `srun-portal service install` 重建平台任务。
   安装会先清理已知的 owned 资源全集，因此不会留下旧触发器。
4. 再次执行 `service status` 和一次 `reconnect`。

从旧版本升级时注意两点：

- 凭据键改为 `v2:<origin>:<account>`。旧的仅按账号保存的密码不会自动绑定到任何
  Portal；`credentials status` 会报告 `legacy_unscoped`。用
  `credentials forget --legacy` 清理旧条目，再 `credentials set` 保存一次。
- 候选配置旁的旧 `srun-portal.credentials.toml` 不再被读取；只有
  `credentials forget --legacy` 会定位并清理它。

## 卸载

```text
srun-portal service uninstall
srun-portal credentials forget
```

`service uninstall` 删除任务定义与安装记录，保留暂停状态、事件历史、业务配置和
密码。删除密码用 `credentials forget`（`--legacy` 一并清理旧版条目）。最后删除
二进制，必要时删除配置文件、控制目录（`service/`）与旧版凭据文件。

如果卸载中途失败，命令会保留 `installed.toml` 并返回 3，便于重试；此时后台入口
看到 `installing`/`paused` 不会发起网络请求。

## 凭据和机密信息

- `SRUN_PORTAL_PASSWORD` 会覆盖已保存凭据，仅对当前进程生效。不要把它写入共享的
  进程启动参数或长期服务定义。
- 首选操作系统凭据设施：Secret Service、Windows Credential Manager 或 macOS 登录钥匙串。
  macOS 走 `security-framework` 原生 API 并在操作期间关闭钥匙串交互，不产生授权窗口。
- 凭据设施不可用时，回退文件为控制目录中的 `credentials.toml`，权限为 `0600`
  （Windows：仅当前 SID 与 SYSTEM 的受保护 DACL）；TOML 配置文件不会保存密码。
- 所有持久记录（`health.json`、`events.json`、CLI `--json`）都不包含密码、密码派生
  值、完整查询串或响应正文。
- 不要提交配置文件、凭据文件或用户级服务管理器文件。

## 验证边界

```text
cargo test --lib --test flow --test vectors --test background
```

是默认回归集：不访问真实 Portal，不写真实钱包，不注册原生任务。

原生生命周期验收需要专用一次性账号（无既有 srun-portal 任务、可用的 user manager /
GUI 会话 / Task Scheduler）：

```text
SRUN_PORTAL_NATIVE_TESTS=1 cargo test --test service_native -- --ignored --test-threads=1
```

它实际执行 `install → install → status → stop → start → restart → uninstall`，并核对
各平台原生字段：systemd 的 `ExecMainStatus` 与 `%`/`$` 转义、launchd 的两个 label 与
启动延迟、Task Scheduler 的 SID/限时/分钟取整。没有专用会话的托管 runner 只能记录
“原生验收不可执行”，不能以生成器断言代替。

其余仍依赖外部状态的检查：

- `cargo run --example live_check -- <portal-url> [username]` 是只读检查，但需要可访问的 Portal。
- 请求差分需要外部参考二进制和记录请求的 stub portal。
