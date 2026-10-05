# p4delta 开发指南

面向要改 p4delta 代码、跑 e2e 沙箱或发版的人。安装与使用见 [README](README.md)。

## 开发与验证

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run --all-targets --all-features
```

CI 以 `-D warnings` 为硬门禁，clippy 有任何警告都算失败。测试跑 cargo-nextest 而不是
`cargo test`（理由、安装、`.config/nextest.toml` 的三条配置见
[docs/dev/testing.md](docs/dev/testing.md)）。

## 文档

开发文档按任务拆在 `docs/dev/` 下；「什么任务读哪篇」的路由与模块地图见
[CLAUDE.md](CLAUDE.md)。

- [docs/dev/testing.md](docs/dev/testing.md) — nextest、单测与 PowerShell 脚本测试
- [docs/dev/e2e.md](docs/dev/e2e.md) — 真实 p4d 沙箱：机制、调试、覆盖与栈边界
- [docs/dev/benchmark.md](docs/dev/benchmark.md) — 预览性能测量、dry run 日志比对、真实工作区验证
- [docs/dev/environment.md](docs/dev/environment.md) — 本地与 CI 的环境差异
- [docs/dev/release.md](docs/dev/release.md) — 发布检查清单与 P4V 验收
- [docs/dev/style.md](docs/dev/style.md) — 注释与输出文案

性能原理见 [docs/why-faster.md](docs/why-faster.md)，`--json` 输出契约见
[docs/json-contract.md](docs/json-contract.md)。
