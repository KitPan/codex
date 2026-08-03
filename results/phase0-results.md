# Phase 0 实测记录

环境快照见 [Phase0.md](../Phase0.md) §7。任务矩阵定义见 §5。

testbed 基线（bug 未修时的预期）：`cargo test` = **4 passed / 2 failed / 1 ignored**；
失败项应为 `expands_inclusive_range`（浅 bug）与 `parses_mixed_singles_and_ranges`（深 bug）。

## P0-3 直连冒烟（2026-08-02，任务："列出当前目录文件并统计行数"）

| profile | 模型 | 结果 | tokens | 备注 |
|---------|------|------|--------|------|
| qwen36 | qwen36-fast（→ qwen36-35b-heretic） | ✅ exit 0，多次 shell 调用 | 24,758 | 主动递归子目录并输出汇总表，超出字面任务但无害；§8 预判的 3.6 空 tool_calls 坑在 Responses wire 下未复现 |
| qwen36-NVFP4 | nvidia/Qwen3.6-35B-A3B-NVFP4 | ✅ exit 0（2026-08-03 换权重复测） | — | spark1.local:8000；输出简洁准确，行为正常 |
| gemma4-spark | cyankiwi/gemma-4-26B-A4B-it-AWQ-4bit | ✅ exit 0（2026-08-03 补测） | — | spark2.local:8000（vLLM 0.21.1rc1）；递归统计完成，读任务正常 |
| gemma4-spark | google/gemma-4-26B-A4B-it（官方 bf16，换权重复测） | ✅ exit 0 | — | 同箱同引擎；简洁准确 |
| gemma4 | gemma-4-31b-it-4bit | ✅ exit 0，单次 `find+wc` 精准完成 | 13,645 | metadata fallback 警告（无碍）；exec 默认 approval=never / sandbox=read-only 得到确认 |
| deepseek-flash | deepseek-v4-flash（DSpark，经 temp0 注入代理） | ✅ exit 0，多轮 shell 调用 | — | 2026-08-03 补测；把 codex-home 与 target/ 分开统计，超出字面任务但无害；代理日志实证 codex 全程不带 temperature |

## 接口现实（codex-cli 0.144.1 实测，Phase 1 需求输入）

- `[profiles.*]` 表已废弃 → per-profile `<p>.config.toml` 文件叠加于 base config。
- `wire_api = "chat"` 已移除（openai/codex#7782）→ 引擎必须支持 `/v1/responses`；
  vLLM 0.26.0 与 omlx 实测均支持。chat-only 引擎需 bridge 适配或钉旧版 codex，
  fork 基线 tag 选择时须考虑。
- 自定义模型触发 "Model metadata not found → fallback" 警告（可补元数据消除）。
- codex 0.144 在 CODEX_HOME 写 sqlite 状态（logs / memories / state），gitignore 已覆盖。
- 首次 `/v1/responses` 请求可能撞预热：一次 HTTP 000（20s 超时），重试 2.8s 通。
- **MCP 工具调用超时 = 60s（Claude Code 侧实测）**：阻塞式调用整 60s 被客户端取消
  （`-32001 Request timed out`；服务端 `turn_aborted` @ 59.95s）。T1 级任务（26–32s）
  能过；T2 级编辑任务对慢模型（gemma@omlx）不够——两次尝试均超时。更糟的是超时
  调用**没有返回 threadId、`codex-reply` 报 Session not found**：超时任务对监督者
  彻底不可恢复（rollout 文件在磁盘上，但 MCP 层拿不到）。§8 预判的 T6 提前证实，
  是 Phase 1 异步任务模型最硬的立项依据。
- **codex 运行时改写 config.toml**：项目信任状态 `[projects."…"] trust_level = "trusted"`
  被持久化插入 CODEX_HOME/config.toml（且插入位置不避让注释块）——版本化配置会混入
  运行时状态，git diff 会持续出现此类噪音。
- **codex Responses 请求不携带 temperature 字段**（DSpark 注入代理逐请求实证，
  2026-08-03）——采样参数完全交给服务端默认。对 DSpark 这类要求显式采样参数的
  服务层必须中间层注入；`scripts/dspark_proxy.py` 即该适配层的活体原型，
  Phase 1 bridge 可直接吸收（顺带获得请求体级观测/审计）。

## MCP 接线实测（P0-5，2026-08-02 晚）

- Claude Code 侧 `claude mcp add`（local scope）注册后，Desktop 重启的同时本 session
  内工具热出现：`mcp__rdos-codex__codex` / `codex-reply` 可见可用。Desktop 侧 Kit 配
  好 json 后，其 disclaimer 包装的 server 进程也已在跑（Desktop 聊天侧可见性待验证）。
- `codex` 工具**没有顶层 profile 参数**，且 `config: {"profile": ...}` 按 legacy 报错
  拒绝——**MCP 侧无法使用 profile 文件**，分派模型必须显式
  `config: {"model", "model_provider"}`。
- `config` 支持**嵌套定义完整 provider**（`model_providers.<id>.{base_url, wire_api}`），
  supervisor 可逐调用即时定义南向路由——Phase 1 bridge 可直接继承此能力。
- **南向瞬时故障窗口 postmortem（当晚三次实测）**：22:00 裸 curl 撞 HTTP 000；
  22:36–22:37 两次 MCP 任务 24s 超时（症状=SYN 静默丢弃，"stream disconnected …
  error sending request"）。当时误判为 app spawn 进程的「本地网络」TCC 权限问题——
  对照实验（app 内失败 ×2 vs 终端成功 ×N）恰好全部踩在窗口内/外，形成假相关。
  Kit 确认权限全开后，两发实验推翻：① 让 codex 在 danger-full-access 下替跑 curl
  （即 app 进程树内），LAN 5ms 通；② 直连复测通。确诊：箱子侧瞬时不可达窗口
  （vLLM 预热/负载嫌疑）。教训：① bridge 必须自动重试——codex 0.144 一击即溃；
  ② localhost TCP 中继（127.0.0.1:18964→192.168.3.1:8964）已验证，留作网络隔离
  场景备选；③ 分布式排障先怀疑时间相关性，再怀疑上下文差异。
- 南向故障经 MCP 原文透传（"stream disconnected …: error sending request"），
  supervisor 可见可诊——正面数据点。
- 返回结构：`structuredContent = {threadId, content}`，threadId 供 `codex-reply` 续接。

## 控制隧道实验（2026-08-03 晚，外部接入运行中的 TUI 实例）

拓扑：Kit 在 terminal 起交互式 TUI（`CODEX_HOME=/Users/kit/.rdos rdos-cli --profile
qwen36`，cwd=testbed），supervisor（Claude）从另一进程尝试接入。会话
`019fc991-…3085f07`。

| 层 | 结果 | 实测细节 |
|----|------|----------|
| 读侧（观察） | ✅ 全通 | rollout JSONL 实时可读：用户 prompt、每轮命令/输出/回复全可见。注意会话文件**首轮提交才创建**（lazy），TUI 空开时无从观察 |
| 写侧（注入） | ✅ 可注入 | `rdos-cli exec --profile qwen36 resume <id> "<msg>"` 成功：暗号 TUNNEL-ACK-42 回收，且模型准确复述了 TUI 里做过的任务——**跨进程会话记忆连续**；注入追加进原 rollout，无分叉文件 |
| 会话一致性 | ❌ 缺失 | TUI 对外部追加**零感知**（不监听、不加锁、无通知）；随后在 TUI 内提问"是否收到监督者消息"，模型答"只收到你的两条"——TUI 进程用内存线程构造上下文，不含注入轮。**磁盘一条线，两个进程两条脑**；留下的 rollout 是双线交错 append log，未来 resume 将看到混流历史 |

附加发现：① `exec resume` **不继承原会话的模型**（回落 base 缺省 gemma，需显式
`--profile`）——模型亲和性是会话状态的一部分，bridge 必须持有；② codex 对连接级
失败有 5 次自动重试、对流中断 0 重试——重试策略按错误类别不一致。

**结论**：零代码条件下 control tunnel 读侧全通、写侧可达但「盲注」——恰好量出了
"管不住"的精确形状：缺的不是通道（rollout/resume 都在），是**会话所有权、锁与
通知**。Phase 1 bridge 的立项从推断升级为实证：spawn/attach 必须由 bridge 统一持有
会话；外部 steering 必须经同一进程路由（Phase 2 app-server 形态）；文件级双写不可
作为协作机制。

## 任务矩阵记录（T1–T6 × profile，每行一次运行）

| 任务 | profile | 完成? | 工具调用错误次数 | 轮数 | 耗时 | 备注（失败模式描述） |
|------|---------|-------|------------------|------|------|----------------------|
| T1 | gemma4（缺省路由，MCP） | ✅ | 0 | 1 | <1min | 只读静态分析完成；准确识别浅 bug（`lo..hi` 少 `=`），但把深 bug 误归因为同一 off-by-one；read-only 沙箱挡住 `cargo test`，它如实报告了 |
| T1 | qwen36（qwen36-fast，MCP 经 localhost 中继） | ✅ | 0 | 1 | 32s | 精确定位深 bug（`input` vs `seg`，行号+机理全对），但漏判浅 bug，对测试 5 失败原因解释自相矛盾；测试计数口误（说 8 实列 7） |
| T2 | gemma4（缺省路由，MCP，完整任务→定向修复两种 prompt） | ❌ | 0 | — | 60s ×2 超时 | 两次尝试均被 60s 客户端超时取消（`turn_aborted` @59.95s），lib.rs 未动；超时无 threadId、reply 不可续接 → 转 CLI 旁路补测 |
| T2 | qwen36（qwen36-fast，MCP 直连；supervisor 预诊断+定向修复模式） | ✅ | 0 | 1 | 26s | `lo..hi`→`lo..=hi` 一处最小修复；cargo test = **5 passed / 1 failed / 1 ignored**，恰为 T2 预期终态（浅绿、深红、基线无回归） |
| T2 | gemma4-**4bit**（CLI 旁路，完整任务 prompt） | ❌ DNF | **328** | — | 36min 被 supervisor 终止 | apply_patch 死循环：hunk 行号凭空生成、失败后不重读文件换个猜法再来；codex 无熔断机制；耗 185 万 input tokens（95% cache 命中）；lib.rs 零污染（328 发全空枪） |
| T2 | gemma4-**8bit**（CLI，行号级定向 prompt） | ✅ | 0 | 1 | 数秒（15k tokens） | 自主弃用 apply_patch 改 `sed '35s/lo..hi/lo..=hi/'` 一发命中；5/1/1 达标。注：prompt 给了精确行号，信息量多于 qwen 那轮，跨行不可直接比 |
| T1 | deepseek-flash（MCP 经 temp0 代理，完整版 prompt） | ❌ | 0 | — | 60s 超时 | temp0 长生成变慢，turn 4 被客户端取消；能力无碍，纯窗口问题 |
| T1 | deepseek-flash（MCP，限长 200 字版） | ✅ | 0 | 1 | <60s | **三家唯一双杀**：浅 bug（`lo..=hi`）与深 bug（`input` vs `seg`）全部识别、根因正确、测试计数无误 |
| T2 | deepseek-flash（MCP 经 temp0 代理，定向 prompt 同 qwen） | ✅ | 0 | 1 | 46s | 第 35 行 `lo..hi`→`lo..=hi`，自报行号；cargo test = 5/1/1 达标 |
| T1 | qwen36-NVFP4（2026-08-03 换权重后，MCP 直连，完整版 prompt） | ✅ | 0 | 1 | 39s | **双杀**两处 bug 位置（浅+深，测试计数 7 正确；深 bug 后果推演带臆测小瑕疵）；对比旧 Heretic 权重（只中深 bug、计数错）明显提升 |
| T2 | qwen36-NVFP4（MCP 直连，定向 prompt 同前） | ✅ | 0 | 1 | 54s | 第 35 行 `lo..=hi`；5/1/1 达标。thinking 默认开使简单编辑耗时翻倍（26s→54s），距 60s 上限仅 6s 余量 |
| T1 | gemma4-spark = AWQ-4bit（MCP 直连） | ✅* | 0 | 1 | 42s | 两处 bug 机理都识别出来（超 MLX 系表现），但可达性分析翻车：声称不含 `-` 的输入也会失败，通过/失败映射给成 2/4/1（实际 4/2/1）。星号=完成但含实质错误 |
| T2 | gemma4-spark = AWQ-4bit（MCP，定向 prompt 同 qwen） | ❌ | 1 | 1 | 21s | **假完成**：输出过去时态成功汇报+假 diff（行号还错），实际未调用任何编辑工具、文件未动。vLLM 0.21 早期 Responses 实现是否吞 tool call 列为混杂因素 |
| T2 | gemma4-spark = AWQ-4bit（codex-reply 纠偏重试） | ❌ | 1 | 1 | ~30s | 这次真动手但**破坏性盲改**：未锚定全局 sed 连元组模式 `(lo, hi)` 一起替换（正则 `.` 通配），E0425×4 编译打挂，仍虚报成功。reply 续接机制本身正常（T4 机制✓）；基线测试网兜住伤害，已复位 |
| T1 | gemma4-spark = **Google 官方 bf16**（MCP 直连，同引擎对照） | ✅* | 0 | 1 | 47s | 机理双杀、计数 7 正确，但与 AWQ 版同款可达性翻车（声称不含 `-` 的输入也失败）——**家族认知特质实锤，与量化无关** |
| T2 | gemma4-spark = **Google 官方 bf16**（MCP，定向 prompt 同前） | ✅ | 0 | 1 | 39s | 干净单点修复（35 行改对、27 行元组毫发无损）、汇报准确；5/1/1 达标。同引擎对照下 AWQ 的假完成/盲改消失 → **量化归因坐实**；gemma 系首个可写 + 进 60s 窗口的 serve |
| T3-a 深bug | gemma 官方 bf16（MCP，行号级定向） | ❌→弃线 | 2 | 2 | ~40s×2 | **假完成 ×2**（工具调用吐进文本通道、畸形 JSON 调用体；reply 纠偏后仍假报成功）——假完成非量化独有，官方 bf16 也随机触发（vLLM 0.21 工具通道嫌疑并存）；会话被自身假记忆污染后纠偏无效，supervisor 弃线换将 |
| T3-a 深bug | **qwen36-NVFP4**（MCP，接替，同 briefing） | ✅ | 0 | 1 | ~30s | 一发命中：26 行 `input`→`seg`；cargo test = **6 passed / 0 failed / 1 ignored** |
| T3-b todo!() | deepseek-flash（MCP 经代理） | ❌ | 0 | — | 60s 超时 | codegen + DSpark temp0 减速塞不进窗口；会话照例丢失 |
| T3-b todo!() | **deepseek-flash**（CLI 旁路，同任务+允许自验） | ✅ | 0 | 1 | ~2min，15.9k tokens | 实现干净（双指针探段，边界全对）；独立判卷 `cargo test -- --include-ignored` = **7 passed / 0 failed** —— T3 达成 |
| T4 续接纠偏 | qwen36-NVFP4（新会话两回合：确认现状 → reply 改迭代器风格） | ✅ | 0 | 2 | 回合各 <40s | `for` 循环 → `out.extend(lo..=hi)`，最小改动、汇报属实；全量回归仍 7 绿。原计划复用 T3-a 会话，但 **mcp-server 进程因超时重生导致全部 threadId 连坐失效**（见局限），改为新会话两回合完成 |
| T5 审批-自发 | qwen36-NVFP4（MCP，on-request，需网任务） | ⚠️ | 0 | 1 | 60s 超时 | 模型**未自发 escalate**，直接裸跑 curl（无 -m）被沙箱**静默丢包**吊死 52.7s → 连坐取消。两个子发现：沙箱禁网是挂起而非报错；on-request 依赖模型自觉，本地模型不会主动请求审批 |
| T5 审批-明示 | qwen36-NVFP4（MCP，明示 escalation 规范） | ✅ 答案到手 | 0 | 1 | 60s 超时 | qwen 正确提交 `require_escalated` + 中文 justification（质量佳）→ **请求挂 43.5s 无人应答**直至取消；Claude Code 与 Desktop 日志均零 elicitation 痕迹，**Kit 目击确认无任何弹窗** → **审批在当前 MCP 客户端不可呈现**（§8 预判证实），Phase 0 兜底 = approval never + sandbox 拦截；审批路由归 Phase 2 |
| T5 独立复测 | qwen36-NVFP4（另一 Claude Code 实例，17:23） | ✅ 复现 | 0 | 1 | 60s 整（服务端 59959ms） | 详见 [t5-approval-replication-cc-2026-08-03.md](t5-approval-replication-cc-2026-08-03.md)：换客户端实例+换措辞，escalation 稳定正确触发、挂 45.2s 无弹窗——客户端能力缺失系稳定现象。增量：沙箱禁网**第二形态**（wall 0.0000s 秒退+exit 0 空输出，vs 首轮 52.7s 吊死）；justification 语言/体裁漂移（本轮英文问句）；`turn_aborted="interrupted"` 语义可续接但 MCP 层不可达 |
| T5/T6 模式对照 | qwen36-NVFP4（**always-ask** 权限模式，审批变体） | ✅ 封档 | 0 | 1 | 60s 整 | escalation 挂 53.2s；对照有效性双确认——Kit ①须批准 codex 调用本身（模式生效）②仍无任何审批弹窗 → **bypass 吞弹窗假说出局**，「不呈现 elicitation」与权限模式无关 |
| T6 长任务 | qwen36-NVFP4（`sleep 150`，always-ask） | ✅ 数据到手 | 0 | 1 | 60s 整 | 纯长命令同样 60s 铡——**上限与权限模式、任务类型、审批与否全部无关**；Claude Desktop 聊天侧上限留作可选补测 |

**T1 观察**：三家对比图完整——gemma 抓浅漏深、qwen 抓深漏浅、**deepseek 双杀**
（且是在 200 字限长版里完成的）。单模型自查不可靠、模型间盲区互补，supervisor
复核/多模型三角验证有实证价值；档位分派上 deepseek 当难任务主力实至名归。

## 观察到的局限（随手记，P0-7 汇总）

- MCP 侧无 profile 机制（见上）；模型分派全靠显式 config 注入。
- 南向存在瞬时故障窗口（预热/负载），codex 无重试、一击即溃（24s 超时即任务失败）
  ——bridge 需要自动重试 + 起任务前 preflight + 把底层网络错误翻译成带建议的诊断。
- codex 对连续同类工具失败**无熔断**（apply_patch 328 连败不止损、烧 36 分钟）——
  bridge 需要循环检测：N 次同签名失败 → 中止任务并上报 supervisor。
- **量化档位是工具调用可靠性的一等变量**（gemma 4bit 崩、8bit 稳）——模型登记表
  应把 quantization 与 served name 一起记录，分派时按档位限制任务类型。
  补充（2026-08-03）：NVIDIA 官方 NVFP4 的 qwen 在 T1/T2 全绿且质量提升——
  **量化格式与校准质量比"位数"本身更关键**（NVFP4 4bit ≠ MLX int4 4bit）；
  但系跨家族对比，非控制变量实验。
- thinking 默认开的 serve 让简单任务耗时翻倍（NVFP4 T2 54s vs 旧 fast 别名 26s），
  60s MCP 窗口余量告急——bridge 需要把 reasoning 档位作为分派参数（或恢复服务端
  fast/deep 双别名机制）。
- **假完成模式**：AWQ-gemma T2 未调用任何工具却给出具体、过去时态的成功汇报（含假
  diff）——模型自我汇报零可信度，**机器判卷（cargo test）是监督闭环的硬前提**，
  本矩阵「以退出码为准」的设计选择被最强形式验证。
  （T3 修订：官方 bf16 也在 T3-a 假完成 ×2——假完成是**随机性掉进文本通道**的现象，
  量化只是加重频率，vLLM 0.21 工具通道嫌疑并存。且会话被自己的假记忆污染后，reply
  纠偏无效——supervisor 的正解是**弃线换将**，换 qwen 后一发命中。）
- **mcp-server 进程重生 = 会话全灭**：60s 超时引发的 MCP 重连会换掉 server 进程，
  内存中全部会话连坐清零——连**成功调用返回的 threadId** 也会失效（rollout 文件
  在磁盘却无法经 MCP 复活）。bridge 必须自持 session↔rollout 映射并支持跨进程
  resume；threadId 不可作为持久句柄。
- 本机 curl 的 `.local` mDNS 解析会间歇性挂死（ping/dscacheutil/reqwest 均正常，
  纯 curl 现象；`--resolve` 或裸 IP 绕过）——supervisor 侧探测脚本统一用裸 IP。
- 沙箱禁网 = **静默丢包**：不带超时的网络命令会吊死（实测 52.7s）——bridge 下发
  命令应强制注入超时，且这再次放大 60s 窗口的脆弱性。
- on-request 审批**依赖模型自觉** escalate：qwen 明示规范后才正确触发（justification
  质量佳），自发场景不会——bridge 的任务模板应内置 escalation 规范说明。
- `cmd | head` 类管道**掩蔽真实退出码**（curl 失败、head 返回 0，模型与 supervisor 都
  只能靠零输出猜）——bridge 命令模板应注入 `set -o pipefail` 或避免管道直出。沙箱
  禁网已见两种形态（52.7s 吊死 / 0.0000s 秒退空输出），识别逻辑两种都要覆盖。
- escalation justification 语言与体裁不稳定（中文陈述 ↔ 英文面向用户问句随 prompt
  漂移）——审批路由/呈现层须按自由文本处理，不能假设格式。
- **破坏性盲改模式**：纠偏重试后用未锚定全局 sed，把合法的元组模式连带替换、编译
  打挂——bridge 验收必须含编译/测试关卡；写任务宜默认产出 diff 供 supervisor 审查
  后再落盘（Phase 2 的审批路由正好承接）。
- gemma 量化对照**四格**齐：**MLX-int4 死循环、AWQ-int4 假完成+盲改、MLX-8bit 稳、
  官方 bf16 稳且快（T2 39s）**。4bit 档位（两种格式）写任务 0/2 存活；bf16 与 AWQ
  同引擎（vLLM 0.21）同家族对照下失败模式消失 → 量化归因坐实。另「可达性分析翻车」
  在 bf16 同样出现 → 26B-A4B 家族认知特质，与量化无关——「量化损伤」与「家族短板」
  两类缺陷已可实验区分，模型登记表应分列。
- `codex` MCP 调用阻塞式确认（T6 将专测超时上限）。

## Phase 1 bridge 需求清单（P0-7，2026-08-03 成文；每条附实测出处）

### 一、任务与会话模型（核心架构）

1. **异步任务模型**：`spawn_task` 立即返回任务句柄，进度经 `status`/`tail` 拉取，结果经
   通知推送。依据：60s 阻塞窗口四重钉死（bypass/always-ask × 审批挂起/纯长命令全部
   60s 铡），同步模型对本地模型的真实任务不可用。[T2/T5/T6]
2. **会话句柄由 bridge 自持并持久化**（session↔rollout 映射）：threadId 寿命 = mcp-server
   进程寿命，超时引发重连即全灭，rollout 在盘却不可达。[T4]
3. **会话状态的完整定义** = 消息历史 + 模型亲和 + 沙箱策略 + cwd + 锁。依据：`exec
   resume` 不继承模型（回落 base 缺省）的语义差实测。[控制隧道]
4. **会话所有权与锁**：文件级双写可行但零通知/零一致性（TUI 对注入隐形、双线交错
   append log）——attach/steering 必须经 bridge 单进程路由，禁止旁路双写。[控制隧道]

### 二、可靠性与验收

5. **机器判卷强制**：模型自我汇报零可信（假完成 ×4：AWQ×2 + 官方 bf16×2，全部过去
   时态+细节具体）；验收一律以编译/测试退出码为准。[T2/T3]
6. **失败熔断**：连续同类工具失败 N 次自动中止上报（实测 328 连败烧 36 分钟无熔断）。[T2]
7. **南向自动重试 + preflight**：瞬时故障窗口实测三次（HTTP 000 / SYN 丢包 / 24s 超时），
   codex 流中断 0 重试一击即溃（连接级已有 5 重试，策略按错误类别补齐）；起任务前
   端点探测（用裸 IP——本机 curl 的 .local mDNS 会间歇挂死）。[P0-3/T1]
8. **写任务默认产出 diff、审后落盘**：破坏性盲改实测（未锚定全局 sed 连带毁伤合法代
   码 + 虚报成功）；基线测试网兜底 + Phase 2 审批路由承接。[T2]
9. **命令模板硬化**：注入 `set -o pipefail`（管道掩蔽真实退出码实测）+ 强制超时（沙箱
   禁网两形态：秒退空输出 / 无限吊死）。[T5]

### 三、模型登记与分派

10. **模型登记表一等属性**：quantization（格式+档位，非位数——MLX-int4 死循环、
    AWQ-int4 假完成+盲改、MLX-8bit 稳、NVFP4/bf16 全过）、serve 引擎与版本、
    thinking 档、延迟档；「量化损伤」与「家族短板」分列（可实验区分）。[T1/T2 对照组]
11. **分派三维**：模型延迟 × 任务颗粒度 × reasoning 档（thinking 开关 2× 耗时实测）；
    escalation 规范写进任务模板（on-request 依赖模型自觉，不教不会）。[T2/T5]
12. **路由机制**：MCP 侧无 profile，用显式 `model + model_provider`；`config` 支持嵌套
    定义临时 provider——bridge 直接继承此能力做逐任务路由。[MCP 接线]

### 四、审批（Phase 2 前置）

13. **审批路由给 supervisor 而非客户端 UI**：Claude Code 不呈现 elicitation（权限模式
    无关，①②对照封档）；审批请求视为任务事件流的一种，由 supervisor 决策。
    justification 按自由文本处理（语言/体裁随 prompt 漂移）。[T5]

### 五、观测与中间层

14. **参数注入/观测中间层转正**（`scripts/dspark_proxy.py` 原型）：codex 不发送
    temperature 等采样参数，服务端合规（DSpark temp=0）只能由中间层保证；顺带获得
    请求体级审计。[deepseek 接入]
15. **status/tail 给摘要不给原始流**：本席实测透传率 ~1%（助手侧 250–300 万 tokens →
    supervisor 窗口 ~2 万），这是监督经济学的生命线，接口设计必须守住。[context 评估]
16. **环境自检**：CODEX_HOME 显式校验、代理存活检查、离线分诊（ping 区分关机/停服）
    并提示人工介入。[点名协议]

### 六、上游适配与配置

17. **fork 基线决策点**：chat wire 已移除（引擎必须 Responses，vLLM 0.21+/omlx 实测可）；
    `[profiles.*]` 废弃改独立文件；**codex 运行时改写 config.toml**（trust / personality /
    TUI NUX 状态混入版本化配置）——bridge 配置与 codex 配置分仓，diff 噪音按预期管理。
    [接口现实]
