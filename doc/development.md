# 开发与构建

[返回首页](../README.md) · [快速开始](getting-started.md) · [配置参考](configuration.md)

## 环境准备

- Node.js 与 pnpm。
- Rust stable 工具链，且 **不低于 1.92**：自定义音效的文件选择用 `rfd` 0.18，它的 MSRV 是 1.92，工具链更旧会在编译期直接报错（仓库没有锁定 `rust-toolchain`，用发行版自带的老 rustc 打包时尤其注意）。
- Windows 上的 Visual Studio C++ 构建工具。

项目使用 Rust workspace、Tauri 2、Vue 3 和 TypeScript。目录职责见 [项目结构](configuration.md#项目结构)。

## 构建安装包

先在仓库根目录跑 Rust 一侧的全量测试：

```powershell
cargo test --manifest-path .\Cargo.toml --workspace --all-targets
```

再到 `app` 目录完成前端依赖、检查与打包：

```powershell
cd app
pnpm install --frozen-lockfile
pnpm typecheck
pnpm test
pnpm test:tools
pnpm tauri build
```

上述命令覆盖 Rust 全量测试、前端依赖安装、类型检查、前端回归测试与探针测试，最后经 Tauri CLI 打包。任何一步失败都应停止打包。打包前建议先退出正在运行的 Agent Bark，否则 exe 被占用会导致链接失败；构建完成后可直接运行 `target\release\AgentBark.exe`。

默认产物：

| 产物 | 路径 |
| --- | --- |
| Windows 安装包 | `target\release\bundle\nsis\` |
| 可执行文件 | `target\release\AgentBark.exe` |

设置了 `CARGO_TARGET_DIR` 时，产物会写入对应目录。

## 本地开发

```powershell
cd app
pnpm install --frozen-lockfile
pnpm tauri dev
```

开发模式使用 `5173` 端口。只调试前端样式可运行 `pnpm dev`；调用原生命令的页面需要 Tauri 运行环境。

## 单独运行检查

在仓库根目录运行：

```powershell
pnpm --dir app typecheck
pnpm --dir app test
pnpm --dir app test:tools
cargo test --workspace --all-targets
pnpm --dir app build
```

前端 `build` 不包含 TypeScript 类型检查，需要单独运行 `typecheck`。

## 手动打包

```powershell
cd app
pnpm tauri build
pnpm tauri build --no-bundle
```

第一条生成可执行文件和安装包，第二条仅生成可执行文件。发布桌面应用应通过 Tauri CLI 构建，以便将前端资源正确嵌入。相关排障与调试环境变量见 [开发说明](configuration.md#开发说明)。

修改 Agent 适配器前请先阅读 [Agent 接入与排查](agent-integration.md)；父子任务的状态规则见 [任务生命周期](task-lifecycle.md)。
