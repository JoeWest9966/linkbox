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
  拿到 HTTP code + RTT 才算健康。但一次失败不等于坏——见下面的「strike」规则。
- **strike 规则（防误报）**：探测通道和业务流量共用同一条 ssh TCP 连接，也就共用
  一个拥塞窗口。跨境 250ms RTT 的链路上，业务上传会把探测的小包堵在 250KB 的
  发送队列后面。所以：`timeout` 连续 **3** 次才降级，硬失败（拒绝/重置/答案不对）
  连续 **2** 次才降级；低于阈值面板仍是 `up`，只标 `slow` 并显示 `strike n/N`。
  一次成功立刻回到 `up`。

## 特性

- 单二进制，内置 SOCKS5 stdio bridge（ssh 的 ProxyCommand），不依赖 nc/connect/proxychains
- ControlMaster 复用主连接：健康探测一次几毫秒，不再新建握手
- 指数退避 + 抖动重连；配置热重载；`enabled` 开关写回 TOML 且保留注释
- **面板里直接增删改隧道**，不必手写 TOML；改动落盘时用 `toml_edit` 只重写
  那一个 `[[tunnels]]` 段，你手写的注释、空行、自定的 `[defaults]` 全部原样保留
- 原子写配置；pid 注册表防双 forwarder；子进程退出保证 reap + 清 socket
- 面板：状态 / RTT / 进程日志尾 / 重启 / 启停 / 增删改 / TOML 编辑器（3s 轮询，零外部依赖）
- 相位语义诚实：**进程起来 ≠ 隧道通了**。spawn 只是 `dialing`，只有端到端探测
  通过才变 `up`，探测转为失败变 `degraded`，进程死了才 `down`

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

`linkbox.service` 里带了 `ProtectSystem=full`（`/etc` 只读），所以必须同时有
`ReadWritePaths=/etc/linkbox` —— 否则面板能跑隧道、却改不了配置，保存时报
`write /etc/linkbox/.linkbox.toml.tmp: Read-only file system`。改 unit 后记得
`systemctl daemon-reload`。

面板：`http://127.0.0.1:8085`（只听回环；对外暴露请挂你已有的网关 + 鉴权）

### 面板 API

| 方法 | 路径 | 作用 |
|---|---|---|
| GET | `/api/state` | 每条隧道的相位、RTT、pid、日志尾 |
| GET | `/api/tunnels` | 配置里的隧道原文 + `[defaults]`（用于表单预填） |
| POST | `/api/tunnels` | 新建或按 `name` 覆盖更新一条隧道 |
| DELETE | `/api/tunnels/{name}` | 删除隧道并断开转发 |
| POST | `/api/tunnels/{name}/restart` \| `/enable` \| `/disable` | 单条操作 |
| GET \| POST | `/api/config` | 读/整份写 TOML（写前校验，非法则拒绝并保留旧配置） |

```bash
curl -X POST http://127.0.0.1:8085/api/tunnels -H 'content-type: application/json' -d '{
  "name":"jellyfin","host":"152.53.165.119","ssh_port":9551,
  "identity":"/root/.ssh/cnb_key","via_socks":"127.0.0.1:31002",
  "remote_port":8096,"local_target":"127.0.0.1:8096","enabled":true
}'
```

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

## 远端端口租约（为什么不会自锁）

反向转发的端口在**别人的机器上**，而且比你的进程活得久。客户端一死，远端 sshd 会继续
占着 `127.0.0.1:<port>` 直到 `ClientAliveInterval` 到期 —— Debian 默认 600×3，最坏半小时。
这期间每次重拨都是 `remote port forwarding failed`，看起来像网络问题，其实是自己上一代
连接留下的僵尸。所以端口不是"抢"，是"租"：

- 连上后远端跑一个极小的 shell，记下 `端口 + 真正持有该端口的 pid + 时间戳`，文件名是
  `实例指纹-隧道名`（指纹来自 `/etc/machine-id` + 配置路径，重启不变，别的机器不会撞车）；
- 撞上端口占用时先**回读租约**：pid 与租约一致、且该进程确实是 `sshd`，才 kill —— 那是我
  们自己的僵尸；
- 任何不匹配都不杀，标 `conflict` 停下等人。杀一个认不出的进程，就是去搞坏别人的生产。

配套三件事：

1. 关转发先 `SIGTERM` 再 `SIGKILL`。`SIGKILL` 不发 FIN，远端继续占端口；`SIGTERM` 让 ssh
   正常收尾，端口立刻释放 —— 改配置、点 restart 都不会再自锁。
2. 失败按语义分流：`port forwarding failed` → 回收后立即重拨；`Permission denied` → 停住标
   `authfailed` 等人装密钥；超时/拒绝 → 指数退避。
3. 退避期间配置一变（或点了 restart）0.5 秒内醒来，不用等满 60 秒。

服务器端再加一行兜底，覆盖断电、`kill -9` 这类来不及收尾的情况：

```bash
# /etc/ssh/sshd_config
ClientAliveInterval 15
ClientAliveCountMax 3
```

## 状态机

| phase | 含义 | 自愈 |
|---|---|---|
| `disabled` | 配置里 `enabled = false` | — |
| `dialing` | ssh 正在拨出，还没证明能用 | 是 |
| `up` | ssh 活着且端到端探测通过（允许少量 strike 未满） | — |
| `degraded` | ssh 活着，但探测连续失败达到阈值（超时 3 次 / 硬失败 2 次） | 是 |
| `down` | ssh 不在，退避重连中 | 是 |
| `conflict` | 远端端口被占，且不是我们的租约 | 每 30 秒复查，绝不杀认不出的进程 |
| `authfailed` | 认证失败，重试无意义 | 否，等改配置或手动 restart |

## 设计笔记（踩过的坑）

- **健康探测会输给业务流量**：`ssh -R` 的转发通道、`ControlMaster` 探测通道共用一条
  TCP 连接和一个拥塞窗口。实测主连接 `cwnd` 被压到 10、`Send-Q` 常年 150–250KB、
  `rcv_ooopack` 5000+，此时探测几 KB 的请求排在队尾，8 秒拿不到响应——隧道没坏，
  只是忙。单次超时就翻红是假警报，所以才有 strike 阈值。
- `ExitOnForwardFailure=yes` 是生死线：否则远端端口被占时 ssh 静默"在线但不转发"
- `ControlPersist=no`：`yes` 会让 master 自行后台化、前台 ssh exit 0，端口被孤儿占死
- ssh 的 ProxyCommand 下 stdin 是关闭的（`-N`）：bridge 收到 stdin EOF 只能半关，
  绝不能 shutdown 整条 socket，否则隧道秒断（这就是那个"exit status: 0"幽灵）
- 子进程用 Drop guard 保证 kill+reap，防止 future 取消时孤儿逃逸
- 关子进程必须先 SIGTERM：SIGKILL 不发 FIN，远端 sshd 会继续占着转发端口最长半小时，
  于是"改个配置"演变成 113 次无意义重试
- `ServerAliveInterval` 只是单向心跳（我发现服务器死了）；服务器发现我死了只认 FIN 或
  `ClientAliveInterval`，两件事别混为一谈

## License

MIT
