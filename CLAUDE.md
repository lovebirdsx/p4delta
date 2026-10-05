# p4delta

Perforce 工作区工具（reconcile / clean / sync），Rust 实现，面向超大工作区做了性能优化。
用户文档在 [README](README.md)，开发入口在 [CONTRIBUTING](CONTRIBUTING.md)。

## 常用命令

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run --all-targets --all-features
```

## 模块地图

```
src/main.rs        bin 薄壳：解析参数、打印错误、返回退出码
src/lib.rs         pub fn run()：编排参数、缓存生命周期与单轮执行
src/cli.rs         命令行参数（clap derive）
src/charset.rs     p4 输出字符集的解析、缓存与解码
src/locate.rs      p4 可执行文件的定位（P4_EXE → PATH → P4V 安装目录）
src/model.rs       领域数据模型：depot 记录、工作区文件、摘要缓存
src/path.rs        本地路径规范化与路径键
src/scope.rs       操作范围：入口解析、`.p4delta-scope` 配置、交集与排除
src/cache.rs       摘要缓存的阶段间保存（全量序列化 + 临时文件改名）
src/digest.rs      p4 摘要计算与「自 sync 起未改动」判定
src/prune.rs       .p4ignore 分析、预扫描、目录剪枝决策
src/workspace.rs   工作区文件收集与忽略过滤
src/p4/            marshal（p4 -G 解析）、process（子进程与批次并发）、fstat（流式查询）
src/reconcile/     一个范围的编排：analyze（差异分析）、changes（报告与应用）、clean、sync
tests/             黑盒用例（e2e_*.rs 按功能分）；support/ 是 e2e 沙箱框架
scripts/           基准、安装、发布脚本及各自的黑盒测试
```

阅读从 `model` / `path` 入手，再看 `p4/*`、`workspace` / `cache` / `digest`，最后是 `reconcile/*`。

## 任务路由

| 要做什么 | 读 |
|---|---|
| 跑测试、加用例、改 PowerShell 脚本 | [docs/dev/testing.md](docs/dev/testing.md) |
| 起 e2e 沙箱、调试失败的 e2e | [docs/dev/e2e.md](docs/dev/e2e.md) |
| 量性能、比对 dry run 输出、真实工作区验证 | [docs/dev/benchmark.md](docs/dev/benchmark.md) |
| CI 红、本地与 CI 行为不一致 | [docs/dev/environment.md](docs/dev/environment.md) |
| 装进 P4V 验证、发版 | [docs/dev/release.md](docs/dev/release.md) |
| 改注释、动 `--help` 文案 | [docs/dev/style.md](docs/dev/style.md) |

## 约定

- **注释一律中文**，技术标识符保留原文；clap 字段上的 `///` 是 `--help` 正文，不是注释——
  改它等于改用户可见输出，要单独提交。详见 [docs/dev/style.md](docs/dev/style.md)。
- **开发阶段不做兼容**：本项目还没有外部用户，版本兼容、数据迁移、为废弃格式保留代码这类
  提议默认跳过——直接改掉旧行为，也别为迁移写代码或测试；只有明确要求才加兼容层。
- **提交信息不要包含 AI/工具署名水印**：如 `Co-Authored-By: Claude <noreply@anthropic.com>`。

## 维护本文件

本仓库**不使用 auto-memory**（`.claude/settings.json` 已关闭）。长期知识写进这里、
CONTRIBUTING.md 或 docs/dev/，不要依赖会话记忆。每条知识只有一个家：机制写进代码注释，
流程与实测记录写进 docs/dev/，这里只留路由与每次会话都要用的约定。

文档地图：[README.md](README.md)（用户文档）、[CONTRIBUTING.md](CONTRIBUTING.md)（开发入口）、
[docs/why-faster.md](docs/why-faster.md)（性能原理）、[docs/json-contract.md](docs/json-contract.md)
（`--json` 输出契约）。
