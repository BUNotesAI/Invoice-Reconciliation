# P1 确定性核心：实现细化

本文记录 P1 实现对 `docs/spec/README.md`（bf26a6a 冻结版）的细化。全部是**兼容扩展**：新增可选字段、给规格里未定义的结构补定义，不改必填字段、语义或状态迁移。凡与冻结设计有出入的地方单列在文末「待外环裁定」，实现没有自行改设计。

## 已实现命令

`ingest`、`extract`、`gates`、`history`、`package`、`verify`。`evidence`、`link`、`missing`、`claim` 属于 P2/P4c，本片返回 `UNKNOWN_COMMAND`。

运行：

```sh
PYTHONPATH=core REIMB_DATA=<私有数据根> uv run python -m reimb_core <command> < request.json
```

## 细化

| 位置 | 细化 | 理由 |
|---|---|---|
| JSON 边界 | 除重复键、NaN/Infinity 外，**任何浮点数**都拒绝（`INVALID_JSON`） | 金额与 revision 只能是整数；浮点在进入领域前就被挡住 |
| `SourceFile.detected_type` | 封闭枚举：`invoice_pdf` / `image_invoice_pdf` / `didi_trip_pdf` / `wechat_bill` / `image` / `unsupported`；id = sha256 = storage_object_id | 设计 §5.2 的六类；外环 F5：微信账单按 xlsx（表头按内容定位），滴滴行程单按 PDF |
| `Provenance.locator` | 带 `type` 的五种对象，按规格；文字层事实用 `pdf_page`（页号 + 字段），视觉读数用 `image_region`，确认事实用 `user_confirmation`，且确认事实的 locator 必须与 `candidate_id / confirmed_by / confirmation_event / confirmed_at` 一致 | 规格要求可定位原件 |
| `Invoice.buyer_tax_id` | 允许空字符串（个人抬头票），门禁以 `WRONG_BUYER` 拒收 | 设计 F11 与「京东个人抬头」情形 |
| 大写金额 | 只接受标准写法（含 `人民币`、`元/圆`、`整/正`、`角` 后可带 `整` 的变体）；实现为「宽松读数 → 生成标准写法 → 必须相等」 | 设计 §5.2 要求「零」各位置与「整」的单测；标准写法唯一，不会误收错位的零 |
| `Decision.payload` | 七种 kind 各有严格 payload：`confirm_visual{fact_ids}`、`choose_evidence{evidence_id}`、`explain_over_limit{explanation}`、`replace_unpaid_invoice{invoice_no}`、`receipt_only{replaces_invoice_no}`、`reject{reason}`、`manual_evidence{service_date,note}` | 规格「每种 payload 单独定义，无自由 action」 |
| 确认快照 | `Snapshot{batch_id, revision, applicant, period, policy_hash, history_hash, items[], decisions[]}`；每个决定恰被其所属 item 引用一次，`expected_revision ≤ revision`；快照哈希 = 规范化 JSON 的 sha256 | P1 由手写 expected 注入服务日期与决定；P2 加入关联与证据 |
| `package` 防篡改 | 文字层发票**重新解析原件**，所有业务字段须与快照一致；图片发票每个事实都须是 confirmed，且有 `confirm_visual` 决定按 `source_event_id / actor / fact_ids` 绑定；同一附件不得被两项占用 | 交接风险 3：不信任快照里的 extracted 值 |
| 重开票 | 原票须「voided 且可证明从未 paid」，金额与原票相同、服务日期取原票、带 `replace_unpaid_invoice` 决定；原票 paid → `HISTORY_ALREADY_PAID`；状态不明或多条候选 → `HISTORY_UNKNOWN`；一张原票只能被替换一次 | 设计 §5.7 |
| `HistoryEntry` | 可选 `events_complete`、`initial_paid`：只有完整事件流且 `initial_paid=false` 才算「未付款」；`paid` 之后的 `voided` 不抹去付款；状态迁移 submitted→approved→paid，voided 可接任一非作废状态 | 规格「缺支付事件完整性证明的导入标 unknown」 |
| `history project` | 输入 `events[]`，每条 `{invoice_no, status, actor, at, note, expense?}`，只有 submitted 带 `expense`；按顺序折叠为条目，得到的条目 `events_complete=true` | 规格只写了「events」，此处补定义 |
| `gates` 结果 | 增加 `invoice_no`、`history_hash`、`history_issues`、`policy_hash` | 编排器据此构造快照，绑定门禁所用的政策与历史 |
| `Manifest` | 增加 `ledger_name`；交接清单按政策 `naming.ledger` 命名（示例：`示例科技-费用报销发票交接清单-林一-CNY4901.4.xlsx`） | 设计 §7 命名 |
| `verify` | 七组检查：设计 §5.6 的六项，加 P1-fix 的 `source_facts`（快照 ↔ 原件与政策）；读 XLSX 实际单元格与磁盘，并按快照独立重建期望工作簿逐字节比对；清单、快照、附件任一损坏都判失败（fail closed），不抛出 | 设计 §5.6、对抗审查 A3/A4 |
| 住宿与类别 | 规则按发票事实触发（`rules.fact_kind`：项目行先于销售方名称），不按类别标签；政策类别带 `kind`（lodging / meal / local_taxi / air / other），类别须与事实一致，否则 `FIELD_CONFLICT`；住宿票服务日期 = 入住日，超每晚标准须有 `explain_over_limit` | F08、对抗审查 A1 |
| 住宿日期 | `service_period` 的入住、离店、晚数都是 Fact：文字层 extracted，视觉读数 candidate，确认后 confirmed 并受 `confirm_visual` 的 `fact_ids` 绑定 | 对抗审查 A2 |
| 快照哈希 | 执行键 = 校验后快照 `model_dump()` 的规范 JSON 哈希；省略或写明可选字段得到同一快照与暂存目录；暂存的 `snapshot.json` 是规范化形式 | 审查员另注 |
| 标签名 | 类别与简称只允许中英文字母、数字、`·`、括号，首字不能是数字、末字不能是数字，且无 `_`，不能在附件名里伪造金额段 | 对抗审查 A9 |
| 输入上限 | 20 MB / 20 页 / 40 MP（页面、任意深度 Form XObject 内的图片与内联图片都按声明尺寸查，不解码；Form 嵌套 ≤ 8 层）/ 解压流 16 MB / XLSX 解包 32 MB、200 项 / 20 秒 / 100 文件 | 设计 §5.8、交接风险 5、对抗审查 A8 |
| 上传读取 | 路径逐级拒绝 symlink；以 `O_NOFOLLOW` 打开，同一 fd 上 `fstat` 要求普通文件且 `st_nlink == 1`，再从该 fd 读取（拒绝硬链接引入根外文件、消除检查与读取之间的窗口） | 对抗审查 A7 |
| 历史事件顺序 | 按解析后的 UTC 时间比较，不按字符串 | 对抗审查 A6 |
| ingest 崩溃恢复 | 元数据文件是提交标记：对象已写、元数据未写时重跑会补写元数据 | 交接风险 4 |

## 测试

`uv run pytest core/` 全部经真实子进程 `python -m reimb_core`（值解析单测除外）。手写标准答案在 `fixtures/expected/`；`fixtures/generate.py` 不读取它们（有测试守住）。

- 演示样例：12 张票的类型、抽取事实、门禁（2 张拒收、F09 待确认、F10 待重开票判定）、入账 10 张、合计 490140 分、分类 289710 / 156000 / 44430、文件名、六项终审全绿；两次打包字节一致；重复打包幂等。
- 终审反例：改金额、改合计、写入公式、删附件、改附件名金额、换附件内容、换附件并改清单、全链路一致伪造（只有快照能识破）、行顺序调换、多余文件、改清单、改快照、损坏工作簿、历史变化。
- 打包拒绝：快照哈希不符、篡改 extracted 金额、未确认候选、确认缺决定、重开票缺决定、超标缺说明、畸形决定与快照（8 种）、历史重复票、抬头错误、政策变化、暂存产物被改。
- 边界集（P1 部分）：已付款原票重开、状态不明原票、视觉读数票号错一位、大小写同时错、19 位票号、大小写不一致、住宿缺晚数、晚数与日期矛盾、双 20 位数字、公式商户名、HTML 商户名、超页、非发票。
- 加固：超大文件、像素上限（PNG 与 PDF 内嵌图）、解压炸弹（PDF 与 XLSX）、根外路径、`..`、symlink（文件与批次目录）、原始文件名只作展示、重复上传、崩溃恢复、对象被改、100 文件上限、信封与政策校验、错误不回显内容、产物权限 0600。
- 守卫反例（P1-fix，`test_guards.py`）：对抗审查 A1–A9 各项与 M01–M20 每个守卫都有一条走真实 CLI 的反例，断言该守卫自己的错误码或 issue 文本；「模拟打包缺陷」夹具绕过 package 直写暂存，终审必须独立判红。
- 变异：`scripts/mutation_check.py` 列出全部关键守卫（47 个），每个在独立副本上跑全量测试，要求全部被杀；确属等价的写在脚本 `EQUIVALENT` 里并附理由（当前为空）。

## 待外环裁定

1. **餐饮与市内打车日限额**：政策有 `meal_per_day_cents: 15000`、`local_taxi_per_day_cents: 5000`，但设计 §10.1 的期望里 F05（52.00，单日打车）标「自动」，F09（386.00，单日餐饮）只标「核对视觉读数」，都没有超标判断。若执行日限额，F05、F09 会多出超标待判断，报告计数不再是 6/4/2。P1 只执行住宿每晚标准（F08 明确要求）。请外环裁定：日限额是否在 P2 执行，以及 F05、F09 的期望是否要改。
2. **微信账单 CSV**：按 F5，演示与标准答案只用 xlsx；解析器不接受 CSV。如需兼容 CSV 请在 P2 前说明。
