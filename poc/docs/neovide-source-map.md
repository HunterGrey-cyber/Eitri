# Neovide 源码结构地图（fork 规划用）

调查对象：`poc/vendor/neovide`（浅克隆后 `git fetch --unshallow` 补全了完整历史用于统计churn，仓库本身仍是 gitignore 的只读参考物，未做任何修改）。

- 版本：`Cargo.toml` `version = "0.16.2"`，HEAD `ade2d9c`，clone 时间 2026-09-05。
- 结构：单 crate（`neovide`），workspace 里唯一的其它 member 是 `neovide-derive`（一个小的 proc-macro crate，与本次调查无关）。不是"渲染器/输入/运行时"式的多 crate 拆分——一切都在 `src/` 一个 crate 里，模块边界靠 `mod` 而不是 crate 边界。
- `src/` 顶层模块：`bridge/`（nvim RPC）、`editor/`（grid/cursor/style 状态，纯数据，不碰 GPU/窗口）、`renderer/`（Skia 绘制 + per-OS GL/Metal/D3D 后端）、`window/`（winit 事件循环、`Application`、`WinitWindowWrapper`、keyboard/mouse manager）、`settings/`、`profiling/`、`platform/macos/`。

结论先说：Neovide 的"每帧画什么"（`editor/`、`renderer/grid_renderer.rs`、`renderer/fonts/`、`renderer/cursor_renderer/`、`renderer/animation_utils.rs`）已经和"窗口在哪、多大、谁的GL上下文"这件事分得比较开——这部分是好消息。真正的问题集中在**驱动这些组件的"胶水层"**：`src/window/window_wrapper.rs`（2870 行）和 `src/renderer/opengl.rs`（GL surface 创建），这两个文件正好就是 fork 必须动的地方，而且也恰好是 upstream 改动最频繁的文件之一。

---

## §6.1 "先不要把 Neovide 改成通用 GUI library"

现状：`WinitWindowWrapper`（`src/window/window_wrapper.rs:251`）持有 `routes: FxHashMap<WindowId, Route>` + `route_cores`，是一个支持多窗口（macOS tab/window切换）的"应用级"状态机，`Route` 里塞了 renderer/skia_renderer/mouse_manager/neovim_handler/macos_feature 等一切东西（`RouteWindow` struct，`window_wrapper.rs:150-165`）。`Application`（`src/window/application.rs`）实现 `winit::application::ApplicationHandler<EventPayload>`，`fn resumed` 里创建窗口，`main.rs` 里 `event_loop.run_app(self)`（`application.rs:178`）——即 **Neovide 自己拥有并驱动整个进程的事件循环**，不是被别的循环驱动的库。

判断：这条本身不是"改不改"的问题，而是给后面几条定了范围——只要不把 `WinitWindowWrapper`/`Application`/多窗口路由这套东西通用化（不支持任意宿主 widget 树），只做"单 Route 无 OS 窗口"这一种特化，改动量可控。不需要现在动。

---

## §6.2 Renderer 只画自己的 viewport

**现状比想象中好一点，但没有做到位。**

- `Renderer::draw_frame`（`src/renderer/mod.rs:252-284`）签名已经是 `draw_frame(&mut self, root_canvas: &Canvas, content_region: Option<&PixelRect<f32>>, dt: f32)`——**已经有一个 viewport rect 参数**，用来 `root_canvas.clip_rect(...)`（`mod.rs:273-278`）。
- 但第 269 行：`root_canvas.clear(default_background);` 发生在 `clip_rect` **之前**，对整个 `root_canvas` 无条件调用——**不管传没传 `content_region`，都会 clear 整个宿主 canvas**，不是只 clear 自己的 viewport。这正是架构文档 §6.2 点名的问题，且是真实存在的、一行代码级的耦合，不是假设。
- 调用方 `draw_frame`（`window_wrapper.rs:1435-1468`）里的 `content_rect` 来自 `get_content_pixel_rect_from_window`（见 §6.3），是"整个 winit 窗口 inner_size 减去 padding"，不是宿主随意指定的子矩形——目前唯一能给 `content_region` 塞值的调用方还是"这块地方等于全窗口刨掉 padding"，没有"宿主分配任意矩形"的路径。
- `RenderedWindow`（`src/renderer/rendered_window.rs`）、`grid_renderer.rs`、`fonts/`、`cursor_renderer/` 这些真正逐字符画字形/光标/动画的代码里没有看到任何"clear 整个 surface"或假设 canvas==window 的调用；它们操作的是 `PixelRect`/`GridRect`（见 units.rs），只要传进来的 rect 和 scale 对，本身不关心 canvas 多大。

需要改的地方：`renderer/mod.rs:269` 这一行 `clear` 需要改成"只 clear `content_region`（如果有）而不是整个 canvas"；`window_wrapper.rs` 里 `get_content_pixel_rect_from_window` 需要能接受外部指定的 viewport 而不是永远从 `saved_inner_size` 推导（见 §6.3）。

侵入程度：**小、局部**。只涉及 `renderer/mod.rs` 里 `draw_frame` 顶部几行 + `window_wrapper.rs` 里 viewport 来源的替换，不触及 `grid_renderer.rs`/`fonts/`/`cursor_renderer/`/`animation_utils.rs` 里的绘制逻辑本体。

---

## §6.3 Geometry 改成基于 viewport（Window Size → Grid Size 变成 Editor Viewport Size → Grid Size）

**现状：写死了"grid size 从 winit `Window.inner_size()` 推导"，这是本次调查里第二重要的发现。**

数据流（都在 `window_wrapper.rs`）：

1. `route.state.saved_inner_size` 的唯一赋值来源：`let saved_inner_size = window.inner_size();`（`window_wrapper.rs:1756`，窗口创建时），以及 `WindowEvent::Resized` 触发的路径（`:2543-2558`, `:2655`）—— 全部来自真实 `winit::window::Window` 的 OS 查询，不接受外部注入。
2. `get_grid_size_from_window`（`:2662-2689`）= `(saved_inner_size - window_padding) / grid_scale`，向下取整，`.max(MIN_GRID_SIZE)`。
3. `update_grid_size_from_window`（`:2737-2779`）把上面算出的 `grid_size` 通过 `send_ui(ParallelCommand::Resize{...}, &neovim_handler)` 发给 nvim，即 `nvim_ui_resize` 的调用点。
4. `get_content_pixel_rect_from_window`（`:2713-2735`）同样基于 `saved_inner_size`，产出 §6.2 用到的 `content_rect`。

也就是说：Window Size → Grid Size 这条链路上，"Window Size"这一端目前**只能是**一个真实 `winit::window::Window` 的 `inner_size()`，没有"Shell 指定 editor_rect，从这个 rect 反推 grid size"的入口。`WindowPadding`（`:107-112`，仅 top/left/right/bottom 四个 `u32`）是唯一现存的"留白"概念，用于 macOS 标题栏之类的场景，而不是任意可变的子区域分配。

好消息：**几何类型系统本身是干净的**——`src/units.rs`（148 行）定义的 `Grid<T>`/`Pixel<T>` 单位、`GridSize`/`PixelSize`/`GridRect`/`PixelRect`/`GridScale`，全部基于 `glamour` 泛型，不依赖 winit/GTK 任何东西，`GridScale` 的 `Mul`/`Div` 运算符已经把"grid ↔ pixel"的换算封装好了。改造不需要重新设计单位系统，只需要替换"`PixelSize` 从哪来"这一步的数据源。

需要改的地方：把 `RouteState.saved_inner_size`（或者一个新的等价字段）从"只能被 `window.inner_size()`/`WindowEvent::Resized` 写入"改成"可以被宿主（GtkGLArea 的 `resize` 回调）显式设置"；`get_grid_size_from_window`/`get_content_pixel_rect_from_window`/`update_grid_size_from_window` 三个函数需要能在没有真实 `winit::Window` 的情况下工作（它们目前不直接调用 `window.*`，只读 `route.state.*`，这点是好事——说明重构点集中在"谁来写 `saved_inner_size`"而不是这三个函数本身）。

侵入程度：**中等，但集中**。全部改动落在 `window_wrapper.rs` 一个文件的 `RouteState`/`get_grid_size_from_window`/`get_content_pixel_rect_from_window`/resize 事件处理路径里，不涉及 `editor/`（grid 状态数据结构）或 `bridge/`（`nvim_ui_resize` 调用协议本身不用改，只是调用它的输入来源变了）。

---

## §6.4 Input 需要适配层（winit `WindowEvent` → `NeovideInputAdapter` → Neovim）

**现状：完全没有适配层，`KeyboardManager`/`MouseManager` 直接 match 原始 `winit::event::WindowEvent`。**

- `KeyboardManager::handle_event(&mut self, event: &WindowEvent, neovim_handler: &NeovimHandler)`（`src/window/keyboard_manager.rs:49`）直接 `match event { WindowEvent::KeyboardInput{..} => .., WindowEvent::Ime(Ime::Commit(..)) => .., WindowEvent::ModifiersChanged(..) => .. }`。没有中间事件类型。
- `MouseManager::handle_event(&mut self, event: &WindowEvent, keyboard_manager: &KeyboardManager, renderer: &Renderer, window: &Window, neovim_handler: &NeovimHandler) -> MouseEventResult`（`src/window/mouse_manager.rs:650-657`）同样直接 match `WindowEvent::CursorMoved/CursorEntered/MouseWheel/Touch/MouseInput/KeyboardInput(用于hide_mouse_when_typing)/Focused`，而且**签名里直接带了 `window: &Window`**（见 §6.5）。
- 唯一的调用入口 `WinitWindowWrapper::preprocess_window_input`（`window_wrapper.rs:910-978`）把同一个 `&WindowEvent` 分别喂给 `mouse_manager.handle_event(...)` 和 `keyboard_manager.handle_event(...)` 和 `renderer.handle_event(event)`（渲染器自己也吃一份原始事件，用于光标动画的 `WindowEvent` 触发，见 `renderer/mod.rs:236-238`，实际只用于 `cursor_renderer.handle_event`）。

也就是说，`WindowEvent` 这个类型在三个不同的下游（keyboard/mouse/cursor-renderer）里被直接消费，没有任何"GTK Event → NeovideInputAdapter → 内部事件枚举 → KeyboardManager/MouseManager"这层转换。要接 GTK 输入事件，要么（a）在 shell 侧伪造/合成 `winit::event::WindowEvent`（`winit` 的 event 类型是 `pub` 的但构造它需要一些内部字段技巧，某些 variant 如 `KeyEvent` 字段不是任意可构造的，可能有阻力），要么（b）把这三处签名改成吃一个自定义中间类型（这才是架构文档想要的 `NeovideInputAdapter`），后者需要同时改 `keyboard_manager.rs`、`mouse_manager.rs`、`window_wrapper.rs::preprocess_window_input` 三处的类型签名，属于"广而浅"的改动——改动点分散在三个文件，但每处都只是把 match 的输入类型换掉，逻辑本体（IME 组合、修饰键状态机、鼠标拖拽选区、滚轮增量换算）不用重写。

侵入程度：**中等（面广但浅）**。真正复杂的部分（IME preedit/commit 状态机、`format_key`/`format_key_text` 的按键转义规则、鼠标拖拽选区/双击三击时序）都可以原样保留，只是外层套一层从 GTK 事件构造等价内部结构的适配代码。

---

## §6.5 Window 依赖抽象成 `SurfaceHost`

**现状：不存在，`SkiaRenderer` trait 是最接近的东西，但它本身就在往外泄漏 `winit::window::Window`。**

- 唯一"看起来像"抽象层的是 `pub trait SkiaRenderer`（`src/renderer/mod.rs:782-790`）：`fn window(&self) -> Rc<Window>; fn flush(&mut self); fn swap_buffers(&mut self); fn canvas(&mut self) -> &Canvas; fn resize(&mut self); fn create_vsync(...) -> VSync;`。这是"选择 OpenGL/Metal/Direct3D 哪个后端"的 trait，不是"抽象掉窗口"的 trait——它的第一个方法就返回一个完整的 `Rc<Window>`，调用方（`window_wrapper.rs` 到处，比如 `route.window.winit_window.clone()`、`skia_renderer.window().set_corner_preference(...)`）继续直接拿这个 `Window` 调 `set_cursor_visible`、`set_ime_allowed`、`set_title`、`set_theme`、`focus_window`、`inner_size()`、`has_focus()`、`current_monitor()` 等一堆 winit 原生 API。
- `MouseManager` 直接在方法签名里吃 `window: &Window`（见 §6.4），内部调用 `window.set_cursor_visible(..)`（`mouse_manager.rs:129/142/144/150`）、`window.has_focus()`（`:684/686/693/728`）、`window.inner_size()`（`:243`）。
- GL 层更深：`OpenGLSkiaRenderer::new`（`src/renderer/opengl.rs:73`）和 `build_window`（`opengl.rs:250`）用 `glutin_winit::DisplayBuilder` + `raw_window_handle::HasWindowHandle` 直接从一个刚创建的 `winit::window::Window` 生成 `glutin::surface::Surface<WindowSurface>` 和 `PossiblyCurrentContext`——即 Skia 的 GPU surface 是"由 Neovide 自己创建的 GL 窗口 surface"，不是"包一层宿主已经建好的 GL 上下文"。要塞进 `GtkGLArea`（GtkGLArea 用自己的 EGL/GLX context），要么完全绕开 `glutin_winit::DisplayBuilder` 改成从外部传入的 GL context/display 包一个 Skia `DirectContext`（`skia-safe` 支持"wrap 现有 GL context"，这条路径技术上通，但要重写 `opengl.rs` 里 `new`/`build_window` 这部分，且 `window()` 这个 trait 方法要么删掉要么改成返回宿主提供的等价物）。

需要新增：一个类似
```rust
trait SurfaceHost {
    fn request_redraw(&self);
    fn set_ime_enabled(&self, enabled: bool);
    fn set_ime_cursor_area(&self, rect: Rect);
    fn set_cursor(&self, cursor: Cursor);
    fn set_title(&self, title: &str);
}
```
的 trait，把 `window_wrapper.rs` 里所有 `route.window.winit_window.xxx()` 调用、`mouse_manager.rs` 里的 `window.set_cursor_visible/has_focus/inner_size`、以及 vsync（`VSync::request_redraw(&window)`，见 `renderer/vsync/mod.rs`）都改成走这个 trait，而不是直接吃 `Rc<Window>`。

侵入程度：**这是六条里最重的一条**，原因有两层：
1. **广度**：`Rc<Window>`/`&Window` 这个类型在 `window_wrapper.rs`（`RouteWindow.winit_window` 字段本身）、`mouse_manager.rs`（函数签名参数）、`SkiaRenderer::window()`（trait 方法签名）、`vsync/*.rs`（`request_redraw`/`get_refresh_rate` 都吃 `&Window`）四处都是硬编码类型，不是"某个模块内部细节"，要抽象成 trait 需要同时改这四处的签名。
2. **深度**：GL surface 创建（`opengl.rs`）那部分不是"调用几个 Window 方法"，而是"用这个 Window 的 raw-window-handle 创建 GL context 本身"，这部分逻辑目前是和"创建一个真实 winit 窗口"强绑定的，要支持"GL 上下文由宿主已经建好、Neovide 只管往里面画"这种模式，需要新增一条不经过 `glutin_winit::DisplayBuilder` 的路径——这已经不是"抽象掉几个 setter 调用"能解决的，属于"必须改 renderer 内部（GL 后端选择/初始化那一层）"的情况，虽然它不碰 `grid_renderer.rs`/字体渲染/动画这些"画什么"的逻辑，但它确实位于架构文档 §7 划的 `renderer/` 目录里。

---

## 额外发现：事件循环所有权（比 §6 五条本身更底层的一条隐藏耦合）

`main.rs` → `event_loop.run_app(self)`（`application.rs:178`），`Application` 实现 `winit::application::ApplicationHandler<EventPayload>`，窗口创建发生在 `fn resumed(&mut self, event_loop: &ActiveEventLoop)`（`application.rs:892`）。**Neovide 进程自己拥有并驱动整个 winit 事件循环**，不是被外部（GTK 主循环）按需 poll 的库。

这条架构文档 §6 没有单列，但它是"component 化"能不能成立的前提性问题：GTK4 的 `GtkGLArea` 是被 GTK 自己的 `GMainLoop` 驱动的（`render` 信号、`resize` 信号等），而 winit 在 Linux（X11/Wayland）下的设计假设也是"我拥有事件循环"。两个都想拥有事件循环是没法直接共存的。可能的出路：(a) 完全不用 winit 的 `EventLoop::run_app`，只保留 `WinitWindowWrapper`/`Renderer`/`bridge`/`keyboard_manager`/`mouse_manager` 这些"逻辑"部分，由 GTK 的信号回调手动调用它们的方法（相当于把 `Application::window_event`/`resumed` 里的分发逻辑搬到 GTK 回调里）；(b) 找 winit 支持的某种"嵌入模式"（据我们看到的源码，当前版本没有）。这属于结构性风险，不是某一行代码能解决的，应该在写代码前先做一个最小 spike 验证。

---

## Upstream churn（用于评估"改动能不能保持在小范围、以后好合并"）

浅克隆后执行 `git fetch --unshallow` 补全历史（HEAD `ade2d9c`，共 1580 次提交）。按路径统计：

| 路径 | 全部历史提交数 | 近 6 个月提交数 | 最近改动日期 | 备注 |
|---|---|---|---|---|
| `src/window/window_wrapper.rs` | 88 | **23** | 2026-08-12 | **fork 必须改的文件，同时是 upstream 改动最频繁的文件之一** |
| `src/bridge/`（整个目录） | 318 | 24 | 2026-09-02 | 改动量大，但多是 UI 命令/宏平台功能（进度条、macOS 文档状态等），协议本体（`session.rs`）很稳 |
| `src/bridge/session.rs`（`nvim --embed` 进程/握手） | — | **2** | — | 我们不需要碰这个文件，且它本身几乎不变 |
| `src/editor/`（grid/cursor/style 状态） | 169 | 11 | — | 中等，非 fork 重点 |
| `src/renderer/`（整个目录） | 413 | 16 | — | 目录整体活跃，但集中在字体/光标/进度条等具体功能，不在 `opengl.rs` |
| `src/renderer/rendered_window.rs` | 91 | 6 | — | 中低 |
| `src/renderer/grid_renderer.rs` | 49 | **2** | 2026-03-10 | 低——好消息，这是"不该碰"的文件之一，它本身也确实不常变 |
| `src/renderer/opengl.rs`（GL surface 创建） | 17 | **1** | 2026-03-10 | 低——但这恰好是 §6.5 里"必须动 renderer 内部"的那个文件；改动量虽小，仍需人工核对每次改动是否与我们的 patch 冲突 |
| `src/renderer/fonts/`、`src/renderer/cursor_renderer/` | 94 / 104 | — | — | 不该碰，且看起来确实经常单独演进（字体渲染的 bug 修复很频繁），如果不小心碰了会明显增加合并成本 |
| `src/window/mouse_manager.rs` | 38 | 2 | 2026-04-20 | 低，但需要改签名（见 §6.4/§6.5） |
| `src/window/keyboard_manager.rs` | 49 | 3 | 2026-04-20 | 低，需要改签名 |
| `src/window/mod.rs` | 113 | — | — | 含 `create_window`/`build_window_config`，中等 |
| `src/window/application.rs` | 20 | — | — | 含事件循环所有权（见上一节），改动少但性质敏感 |
| `src/units.rs` | 6 | — | — | 几乎不变，且不需要改——好消息 |
| `src/dimensions.rs` | 8 | — | — | 几乎不变 |

解读：**风险不是均匀分布的**。真正需要改的文件里，`grid_renderer.rs`/`opengl.rs`/`mouse_manager.rs`/`keyboard_manager.rs`/`units.rs` 本身 upstream 改动都很少（近 6 个月 1-3 次提交），这部分改动"扎根"以后应该比较容易长期维持。**唯一的例外、也是最大的风险点是 `window_wrapper.rs`**：这个文件近 6 个月有 23 次提交，而且正是我们计划大改的文件（`RouteState`/`saved_inner_size`/`get_grid_size_from_window`/`get_content_pixel_rect_from_window`/`preprocess_window_input`/多窗口路由 `routes`/`route_cores` 全在这一个 2870 行文件里）。这意味着：即使我们把改动"逻辑上"限制在 geometry/input-adapter/surface-host 范围内，它们物理上仍然会散布在 upstream 改动最频繁的一个文件的很多不同位置，rebase 冲突大概率会持续出现，且冲突大小和"upstream 这半年高频改这个文件"直接相关。缓解方向：尽早把 `window_wrapper.rs` 里我们要改的那几个函数（`get_grid_size_from_window`/`get_content_pixel_rect_from_window`/`update_grid_size_from_window`/`preprocess_window_input`/`draw_frame`）抽成独立的小函数或者独立文件（哪怕只是同一 crate 内的新 module），减少和 upstream 高频改动的那些函数（多窗口/macOS tab 切换/进度条相关，全在同一文件里但和我们无关）产生逐行 diff 冲突的概率。

---

## 结论：§13 "改动集中在 surface/geometry/input-adapter/renderer-viewport，不碰 font/grid/animation/bridge 协议" 这个计划现实吗？

**大方向现实，但有一处必须修正预期，一处结构性前提需要提前验证：**

1. **"不碰 font/grid/animation/bridge 协议"这部分完全站得住**：`grid_renderer.rs`、`fonts/`、`cursor_renderer/`、`animation_utils.rs`、`editor/`、`bridge/session.rs`（握手协议本身）在我们的改动范围之外，且这些文件本身 upstream 改动频率对我们改动范围没有影响——可以放心不碰。
2. **"renderer-viewport"这个词需要重新定义边界**：draw_frame 的 clear() 那一行、`content_region` 的来源，改动确实小；但 §6.5 host 抽象牵扯到 `renderer/opengl.rs` 里 GL context/surface **创建**逻辑本身，这是"renderer 内部"，不是外围胶水——feasibility 文档把它归为"不要碰的 renderer"有点过于乐观，应该明确写成"renderer 目录下允许碰 GL-surface-bootstrap 这一层（`opengl.rs`/`metal.rs`/`d3d.rs`），但不碰 paint 这一层（`grid_renderer.rs`/`rendered_window.rs`/`fonts/`/`cursor_renderer/`）"，两者都在 `src/renderer/` 目录下，笼统地说"不碰 renderer"会造成误判。
3. **真正不可避免的是事件循环所有权**（上一节）：这不是"哪个文件该不该改"的问题，而是"winit 的 `EventLoop::run_app` 模式和 GTK 的 `GMainLoop` 谁来当家"的结构性冲突，目前看不到 upstream 有现成的"嵌入模式"可用。这条如果走不通，§6.2-6.5 的所有局部改动都无意义——**这应该是 Phase 0 Surface PoC 第一个要验证、而不是最后才发现的东西**，建议提前到 P1 之前做一个"能不能不调用 `event_loop.run_app`、纯手动驱动 `WinitWindowWrapper`"的最小 spike，而不是等到正式 fork 之后才撞上。
4. `window_wrapper.rs` 的 upstream 高频修改（近 6 个月 23 次提交）意味着即使改动范围本身很小，rebase 冲突频率也会明显高于 `grid_renderer.rs`/`opengl.rs`这类"该改的地方本身很少变"的文件——这是可以接受的维护成本，但不应该被低估为"零冲突"。

**结论：Go，但需要把 P10 的"最小化 fork 范围"这条 Go 条件拆成两半分别验证**——(a) 局部几何/输入/GL-surface 改动（现实，风险可控，且有具体的函数/行号可以对照），(b) 事件循环所有权的可行性（未验证，建议作为独立的、更早的 spike，其结论会决定整个方案是否成立，而不只是决定合并成本高低）。
