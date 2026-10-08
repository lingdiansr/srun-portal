# 宿舍区 Dr.COM 认证适配 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在保留办公区 Srun 流程的前提下，让根路径宿舍区 URL 自动使用 Dr.COM 4.0 EPortal 登录。

**Architecture:** `config::PortalTarget` 根据 URL 路径选择 Srun 或 Dr.COM。新增 `drcom` 模块封装页面解析、JSONP、在线检查、登录和注销；CLI 与无交互重连接只在顶层分派，凭据和服务调度保持不变。

**Tech Stack:** Rust 2021、现有 `ureq` transport、`serde_json`、标准库测试 stub。

---

### Task 1: 协议选择与失败测试

**Files:**
- Modify: `src/config.rs`
- Modify: `src/lib.rs`
- Test: `tests/drcom.rs`

- [x] 为根 URL、Srun URL 和不支持路径写解析测试。
- [x] 运行 `cargo test --test drcom`，确认因新类型/函数不存在而失败。
- [x] 添加 `PortalTarget`、`DrcomUrl` 和 `parse_portal_target`，不改变 `parse_portal_url`。
- [x] 重跑解析测试。

### Task 2: Dr.COM 客户端与 stub 流程

**Files:**
- Create: `src/drcom.rs`
- Modify: `src/lib.rs`
- Test: `tests/drcom.rs`

- [x] 先添加 stub 对配置页、状态、登录和注销的行为断言，并确认失败。
- [x] 实现根页面终端 IP/端口解析、loadConfig JSONP 解包、`chkstatus`/`online_list`、登录和注销参数。
- [x] 实现 `result == 1`/`"ok"` 成功判定和失败响应错误。
- [x] 运行 `cargo test --test drcom`。

### Task 3: CLI、重连接和服务分派

**Files:**
- Modify: `src/reconnect.rs`
- Modify: `src/cli.rs`
- Modify: `tests/flow.rs`

- [x] 将重连接目标改为 `PortalTarget`，保留 Srun 分支请求行为。
- [x] 将交互登录、`reconnect` 和 `service install` 连接到 Dr.COM 分支；Dr.COM 在线时不自动注销。
- [x] 修正现有 Srun 流程测试的目标构造，运行完整测试。

### Task 4: 文档和最终验证

**Files:**
- Modify: `README.md`
- Modify: `docs/configuration.md`
- Modify: `docs/protocol.md`

- [x] 补充宿舍区 URL、账号字段和不存储密码说明。
- [x] 运行 `cargo fmt --all -- --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test --all-targets` 和 `cargo build --release`。
- [x] 对宿舍门户执行只读 `chkstatus`/配置 smoke；真实登录仅在获得点位确认后使用用户凭据执行。
