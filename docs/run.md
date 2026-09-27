# 本机启动与验证

当前实现：剧本 1–15 与 17 的完整闭环——确定性核心（收件、读票、门禁、历史、关联、漏票、打包、终审）、Agent 层（录制回放 / 真实 octos / 规则模式）、Matrix bot 与状态机、对账台（聊天配对、判断、确认、财务审核与退回）、失败态与恢复、漏票跟进。所有账号、公司与票据都是虚构的；这是演示实现，不能用于真实报销。

## 一条命令端到端复现（P5）

```sh
uv sync
python3 scripts/e2e.py
```

它先按下文「一条命令启动后端环境」建好本机 Palpo 与虚构账号，再在 `$REIMB_DATA/e2e/<时间>/` 新建一套私有数据目录与配置，编译并启动真实的 `reimb-bot`（Agent 用 `fixtures/agent-replay/demo` 的录制回放，对账台端口 8797），由脚本扮演林一（申请人）、周敏（财务）和一个无权账号，经 Matrix 与对账台 HTTP 接口依次走：

| 步骤 | 内容 | 对比的标准答案 |
|---|---|---|
| 1 | 月底提醒（时钟置于 2026-10-31 09:00） | — |
| 2 | 陌生人发文件被拒；林一发 15 个文件 | — |
| 3–6 | 开始对账，报告 6/4/2 与两笔漏票候选 | `fixtures/expected/link.json` |
| 7 | 聊天配对；陌生人拿别人的码被拒 | — |
| 8–9 | 视觉核对、截图证据、三项判断 | `demo.json` 的决定 |
| 10–11 | 旧 revision 确认被拒；确认新简称；打包、终审 7/7、发布、下载交接清单 | `demo.json` 的简称、文件名、合计、清单名 |
| 12–14 | 提交财务、周敏用自己的账号配对、退回 F08、补充说明、重建、重新提交、通过 | — |
| 15 | 漏票跟进：开票信息、周一提醒（时钟拨到 2026-11-02 09:00）、个人抬头被拒、换开票与酒店票认领 | `claim.json` |
| 17 | 同一文件再发、错码超限、过期页面操作被拒、停机期间发的消息在重启后被处理、每条结果回帖恰好一次 | — |

每步打印 `ok` 或失败原因，最后一行是 JSON 摘要；逐项检查写在 `$REIMB_DATA/e2e/<时间>/result.json`，bot 日志在同目录 `bot.log`。退出码 0 表示全部通过。本机已有别的 `reimb-bot` 在跑时脚本拒绝启动（它会抢答同一批房间），请先停掉 `scripts/dev.py`。

可替换的边界只有两处：Agent（录制回放）与时钟（配置项 `clock_file`，bot 每次取时间都读这个文件；只供端到端脚本用，正常运行不设）。Matrix、SQLite、核心子进程、对账台与文件系统都是真实组件。

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
- 同步令牌存进状态库；重启后从该令牌续同步，处理器先就位，停机期间收到的消息在重启后补处理，入站事件按 event_id 去重。待发消息直接经客户端 API 按原 txn_id 发送，不依赖同步到的房间状态。
- 对账台访问日志：`$REIMB_DATA/work/desk-access.log`，每行只有时间、方法、路径、状态码、耗时，不记 cookie、令牌或请求体。

## 手动走剧本（`scripts/dev.py` 起的 bot）

对账台地址 `http://127.0.0.1:8787/desk/b/<batch_id>`，由聊天里的卡片打开。

1. 月底最后一天 09:00（Asia/Shanghai）bot 给申请人发一次提醒（同一人同一月只发一次）。
2. 申请人在与报销助手的私聊里发送发票、微信账单 xlsx、滴滴行程单 PDF、订单截图；每个文件回一条收件确认，重复文件回「已处理过，未重复入账」；名单外的人被拒绝。
3–6. 说「开始对账」：封存收件、读票（图片票经 A1 为候选）、门禁、证据与关联、漏票，回一张报告（6/4/2、可能漏票 2 笔）和一张对账台卡片，卡片 URL 只含批次 id。
7. 打开卡片：页面只显示 6 位配对码；把码发给报销助手完成配对（10 分钟有效、一次性、每码最多 5 次错误尝试；同一发送者 10 分钟内错 5 次锁定；只能配对自己有权看的批次）。配对 1 小时有效，页面每 5 秒刷新一次状态。
8–9. 在对账台核对图片票读数、确认截图读数、填写超标说明、确认重开票替换；每次提交带 `expected_revision`，版本过期会提示刷新。说「重开收件」可在确认前补文件，之前的判断作废、重新对账后再判断。
10. 全部判断完后核对「简称」（不在公司简称表里的可修改），点「确认生成」：冻结快照 → 打包 → 终审（七组）→ 原子发布到 `published/<revision>/` 并更新 `CURRENT`。
11. bot 回帖「终审全部通过（7/7）」和对账台卡片，报销包可在对账台下载。
12. 申请人点「提交给财务」：财务私聊收到通知与同一张卡片；财务用自己的账号配对，只能审批或退回。
13. 财务按条目填写原因退回：批次回到待判断、版本加一，只有被退回的条目可改。
14. 申请人填写事由与人数（模板生成说明）→ 确认生成（其余条目逐字段核对未变）→ 重新提交 → 财务通过。
15. 通过后，bot 列出可能漏票：在对账台逐笔选「是公务 / 不是（只忽略这一笔）/ 以后都不提醒这个商户」，是公务再选取票办法，聊天里会收到开票信息；每周一 09:00、截止前 3 天各提醒一次，过截止日提示「写无票说明，还是放弃」。开好的发票直接发进聊天，bot 自动认领并归入下一批次；抬头、金额或日期不对会说明原因、不收。
17. 失败与重复：同一文件再发不重复入账；超限或读不出的文件说明原因；核心崩溃转「等人工」，回复「重试」继续；产物目录被占用时进入执行等待、不覆盖、自动重试；终审不过退回待确认，三次转人工；停机期间发来的消息在重启后处理，已发出的回帖不重复。

自动化覆盖：`cargo test --locked --manifest-path bot/Cargo.toml`（服务层剧本、对账台授权必测项、财务往返、失败出口、漏票跟进、崩溃恢复）；真实 Matrix（需本机 Palpo，剧本 2–15）：`REIMB_LIVE=1 cargo test --locked --manifest-path bot/Cargo.toml --test matrix_live`；真实浏览器轮询（需 Chrome）：`REIMB_BROWSER=<Chrome 可执行文件> cargo test --locked --manifest-path bot/Cargo.toml --test browser`；一条命令全流程见上文 `scripts/e2e.py`。

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

先用 `python3 scripts/dev.py` 起 bot，再打开申请人 app，在与报销助手的私聊里按上面的剧本操作；财务 app 用来走 12–14。报告与回帖里的卡片在 Rinx 的原生 mini_app 容器里打开对账台。

## 原生界面验收

本任务外环裁定使用固定 Makepad 自带遥控接口代替 CUA。开发验收时在启动 app 前设置 `MAKEPAD_REMOTE=18131`（申请人）或 `18132`（财务），并为每个进程设置独立 ROBRIX_DATA_DIR。接口只监听 loopback。

- 先 GET `/help` 核协议；GET `/status` 记录版本实例对应的窗口信息。
- `/dump` 控件树与 `/grab` PNG 必须来自同一实例；操作从可见控件坐标决定，保存输入及响应记录。
- `/click?x=&y=`、`/text?t=`、`/key?k=down&c=ReturnKey` 是真实输入事件；不得向遥控 URL 传密码或 token。
- `/grab` 失败时不得把 Matrix API 结果冒充 UI 成功；保存 `/status`、`/log` 与错误响应，交外环补人工截图。
- 验证后 `/gq` 保存终态并退出；若截图仍失败使用 `/quit`。录制正式演示时不开遥控模式。

## 真实冒烟命令（P0 探路）

证据输出路径放源码之外：

```sh
python3 scripts/matrix_smoke.py --out /absolute/reports/matrix-smoke.json
python3 scripts/octos_smoke.py --out /absolute/reports/octos-text.json
python3 scripts/octos_smoke.py --image fixtures/smoke/receipt.png --out /absolute/reports/octos-image.json
```

`matrix_smoke.py` 是 P0 探路时对回声 bot 的冒烟（向测试房间发 `P0 echo`）；现在的 bot 不再回声，这条命令已被 `scripts/e2e.py` 取代，保留仅作 P0 证据的复现记录。octos 冒烟使用本机 `octos` 档的模型配置与凭据，复制到项目私有数据目录；移除 channels/MCP/hooks，工具策略 `deny:["*"]`，不改原档。调用 stdio JSON-RPC，60 秒回合上限；严格 JSON 不合法时修一次。图片只使用本仓固定虚构收据，手写期望为 DEMO CAFE、38600 分。

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

P3 界面实跑已验证卡片 → 原生容器 → 内嵌对账台页面（服务端访问日志与系统截图两路取证）。Makepad 遥控接口进不了网页叠层，自动化里网页内点击改由同一 HTTP 接口完成。截图、控件快照和操作序列保存在任务 reports 中。
