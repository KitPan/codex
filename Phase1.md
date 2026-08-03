# RDOSCli · Phase 1 — bridge v1（异步监督桥）

> 一句话目标：fork codex 钉 tag，新增 `codex-supervisor` crate，把监督链路从
> 「60s 阻塞盲盒」升级为「异步任务注册表 + MCP 五件套」
> （`spawn_task` / `status` / `tail` / `reply` / `interrupt`）。

---

## 0. 新 session 上下文恢复

- **Phase 0 已通关**（2026-08-03，initial commit `b768816`）：全部实测记录在
  [results/phase0-results.md](results/phase0-results.md)，文末《Phase 1 bridge 需求
  清单》**17 条、六大类、每条附实测出处**——本文件所有任务由它导出，动工前通读。
- 三台本地助理（已入 supervisor 长期记忆；离线时 ping 分诊后提醒 Kit 开机）：
  qwen-NVFP4 @ `spark1.local:8000`、gemma 官方 bf16 @ `spark2.local:8000`、
  deepseek+DSpark @ `studio.local:8000`（**须先跑 `python3 scripts/dspark_proxy.py`**，
  temp=0 注入代理 @ 127.0.0.1:18300）。
- 隔离约定不变：一切调用显式 `CODEX_HOME=/Users/kit/.rdos`（= repo 内 `codex-home/`），
  绝不读写 `~/.codex`；命令名 `rdos-cli`。
- 探测端点用**裸 IP**（本机 curl 的 .local mDNS 会间歇挂死；codex 的 reqwest 不受影响）。

## 1. 技术决策（Phase 0 实证捆绑）

| 决策 | 内容 | 实证依据 |
|---|---|---|
| fork 基线 | openai/codex 钉 release tag（0.144.x 起步） | chat wire 已移除（引擎须 Responses）、profile 表已废弃、config.toml 被运行时改写——bridge 配置**独立文件**，不与 codex config 混放 |
| fork 策略 | 薄 fork：定制 = workspace 新增 `codex-supervisor` crate，不散改内核，定期 rebase | Phase0 §0 既定 |
| 南向 v1 | per-task spawn `codex exec --json` 子进程（JSONL 事件流） | 60s 上限四重实测；exec 无超时限制已验证 |
| 架构约束 | **会话建模为常驻服务对象，传输可插拔**——不把 exec/stdio 假设焊进数据结构 | Phase 2 换 app-server/Unix sock 时不动骨头（Phase0 §10 设计注记） |
| 北向 | MCP（rmcp 或复用 codex mcp-server 骨架），五件套 | Claude Desktop/Code 唯一扩展面 |
| 任务隔离 | spawn_task 内置 git worktree 分配/回收 | §10 文件系统硬约束 |

## 2. 需求 → 模块映射（编号对应 results 文末清单）

- **任务注册表 + 会话管理**：#1 异步模型、#2 会话自持与持久化（session↔rollout）、
  #3 会话状态完整定义（历史+模型亲和+沙箱+cwd+锁）、#4 所有权与锁
- **执行器**：#7 重试+preflight、#9 命令硬化（pipefail+强制超时）、#17 上游适配
- **验收器**：#5 机器判卷、#6 失败熔断、#8 diff-first 写任务
- **模型登记与路由**：#10 量化/引擎/推理档一等属性、#11 三维分派+escalation 模板、
  #12 显式 model+provider 路由（嵌套 provider override 直接继承）
- **观测**：#15 status/tail 摘要化（1% 透传率红线）、#16 环境自检
- **中间层**：#14 dspark_proxy 收编为 provider 中间件
- **Phase 2 挂账**：#13 审批路由（v1 先 approval=never + sandbox 兜底）

## 3. 任务清单（按序执行，完成勾一项）

- [ ] **P1-1 fork 与工作区**：fork openai/codex、钉 tag、workspace 添加
      `codex-supervisor` crate 骨架，`cargo build` 全绿；记录 tag 与 rebase 策略。
- [ ] **P1-2 数据模型**：Task / Session / ModelRegistry 三张表（含持久化），
      会话状态按需求 #3 全字段建模。
- [ ] **P1-3 执行器**：spawn `codex exec --json`、JSONL 事件解析、任务生命周期
      状态机（含 interrupt 杀进程 + worktree 回收）。
- [ ] **P1-4 MCP 北向五件套**：spawn_task（含 worktree 分配）/ status / tail（摘要，
      非原始流）/ reply / interrupt。
- [ ] **P1-5 可靠性层**：judge 钩子（cargo test 等判卷命令随任务注册）、同签名失败
      熔断、南向重试+preflight、diff-first 落盘开关。
- [ ] **P1-6 模型登记与分派**：三台助理入登记表（量化/引擎/延迟/推理档），
      spawn_task 按登记表路由；escalation 规范注入任务模板。
- [ ] **P1-7 中间层收编**：dspark_proxy 逻辑内化为 provider 中间件（参数注入 +
      请求体审计日志）。
- [ ] **P1-8 回归验收**：用 Phase 0 testbed 跑 T1–T6 全矩阵回归（基线预期见
      results 首节：4/2/1），三模型全过。
- [ ] **P1-9 收尾**：`results/phase1-results.md` + 文档更新 + Phase 2 立项清单。

## 4. 验收标准（Phase 1 完成的定义）

- [ ] supervisor 经 MCP：`spawn_task` 立即返回句柄；`tail` 可见进度摘要；完成有
      通知；判卷结果自动附带——全程**无一次 60s 阻塞超时**。
- [ ] 两个任务在两个 worktree 并行执行互不串扰。
- [ ] 会话跨 bridge 进程重启可恢复（对照 Phase 0 的 threadId 连坐全灭）。
- [ ] T4 类续接经 `reply` 完成；`interrupt` 可中止执行中任务且状态干净。
- [ ] testbed T1–T6 三模型回归全过，结果机器可判。

## 5. Phase 2/3 预研停车场（不阻塞 v1）

- app-server + Unix sock 多客户端：TUI 客户端化、60s 天花板消失、三方共享终端
  （详见 Phase0.md §10 设计注记）。
- tmux 三方实验（零代码 UX 原型：send-keys 注入 + capture-pane 读屏）；
  `mcp__terminal__read_terminal` 工具排查。
- 多监督者纪律：角色三档 + 执行租约唯一、事件 author 归因、回合串行化；
  EIMP = 发现/通知面，sock = 数据面（GPT 首期只读观察员）。
- 审批路由完全体（需求 #13）：approval 事件 → supervisor 裁决 → 回写。
