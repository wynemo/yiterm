# 文档维护

- `readme.md` 面向终端用户，保持简短，只介绍产品、从 GitHub Releases 下载运行的方法和主要功能。
- `TECHNICAL.md` 存放技术与详细使用说明，包括源码构建、快捷键、分栏、配置和日志。新增技术细节写在这里，并保持 README 中的链接有效。

# Windows 验证

- 构建统一使用 Release 模式，不使用 Debug 构建。
- 不在 macOS 上安装 Windows 工具链、SDK 或交叉编译 Windows。
- 本机只运行 macOS 构建；Windows 构建交给 Windows CI runner。
- 汇报时区分本机已验证结果与尚未运行的 Windows CI 验证。

# 测试

- 后续不新增测试代码、测试脚本或 CI 测试步骤，也不编译或运行测试。
