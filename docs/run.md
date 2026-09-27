# 本机启动与 P0 验证

当前实现是 P0 连接探路：独立 Palpo、测试账号、Rust Matrix bot、静态连接页、octos 文字/图像探测。业务报销流程、授权配对与打包尚未实现，不能用于真实报销。

## 版本与前提

macOS arm64；Docker daemon 已启动；Python 3.9+（运行脚本）、Rust/Cargo。业务核心后续固定 Python 3.12。版本真源为 `scripts/versions.json`；Rust 依赖由 `bot/Cargo.lock` 锁定。Matrix SDK 同 Rinx 依赖的源提交，但 bot 不启用加密模块。

凭据只在 `$REIMB_DATA`（默认 `~/.reimb-demo`）中生成，目录仅本人可读；不得选择源码目录作为数据目录，不要把该目录打包提交。现有容器不被删除或修改；本项目容器/网络使用 `reimb-demo` 前缀和所有权标签，端口固定为 loopback `18128`。容器名或端口冲突会失败，不自动抢占资源。

## 一条命令启动后端环境

```sh
python3 scripts/dev.py
```

它先建立独立 PostgreSQL/Palpo、虚构账号 `reimb-linyi`、`reimb-zhoumin`、`reimb-bot` 及三个不加密私聊，再按 lockfile 编译并前台运行 Rust bot。保留该终端；Ctrl-C 停 bot，项目容器保留以便复跑。单独补建环境运行 `python3 scripts/dev_env.py`，重复执行复用本项目账号/房间，不换密码。

页面地址：`http://127.0.0.1:8787/desk/b/p0-demo`。它仅证明嵌入页面链路，不展示发票，不含凭据。

## 准备 Rinx

克隆官方仓库至自选外部目录（不得嵌套在本项目里），checkout `scripts/versions.json` 的 `rinx_commit`。在该目录执行：

```sh
cargo build --locked --features agent_chat
```

该基线的包和可执行名为 **robrix**。Cargo 全局 target-dir 可改变产物位置，用 `cargo metadata --format-version 1 --no-deps` 的 target_directory 确认；不要沿用其他提交产生的二进制。

回本项目执行（路径换成本机实际值）：

```sh
python3 scripts/prepare_rinx.py --checkout /absolute/Rinx --binary /absolute/cargo-target/debug/robrix
```

该脚本通过本机 Palpo 真实登录创建独立设备 session，写入 Rinx 官方持久化格式；不显示密码或 token，不使用个人账户。两个 app 在 `$REIMB_DATA/apps/`，可分别打开申请人和财务。隔离变量实际为 `ROBRIX_DATA_DIR`，是对冻结设计中变量名的实测勘误；两个实例不得共用数据目录。

申请人在与报销助手的测试房间输入 `开始对账` 或 `P0 echo`，bot 返回固定测试回声、格式化说明和原生 mini_app 卡片。打开卡片应显示连接检查页。P0 bot 只响应虚构申请人，尚无持久 outbox 或业务权限逻辑。

## 原生界面验收

本任务外环裁定使用固定 Makepad 自带遥控接口代替 CUA。开发验收时在启动 app 前设置 `MAKEPAD_REMOTE=18131`（申请人）或 `18132`（财务），并为每个进程设置独立 ROBRIX_DATA_DIR。接口只监听 loopback。

- 先 GET `/help` 核协议；GET `/status` 记录版本实例对应的窗口信息。
- `/dump` 控件树与 `/grab` PNG 必须来自同一实例；操作从可见控件坐标决定，保存输入及响应记录。
- `/click?x=&y=`、`/text?t=`、`/key?k=down&c=ReturnKey` 是真实输入事件；不得向遥控 URL 传密码或 token。
- `/grab` 失败时不得把 Matrix API 结果冒充 UI 成功；保存 `/status`、`/log` 与错误响应，交外环补人工截图。
- 验证后 `/gq` 保存终态并退出；若截图仍失败使用 `/quit`。录制正式演示时不开遥控模式。

## 真实冒烟命令

证据输出路径放源码之外：

```sh
python3 scripts/matrix_smoke.py --out /absolute/reports/matrix-smoke.json
python3 scripts/octos_smoke.py --out /absolute/reports/octos-text.json
python3 scripts/octos_smoke.py --image fixtures/smoke/receipt.png --out /absolute/reports/octos-image.json
```

Matrix 冒烟向测试房间发 `P0 echo`，观察真实 bot 回复并访问卡片 URL；它明确不证明客户端渲染。octos 冒烟使用本机 `octos` 档的模型配置与凭据，复制到项目私有数据目录；移除 channels/MCP/hooks，工具策略 `deny:["*"]`，不改原档。调用 stdio JSON-RPC，60 秒回合上限；严格 JSON 不合法时修一次。图片只使用本仓固定虚构收据，手写期望为 DEMO CAFE、38600 分。

补充检查：

```sh
cargo fmt --manifest-path bot/Cargo.toml -- --check
cargo clippy --locked --manifest-path bot/Cargo.toml -- -D warnings
python3 -m py_compile scripts/*.py
python3 scripts/check_public.py
```

check_public 只对本机已知测试凭据和私钥标记做内容扫描，不等于完整隐私审计；公开提交仍须人工检查差异，确认都是虚构数据。

接口来源：官方 SDK Client 文档 https://matrix-org.github.io/matrix-rust-sdk/matrix_sdk/struct.Client.html 。本项目实际编译以 lockfile 固定源码为准。

## P0 界面取证经验

显示器休眠时 Makepad remote 的状态接口仍正常，截图与控件接口会超时。经外环授权可用进程级 `caffeinate -d -i` 保持屏幕常亮，结束后停止该进程；已锁屏则停止验收，不尝试解锁。测试客户端以 `/quit` 正常退出。

当前已验证卡片 → 原生容器 → Open in browser → 对账台页面路径；内嵌网页内容未验证。截图、控件快照和操作序列保存在任务 reports 中。
