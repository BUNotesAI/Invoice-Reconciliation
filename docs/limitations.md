# 当前限制

- 当前仅 P0 探路。尚无发票核心、配对权限、revision、outbox、终审和财务流程；不能用于真实数据或多人服务。
- 固定本机端口与虚构账号；所有服务只绑定 loopback，测试房间未加密。
- Rinx 基线实际二进制叫 robrix，隔离变量 ROBRIX_DATA_DIR。构建成功不代表界面成功，原生截图验收单独记录。
- P0 bot 回声每次生成新的 Matrix 事务号；崩溃恢复与幂等属于后续切片，不宣称已实现。
- octos 图像输出可能带 Markdown 围栏，真实冒烟已观察到；严格 JSON 校验与一次修复不可省略。P0 图片字段只验证固定虚构图，不证明通用票面准确率。
- octos_smoke 是验证脚本，生产 AgentPort 在 P2 实现；P0 的传输选择为 stdio，未实测 REST。
- prepare_rinx 使用通过 HTTP 登录的真实设备 session 初始化原生客户端，以避开在自动化日志中输入密码；不证明客户端手工登录表单路径。
- 环境初始化中途失败不会删除已有项目资源；修复后重跑，不自动清理。凭据只在本机私有数据目录，丢失后不得盲目重建同名账号。

- 钉定 Rinx 的窗口标题与欢迎页实际为 Robrix。mini_app 卡片作者显示 Username not available；富文本粗体与列表能渲染。卡片进入原生容器，但 Makepad 抓图的网页区域为空白，内嵌 WebKit 内容未验证；点击 Open in browser 已验证能打开正确 URL 并显示页面。
