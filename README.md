# qq-wayland-clipboard

一个修复 Linux QQ 原生 Wayland 模式剪贴板兼容问题的 wrapper。

QQ 的窗口虽然运行在 Wayland 上，部分剪贴板功能仍由隐藏的 Xlib client 实现，可能导致 QQ 读取旧图片，或让其他 Wayland 应用无法识别 QQ 复制的图片、富文本和文件。本项目使用专用 Xvfb 隔离这些遗留 X11 操作，再将受支持的内容转换并发布到真实的 Wayland 剪贴板。

## 1. 安装

运行环境需要：

- Linux QQ 和 Wayland 会话；
- 支持 `ext-data-control` 或 `wlr-data-control` 的 compositor；
- 系统中可用的 `Xvfb` 命令。

当前方案已在 KDE Plasma / KWin 6.7.3 上验证。其他实现上述 data-control 协议的 compositor 尚未逐一测试。

Arch Linux 可通过以下命令安装 Xvfb：

```bash
sudo pacman -S --needed xorg-server-xvfb
```

从源码安装：

```bash
cargo install --path .
```

如果只需构建当前工作区：

```bash
cargo build --release
```

## 2. 使用

先完全退出所有 QQ 进程，再把 QQ 可执行文件作为第一个参数传入：

```bash
qq-wayland-clipboard /opt/QQ/qq
```

后续参数会原样传递给 QQ：

```bash
qq-wayland-clipboard /opt/QQ/qq --enable-logging
```

wrapper 在前台运行并管理 QQ 和 Xvfb 的生命周期。按 Ctrl+C 可停止整个实例。

## 3. 支持范围

| 内容 | QQ → Wayland 应用 | Wayland 应用 → QQ |
| --- | --- | --- |
| 纯文本、HTML | 转发 QQ 私有富文本，并提供纯文本和 HTML fallback | 由 QQ 原生读取 |
| 图片 | 转发 PNG；将错误标记为 PNG 的 JPEG、BMP、GIF、ICO、TIFF 和 WebP 重新编码为 PNG | 由 QQ 原生读取 |
| 单个本地文件 | 将 QQ 私有文件元素转换为标准 `text/uri-list` | 由 QQ 原生读取 `text/uri-list` |
| 混合图文 | 保留 QQ 私有格式，同时提供标准 MIME fallback | 取决于 QQ 和来源应用提供的格式 |

## 4. 工作原理

**问题一：即使在 Wayland 会话下，QQ 仍然优先检查 X11 剪贴板，而 KDE 的剪贴板同步器只会将 Wayland 剪贴板内容同步给 X11 前台应用。这导致 QQ 总是读取到 X11 剪贴板的旧内容。**

因此，使用单独的 X11 服务器（Xvfb）隔离 QQ，使它总是读取失败，从而 Fallback 到 Wayland 剪贴板逻辑。

**问题二：QQ 复制的图片、富文本和文件使用了 QQ 私有格式，其他 Wayland 应用无法识别。**
用 Bridge 监听单独的 X11 剪贴板，转换 QQ 私有格式为标准 MIME，并发布到 Wayland 剪贴板。

## 5. 已知限制

- 不同应用对混合图文 MIME 的选择方式不同，可能不支持 QQ 的 `text/html` 格式，从而只能读取纯文本。
- Wayland data-control 不提供条件更新 selection 的原子操作。目前 bridge 会通过 Generation Ticking 以缩小竞态窗口，但无法保证与另一个应用完全同时复制时的绝对顺序。

## 6. 许可

Copyright (C) 2026 w568w

本项目以 GNU General Public License v3.0 or later 发布，详见 [LICENSE](LICENSE)。
