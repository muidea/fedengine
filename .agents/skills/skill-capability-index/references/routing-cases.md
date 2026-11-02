# Skill 路由验收用例

以下用例用于修改 `skill-capability-index` 后的人工回归。所有候选都必须先确认当前可用；表中的 skill 不可用时，预期结果降级为 `none`，而不是推荐一个缺失 skill。

| 用户目标 | 预期主 skill | 关键原因 |
| --- | --- | --- |
| 调整模型配置、endpoint 与运行期路由语义 | 项目领域 skill 或 `none` | 外部合同变化，不是行为保持重构 |
| 合并重复 Go helper，公开行为与合同不变 | `go-refactor-pro` | 纯内部去重，且仅在该 skill 可用时 |
| 新增 framework 运行单元并完成生命周期接线 | `go-multi-module-dev` | 精确命中运行单元能力 |
| 更新 skill-hub 托管的 `SKILL.md` | `skill-hub-skill-authoring` | 托管 skill 作者流程 |
| 在业务项目启用已有托管 skill | `skill-hub-project-usage` | 消费已有 skill，不是作者流程 |
| 不确定应该创建 skill 还是在项目中启用 skill | `skill-hub-workflow` | 只做 skill-hub 工作流分流 |
| 评审或诊断普通 Go 功能缺陷 | 项目领域 skill 或 `none` | 评审、诊断本身不等于重构 |
| 修复部署 Shell 脚本 | Shell/部署领域 skill 或 `none` | 不是 Go 重构 |
| 用户明确点名一个当前可用 skill | 用户点名 skill | 明确指令优先 |
| 用户点名的 skill 当前不可用 | 同目标可执行 fallback 或 `none` | 只提示一次，不把归档当可用候选 |
| 连续请求继续同一能力域，候选未变化 | 复用既有判断 | 不重复运行索引或重复报告缺失项 |
| 项目本地 skill 与全局通用 skill 同时匹配 | 项目本地精确 skill | local-first 且精确领域优先 |

## 反例关键词

以下词语不能单独决定 skill：

- 重构
- 清理
- 收口
- 统一
- 解耦
- 优化
- 完善

必须结合用户目标、外部合同是否变化、修改对象和当前可用候选判断。
