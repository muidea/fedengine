---
name: skill-capability-index
description: 当一个任务同时匹配两个或以上当前可用 skill、需要判断唯一主 skill，或需要区分项目本地与全局 skill 时使用。只负责选择，不执行具体任务；普通功能开发、语义明确的单一任务、仅出现“重构/收口/清理”等字样，均不应单独触发本 skill。
compatibility: Compatible with open_code
metadata:
  version: "2.0.0"
  author: "rangh-codespace"
  created_at: "2026-04-18T23:12:34+08:00"
  tags: "skill-routing,capability-index,local-first"
---

# Skill Capability Index

本 skill 是一个轻量选择器，只在候选确实存在歧义时选出一个主 skill。它不替代领域 skill，不把托管仓库中“存在但未启用”的 skill 当成可执行候选，也不要求每个任务都必须使用专用 skill。

## 输入事实

选择前只收集以下事实：

- 用户的实际目标，而不是请求中孤立出现的关键词。
- 修改是否改变配置、API、数据、路由、持久化或运行期行为等外部合同。
- 当前会话明确列出的可用 skill。
- 当前项目 `.agents/skills/` 中可用的项目本地 skill 与本地 capability index。
- 当前请求是否延续同一目标，还是已经切换能力域。

本地 skill 仓库、skill-hub 远端搜索结果或默认仓库中的归档只表示“可以安装”，不表示“当前可用”。

## 硬性选择规则

1. 最终选择的主 skill 必须在当前会话可用列表或当前项目本地目录中真实存在。
2. 用户明确点名且可用的 skill 必须使用；若不可用，只说明一次并采用同目标下的可执行 fallback。
3. 精确领域匹配优先于通用方法 skill，项目本地精确匹配优先于全局通用匹配。
4. 只有任务明确跨越独立能力域，才增加 supporting skill；“相关”“可能有帮助”不构成追加依据。
5. 没有精确且可用的候选时，返回“无需专用 skill”，直接依据项目合同、代码和测试工作。
6. 同一连续任务的目标、能力域和候选集合均未变化时，不重新运行本索引，也不重复报告缺失候选。
7. deprecated、备份副本、未启用归档和名称通配符不能成为最终选择结果。

## 决策顺序

按以下顺序选择，命中后停止：

1. **用户点名**：使用用户明确指定且当前可用的 skill。
2. **Skill 管理**：
   - 创建、更新、修复、校验、归档托管 skill：`skill-hub-skill-authoring`。
   - 在项目或全局启用、应用已有托管 skill：`skill-hub-project-usage`。
   - 仅判断上述两类 skill-hub 流程：`skill-hub-workflow`。
   - 非 skill-hub 管理环境下设计新 skill：`skill-creator`。
3. **项目本地精确能力**：使用当前项目提供、且描述直接覆盖目标的本地 skill。
4. **全局精确能力**：从当前会话可用列表中选择直接覆盖目标的专用 skill。
5. **行为保持的 Go 内部重构**：仅在满足下节准入条件时选择 `go-refactor-pro`。
6. **无专用 skill**：没有命中时结束选择，不把任务强塞进相近分类。

上述 ID 仍必须经过可用性检查；本表不赋予未启用 skill 可执行资格。

## Go 重构准入边界

只有同时满足以下条件，`go-refactor-pro` 才能成为主 skill：

- 主要目标是改善 Go 内部结构、去重、解耦或已有热点性能。
- 预期外部行为和业务合同保持不变。
- 不存在更精确的项目本地或领域 skill。
- `go-refactor-pro` 在当前会话或项目中真实可用。

以下情况不得因为出现“重构、清理、统一、收口、解耦”等词而选择它：

- 新增或修改功能行为。
- 重命名、删除或迁移配置项。
- 修改 API、endpoint、路由、模型来源或数据归属合同。
- 修改兼容策略、持久化语义、权限或生命周期。
- 以缺陷诊断、代码评审、文档同步或脚本修复为主要目标。

功能或合同调整中包含内部整理时，主 skill 仍由功能领域决定；内部整理通常不需要额外 supporting skill。

## 框架与项目专属能力

全局索引不维护 `magicCommon`、`magicOrm`、`magicEngine` 或具体业务仓库的完整技能矩阵。这些映射应位于对应项目的本地 capability index；只有当前项目实际提供且当前任务直接命中时才参与选择。

如果项目没有本地定义，本索引只能从当前会话已提供的通用 skill 中选择，不能根据托管仓库目录猜测候选。

## 选择结果

选择结果保持最小化：

```yaml
primary: <skill-id | none>
supporting: []
reason: <命中目标与边界的一句话证据>
availability: verified
```

- `primary: none` 是正常结果，不是失败。
- 只有用户询问选择依据或存在真实歧义时才展开候选比较。
- 进入主 skill 后按其流程执行；本索引不继续参与实现决策。

详细正反例见 [路由验收用例](references/routing-cases.md)。

## Formatter

- Markdown/YAML：保持现有格式，并运行 `skill-hub validate --pattern skill-capability-index --links`。
- 修改路由规则后逐项核对 `references/routing-cases.md`，确保所有预期主 skill 先经过可用性门禁。
- 在 `skill-hub feedback --pattern skill-capability-index --force` 前完成格式与链接校验。
