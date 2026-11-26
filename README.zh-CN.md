# bumpr

使用 Rust 编写的跨语言交互式版本管理工具，灵感来自 [bumpp](https://github.com/antfu-collective/bumpp)。无需安装 Node.js 或 Java 即可读取和修改项目版本。

[English](README.md)

## 安装与使用

```sh
cargo install --git https://github.com/elonehoo/bumpr --locked
```

需要 Rust 1.88 或更高版本。进入任意项目目录运行：

```sh
bumpr                         # 交互式选择新版本
bumpr --current               # 查看识别到的版本字段
bumpr patch --dry-run         # 预览，不修改文件
bumpr minor --yes             # 更新版本
bumpr prerelease --preid rc --yes
bumpr 2.0.0 --yes              # 设置指定版本
bumpr patch --commit --yes    # 更新并提交
bumpr patch --tag --yes       # 更新、提交、打 tag
bumpr patch --push --yes      # 更新、提交、打 tag、推送
```

默认只修改版本文件，不自动推送或发布。Git 操作需要干净的工作区；`--push` 推送的 tag 可能触发现有 CI/CD。

## 支持的项目

- Rust Cargo：工作区成员、继承版本、本地依赖版本、`Cargo.lock`。
- Node.js：npm、Yarn、pnpm 工作区，以及 npm、pnpm 6.0/9.0 格式本地链接和 Bun 文本 lockfile。
- Java / Kotlin：Maven 多模块、父项目版本、本地依赖，以及 Gradle 的 `gradle.properties` 和静态版本声明。
- Python、.NET、PHP、Ruby、Dart/Flutter、CMake、普通 `VERSION` 文件。
- Go、Swift 等使用 Git tag 控制版本的项目。

```sh
bumpr --recursive --current
bumpr patch --recursive --from 1.2.3 --yes
```

使用 `--recursive` 识别独立的嵌套项目；如有多个不同版本，可通过 `--from` 选择要更新的版本组。其他语言或自行定义的版本文件可以通过 `bumpr.toml` 支持：

```toml
auto = false
[[files]]
path = "include/**/version.h"
pattern = '(?m)^#define APP_VERSION "(?P<version>[^"\n]+)"$'
```

工具只替换明确识别的版本字段，保留其余内容与格式，不执行构建脚本或自动下载依赖。动态计算的版本和未支持的 lockfile 格式需要自定义规则或由对应包管理器刷新，具体范围见 [English README](README.md)。

## 许可证

[MIT](LICENSE)
