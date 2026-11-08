# 真机测试指南（Windows / RC003）

本仓库在 Linux orb 上开发，产物通过 mingw 交叉编译为 Windows x64 二进制。
机制链路只能在真机（Windows 10/11 x64 + 小米 RC003 蓝牙遥控器）上验证。

## 0. 前置条件

- Windows 10/11 x64，管理员账户
- 小米 RC003（型号 R861，VID 0x2717 / PID 0x32B8）已配对连接
- 可选：杀软放行（未签名的 helper 注入系统进程，Defender 可能拦截；先加排除目录）

## 1. 构建

在 Windows 上（推荐，MSVC）：

```powershell
rustup default stable-x86_64-pc-windows-msvc
cargo build --release
```

或使用仓库 CI/交叉产物（mingw 构建，功能等价）：

```bash
cargo build --release --target x86_64-pc-windows-gnu
```

产物：`rc-helper.exe`、`rc_agent.dll`、`rc-app.exe`。**`rc_agent.dll` 必须与
`rc-helper.exe` 同目录**。

## 2. 无 UI 逐步验证（推荐第一次真机跑这条线）

以管理员身份打开 PowerShell，在产物目录：

```powershell
# 1) 扫描定位：应输出 Exclusive 判决 + WUDFHost pid（不注入）
.\rc-helper.exe --dry-run

# 2) 观察模式：注入但只上报不清键；按遥控器按键，观察日志里的 edge 行
.\rc-helper.exe --observe --duration 60 --log con

# 3) 哨兵键验收（关键！）：清一个 Windows 本来可用的键（Home 0x4A）
#    预期：按 Home 无反应（被清）；Ctrl+C 退出 helper 后 2 秒内 Home 恢复原生
#    这就是"拦截生效 + 租约 fail-open"的直接证据
.\rc-helper.exe --canary-usage 0x4A --duration 60 --log con
```

判读：
- `dry-run` 报 `Shared`/`Ambiguous` → 该 WUDFHost 还承载别的 BLE 设备，默认
  拒绝注入（安全闸门，等价参考实现）；确认后再用 `--force --target-pid <pid>`。
- 观察模式日志出现 `edge usages=[0x28]`（按 OK 键）等 → 报告层读取链路通。
- 哨兵键生效且退出即恢复 → 清键写回 + 租约机制正确。
- 日志出现 `unsupported_prologue:xxxx` → ntdll 前导码不在模式表内（预期
  fail-open，不拦截），把十六进制字节记下来报回，扩展 `rc-agent::hook` 的
  模式表即可。

## 3. 完整链路（带 UI）

```powershell
.\rc-app.exe
```

1. App 内 "Install helper task (UAC once)" → 一次 UAC；
2. 打开 "Enhanced capture" → 状态面板应变为 Connected + generation 增长；
3. 给 Back / Vol+/Vol- 配置动作（这三个键没有原生路径，是最硬的证据：
   配置 VolumeUp=Volume Up 后按遥控器音量键，系统音量应变化）；
4. 关闭 capture / 退出 app → helper 2 秒内 fail-open，其余按键恢复原生。

## 4. 故障排查

| 现象 | 检查 |
| ---- | ---- |
| helper 连不上 app | `%LOCALAPPDATA%\RemoteMicRune\bridge.ini` 是否为本次运行的 pid/token |
| agent 没上线 | `%TEMP%\rcr-agent-<WUDFHost pid>.log`；helper 日志在 `%ProgramData%\RemoteMicRune\` |
| 按键卡住 | 预期不会发生（报告是状态语义 + 租约 fail-open）；若复现，记录 helper 日志的 `lease_expired` 计数 |
| 音量键无效 | 确认 remote 是 RC003（RC001 报告格式不同，本版不支持） |

## 5. 安全机制自检清单（机制对等验收）

- [ ] helper 被 `taskkill /f` 强杀后 ≤2 s：被清按键恢复原生（租约 fail-open）
- [ ] app 退出后 helper 自杀（`--follow-app`），agent 收到 Disarm/Bye
- [ ] 断开蓝牙重连：helper 走 R 恢复窗口 → pnputil → 重注入（60 s 冷却）
- [ ] 共享宿主拒绝注入；`--dry-run` 不写任何进程内存
- [ ] `--observe` 模式零清键（只上报）
- [ ] MAX_CLEARS 上限存在（代码常量，不必真机触发）
