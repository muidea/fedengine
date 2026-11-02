# Git 提交边界

## 应当提交

| 路径 | 内容与提交原因 |
|---|---|
| `Cargo.toml`、`Cargo.lock`、`rust-toolchain.toml` | Workspace、锁定依赖及开发工具链；可执行应用保留根 Cargo.lock |
| `src/`、`crates/` | 进程入口、11 个组件库及各库所属测试 |
| `config/application.toml`、`examples/config/federation.toml` | 默认运行配置及独立联邦配置示例；密码通过环境变量 SecretRef 读取 |
| `.cargo/config.toml` | 启用 DuckDB 官方原生库下载的项目构建设置 |
| `tests/` | 系统级协议、联邦查询测试、临时目录清理辅助代码及公开 TLS 夹具 |
| `scripts/` 中的 `.py`、`.sh` | 扩展获取、冒烟和隔离数据库测试脚本 |
| `docker/`、`.github/workflows/` | 镜像配方、隔离测试环境和 CI |
| `.agents/skills/` | 5 项经 skill-hub 管理的项目快照；CI 直接调用其中的 preflight |
| `README.md`、`LICENSE`、`.gitignore` | 使用说明、许可证和本地产物排除规则 |
| `docs/` 中的设计、架构、开发、能力、协议及本文件 | 长期维护的项目说明；不包含被忽略的 `docs/validation/` |

Skill 的启用、更新与同步仍通过 skill-hub 操作，不手工修改项目副本，不提交用户级 skill-hub 缓存。Skill 内的模板 Cargo.lock、references、assets、scripts 都属于完整快照，应保留。

`tests/fixtures/server.key` 是专门生成并公开随仓库分发的测试材料，测试代码依赖它；不能用 `*.key` 或 `*.crt` 的全局忽略规则排除这些夹具。实际部署私钥和凭证放在被忽略的 `secrets/`、本地环境配置或仓库之外。

## 不提交、不归档的本地产物

| 路径 | 内容与清理方式 |
|---|---|
| `target/` | 编译产物、增量缓存、Rustdoc、下载的 DuckDB 原生库；可用 `cargo clean` 删除，后续构建重新生成 |
| `data/` | 示例配置使用的运行临时目录及下载扩展；扩展通过 `scripts/fetch-extensions.py` 重新获取。实际部署持久化数据须独立管理，不能当构建缓存删除 |
| `docs/validation/` | 中间验证日志、报告、环境快照与摘要；无需保留或额外归档 |
| `__pycache__/`、`*.pyc`、`*.pyo` | Python 字节码缓存，可直接清理 |
| `*.log`、`*.swp`、`*.swo`、`*~` | 运行日志和编辑器临时文件，可在确认无活动进程使用后清理 |
| `.env`、`.env.*`、`config/*.local.toml`、`secrets/` | 本地配置与凭证；不属于可随意删除的缓存，按用途保留。`.env.example`、`.env.sample` 允许提交 |
| `/tmp/fedengine-*` | 工作区外的开发诊断及临时测试目录，不归档；测试自行清理所创建的目录 |

同级 `../modorch-rs/` 和用户级 Cargo、Rustup、skill-hub 缓存均不属于本仓库。保留这些共享依赖，不能作为本项目临时文件清理。框架 revision 和获取方式见开发文档及 CI。

## 提交前检查

先完成开发文档中的构建与测试，再检查完整待提交清单：

```sh
git status --short --untracked-files=all
git add --dry-run -- .gitignore .cargo/config.toml .github/workflows \
  Cargo.toml Cargo.lock rust-toolchain.toml src crates config examples tests scripts docker \
  .agents/skills README.md LICENSE docs
```

暂存后审查实际内容，再提交：

```sh
git diff --cached --stat
git diff --cached --check
git diff --cached
```

当前实现、测试、锁文件、skill 快照、CI 和长期维护文档应作为一个可构建版本提交，避免遗漏测试夹具或 CI 所需脚本。
