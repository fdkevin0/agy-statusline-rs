# agy-statusline

[![Crates.io](https://img.shields.io/crates/v/agy-statusline.svg)](https://crates.io/crates/agy-statusline)
[![CI](https://github.com/fdkevin0/agy-statusline-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/fdkevin0/agy-statusline-rs/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

[English](README.md) | 简体中文

基于 `#![forbid(unsafe_code)]` 纯安全 Rust 构建的 Antigravity CLI（`agy >= 1.5.15`）状态栏与本地用量统计面板，支持 5h/7d 配额消耗速率与多池/AI 额度展示、真实压缩阈值探测、支持持久化扫描缓存与分支去重的本地 SQLite 统计与重试/压缩诊断、含子代理的 `models.dev` 成本估算、纯 Rust `.git/HEAD` 与 worktree 识别，以及交互式三栏 TUI 配置器。

![agy-statusline 内置预设效果](docs/assets/presets.png)

## 环境要求

- **Antigravity CLI `>= 1.5.15`**（如需合并聚合 `~/.gemini/antigravity` 桌面端用量，需 **Antigravity Desktop `>= 2.35.0`**），直接使用官方未裁剪的 `conversations/*.db` 作为唯一权威数据源。

## 安装

通过 Cargo 安装：

```sh
cargo install agy-statusline
```

也可以直接从 [GitHub Releases](https://github.com/fdkevin0/agy-statusline-rs/releases) 下载预编译二进制，或从源码构建（需要 Rust 2021；默认内置 `libsqlite3`，传入 `--no-default-features` 可链接系统 `libsqlite3`）：

```sh
git clone https://github.com/fdkevin0/agy-statusline-rs.git
cd agy-statusline-rs
cargo install --path . --locked
```

## 快速开始

1. 启动交互式配置器自定义布局与配色（支持键盘与鼠标操作；按 `s` 可一键保存并写入 AGY 配置）：

```sh
agy-statusline config
```

![agy-statusline 交互式 TUI 配置器](docs/assets/config-tui.png)

2. 注册到 AGY（`~/.gemini/antigravity-cli/settings.json`）并运行环境自检：

```sh
agy-statusline apply --yes
agy-statusline doctor
```

3. 查看本地 CLI 与桌面端的按日/按模型 Token 用量、实时/缓存多池配额与 AI 额度、缓存命中率、生成速度、TTFT、重试/压缩诊断与估算 API 成本（终端自动进入交互式面板，也可通过 `--plain` / `--json` 输出静态报告）：

```sh
agy-statusline stats
agy-statusline stats --plain --metric cost --days 30
agy-statusline conversations
agy-statusline stats <conversation-id>
```

![agy-statusline 本地用量统计看板](docs/assets/stats-tui.png)

## 文档

- [配置、组件与 TUI 完整指南（英文）](docs/configuration.md)
- [贡献指南](CONTRIBUTING.md) · [行为准则](CODE_OF_CONDUCT.md) · [安全策略](SECURITY.md)

## 许可证

[MIT](LICENSE)
