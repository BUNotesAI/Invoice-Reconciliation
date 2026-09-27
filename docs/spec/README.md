# P0 接口与状态契约

本契约细化冻结设计 v1.0，是进入 P1 的评审门禁；此文件不是已实现能力清单。协议版本为整数 `1`。实现只能扩展兼容的可选字段；必填字段、语义或状态迁移变化需外环裁定。

## 进程边界

编排器执行 `python -m reimb_core <command>`，每次一个 JSON 请求，经 stdin 传入，EOF 结束；argv 的 command 必须与 envelope.command 一致。核心不读网络、不调用模型。只输出一份 UTF-8 JSON 和结尾 LF，stdout 禁止诊断文字；stderr 只输出不含数据和凭据的英文诊断。

请求格式：

```json
{"schema_version":1,"command":"ingest","request_id":"req-1","batch_dir":"/absolute/runtime/batch","policy_path":"/absolute/runtime/policy.yaml","input":{"source_path":"/absolute/runtime/upload","original_name":"示例.pdf"}}
```

成功：`{schema_version:1,request_id,ok:true,result:<command result>}`。

失败：`{schema_version:1,request_id:string|null,ok:false,error:{code,message,retriable}}`。完整失败也包含版本和请求 id；无法解析请求时 request_id=null。错误 message 是英文安全说明，不能含文件内容、凭据、原始模型响应或 traceback。

- command 是封闭枚举：ingest / extract / gates / evidence / link / missing / history / package / verify / claim。未知值拒绝。
- 请求最大 4 MiB，响应最大 8 MiB，子进程墙钟 30 秒；单文件解析 20 秒。超限/超时由编排器终止整个子进程组并记录安全错误。
- batch_dir、policy_path、source_path 都是服务端配置或已登记对象解析出的绝对路径，不由 HTTP 客户端直接指定。核心拒绝越出数据根的产物路径和 symlink 上传对象；服务端维护原始文件名仅作展示。
- JSON 对象拒绝重复键；schema_version、revision、金额须为整数，bool 不算整数；拒绝 NaN/Infinity。未知业务字段拒绝，避免拼写错误被静默忽略。
- 所有数据写入采用同目录临时文件 + flush/fsync + rename；发布步骤另外遵循快照契约。原始文件不可修改。

## 错误码

| 退出码 | code | 是否可自动重试 | 编排器处理 |
|---|---|---|---|
| 2 | INVALID_JSON / INVALID_SCHEMA / UNKNOWN_COMMAND / UNSUPPORTED_VERSION | 否 | 拒绝请求，诊断契约 |
| 2 | INVALID_POLICY / INVALID_PATH / INPUT_TOO_LARGE / FILE_LIMIT / PARSE_LIMIT | 否 | 指出安全字段路径或超限原因 |
| 2 | UNSUPPORTED_FILE | 否 | 文件留在不支持清单，批次继续 |
| 3 | FACT_UNCONFIRMED / FIELD_CONFLICT / EVIDENCE_MISSING / LINK_CONFLICT | 否 | 待判断，不执行打包 |
| 3 | DUPLICATE_INVOICE / WRONG_BUYER / FULL_REFUND / HISTORY_ALREADY_PAID | 否 | 该票拒收或仅更换凭证 |
| 3 | HISTORY_UNKNOWN / PARTIAL_REFUND / UNSUPPORTED_LINK | 否 | 待判断，说明基础版限制 |
| 3 | STALE_REVISION / SNAPSHOT_MISMATCH / AUTHORIZATION_REQUIRED | 否 | 拒绝变更，重新展示并确认 |
| 3 | OUTPUT_BUSY | 是 | 执行等待；不覆盖已打开文件 |
| 3 | VERIFY_FAILED | 否 | 出报告，最多三轮，再转人工 |
| 4 | IO_ERROR / INTERNAL_ERROR | 否 | 等人工；明确重试后再执行 |
| 无正常退出 | CORE_TIMEOUT / CORE_OUTPUT_LIMIT / CORE_CRASH | 否 | 编排器生成错误；等人工 |

门禁的逐票业务结果放在成功响应的 items 里；上述业务失败退出码用于整个命令不能继续的情形，不因一张拒收票终止整批。

## 基础值与来源

| 类型 | 约束 |
|---|---|
| AmountCents | 非负有符号 64 位整数；累计使用 checked 运算；超过上限拒绝。退款另存 refund_cents，不编码为负发票 |
| InvoiceNumber | 恰好 20 位 ASCII 数字字符串，保留前导零 |
| Sha256 | 64 位小写十六进制 |
| LocalDate | 严格 ISO YYYY-MM-DD，必须是合法日历日 |
| Instant | UTC RFC3339，业务日换算使用 Asia/Shanghai |
| BatchId / ItemId / EvidenceId / FactId / DecisionId | 各自不互换的 opaque id；服务端生成，HTTP 解析时验证对应对象所属批次 |
| RevisionNumber | 非负单调整数；状态事件序号另外递增，不混用 |
| MatrixUserId | Matrix SDK 的 OwnedUserId；身份只来自已认证 Matrix sender 或已配对的服务端会话 |

Rust 内部对身份、revision、hash 用 newtype；状态、决策、事实级别、错误用枚举。HTTP/JSON/数据库边界先解析，再进入领域；禁止用 serde_json::Value 搬运已进入领域的业务记录。Python 使用冻结 dataclass/受控构造，验证范围与 Rust 一致。

`Provenance = {file_sha256,method:text|parser|vision|user,locator}`。

locator 是带 type 的对象：pdf_page（从 1 开始，带 field/可选 bbox）、sheet_cell（sheet、cell）、image_region（坐标、field）、json_pointer（pointer）、user_confirmation（event_id、actor、confirmed_at、original_fact_id）。不能用一句自然语言取代可定位原件。

`Fact<T>` 是三选一：

- extracted：id、typed value、source、validation_results。
- candidate：id、typed value、source、validation_results；不能被直接用于入账事实。
- confirmed：id、typed value、source、原 candidate id、确认人、确认事件与 UTC 时间；原 candidate 保留，修正不覆盖。

相同图片的大写/小写金额一致只能证明自洽，不能使 candidate 自动成为 extracted。

## 最小业务记录

| 类型 | 必填字段与不变量 |
|---|---|
| SourceFile | id、sha256、byte_length、detected_type、original_name、storage_object_id；内容类型来自文件签名，storage_object_id 由服务器映射路径 |
| Invoice | id、source_file_id、invoice_no、issue_date、amount_cents、amount_upper、buyer_name、buyer_tax_id、seller_name、project、remark；上述读数都是 Fact；order_ref 可空；住宿 service_period（check_in/check_out/nights）可空，缺失成为待判断理由 |
| Evidence | id、source_file_id、kind、merchant、amount_cents、service_date/payment_date（按 kind 至少其一）、transaction_ref、payment_status、provenance；截图字段为 Fact，确认前不能占用 |
| Link | item_id、payment_evidence_id 可空、supporting_evidence_ids、service_date、resolution:automatic|confirmed|manual_exception、decision_id 可空；automatic 必须有唯一非巧合支付；全批支付与行程单行占用唯一 |
| Item | id、invoice_id、facts、classification、link、review_reasons、disposition；业务字段与 derived（row_no、formatted_amount、output_filename）分开。disposition 为 rejected / needs_decision / accepted / receipt_only |
| Decision | id、item_id、kind、payload、actor、at、expected_revision、source_event_id；kind 为 confirm_visual / choose_evidence / explain_over_limit / replace_unpaid_invoice / receipt_only / reject / manual_evidence；每种 payload 单独定义，无自由 action 字符串 |
| Revision | batch_id、number、parent_revision 可空、applicant、period、files、facts、links、decisions、policy_hash、history_hash、snapshot_hash 可空；已确认快照冻结；新 revision 保留父指针 |
| Manifest | schema_version、batch_id、revision、snapshot_hash、policy_hash、history_hash、files[{relative_name,sha256,bytes}]、rows[{item_id,business_hash,attachment_hash}]、total_cents、category_totals；只允许相对文件名，无 .. 或分隔目录逃逸 |
| MissingSpend | id、payment_evidence_id、likelihood、status、decision_history、deadline、reminder_slots、claimed_invoice_id 可空；状态是设计 §8.3 的封闭枚举 |
| HistoryEntry | invoice_no、order_ref、seller_name、service_date、amount_cents、batch_id、revision、current_status、events[]、replaces_invoice_no 可空；events 是 submitted/approved/paid/voided 的追加事件，含 actor/at/note，ever_paid 由 events 推导 |

历史导入不能只提供 current_status=voided 就证明未付款；缺支付事件完整性证明的导入标 unknown，交人工。样例 history 包含明确 paid=false 的可信虚构初始快照和完整事件。新提交以费用身份加唯一占用，付款与作废不删除历史；同订单替换在事务内转移占用。

基础版一票一支付：F06 截图确认后可成为一条确认支付证据，原有个人转账仍是 coincidence。F10 从 voided 未付款历史恢复费用与支付来源，须经替换决定，不凭票号不同创建一笔新费用。

## 各命令 input / result

| command | input | result / 写入边界 |
|---|---|---|
| ingest | source_path、original_name | source_file、duplicate；内容哈希目录追加文件，不覆盖 |
| extract | source_file_id、可选 vision_candidate（仅字段数据） | invoice、issues；返回事实，不执行 Agent；candidate 保留级别 |
| gates | invoices、history_snapshot | items 的抬头/票号结果与重开票候选；退款在 link 后判定 |
| evidence | source_file_id、可选 confirmed_visual_facts | evidence[]、issues；截图未确认时只返回候选 |
| link | items、evidence、history_snapshot、decisions、period | links、occupancy、review_reasons、refund_results；一次全批决策，冲突双方都不自动占用 |
| missing | evidence、occupancy、policy、ignored_transactions | candidates、not_included、ignored_merchants；只有规则分级，无自动确认公务 |
| history | action:validate_import|project、entries 或 events | validated_snapshot、history_hash、issues；核心返回投影，编排器事务提交历史 |
| package | confirmed_snapshot、expected_snapshot_hash | staging_manifest；只写 staging，不发送、审批或更新业务状态 |
| verify | snapshot_hash、manifest_object_id、history_snapshot | checks[六项]、passed、issues；重读 XLSX 和附件，不信缓存，不自动发布 |
| claim | missing_spends、invoice、evidence、history_snapshot、decisions | match|needs_decision|rejected、reason、next_batch_item；P4c 才实现 |

核心返回的 command data 不能直接作为授权；编排器按当前 revision、actor 和快照再核验。policy 加载使用安全 YAML，固定类别/金额/时区/命名字段校验；policy_hash 取经校验规范化 JSON，不取 YAML 排版。

## 快照、产物与审计

规范化 JSON：对象键排序，UTF-8，紧凑分隔，不转义中文，无浮点，数组按已规定的语义顺序；id 集合排序，按序事件不得重排。快照加入 batch_id，避免跨批复用。快照包含文件哈希、事实级别/来源、关联、决定、政策/历史摘要与 revision。

SQLite 负责领域状态，Python 不直接写 SQLite。一次事务写 batch event、当前投影、审计、outbox；event_id/Matrix event_id 唯一防重复。审计含 seq、ts、batch、revision、from、to、event、actor、payload_sha256。

发布步骤：

1. 独占该批次执行租约，读取冻结快照；package 写 staging/<snapshot>。
2. verify 读取实际单元格与附件；全部通过后 fsync manifest 和文件。
3. 同文件系统 rename 至 published/<revision>；目标已存在时核 manifest/哈希，一致才复用，不一致报错。
4. 原子更新 CURRENT；SQLite 记录已发布版本和待发送 outbox。磁盘成功、事务未完成时重启用 manifest 补账。
5. outbox 以持久化 txn_id 发送，成功后标记。重试不能生成新 txn_id。

所有“已提交”“终审通过”等文案由程序根据状态渲染。报销包路径中可以有中文展示文件名，但 locator、相对路径和 manifest 必须校验。Excel 金额以精确到分的十进制单元格值独立核验；公式只允许受控序号/合计模板，合计值不得只依赖公式缓存。

## 事件表

共同前置：UI 变更要求会话权限、CSRF、Origin/Host、expected_revision；Matrix 变更要求受信服务器 sender 和 event_id。每批串行事务；任何 guard 失败时不发生状态变更或 outbox。

| 来源状态 | event / actor | guard | 目标状态与效果 | 幂等键 |
|---|---|---|---|---|
| 待命/收件 | upload / applicant | 自己的批次、文件在限额内 | 收件；追加 SourceFile，rev+1 | batch+file hash |
| 收件 | start_reconciliation / applicant | 至少一份支持文件、rev 匹配 | 读票；封存收件，rev+1，排 extract | event id |
| 封存后、未执行 | reopen_collection / applicant | 明确确认、未提交 | 收件；保留旧版，rev+1 | event id |
| 读票 | extraction_finished / system | 结果绑定当前输入 revision | 门禁；存事实、issues | job id |
| 门禁 | gates_finished / system | 事实所属当前 revision | 对账；拒收项留报告 | job id |
| 对账 | linking_finished / system | 全批占用检查完成 | 出报告；排 A4，事实先用模板 | job id |
| 出报告 | report_ready / system | 结果引用当前事实 | 有未决→待判断，否则待确认 | job id |
| 待判断 | evidence_requested / applicant| 缺证据项有效 | 补证据；排提示 | event id |
| 补证据 | evidence_confirmed / applicant | 原图关键字段已确认 | 对账；rev+1，相关项重算、全批复核占用 | event id |
| 待判断 | decide_item / applicant | 决定 kind 在该项允许集 | 未决则待判断，否则待确认；rev+1 | event id |
| 待确认 | confirm / applicant | 无未决、事实可信、rev 匹配 | 执行；存不可变快照与 job | batch+snapshot hash |
| 待确认 | decline / applicant | rev 匹配 | 待命；保留输入与决定 | event id |
| 执行 | package_ready / system | 当前执行 job | 终审；排独立 verify | job id |
| 执行 | output_busy / system | 无发布成功 | 执行等待；排有限退避重试 | job id+attempt |
| 执行等待 | retry / system或applicant | 未取消、锁释放、同快照 | 执行 | 原 job id |
| 终审 | verify_failed / system | 同 job | 出报告；三轮后等人工，禁止发布 | job id+attempt |
| 终审 | verify_passed / system | 六项全绿、manifest 匹配 | 待分享；发布、回帖 | snapshot hash |
| 待分享 | submit_finance / applicant | 指定已发布 revision | 已交财务；登记提交和历史 | batch+revision+submit |
| 已交财务 | approve / finance | policy allowlist、审批版本一致 | 已通过；追加历史 approved | batch+revision+approve |
| 已交财务 | return_items / finance | 非空有效 item 集合 | 待判断；新 revision，仅这些字段解冻 | batch+revision+return |
| 已通过 | finalize / system | 有/无漏票 | 跟进漏票/结束 | batch+revision+finalize |
| 已通过/结束 | mark_paid / finance | 历史费用已 approved | 批次状态不变，追加 paid | expense+payment ref |
| 任意适用历史 | void_history / finance | 带原因、paid 不被清除 | 追加 voided；保留 ever_paid | event id |
| 处理中的状态 | core_failed / system | job id 与 revision 匹配 | 等人工，记 resume_state | job id+attempt |
| 等人工 | retry / applicant | 同输入仍有效、明确选择 | resume_state；新 attempt | event id |
| 任意 | agent_unavailable / system | 当前请求 | 状态不变，rules_mode 标记 | request id |
| 任意 | stale_result / system | request/revision 已过期 | 丢弃结果，记录审计，不回写 | request id |

“待分享”是 §8.1「终审全绿→已交财务（由申请人分享后）」的中间持久化状态：不能把文件发布误报为财务已收到。财务看见当前卡片时仍须审批具体已提交 revision；单纯打开 URL 不自动改变提交状态。此细化随 P0 提交外环 review，P1 前冻结。

## 验收映射与阶段约束

- P1：真实 CLI 子进程→解析/门禁/暂存/终审；手写 expected；历史已付款反例；被篡改 XLSX/附件必须失败。
- P2：真实解析与全批占用；同支付两票均待判断；Agent 回放走同校验器；事实引用和值、数字语义角色与状态声明检查。
- P3：两个身份、第三个无权限身份、跨批次访问、码复用、尝试限额、CSRF/Origin、过期 revision；用户确认的事实才可执行。
- P4：退回仅解冻指定业务字段；崩溃注入在发送成功后、产物生成后、确认与上传交错点；恢复后最终可见副作用与无崩溃一致。
- P5：本机 Palpo + 回放 Agent，一条命令走真实编排路径。SDK、数据库、文件系统用真实组件；模型与时钟才是可替换边界。
