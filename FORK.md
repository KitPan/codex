# FORK.md — codex fork 档案（P1-1，2026-08-03）

> fork 的一切结构性事实与 rebase 操作手册。改动了 fork 的形态（换 tag、
> 加 remote、新增上游触碰面）就更新本文件。

## 基线

| 项 | 值 |
|---|---|
| 上游 | `https://github.com/openai/codex.git`（remote 名 **`upstream`**；`origin` 留给未来的 KitPan/codex GitHub fork，暂未创建） |
| 钉定 tag | **`rust-v0.144.1`**（commit `44918ea10c`）——与 brew 安装的 `codex-cli 0.144.1` 严格同版，Phase 0 全部实测结论直接适用 |
| 本地位置 | `RDOSCli/codex/`（**嵌套独立 git 仓库**，父仓 .gitignore 排除；本文件是父仓对 fork 状态的文字指针） |
| 工作分支 | `rdos-main`（基于 tag，全部定制在此） |
| toolchain | rust-toolchain.toml 钉 `1.95.0`（rustup 自动管理） |
| 构建 | 只用 cargo（workspace `codex-rs/`）；上游并存的 Bazel 构建不维护，新增 crate 无 BUILD.bazel |

选 0.144.1 而非更新版（0.144.5 已存在）的理由：**实测同版性 > 新鲜度**。60s 窗口、
profile 独立文件、Responses-only、config.toml 运行时改写等全部 Phase 0 结论都在
此版本上取证；升级经由下述 rebase 流程有意识地做。

## 定制清单（`git log --oneline rust-v0.144.1..rdos-main`）

| commit | 内容 | 上游触碰面 |
|---|---|---|
| `32b9b32a7b` | cli crate 并列新增 `[[bin]] rdos-cli`（同 main.rs 双命名编译；`codex` bin 保留，上游 24 处按名解析二进制的测试不受伤） | `cli/Cargo.toml` +6 行 |
| `2152d9907a` | 默认 CODEX_HOME `~/.codex` → `~/.rdos`（叶子 crate `utils/home-dir`；env var 名与语义不变，单测同步改） | `utils/home-dir/src/lib.rs` ±7 行 |
| `b7e3fd4df6` | 新增 `codex-supervisor` crate 骨架（lib + `rdos-supervisor` bin + 9 个模块 stub，doc 注释锚定需求编号）；workspace members 尾部追加 | `Cargo.toml` +2 行、`Cargo.lock`（+supervisor 条目；另含版本号补章，见下） |

原则（Phase0 §0 既定）：**薄 fork——定制 = 新增 crate，不散改内核**。上游文件
触碰面永远保持个位数行，每笔定制独立 commit，rebase 冲突可逐笔裁决。

上表只登记**触碰上游文件**的 commit；supervisor crate 内部的日常开发
（P1-2 数据模型、P1-3 执行器等）不逐笔入表，看 `git log -- codex-rs/supervisor`。
supervisor 对内核的依赖走 crate 边界（如 `codex-exec` 的 `ThreadEvent` 类型），
rebase 后协议漂移由编译器报警。

已知偏差备忘：

- fork 内 `codex` bin 的默认 home 也变成 `~/.rdos`——对 Kit 的真 `~/.codex` 是保护
  （漏带 env var 也摸不到）；上游测试若有依赖默认路径的，跑 `cargo test` 全量时留意。
- Cargo.lock 首次 cargo 调用会把 workspace crate 版本 `0.0.0 → 0.144.1` 补章
  （上游发版只 stamp Cargo.toml 不 stamp lock）——属 tag 状态的确定性结果，
  外部依赖 rev 零变动，已并入 supervisor commit。

## rebase 手册（升级到新 tag 时照做）

```bash
cd /Users/kit/Workspace/RDOSCli/codex
git fetch upstream --tags
git checkout rdos-main
git rebase rust-vX.Y.Z          # 预期冲突面：workspace Cargo.toml members 尾部、Cargo.lock
# Cargo.lock 冲突直接取上游版，随后 cargo build 自动补齐 supervisor 条目
cd codex-rs && cargo build && cargo test -p codex-utils-home-dir && cargo clippy -p codex-supervisor
```

- 升级动机审查：新 tag 的 CHANGELOG 须过目**协议面变化**（wire_api、profile 机制、
  MCP 工具形状、exec JSONL schema）——协议漂移由编译器兜住是薄 fork 的立身前提，
  supervisor 对内核的一切依赖走 crate 边界、不走字符串约定。
- 每次 rebase 后在本文件追加一行记录：日期、新 tag、冲突面、回归结果。
- 旧 `rdos-main` 在 rebase 前打备份 tag：`git tag rdos-pre-rebase-$(date +%Y%m%d)`。

## rebase 日志

| 日期 | 动作 | 冲突面 | 回归 |
|---|---|---|---|
| 2026-08-03 | 初建：`rust-v0.144.1` + 3 定制 commit | — | cargo build 全绿；home-dir 4/4；clippy 干净 |

## 待办

- [ ] Kit 决定后在 GitHub 建 `KitPan/codex` fork，`git remote add origin …` 并推
      `rdos-main`（本地历史获得异地备份；届时可评估父仓改用 submodule 指针）。
