# 宿舍区 Dr.COM 认证适配设计

## 背景

办公区使用现有 Srun Portal 协议；宿舍区 `http://172.30.255.42/` 返回 Dr.COM 4.0 EPortal 页面，不能复用 Srun 的 `/srun_portal_pc`、challenge、HMAC-MD5 和 `{SRBX1}` 流程。

## 决策

采用双协议自动识别：

- `/srun_portal...` URL 继续解析为现有 Srun 目标。
- 根路径 HTTP/HTTPS URL（例如 `http://172.30.255.42/`）解析为 Dr.COM 目标。
- 不新增协议配置键；URL 本身是协议选择，保留现有办公区配置兼容性。

## Dr.COM 数据流

1. GET 门户根页面，读取页面内的 `v4ip`、`v6ip` 和 `authloginport`；缺失 IP 时回退到本机非回环 IPv4。
2. GET `{scheme}://{hostname}:801/eportal/portal/page/loadConfig` JSONP，读取 `login_method`、`account_prefix`、`account_suffix`、`check_online_method`、`cvlan_id`、`ac_logout`、`register_mode`、`ipv6_state` 和 EPortal 端口。
3. 在线检查：当前宿舍配置 `check_online_method=0`，请求 `{origin}/drcom/chkstatus` JSONP；`result == 1` 表示在线，`result == 0` 表示离线。兼容 `online_list` 配置路径。
4. 登录：请求 `{eportal_origin}/eportal/portal/login` JSONP。当前配置使用 `login_method=1`、桌面账号前缀 `,0,`、明文密码字段 `user_password`，成功条件为 `result == 1` 或 `result == "ok"`。
5. 注销：请求对应 `/eportal/portal/logout` JSONP，使用门户要求的终端字段；成功条件同登录。

所有 JSONP 请求使用随机回调和缓存参数，不保存或硬编码账号密码。

## 代码边界

- 新增 `drcom` 模块，负责页面解析、JSONP 参数、状态、登录、注销及 Dr.COM 错误格式化。
- `config` 新增 `PortalTarget`，保持原 `parse_portal_url` 语义不变，仅新增自动识别入口。
- `reconnect::Target` 改为携带 `PortalTarget`，Srun 分支保持原状态机，Dr.COM 分支调用新客户端。
- CLI 交互登录、`reconnect` 和 `service install` 共用同一个重连接分发；凭据设施和配置优先级不变。
- README、配置文档和协议文档补充宿舍区 URL、协议行为及验证方式。

## 错误和安全

- Dr.COM 页面/JSONP 结构错误、HTTP 错误和非成功 `result` 均转换为可读错误并返回失败状态。
- 账号密码只经过现有交互、环境变量或凭据设施进入运行时；测试使用固定假数据，仓库不写入真实凭据。
- 自动重连接不交互、不主动注销；交互模式沿用现有在线时询问注销的行为。

## 验收标准

- Srun 现有测试与请求向量保持通过。
- `http://172.30.255.42/` 可被识别为 Dr.COM，Srun URL 仍识别为 Srun。
- Dr.COM 本地 stub 覆盖配置加载、离线检查、登录成功、登录失败、在线检查和注销请求的消费者可见行为。
- `reconnect`/服务安装分支可以复用同一 Dr.COM 逻辑。
- 文档不再把宿舍区 Dr.COM 列为未实现范围。
