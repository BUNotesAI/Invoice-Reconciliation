# Invoice-Reconciliation · 报销对账 Agent

GOSIM Agentic App 黑客松 2026 参赛作品。

月底把发票、微信支付账单和滴滴行程单发进 Rinx 聊天：Agent 读票、过门禁、按证据对齐实际消费日期，把拿不准的事交给人判断；确认后生成报销包、自己核验、交给财务，并继续跟进还没到的发票。

原则：模型写理解和方案，不写事实。所有金额、日期、票号都来自确定性代码，并经过校验。

## 状态

P0 环境探路已实现，等待外环审查；业务流程尚未实现。

- [冻结设计](docs/design.md)
- [运行与复现](docs/run.md)
- [核心契约草案](docs/spec/README.md)
- [当前限制](docs/limitations.md)

## 许可证

[Apache License 2.0](LICENSE)
