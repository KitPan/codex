# RDOSCli · Phase 0 — 监督链路验证（零代码）

> 一句话目标：不写一行定制代码，用官方 `codex mcp-server` 打通
> **Claude（Supervisor）→ MCP → Codex → 本地模型** 的最小监督闭环，
> 实测三个模型家族的表现与现有接口的局限，产出 Phase 1 bridge 的需求清单。

---

## 0. 决策上下文（新 session 从这里恢复记忆）

**RDOSCli 是什么**：基于 openai/codex（Rust）fork 的本地模型 CLI agent。核心定制需求：
可被 frontier agent（Claude Desktop / Claude Code）通过 control tunnel **监督执行**——
分配任务、监控进度、审批把关、中途打断、多轮纠偏。

2026-08-02 讨论敲定的技术路线：

| 层 | 选型 | 理由 |
|---|---|---|
| 底座 | openai/codex fork（Rust, Apache-2.0） | 开源可深改；SQ/EQ 消息化内核天然可被监督；Kit 喜欢 Rust |
| 南向协议 | Phase 1 用 `codex exec --json`（JSONL 事件流）；Phase 2 换 `codex app-server`（官方 JSON-RPC 协议） | app-server 的审批是协议内置的暂停点，是天然 supervision 钩子 |
| 北向协议 | MCP（官方 Rust SDK `rmcp`，或复用 codex 自带 mcp-server 骨架） | Claude Desktop 唯一扩展面；Claude Code 同样吃 MCP，一份 bridge 两边通用 |
| 推理层 | vLLM 优先（同时出 OpenAI + Anthropic 双协议）；小模型可走 Ollama / LM Studio / mlx-lm | |
| 本地模型 | DeepSeek-V4-Flash、Qwen3.5（3.6 有已知坑）、Gemma 4 | 各模型注意事项见 §4 |
| Fork 策略 | 薄 fork：定制以新增 crate 形式存在（未来的 `codex-supervisor`），不散改内核；钉 release tag，定期 rebase | 协议漂移由编译器兜住 |

**Phase 0 特意不 fork、不写代码**——先用现成接口跑通链路，让局限自己暴露出来，
Phase 1 的 bridge 需求清单要来自实测而不是想象。

**2026-08-02 命名决策（Kit 确认，已生效）**：产品命令名 **`rdos-cli`**，配置家目录 **`~/.rdos`**。

- Phase 0 零代码实现：`/opt/homebrew/bin/rdos-cli` 软链 → codex 二进制（安全性依据：
  codex 仅对 argv[0] 为 `codex-execve-wrapper` / `codex-linux-sandbox` / `apply_patch` /
  `applypatch` 特殊分发，其余名字一律走正常 CLI）；`~/.rdos` 软链 → repo 内 `codex-home/`
  （config.toml 保持入库，两个路径等价可混用）。
- 已知边界：`CODEX_HOME` 显式设置时要求目录已存在、会被 canonicalize（日志里显示 repo
  真实路径）；env 值不做 `~` 展开（JSON 配置里写绝对路径）；`--help` 文案仍显示 codex
  （clap 编译期写死，纯外观）。**Phase 0 每次调用仍须显式带 `CODEX_HOME`**——编译期默认
  仍是 `~/.codex`。
- Phase 1 fork 时固化：`cli` crate 的 `[[bin]]` 名改 `rdos-cli`；`utils/home-dir` crate 的
  默认值改 `~/.rdos`（叶子 crate，改动极小），此后漏带环境变量也不会再摸到 `~/.codex`。

Phase 分期总览：

- **Phase 0（本文件）**：官方 `codex mcp-server` 直连验证，零代码。
- **Phase 1**：bridge v1（workspace 新 crate）：spawn `codex exec --json` 子进程 + 任务注册表 +
  MCP 北向（`spawn_task` / `status` / `tail` / `reply` / `interrupt`），解决异步与并行。
- **Phase 2**：南向换 app-server 协议：审批路由给 supervisor、中途 steering、细粒度事件。
- **Phase 3（可选）**：任务事件镜像到 EIMP 总线，其他 agent（GPT/Gemini）可观察或接管监督。

---

## 1. 与本机既有 Codex 数据的隔离（重要）

本机 `~/.codex/` 已存在且含真实数据（`auth.json` 登录凭证、cache、AGENTS.md 等）。
2026-08-02 探测：**codex 二进制目前不在 PATH**（homebrew / npm 全局 / cargo bin 均无），
需要重新安装——但 `~/.codex` 数据必须保持原样不动。

隔离约定（全程遵守）：

- 一切 RDOSCli 相关的 codex 调用都带 `CODEX_HOME=/Users/kit/Workspace/RDOSCli/codex-home`，
  配置、会话、日志全部落在 repo 内，**绝不读写 `~/.codex`**。
- 建议在 testbed 跑第一个任务后验证一次：`ls -lt ~/.codex` 确认无新增文件。
- Phase 0 用官方发行版二进制；fork 自编译的二进制到 Phase 1 才出现。

---

## 2. 仓库布局（Phase 0 结束时应有）

```
RDOSCli/
├── Phase0.md              ← 本文件（计划 + 决策上下文）
├── CLAUDE.md              ← 新 session 上下文入口
├── README.md
├── codex-home/            ← 隔离的 CODEX_HOME（config.toml 入库；sessions/log 已 gitignore；~/.rdos 软链指向这里）
│   └── config.toml
├── scripts/               ← 测试基建（dspark_proxy.py：DSpark temp0 注入代理）
├── testbed/               ← 测试项目：小型 Rust crate（见 §5）
└── results/
    └── phase0-results.md  ← 测试记录表 + 局限清单 + Phase 1 需求清单
```

---

## 3. 任务清单（按序执行，完成一项勾一项）

- [x] **P0-1 环境盘点**：✅ codex CLI 已装（`codex-cli 0.144.1`，brew），`rdos-cli` 与
      `~/.rdos` 软链已建并验证；✅ 端点已盘点入 §7（Qwen3.6 @ vLLM box、Gemma 4 @ 本机
      omlx；DeepSeek-V4-Flash 2026-08-03 部署 @ 192.168.3.3，经 DSpark temp0 代理接入）。
- [x] **P0-2 写 `codex-home/config.toml`**：✅ 已按 0.144 新机制写就——base config
      （3 个 provider，wire_api=responses）+ 4 个 profile 文件（qwen36 / qwen36-deep /
      gemma4 / deepseek-flash）；rdos-dsflash 经 DSpark temp0 注入代理（2026-08-03）。
- [x] **P0-3 直连冒烟（先绕过 MCP）**：逐个 profile 跑
      `CODEX_HOME=/Users/kit/.rdos rdos-cli exec --profile <p> "列出当前目录文件并统计行数"`，
      验证推理链路和工具调用格式。**先排除推理层问题，再上 MCP**——分层调试，
      否则问题会在三层（harness / 协议 / parser）之间来回猜。
      （✅ 2026-08-02：qwen36、gemma4 均 exit 0、工具调用正常；期间揪出并解决 profile /
      wire_api 两个 0.144 接口变化，见 §8。deepseek 待部署。）
- [x] **P0-4 搭 testbed**：按 §5 规格创建 Rust crate，确认 `cargo test` 呈现预期的 4 过 2 败。
      （✅ 2026-08-02 实测：4 passed / 2 failed / 1 ignored，失败项与预埋 bug 一致。）
- [x] **P0-5 MCP 接线**：按 §6 给 Claude Desktop（和/或 Claude Code）挂上 codex mcp-server，
      重启后确认 `codex` / `codex-reply` 两个工具可见。
      （✅ 2026-08-02：Claude Code 侧工具在 session 内可见并实测可用（T1 已跑通）；
      Desktop 侧 Kit 已配 json，disclaimer 包装的 server 进程在跑，聊天侧可见性待验证。
      ⚠️ 期间遭遇南向瞬时故障窗口并完成定位——见 §8「南向瞬时故障窗口」条目。）
- [x] **P0-6 监督闭环测试**：跑 §5 任务矩阵（T1–T6 × 3 profile），逐项填 `results/phase0-results.md`。
      （T1 ✅ ×4 serve、T2 ✅ ×4 serve、T3 ✅（qwen 修深 bug + deepseek 实现 todo!()，
      include-ignored 7 绿）、T4 ✅（qwen 会话续接改迭代器风格，零回归）、T5 ✅
      （escalation 合格、客户端不呈现、与权限模式无关）、T6 ✅（60s 上限四重钉死；
      Desktop 聊天侧可选补测）。加测 ✅ 控制隧道实验（读侧全通、写侧盲注、一致性
      缺失——见 results 专节）。）
- [x] **P0-7 收尾**：把实测局限写成《Phase 1 bridge 需求清单》，附在 results 文末。
      （✅ 2026-08-03：17 条需求、六大类，每条附实测出处——见 results 文末。）

---

## 4. config.toml 规划

原模板已被 2026-08-02 实测推翻——codex 0.144 有两个接口变化（§8 有对应条目）：

1. **`[profiles.*]` 表已废弃**：`--profile <p>` 改读 `CODEX_HOME/<p>.config.toml`
   独立文件，叠加在基础 `config.toml` 之上；旧表语法直接报错拒启。
2. **`wire_api = "chat"` 已移除**（openai/codex#7782）：只剩 `"responses"`，
   本地引擎必须支持 OpenAI Responses API（vLLM 0.26.0 与 omlx 实测均支持）。

生效配置见 [codex-home/config.toml](codex-home/config.toml)（base：2 个 provider +
缺省 gemma 跑腿）与三个 profile 文件 `qwen36 / qwen36-deep / gemma4 .config.toml`。
自定义 provider id 不能用保留名 `openai` / `ollama` / `lmstudio`，统一 `rdos-` 前缀。

各模型注意事项（2026-08 调研结论）：

- **DeepSeek-V4-Flash**：✅ 2026-08-03 部署 @ `http://192.168.3.3:8000/v1`（引擎标识
  ds4.c，非 vLLM——原 vLLM 起服参数指引不再适用；serve deepseek-v4-flash / -pro，
  ctx 100k）。Kit 开了 **DSpark**：请求必须显式 `temperature=0`（置信度阈值默认 0.7）。
  代理逐请求实证 **codex 的 Responses 请求从不携带 temperature**，故经
  `scripts/dspark_proxy.py`（127.0.0.1:18300）注入中转。temp0 下长生成明显变慢：
  MCP 60s 窗口装不下完整版 T1（限长 200 字版可过），T2 46s 过。实测确是三家最强——
  T1 限长版唯一双杀两个 bug 的模型，难任务主力实至名归。
- **Qwen3.5 / 3.6**：原预判「3.6 在 vLLM 上 tool parser 与 chat template 不匹配、返回空
  tool_calls」针对的是 chat completions 链路。实际部署为 **Qwen3.6-35B Heretic**（Kit
  有意选择），而 codex 0.144 只走 Responses 协议——P0-3 冒烟工具调用正常、坑未复现
  （协议路径不同，不经过 chat template 那条 parser 线）。矩阵继续用 3.6 实测。
  **2026-08-03 换权重**：NVIDIA 官方 NVFP4 版 `nvidia/Qwen3.6-35B-A3B-NVFP4` @
  spark1.local:8000。实测 T1 双杀两 bug（39s）、T2 达标（54s）——NVFP4 4bit 没有
  复现 MLX int4 的工具精度崩坏（注意系跨家族对比）；thinking 默认开使耗时上浮，
  60s MCP 窗口余量变薄。
- **Gemma 4**：工具调用最"随和"（社区实测 Codex CLI / Claude Code / OpenCode 三条路全通），
  E4B 小变体即可稳定驱动 agent 循环，适合当跑腿模型和快速冒烟用。
  （实际部署：本机 omlx :9999 的 gemma-4-31B-it，4bit/8bit 两档；P0-3 冒烟 4bit 通过，
  仅 metadata fallback 警告。**2026-08-03 T2 补充：4bit 在写任务上不可用**——apply_patch
  死循环 328 次、hunk 行号凭空生成、36 分钟无产出被终止；**8bit 秒过**（弃 apply_patch
  改用 sed，一发命中）。gemma4 profile 缺省已切 8bit；「Gemma 工具调用最随和」的社区
  结论在 codex 0.144 的补丁格式下仅对 ≥8bit 成立，量化档位是工具可靠性的一等变量。）
  （2026-08-03 深夜补测 spark2 的 AWQ-4bit 版 `cyankiwi/gemma-4-26B-A4B-it-AWQ-4bit`：
  T1 机理双杀但通过/失败映射错（42s）；T2 先**假完成**（只输出假 diff 不动手，21s），
  reply 纠偏后**破坏性盲改**（未锚定全局 sed 连元组模式一起替换、编译打挂），两轮均
  虚报成功——AWQ-4bit 同样不可写任务。gemma 系 4bit 两种量化格式写任务 0/2 存活；
  「基线测试防乱改」设计当场兜住破坏。）
  （同夜续：换 **Google 官方 `google/gemma-4-26B-A4B-it`**（bf16，同一 vLLM 0.21）——
  T1 机理双杀但通过/失败映射仍错（**可达性翻车是 26B-A4B 家族特质，与量化无关**）；
  T2 39s 干净单点修复 5/1/1，同引擎对照下 AWQ 的假完成/盲改消失，**量化归因坐实**。
  官方版成为 gemma 系首个既可写又装得进 60s MCP 窗口的 serve，`gemma4-spark` 转正。）

---

## 5. testbed 与任务矩阵

**设计原则：任务结果必须机器可判**（`cargo test` 退出码），三个模型横向对比才客观，
未来 Phase 1/2 的回归测试也直接复用这套任务。

testbed 规格（小型 Rust crate，`cargo new testbed --lib`）：

- `src/lib.rs`：一个简单的纯函数模块（建议：字符串区间解析器，如 `"1-3,7,10-12" → Vec<usize>`，
  逻辑足够浅显但有边界条件可埋 bug）。
- 测试共 7 个：
  - 4 个通过（基线，防止 agent 乱改把好的改坏）；
  - 1 个**浅 bug** 导致失败（边界条件，如 off-by-one）——单步修复难度；
  - 1 个**深 bug** 导致失败（逻辑分支错误）——需要读懂意图才能修；
  - 1 个 `#[ignore]` 测试对应一个 `todo!()` 未实现函数——考察从零实现。

任务矩阵（每任务 × 3 profile，都通过 supervisor 从 MCP 侧发起）：

| ID | 任务 | sandbox / approval | 验证方式 | 考察点 |
|----|------|--------------------|----------|--------|
| T1 | 总结 testbed 代码结构 | read-only | 人工判 | 基础链路、指令跟随 |
| T2 | 修浅 bug 使测试通过 | workspace-write | `cargo test` | 工具调用可靠性（核心指标） |
| T3 | 修深 bug + 实现 todo!() | workspace-write | `cargo test -- --include-ignored` | 多步规划能力 |
| T4 | 在 T2 会话上用 codex-reply 追加纠偏（如"改用迭代器风格重写"） | workspace-write | 人工判 + cargo test | 会话续接、多轮 |
| T5 | 布置一个需审批的操作（如访问网络下载依赖） | approval=on-request | 观察 | 审批请求在 MCP 侧如何呈现（elicitation 客户端支持度） |
| T6 | 故意的长任务（预期 >2 分钟） | - | 观察 | Claude Desktop 对长 MCP 调用的超时上限——Phase 1 异步模型的实证依据 |

记录表模板（`results/phase0-results.md`，每行一次运行）：

```
| 任务 | profile | 完成? | 工具调用错误次数 | 轮数 | 耗时 | 备注（失败模式描述） |
```

---

## 6. Supervisor 接线

**Claude Desktop**（`claude_desktop_config.json`）：

```json
{
  "mcpServers": {
    "rdos-codex": {
      "command": "/opt/homebrew/bin/rdos-cli",
      "args": ["mcp-server"],
      "env": {
        "CODEX_HOME": "/Users/kit/.rdos"
      }
    }
  }
}
```

**Claude Code**（备选/并行验证，一条命令）：

```bash
claude mcp add rdos-codex --env CODEX_HOME=/Users/kit/.rdos -- rdos-cli mcp-server
```

给 supervisor 的开场话术（示例，可直接粘给 Claude Desktop）：

> 你通过 rdos-codex 这个 MCP server 监督一个跑本地模型的 codex agent。
> 用 `codex` 工具发起任务、`codex-reply` 续接。现在：让它修复
> /Users/kit/Workspace/RDOSCli/testbed 中失败的测试，模型分派用
> config {"model": "qwen36-fast", "model_provider": "rdos-vllm"}（MCP 侧无 profile 参数）、
> cwd 指向 testbed、sandbox=workspace-write。完成后让它跑 cargo test 自验，你复核结果并汇报。

---

## 7. 环境记录（P0-1 时填写）

| 项 | 值 |
|---|---|
| codex CLI 版本 | `codex-cli 0.144.1`（2026-08-02 实测；`rdos-cli --version` 输出相同） |
| 安装方式 | Homebrew `brew install codex`；软链 `/opt/homebrew/bin/rdos-cli` → codex、`~/.rdos` → repo 内 codex-home |
| DeepSeek-V4-Flash 端点 | `http://studio.local:8000/v1`（=192.168.3.3，2026-08-03 部署；引擎标识 ds4.c；serve deepseek-v4-flash / -pro，ctx 100k；**DSpark 开启**：请求须显式 temperature=0、置信度阈值默认 0.7 → 经 `scripts/dspark_proxy.py` @ 127.0.0.1:18300 注入接入） |
| Qwen3.6 端点 | `http://spark1.local:8000/v1`（=192.168.3.1，DGX Spark；2026-08-03 换 NVIDIA 官方 NVFP4：`nvidia/Qwen3.6-35B-A3B-NVFP4`，thinking 默认开，ctx 262144，vLLM 0.26.1rc1.dev30）。旧 :8964 serve（Heretic 权重 + fast/deep 别名，vLLM 0.26.0+aeon）已下线 |
| Gemma 4 端点 | `http://127.0.0.1:9999/v1`，omlx（本机 MLX 多模型服务），gemma-4-31b-it-4bit / MLX-8bit（8bit 为 gemma4 profile 缺省）；另 spark2.local:8000（=192.168.3.2，vLLM 0.21.1rc1）现 serve **Google 官方 `google/gemma-4-26B-A4B-it`**（profile `gemma4-spark`，T1/T2 全过且进 60s MCP 窗口，gemma 写任务首选；此前的 cyankiwi AWQ-4bit 假完成+盲改，已换下） |
| Claude Desktop 版本 | 待填 |

（2026-08-02 21:35 曾探测本机常用端口均无服务；随后 Kit 给出两端点如上——omlx 用的
是非常规端口 :9999。DeepSeek 待部署。）

---

## 8. 预判的坑（遇到先对号，别从零排查）

- **Qwen3.6 空 tool_calls**：parser/chat template 不匹配的已知问题 → Phase 0 用 3.5 绕开。
- **deepseek_v4 parser 需较新 vLLM**：起服旗标见 §4；旧版 vLLM 没有这个 parser。
- **长任务超时**：`codex` MCP 工具调用是阻塞式的，Claude Desktop 对长调用有超时——
  T6 专门测上限。这不是要修的 bug，是 Phase 1 异步任务模型的立项依据。
  （终判：**Claude Code 侧上限 60s 整**，与权限模式（bypass/always-ask 对照）、任务
  类型（审批挂起/纯长命令）全部无关；超时调用拿不到 threadId、reply 报 Session not
  found，且超时引发的重连会让全部会话连坐失效。Desktop 聊天侧上限留作可选补测。）
- **codex 运行时改写 config.toml（实测新增）**：项目信任状态 `[projects.*] trust_level`
  会被持久化写进 CODEX_HOME/config.toml（插入位置不避让注释）——版本化配置混入
  运行时状态，git diff 出现此类噪音属预期。
- **审批呈现**：codex mcp-server 的审批走 MCP elicitation，Claude Desktop 支持度未知——
  T5 专测；如果不支持，Phase 0 期间用 `approval_policy=never` + `sandbox=workspace-write`
  的组合兜底（沙箱仍拦住越界操作），把审批路由留给 Phase 2。
  （T5 实测 2026-08-03：模型侧能正确发出 `require_escalated` + justification，但
  Claude Code 客户端零 elicitation 痕迹，请求挂 43.5s 无人应答 → 60s 连坐取消。
  **不支持证实**（且与权限模式无关——always-ask 对照实验「①调用须批准②弹窗仍无」
  封档），兜底方案转正；Desktop 聊天侧支持度留作可选补测。附带发现：沙箱禁网是
  静默丢包，不带超时的网络命令会吊死整个窗口。）
- **中途进度不可见**：mcp-server 模式的已知设计局限，确认现象即可不必深究——
  正是 Phase 1 要解决的核心问题。
- **`[profiles.*]` 表已废弃（0.144 实测新增）**：`--profile p` 改读
  `CODEX_HOME/p.config.toml` 独立文件叠加；旧表语法直接报错拒启。
- **`wire_api = "chat"` 已移除（0.144 实测新增，openai/codex#7782）**：南向只剩
  Responses 协议，引擎必须暴露 `/v1/responses`（vLLM 0.26.0、omlx 均实测支持）。
  Phase 1 bridge 若要兼容 chat-only 引擎（旧版 Ollama 等），要么自带适配层，
  要么钉旧版 codex——fork 基线选 tag 时必须考虑这一点。
- **自定义模型 metadata fallback 警告（0.144 实测新增）**："Model metadata for
  `<m>` not found. Defaulting to fallback metadata"——不影响运行，后续可补模型
  元数据消除，顺带校准 context window。
- **首次 `/v1/responses` 可能撞预热（实测新增）**：一次 HTTP 000（20s 超时）后
  重试即通；脚本给足超时或先 ping 一发。
- **MCP 侧没有 profile（0.144 实测新增）**：`codex` 工具无顶层 profile 参数，
  `config.profile` 按 legacy 拒绝——supervisor 分派模型要显式传
  `config {"model", "model_provider"}`；`config` 支持嵌套定义完整 provider，可临时路由。
- **南向瞬时故障窗口（实测新增，当晚三次）**：症状为 SYN 静默丢弃 → 客户端挂 24s
  超时，窗口外一切正常（vLLM 预热/负载嫌疑）。codex 0.144 无重试、一击即溃——
  这是 Phase 1 bridge 必须内置「自动重试 + preflight 自检」的实证。曾误判为 macOS
  本地网络权限（对照实验恰好踩在窗口内外形成假相关；Kit 确认权限全开 + app 进程树
  内 curl 5ms 通 + 直连复测通，三证推翻）。排障用的 localhost TCP 中继方案已验证
  可行，留作真正网络隔离场景的备选。

---

## 9. 成功标准（Phase 0 完成的定义）

- [x] Claude Desktop 发起任务 → 本地模型执行 → 结果返回，全链路走通；
      （经 Desktop app 内的 Claude Code 实证 ×N；Desktop 聊天端 server 进程在跑，
      任务级验证可选补。）
- [x] 三个 profile 各至少完成一次 T2（修浅 bug 且 cargo test 通过）；
      （✅ 2026-08-03 集齐：qwen36 MCP 26s、deepseek-flash MCP 46s、gemma4 CLI-8bit
      数秒——gemma 经 MCP 不可行（60s 上限），走 CLI 完成；判定均为 5/1/1 达标。）
- [x] `results/phase0-results.md` 填完 T1–T6 记录；
- [x] 局限清单成文，转写为《Phase 1 bridge 需求清单》。

**Phase 0 于 2026-08-03 达成全部成功标准。**

---

## 10. 多实例 / 多隧道拓扑（2026-08-02 确认）

**结论：支持，且是天然形态。** Codex 无守护进程、无单例锁，每次调用都是独立 OS 进程；
`mcp-server` / `app-server` 均走 **stdio**（不占端口），多实例之间零冲突。
同一 CODEX_HOME 下的并发会话各写各的 UUID 会话文件，互不干扰。

三种拓扑，按阶段选用：

| 拓扑 | 做法 | 适用 |
|---|---|---|
| **A. 一隧道多模型** | 单个 `codex mcp-server`；supervisor 每次 `codex` 工具调用用 `profile` 参数选模型 | Phase 0 默认，最简单 |
| **B. N 隧道 N 模型** | Claude Desktop 注册 N 个 MCP 条目（`rdos-codex-deepseek` / `-qwen` / `-gemma`），各自独立进程，需要强隔离时可各配独立 CODEX_HOME | 独立故障域 / 按模型分权限时 |
| **C. bridge 多路复用** | Phase 1 起由 bridge 管理 per-task `codex exec --json` 子进程——**每个任务就是一个实例**，北向一条隧道多路复用全部实例 | 目标形态，A/B 的问题在此消解 |

**真正的硬约束不在 codex，在文件系统**：社区共识是多实例同时写同一个 checkout 必丢工作，
标准解法是 git worktree 按实例隔离。因此 **Phase 1 的 `spawn_task` 必须把
worktree 分配做成内置步骤**（任务开始建 worktree，结束后合并/丢弃）。
vLLM 侧无需担心并发：continuous batching 下多实例同时请求反而提升 GPU 利用率。

社区实测的另一条经验恰好是 RDOSCli 的立项依据：并行 codex 实例"起得来，管不住"——
多个 agent 同时改文件、要审批、在不同时刻结束，跟踪它们正是大多数 swarm 方案垮掉的地方。
这个"管不住"就是 supervisor bridge 要补的缺。

**2026-08-03 深夜设计注记（sock 拓扑讨论，Phase 2/3 输入）**：

- stdio → Unix socket 的本质是把会话从「进程属性」升格为「有主权的常驻服务」：
  多客户端约会点、late join、断线重连——恰好补上控制隧道实验量出的三缺
  （所有权 / 锁 / 通知）。TUI 降级为客户端之一后，supervisor 注入自动对人类可见。
- supervisor 走 sock = 绕开 MCP 工具层 = **60s 天花板消失**。Claude 的持续监督形态 =
  连接器进程持锁 + 事件 journal + 唤醒机制（回合制在场，秒~几十秒延迟）；亚秒级
  硬实时用分层监督：策略下发给连接器执行，例外唤醒 supervisor 裁决。
- 多 agent 接入（GPT 等）四纪律：角色三档（观察/顾问/执行）且**执行租约唯一**、
  事件 author 归因（UDS peer cred 只认 OS 用户，身份须应用层握手——隧道实验里
  qwen 已把 supervisor 注入误记在 Kit 头上）、server 串行化回合写入、
  EIMP 降为发现/通知面 + sock 做数据面。
- 三方共享终端两条路：正式版 = app-server 挂 sock + TUI 客户端化（Phase 2）；
  零代码原型 = tmux（send-keys 注入 + capture-pane 读屏），列 Phase 2 预研第一项。

---

## 参考链接

- Codex App Server 官方文档：https://developers.openai.com/codex/app-server
- OpenAI 博客 Unlocking the Codex harness：https://openai.com/index/unlocking-the-codex-harness/
- codex-rs/app-server 源码：https://github.com/openai/codex/tree/main/codex-rs/app-server
- 非交互模式（codex exec）：https://developers.openai.com/codex/noninteractive
- Codex 高级配置（model_providers/profiles）：https://developers.openai.com/codex/config-advanced
- vLLM 的 Claude Code 集成（Anthropic 端点）：https://docs.vllm.ai/en/stable/serving/integrations/claude_code/
- vLLM DeepSeek-V4-Flash recipe：https://recipes.vllm.ai/deepseek-ai/DeepSeek-V4-Flash
- Qwen3.6 工具调用问题讨论：https://huggingface.co/Qwen/Qwen3.6-27B/discussions/13
- 官方 Rust MCP SDK（rmcp）：https://github.com/modelcontextprotocol/rust-sdk
- 多实例并行编排模式：https://codex.danielvaughan.com/2026/04/18/running-multiple-codex-agents-parallel-orchestration/
