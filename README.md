# Invoice-Reconciliation · 报销对账 Agent

GOSIM Agentic App 黑客松 2026 参赛作品。

月底把发票、微信支付账单和滴滴行程单发进 Rinx 聊天：Agent 读票、过门禁、按证据对齐实际消费日期，把拿不准的事交给人判断；确认后生成报销包、自己终审、交给财务，并继续跟进还没到的发票。

**原则：模型写理解和说明，不写事实。** 金额、日期、票号、终审结果都来自确定性代码；模型的读数只是「候选」，须本人对照原件确认后才能入账；模型的说明文字经校验器逐字检查，编造的数字、日期、状态会被拒掉。

## 一次完整的报销（剧本）

| # | 发生了什么 | 谁来做 |
|---|---|---|
| 1 | 月底最后一天 09:00 提醒申请人交材料 | bot |
| 2 | 申请人在私聊里发文件，逐一回执；名单外的人被拒 | 申请人 |
| 3–6 | 「开始对账」：收件封存、读票（图片票经模型读成候选）、门禁（抬头、查重、重开票）、按账单与行程单关联、找漏票，回一张报告：自动匹配 6 张 / 待你判断 4 张 / 拒收 2 张 | bot + 核心 + Agent |
| 7 | 打开报告卡片进入对账台，页面只显示 6 位码；把码发给 bot 完成配对 | 申请人 |
| 8–9 | 核对图片票读数、确认截图、写超标说明、确认重开票替换 | 申请人 |
| 10–11 | 确认生成（绑定版本，确认新简称）→ 打包 → 七组终审 → 原子发布 → 回帖与下载 | 申请人 → 核心 |
| 12–14 | 分享给财务；财务用自己的账号配对审核；退回某项 → 新版本 → 模板化补充说明 → 重建（其余不变）→ 重新提交 → 通过 | 财务、申请人 |
| 15 | 漏票跟进：是不是公务、怎么取票、开票信息、定期提醒；把票发进聊天自动认领，归入下一批次 | 申请人 + bot |
| 17 | 失败与重复：重复文件、坏文件、核心崩溃、产物被占用、终审不过、过期页面、重启 | 全链路 |

详细流程与设计见 [冻结设计](docs/design.md)，演示讲稿见 [demo-script.md](docs/demo-script.md)。

## 演示截图

> 截图位：以下画面在录制演示时补入 `docs/images/`（全部为虚构数据）。

| 画面 | 文件 |
|---|---|
| Rinx 私聊：15 个文件逐一回执 | `docs/images/01-files.png`（待补） |
| 报告卡：6/4/2、可能漏票 2 笔、对账台卡片 | `docs/images/02-report.png`（待补） |
| 对账台：配对码 → 配对成功 | `docs/images/03-pairing.png`（待补） |
| 对账台：图片票读数核对（候选 → 本人确认） | `docs/images/04-visual-check.png`（待补） |
| 回帖：终审全部通过（7/7）、报销包下载 | `docs/images/05-published.png`（待补） |
| 财务视角：审核、退回一项 | `docs/images/06-finance.png`（待补） |
| 漏票跟进：开票信息、自动认领 | `docs/images/07-follow-up.png`（待补） |

## 架构

```
Rinx（Matrix 客户端，原生 mini_app 卡片）
   │  Matrix（本机 Palpo）
reimb-bot（Rust）
   ├─ Matrix 适配：持久设备会话、按保存的同步令牌续跑、入站事件去重、outbox 按固定 txn_id 发送
   ├─ 编排与状态机：SQLite；状态转移、审计、待发消息同一事务；revision 绑定；确认快照；崩溃恢复
   ├─ 对账台（HTTP）：聊天配对、会话 cookie、Host/Origin/CSRF、按批次与身份鉴权
   └─ AgentPort：octos（真实模型）/ 录制回放 / 规则模式，输出一律经校验器
        │  JSON 信封，子进程
reimb_core（Python）：收件、读票、门禁、证据、关联与占用、漏票、认领、打包、终审、历史台账
```

## 快速开始

需要 macOS、Docker、Rust、Python 3.12 与 [uv](https://docs.astral.sh/uv/)。

```sh
uv sync
python3 scripts/e2e.py        # 一条命令：起本机 Palpo，跑真实 bot，走剧本 1–15、17 并对比标准答案
```

手动演示、在 Rinx 里操作、只跑测试，见 [运行与复现](docs/run.md)。

```sh
uv run pytest core/                                          # 核心，经真实 CLI 子进程
cargo test --locked --manifest-path bot/Cargo.toml           # bot、对账台、状态机、恢复
```

## 数据与安全

- 仓库里的公司、人名、票号、账单全部是虚构的（`fixtures/generate.py` 生成，字节固定）；标准答案在 `fixtures/expected/` 手写，不由生成器或生产代码产生。
- 凭据只生成在本机 `$REIMB_DATA`（默认 `~/.reimb-demo`，仅本人可读），不进仓库；`scripts/check_public.py` 扫描公开内容。
- 服务只绑定 127.0.0.1；这是演示实现，不能用于真实报销，限制见 [limitations.md](docs/limitations.md)。

## 文档

- [冻结设计](docs/design.md) 与 [设计勘误](docs/design-amendments.md)
- [核心契约](docs/spec/README.md)
- [运行与复现](docs/run.md)
- [演示讲稿](docs/demo-script.md)
- [当前限制](docs/limitations.md)

## 许可证

[Apache License 2.0](LICENSE)
