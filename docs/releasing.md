# 提交、验证与发布规则

采用自动验证、显式发布。提交、推送、SDK 发版和数据更新是独立动作；是否发布按实际行为及交付产物判断，不仅看文件后缀或 Conventional Commit 类型。

## 默认操作

- “提交”只创建当前任务的 commit。
- “提交推送”创建 commit 并推送远端，不创建 SDK 标签或 Release。
- “提交并发布”先判断交付对象。纯文档推送到 `main` 即完成文档更新，并说明无需 SDK 发版；有待交付的 SDK 变化时才准备 SDK 版本；数据变化走独立数据发布流程。
- 用户明确要求创建 SDK Release，即使只有文档变化，也可作为一次明确例外执行。不把一次发布授权当成之后所有任务的发版授权。
- 普通提交不修改版本号。正式准备 SDK 发版时，才同时修改 `crates/future-meta/Cargo.toml`、`crates/future-meta-py/Cargo.toml` 和 `Cargo.lock`。

## 按影响选择发布对象

| 改动 | 默认处理 | SDK 版本 |
| --- | --- | --- |
| README、文档、AGENTS、技能说明或提示词 | 提交，按要求推送；运行相关轻量检查 | 不变 |
| 测试、格式、行为不变的内部重构 | 提交推送，运行对应检查 | 不变 |
| 采集脚本、daemon、官方规则、交易日历、数据发布流程 | 提交推送；在合法调度或获授权验证时应用 | 通常不变 |
| 手续费、保证金、规格、生命周期等业务数据真实变化 | 发布不可变 `data-*` 附件，原子切换 manifest 与检查点 | 客户端仍可读取时不变 |
| 日度检查成功但业务数据未变 | 仅更新状态，保留 archive、manifest 和数据 Release | 不变 |
| 需要交付给客户端的 SDK 修复、性能、安全或依赖修复 | 发布 SDK | 升补丁版本 |
| 新增 SDK API、合约类型或改变客户端兼容边界 | 发布 SDK，兼容性变化附迁移说明 | 当前 `0.x` 阶段升次版本 |

例：`0.8.1` 的兼容修复使用 `0.8.2`；新增公开 API 或破坏兼容使用 `0.9.0`。进入 `1.x` 后按 SemVer 区分兼容功能的次版本和破坏性变更的主版本。

发版判断覆盖最近 SDK 标签到待发布提交的累计变化，不只看最后一次提交。纯重构也可能改变性能、错误或构建兼容性；依赖、配置和生成输入变化同样需要判断实际影响。Conventional Commits 记录意图，不自动决定发版。

技能文件可跟随 `main` 或固定 commit 获取；示例固定到提供相应 API 的已发布 SDK。后续正式 SDK Release 自然包含之前积累的文档，不为同步技能而专门升级 SDK。

## 普通 CI

`.github/workflows/ci.yml` 在 `main` 的 push 和面向 `main` 的 pull request 上运行，只有读取仓库的权限：

- Markdown 文件链接、代码块及技能 YAML/元数据检查始终运行，不访问外部链接。
- 所有工作流使用 actionlint 检查语法。
- CI 范围判定和发版前检查的回归测试始终运行。
- 完整变更范围仅包含已识别文档路径时，跳过 Rust 和采集器测试。重命名同时检查原路径；混合改动、程序输入、工作流和未知路径运行完整离线套件。
- 完整套件包括 Rust 格式、构建、工作区和 download 测试、严格 Clippy，以及 Python 测试。安装测试依赖不启动采集器或浏览器，不读取线上数据。

这里的离线验证指测试不访问交易所或生产数据；工具链和依赖安装仍会访问各自分发站。

本地检查：

```bash
python -m pip install PyYAML
python scripts/check_docs.py
python -m unittest discover -s tests -p 'test_release_rules.py'
python scripts/ci_scope.py --base HEAD~1
```

文档内代码示例变化时，按需将受影响示例放入临时项目并编译。文字或链接修订不因此运行整个 Rust 工作区。链接检查只验证本地文件存在，未检查外部 URL 或页面锚点。

## SDK 标签发布

SDK 通过 Git 标签和 GitHub Release 分发，不以本流程发布到 crates.io。`.github/workflows/release-sdk.yml` 仅由 `v*` 标签的 push 触发，不由普通提交触发，也不处理 `data-*`。

1. 明确决定发布 SDK，检查自上一 SDK 标签以来的用户可见变化并选择版本。
2. 更新 SDK `Cargo.toml` 和锁文件，完成必要的代码、文档、技能与迁移说明；验证后提交推送到 `main`。
3. 创建稳定版本的 annotated tag，先在本地检查，再推送该标签。推送 SDK 版本标签就是正式发版指令。
4. 工作流校验稳定 `vX.Y.Z` 格式、Cargo 与锁文件版本（含 `future-meta` 与 `future-meta-py`）、标签与 checkout 一致、提交已进入 `main`、版本递增，再执行完整离线检查并构建跨平台 Python Wheel 与源码包。
5. 验证通过后，独立的 `contents: write` job 使用内置 `GITHUB_TOKEN` 创建 SDK Release、上传 Python Wheel/sdist 附件并设置 Latest。说明从前一个稳定 SDK 标签生成，排除独立的数据标签比较范围。checkout 不持久化凭据。
6. 观察 Actions 完成并读回 Release，确认 SDK Latest 和标签 commit。正式发布报告给出 commit、版本、验证命令和 Actions URL。

示例版本仅作演示，需按待交付变化选择：

```bash
git tag -a v0.8.2 -m 'v0.8.2'
python scripts/check_sdk_release.py v0.8.2
git push origin refs/tags/v0.8.2
gh run list --workflow release-sdk.yml
gh run watch <run-id> --exit-status
gh release view v0.8.2
```

当前自动流程仅支持稳定版本标签；不隐式发布预览版。一次完成一个 SDK 版本的发布，再创建下一版本标签；SDK 工作流串行且不取消运行中的发布。SDK 发布不调用 `update_official.py`、不写数据分支、不访问交易所。发布失败先核对 Actions 和 Release 实际状态，保留原标签并重试对应工作流；已创建的正式 Release 仅做读回验证，不重复创建，不改写已发布标签或旧版内容。

## 数据更新与生产验证

生产日更继续使用 `.github/workflows/update-fee-data.yml` 的既有时段、日历、串行采集和原子发布规则，见 [部署说明](deployment.md)。数据 Release 保持 `--latest=false`，与 SDK 版本独立。

`republish_current=true` 会验证线上 archive，并在数据分支写内容不变的 commit 来测试发布权限。仅在数据分发链路、发布权限或线上索引流程发生实际变化且已有相应授权时，或用户明确要求线上发布验证时使用。

纯文档、技能同步或普通 SDK 改动使用普通 CI/SDK 工作流，不为了获得 Actions URL 而运行生产日更或 `republish_current`。没有合法访问窗口时只做离线检查或获授权的无采集验证，不能宣称真实官方采集成功。

若新数据需要旧 SDK 不支持的 schema 或 symbol，先发布支持它的 SDK，再切换数据入口；不生成旧客户端兼容包。已发布的 `v0.8.1` 保留，规则从后续提交和发版开始应用。

参考：[Semantic Versioning](https://semver.org/lang/zh-CN/)、[Conventional Commits](https://www.conventionalcommits.org/zh-hans/v1.0.0/)。
