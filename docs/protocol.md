# 协议保真与验证

本文记录 `srun-portal` 的协议目标、已知偏差、验证边界和 clean-room 约束。项目
入口见 [README](../README.md)；配置和后台任务见
[配置与后台运行](configuration.md)。

## 保真目标

项目针对 `portal-core@1.9.15` 的行为规格实现。规格没有随仓库发布，仓库中的
`§` 引用指向外部规格。目标不是“兼容一个看起来相似的 Portal”，而是：

> 对同一输入，生成与参考客户端逐字节一致的 HTTP 请求。

因此以下内容都是协议的一部分：参数顺序、URL 编码、JSONP 回调位置、整秒时间戳、
双栈请求顺序和 Portal HTML 中的 `acid`。

## 实现范围

- `crypto`：变体 Base64、Srun 的 XXTEA `XEncode`、HMAC-MD5、SHA-1、`{SRBX1}` 信息块。
- `transport`：`URLSearchParams` 编码、JSONP、HTTP 超时、双栈端点切换。
- `html` / `portal_config`：解析配置页中所需的 `$('#id').html()` 子集。
- `api`：配置页、在线检查、challenge、登录、普通登出、DM 登出、短信访客接口。
- `runtime`：在线检查、重试、双栈认证和登出状态机。
- `translate`：错误码到文本的分派算法。
- `config` / `settings`：旧 JSON 只读兼容、TOML 配置发现、分层和首次创建。
- `reconnect` / `service`：无交互一次性重连接和各平台调度器。

## 已确认的保真行为

| 行为 | 当前实现 |
|---|---|
| 登录密码 | 使用 HMAC-MD5，而不是 `md5(password + token)` |
| `info` | 使用变体 Base64、XXTEA 和 JSON 组合成 `{SRBX1}` |
| `acid` | 来自 Portal HTML 的 `#acid`，不是 URL 的 `ac_id` |
| 设备字段 | `os=os.type()`，`name=os.platform()`；Linux 下分别是 `Linux` 和 `linux` |
| 双栈登录 | 先本栈、后另一栈，保持参考客户端的串行行为 |
| 其他栈探测 | 保留参考客户端 fire-and-forget 探测造成的竞态 |
| 时间戳 | 使用整秒；同一个字符串同时用于 query 和签名 |
| 登录失败 | 按参考流程检查三次后报告 `Login failed` |
| `getNotice` | 保留原始缺陷参数名，生成 `per-page==100` |
| 配置页 | 校验 challenge 字段，避免缺失 challenge 被错误参与哈希 |
| 请求顺序 | `callback` 位于 JSONP query 最后，参数顺序由向量测试固定 |

## 明确偏差与设计决策

- 参考客户端在 `rad_user_info` 缺少 `sysver` 时会崩溃；本实现返回
  `apiVersion = None`，其余版本推导保持一致。
- 参考客户端的 `alert()` 分支在 Node 环境不可用；本实现不模拟该异常，而是按
  双栈条件继续处理。
- 旧版 `~/.srun_portal.json` 不再写入。它只作为首次创建配置时的种子和最后的
  URL 回退来源；新 URL 写入 `srun-portal.toml`。
- `signOutNormal` 作为公开 API 保留，但 CLI 按参考行为使用 DM 登出。
- 协议未规定 HTTP 超时，因此实现选择 5 秒连接、10 秒读取超时，并允许配置覆盖。
- 通知和用户协议 API 已实现，但登录和登出流程不会主动请求它们；增加额外请求
  会破坏请求集合等价性。
- 错误文本字典默认为空，`Translate` 回退到原始错误文本；本地化字典可通过
  `Translate::insert` 或 `with_dictionary` 注入。
- 参考 CLI 的 spinner、emoji、ANSI 颜色和光标重绘不属于协议语义，因此没有复制。

## 明确不在当前范围内

以下内容属于外部规格的借用列表，当前不实现：

- 网卡绑定（`SO_BINDTODEVICE`）
- 显式认证服务器 IP 覆盖
- dorm `eportal` 协议族
- User-Agent 伪装
- captcha 探测
- 常驻 monitor/reconnect daemon
- 设备、接口和 `ac_id` 的配置覆盖

后台重连接是一次性 `reconnect` 进程，由 systemd、launchd 或 Task Scheduler
负责周期启动，而不是常驻 daemon。

## 测试与验证

本地基线：

```text
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets       # 153 passed
cargo build --release
```

### 捕获向量

`tests/vectors.rs` 固定：

- HMAC 密码字段
- `{SRBX1}` 信息块
- `chksum`
- 登录 query 全串
- DM 登出签名和 query 全串
- 配置页 query 参数顺序

### Stub portal 流程

`tests/flow.rs` 使用标准库实现本地 stub portal，覆盖：

- 配置页 → 在线检查 → challenge → 登录 → 在线复查 → DM 登出
- 三次在线检查后的失败路径
- 双栈认证顺序
- 双栈登出竞态
- 自定义 JSONP callback
- `reconnect::reconnect_once` 的登录、已在线和失败分支
- systemd、launchd、Task Scheduler 产物和命令行参数
- 配置分层、凭据设施和 `0600` 文件回退

### 差分验证

差分验证需要外部参考二进制和一个记录原始 query 的 stub portal。相同输入下，
比较两边的路径顺序和每个 path 的原始 query，而不是只比较解析后的键值。
参考二进制及其规格不在仓库中，因此该检查不能作为自包含 CI 测试。

### 真实 Portal 只读检查

```text
cargo run --example live_check -- <portal-url> [username]
```

该命令只读取配置页、challenge 和在线信息，不执行登录、登出，也不创建配置文件。
它需要可访问的真实 Portal，因此属于手工验证；CI 仅负责编译和 CLI smoke check。

## Clean-room 说明

实现依据外部行为规格完成。参考二进制只被执行用于差分测试和观察终端输出；没有
检查其 payload，没有提取源代码，也没有读取或复制第三方客户端实现。
