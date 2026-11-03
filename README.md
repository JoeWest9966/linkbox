# linkbox

**Client-side reverse tunnel supervisor with a local control panel.**

无公网 IP 的机器（家宽/NAT 后）把本地端口"寄存"到远端主机的 loopback 上——
远端访问 `127.0.0.1:PORT` 即可触达你家里的服务，全程走本机主动拨出的 ssh，
可经 SOCKS 代理加速，**不依赖 tailscale/DERP 或任何入站路径**。

```
┌─ 家里 (无公网 IP) ─────────────┐         ┌─ 海外 VPS ────────────────┐
│ linkbox (常驻)                  │         │                           │
│  ├─ ssh -N -R (主动拨出) ───────┼─ SOCKS ─┼─> sshd ──> 127.0.0.1:7864 │
│  ├─ 端到端健康探测 (复用主连接)  │         │    │                      │
│  └─ 面板 127.0.0.1:8085        │         │    └─ 由你的服务消费       │
└────────────────────────────────┘         └───────────────────────────┘
```

## 为什么不是 frp / autossh

- **frp 的 dashboard 在服务端。** 你人在家里，看不到自己拨出去的隧道死没死。
  linkbox 的面板在拨出侧——这正是它的存在理由。
- **autossh 只会重启进程，不管转发是否真的可用。** ssh 进程活着 ≠ 隧道能用：
  远端端口被抢时 ssh 正常登录但静默不转发（除非 `ExitOnForwardFailure`）。
- linkbox 的健康检查是**端到端**的：周期性从远端主机对转发端口发真实请求，
  拿到 HTTP code + RTT 才算健康，否则面板标 `degraded`。

## 特性

- 单二进制，内置 SOCKS5 stdio bridge（ssh 的 ProxyCommand），不依赖 nc/connect/proxychains
- ControlMaster 复用主连接：健康探测一次几毫秒，不再新建握手
- 指数退避 + 抖动重连；配置热重载；`enabled` 开关写回 TOML 且保留注释
- 原子写配置；pid 注册表防双 forwarder；子进程退出保证 reap + 清 socket
- 面板：状态 / RTT / 进程日志尾 / 重启 / 启停 / TOML 编辑器（3s 轮询，零外部依赖）

## 快速开始

```bash
cargo build --release
sudo cp target/release/linkbox /usr/local/bin/
sudo mkdir -p /etc/linkbox
sudo cp linkbox.toml /etc/linkbox/
linkbox check --config /etc/linkbox/linkbox.toml   # 预览每条隧道的 ssh 命令
sudo cp linkbox.service /etc/systemd/system/
sudo systemctl enable --now linkbox
```

面板：`http://127.0.0.1:8085`（只听回环；对外暴露请挂你已有的网关 + 鉴权）

## 配置

```toml
[panel]
bind = "127.0.0.1:8085"
# token = "change-me"        # 设置后 API/面板需 Authorization: Bearer <token>

[defaults]
health_interval_secs = 15
health_cmd = "curl -sS -o /dev/null -w \"%{http_code} %{time_total}\" --max-time 8 http://127.0.0.1:{remote_port}{health_path}"
health_expect = "200"

[[tunnels]]
name = "workbuddy-api"
host = "152.53.165.119"
ssh_port = 9551
via_socks = "127.0.0.1:31002"   # 删掉此行 = 直连
remote_port = 7864
local_target = "127.0.0.1:7864"
health_path = "/dashboard"
```

占位符：`{remote_port}` `{local_target}` `{health_path}`。
把 `health_cmd` 设为空串可禁用端到端探测（只剩进程存活判断）。

## 状态机

| phase | 含义 |
|---|---|
| `disabled` | 配置里 `enabled = false` |
| `dialing` | ssh 正在拨出 |
| `up` | ssh 活着且端到端探测通过 |
| `degraded` | ssh 活着但远端探测失败（端口被抢/服务挂了） |
| `down` | ssh 不在，退避重连中 |

## 设计笔记（踩过的坑）

- `ExitOnForwardFailure=yes` 是生死线：否则远端端口被占时 ssh 静默"在线但不转发"
- `ControlPersist=no`：`yes` 会让 master 自行后台化、前台 ssh exit 0，端口被孤儿占死
- ssh 的 ProxyCommand 下 stdin 是关闭的（`-N`）：bridge 收到 stdin EOF 只能半关，
  绝不能 shutdown 整条 socket，否则隧道秒断（这就是那个"exit status: 0"幽灵）
- 子进程用 Drop guard 保证 kill+reap，防止 future 取消时孤儿逃逸

## License

MIT
