# 本机启动与验证

当前实现：P0 连接探路（独立 Palpo、测试账号、Rust Matrix bot、静态连接页、octos 文字/图像探测）、P1 确定性核心（收件、读票、门禁、历史、打包、终审），以及 P2 关联与 Agent 层（证据、关联与占用、退款、日限额、漏票、AgentPort 与校验、命令行对账）。授权配对与聊天流程尚未实现，不能用于真实报销。

## P1 确定性核心

Python 3.12，依赖由 `uv.lock` 锁定：

```sh
uv sync
uv run pytest core/                        # 全部测试，经真实 CLI 子进程
uv run python fixtures/generate.py         # 重新生成虚构样例（字节固定，测试会比对已提交版本）
uv run python scripts/check_public.py      # 公开仓库扫描
```

单独调用核心：stdin 送一份 JSON 信封，数据根由环境变量给出，所有路径须在数据根之内：

```sh
PYTHONPATH=core REIMB_DATA=/absolute/private/root uv run python -m reimb_core ingest < request.json
```

信封、命令与错误码见 `docs/spec/README.md`，P1 的细化见 `docs/spec/p1-core.md`，设计勘误见 `docs/design-amendments.md`。

## P2 命令行对账

`reimb-reconcile` 跑一遍完整的第一轮对账：收件 → 读票（A1）→ 门禁 → 分类建议（A3）→ 证据 → 关联（多候选时 A2）→ 漏票 → 解释（A4）→ 报告卡 JSON。所有路径须为绝对路径，上传目录与批次目录都在数据根内。

```sh
cargo build --locked --manifest-path bot/Cargo.toml --bin reimb-reconcile
reimb-reconcile --data-root /abs/root --batch-dir /abs/root/batch-2026-10 --policy /abs/root/policy.yaml \
  --uploads /abs/root/uploads --history /abs/fixtures/demo/history.json --period 2026-10 --applicant 林一 \
  --python /abs/repo/.venv/bin/python --core-dir /abs/repo/core \
  --agent replay:/abs/repo/fixtures/agent-replay/demo      # 或 octos:<数据目录>，或 none（规则模式）
```

- `--agent replay:` 用 `fixtures/agent-replay/demo/` 里一次真实 octos 运行的录制（按请求哈希取回），不连模型。
- `--agent octos:<目录>` 走真实 octos stdio，目录用 `scripts/octos_smoke.py` 准备的隔离档（工具全禁，档案 id `reimb-smoke`）；加 `--record <目录>` 把回答录下来。octos 起不来时整批进入规则模式并在报告里标明。
- `--agent none` 为规则模式：门禁、关联、打包、终审照常，说明文字用规则模板；图片票读不出时报告会说明漏票候选可能含它的支付。
- 测试：`cargo test --locked --manifest-path bot/Cargo.toml`（端到端测试调用真实核心子进程，需先 `uv sync`），`uv run pytest core/`。样例生成依赖 macOS 上 pdfium 对 STSong-Light 的字体替换来渲染 F09 与 F06 截图；其他平台生成的图片字节可能不同。

## 版本与前提

macOS arm64；Docker daemon 已启动；Python 3.9+（运行脚本）、Rust/Cargo。业务核心后续固定 Python 3.12。版本真源为 `scripts/versions.json`；Rust 依赖由 `bot/Cargo.lock` 锁定。Matrix SDK 同 Rinx 依赖的源提交，但 bot 不启用加密模块。

凭据只在 `$REIMB_DATA`（默认 `~/.reimb-demo`）中生成，目录仅本人可读；不得选择源码目录作为数据目录，不要把该目录打包提交。现有容器不被删除或修改；本项目容器/网络使用 `reimb-demo` 前缀和所有权标签，端口固定为 loopback `18128`。容器名或端口冲突会失败，不自动抢占资源。

## 一条命令启动后端环境

```sh
uv sync                       # 核心依赖（bot 通过 .venv/bin/python 调用核心）
python3 scripts/dev.py
```

它先建立独立 PostgreSQL/Palpo、虚构账号 `reimb-linyi`（申请人）、`reimb-zhoumin`（财务）、`reimb-bot`、`reimb-intruder`（无权限的第三方，只用于权限测试）及四个不加密私聊，再把私有配置写到 `$REIMB_DATA/bot.json`（0600），按 lockfile 编译并前台运行 `reimb-bot`。保留该终端；Ctrl-C 停 bot，项目容器保留以便复跑。单独补建环境运行 `python3 scripts/dev_env.py`，重复执行复用本项目账号/房间，不换密码。

- Agent 默认用 `fixtures/agent-replay/demo` 的录制回放（只对演示样例有效）；`REIMB_AGENT=octos:<目录>` 走真实 octos，`REIMB_AGENT=none` 为规则模式。
- bot 的 Matrix 会话保存在 `$REIMB_DATA/work/bot/matrix-session.json`（0600），重启复用同一设备，不再每次登录新增设备。
- 同步令牌存进状态库；bot 停机期间收到的消息在重启后补处理，入站事件按 event_id 去重。
- 对账台访问日志：`$REIMB_DATA/work/desk-access.log`，每行只有时间、方法、路径、状态码、耗时，不记 cookie、令牌或请求体。

## 剧本（P3：1–11）

1. 月底最后一天 09:00（Asia/Shanghai）bot 给申请人发一次提醒（同一人同一月只发一次）。
2. 申请人在与报销助手的私聊里发送发票、微信账单 xlsx、滴滴行程单 PDF、订单截图；每个文件回一条收件确认，重复文件回「已处理，未重复入账」；名单外的人被拒绝。
3–6. 说「开始对账」：封存收件、读票（图片票经 A1 为候选）、门禁、证据与关联、漏票，回一张报告（6/4/2）和一张对账台卡片，卡片 URL 只含批次 id：`http://127.0.0.1:8787/desk/b/<batch_id>`。
7. 打开卡片：页面只显示 6 位配对码；把码发给报销助手完成配对（10 分钟有效、一次性；同一发送者 10 分钟内错 5 次锁定；只能配对自己的批次，财务只读）。
8–9. 在对账台核对图片票读数、确认截图读数、填写超标说明、确认重开票替换；每次提交带 `expected_revision`，版本过期会提示刷新。
10. 全部判断完后「确认生成」：冻结快照 → 打包 → 终审（七组）→ 原子发布到 `published/<revision>/` 并更新 `CURRENT`。
11. bot 回帖「终审全部通过（7/7）」和对账台卡片，报销包可在对账台下载。

自动化覆盖：`cargo test --locked --manifest-path bot/Cargo.toml`（服务层剧本、对账台授权必测项、崩溃恢复）；真实 Matrix 端到端需本机 Palpo：`REIMB_LIVE=1 cargo test --locked --manifest-path bot/Cargo.toml --test matrix_live`。

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

check_public 扫描本机已知测试凭据、私钥标记，以及税号、手机号、邮箱、20 位票号模式（虚构税号须含 `XXXXXXXX`；邮箱只放行 `.example`、`.invalid`、`reimb.local`；票号只允许出现在 `fixtures/` 与 `tests/`）。PDF 与 XLSX 按文字层与单元格扫描，图片不扫描。它不等于完整隐私审计；公开提交仍须人工检查差异，确认都是虚构数据。

接口来源：官方 SDK Client 文档 https://matrix-org.github.io/matrix-rust-sdk/matrix_sdk/struct.Client.html 。本项目实际编译以 lockfile 固定源码为准。

## P0 界面取证经验

显示器休眠时 Makepad remote 的状态接口仍正常，截图与控件接口会超时。经外环授权可用进程级 `caffeinate -d -i` 保持屏幕常亮，结束后停止该进程；已锁屏则停止验收，不尝试解锁。测试客户端以 `/quit` 正常退出。

当前已验证卡片 → 原生容器 → Open in browser → 对账台页面路径；内嵌网页内容未验证。截图、控件快照和操作序列保存在任务 reports 中。
