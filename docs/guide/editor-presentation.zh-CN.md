[English](editor-presentation.md) | 简体中文
<!-- translated-from: editor-presentation.md sha256=c8467996a9ddd0c6777aa3b3551799f20cdffba2efece307ab1632f182577932 -->

# 编辑器怎样把画面送上屏幕

编辑器用 GPU 绘制 Neovim。在 Wayland 桌面上，Eitri 可以把这些帧放在一个独立的表面上直接交给合成器，和窗口里的其他模块并排；在其他环境里，它像普通控件一样把帧画进 GTK 窗口。前一种方式在每次按键时少一次拷贝、少一个 GTK 帧：我们实测时，输入的字母上屏比在 Neovide 里慢不到半毫秒，而 GTK 方式在 165 Hz 下多出 6 ms，在 60 Hz 下多出 17 ms。

在 `init.lua` 里用 `editor.presentation` 选择：

```lua
eitri.config.set("editor.presentation", "auto")
```

| 值 | 作用 |
|---|---|
| `"auto"`（默认） | 在 Wayland 上、GTK 4.24 或更新、显卡驱动不是 NVIDIA 时用独立表面；否则用 GTK 方式。 |
| `"subsurface"` | 在任何 Wayland 桌面上都用独立表面，不看 GTK 版本和驱动。可以用来试 `"auto"` 排除掉的组合。 |
| `"gtk"` | 始终用 GTK 方式。 |

在 X11 上编辑器始终用 GTK 方式。这个设置在窗口启动时读取，编辑器重启后仍然保留；改了之后要开一个新窗口才生效。

环境变量 `EITRI_EDITOR_PRESENTATION` 取同样三个值，对这个进程打开的每个窗口覆盖这个设置。其他值会让 Eitri 在启动时停下，并在消息里指出这个变量。

## 什么时候退回

如果合成器拒绝了独立表面需要的东西（缓冲区格式、某个缓冲区、图形上下文），编辑器会在这个窗口剩下的时间里改用 GTK 方式，并在 stderr 打印一行：

```
[editor] subsurface: <原因>; falls back to the GTK path
```

编辑器照常可用，只是没有了延迟上的好处。看到这一行时，请把其中的原因、你的合成器和驱动告诉我们。

## 试过哪些环境

独立表面在 sway 和 Mutter（GNOME）上、缩放 1 和 1.5、AMD 显卡上测试过；没有在 KWin、Hyprland 和 NVIDIA 上试过，所以 `"auto"` 让 NVIDIA 保持 GTK 方式。如果在你的桌面上看起来不对（编辑器位置错了、旁边有缝、画面停在旧的一帧），请设成 `"gtk"` 并报告。

## 平滑滚动

Neovide 自己就会给滚动加动画。为了不让第二套动画叠在上面，Eitri 启动编辑器的 Neovim 时设置 `vim.g.snacks_scroll = false`，关掉 [snacks.nvim](https://github.com/folke/snacks.nvim) 的滚动动画（LazyVim 会把这个插件的 `scroll` 打开；它和 render-markdown 同时开着时，在 Markdown 缓冲区里按 `k` 之类的移动可能让光标绕圈）。想要 snacks 的滚动动画回来，在你自己的 Neovim 配置里写：

```lua
vim.g.snacks_scroll = true
```

你的配置在 Eitri 的默认值之后运行，所以这一行生效。
