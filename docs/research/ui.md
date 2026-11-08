# Native interface: measured comparison (decision D17)

The owner's decision D17 (2026-11-07): no Tauri and no WebView, because WebView2 costs about 140 MB of private memory (dependencies.md 3.1: host process 5.1 to 5.2 MB private, plus 6 WebView2 processes with 136 to 147 MB private). This file compares the ways to build a native interface so the owner can pick one. Nothing here is decided until the owner picks; D4, D14, SPEC and CONTRACTS stay as they are until then.

Marks as in the SPEC: **UNPROVEN** = neither measured here nor settled by a primary page; **GUESS** = an estimate or inference to confirm in its stage. Every version, date and license below was read on **2026-11-07** from the crates.io API (`https://crates.io/api/v1/crates/<name>`), the GitHub API (`https://api.github.com/repos/<owner>/<repo>`) or the page cited next to it.

## How the numbers were made

- One throwaway Cargo project per candidate under the session's scratch folder (`.../scratchpad/uicheck`), Rust 1.99.1 stable, this PC (Core Ultra 9 275HX, 24 cores, Intel iGPU + RTX 5070 Laptop, Windows 11, display scaled 150%). Crates fetched by cargo; nothing installed.
- **The sample** is the same screen in each: a 900 × 600 window, a 272 px sidebar with a caption and 4 device rows (dot, name, status, the selected one in a rounded panel), a title, a status line with a click counter, a rounded bordered panel with three buttons (one primary), a switch with its label, and a footer line. Clicking a row selects it; buttons count; the switch toggles. Colours are the design tokens of `docs/design/Main.dc.html`. System font (Segoe UI) for Win32 and WinUI; each toolkit's default font elsewhere (Instrument Sans was not loaded in any sample).
- **Release profile** = the shipping profile proposed in dependencies.md 5.2 (`opt-level 3`, fat LTO, 1 codegen unit, `panic = "abort"`, line tables), the same one the Tauri measurement used.
- **Build** = wall time of `cargo build --release` in an empty `target/`, crates already downloaded, all 24 cores, one build at a time.
- **Crates** = unique `name version` pairs in `cargo tree --target <triple> -e normal --prefix none --no-dedupe`, the sample itself included (same method as dependencies.md). Windows = `x86_64-pc-windows-msvc`, macOS = `aarch64-apple-darwin` (resolution only, nothing built for macOS).
- **Memory and CPU** (`scripts/measure.ps1` in the scratch folder): start the exe, wait 10 s, read the private working set (`Win32_PerfRawData_PerfProc_Process.WorkingSetPrivate`, Task Manager's "Memory" column), private bytes and working set, thread count and the GPU driver DLLs loaded; then read `TotalProcessorTime` over the next 30 s. 1% of the 24 cores = 7 200 ms per 30 s. Every window was closed with `CloseMainWindow` and every process had exited (checked with `tasklist`).
- Limits: the windows were started by a background shell, so they were open but not the focused foreground window during the idle reading; idle CPU with the window focused and the pointer over it is **UNPROVEN**. GPU memory (dedicated VRAM) was not read (**UNPROVEN**). Two runs per default build, one or two per variant.
- Screenshots of each sample (taken with `PrintWindow`, the sample's window only) are in `.../scratchpad/uicheck/shots/`.

## Measured results (this PC)

| Candidate (renderer) | Private WS after 10 s | Private bytes | Idle CPU over 30 s | Threads | Release exe | Clean build | Crates Win / Mac |
|---|---|---|---|---|---|---|---|
| **Win32 + Direct2D/DirectWrite** (windows-rs 0.62.2, `ID2D1HwndRenderTarget`) | **28.4 / 27.4 MB** | 41 to 42 MB | **0 ms / 0 ms** | 5 | **129 024 B** | **14.6 s** | **16** / n.a. (Windows only) |
| Win32 + D2D + AccessKit adapter (tree only) | not built | | | | | | 23 / n.a. |
| **WinUI 3** through `windows-reactor` 0.100.0 (preview) | 55.2 / 52.0 MB | 67.5 to 70.7 MB | 0 ms / 0 ms | 19 | 1 711 616 B (+ the Windows App Runtime 2 framework package, installed separately) | 43 s | 13 / n.a. |
| **Slint** 1.18.1, default (winit + FemtoVG on OpenGL) | 28.1 / 26.7 MB | 41 to 43 MB | 94 / 62 ms (≤ 0.013%) | 12 | 9 061 376 B | 119.6 s | 238 / 245 |
| Slint, software renderer (`SLINT_BACKEND=winit-software`) | 7.6 / 7.5 MB | 8.2 to 8.4 MB | 0 ms / 0 ms | 10 | same exe | | |
| **iced** 0.14.0, default (wgpu) | 154.4 / 130.2 MB | 253 to 277 MB | 219 / 125 ms (≤ 0.03%) | 60 | 7 345 664 B | 123.2 s | 154 / 150 |
| iced, wgpu with `WGPU_BACKEND=dx12 WGPU_POWER_PREF=low` | 55.1 MB | 109.7 MB | 0 ms (15 s) | 36 | same exe | | |
| iced, tiny-skia software (`ICED_BACKEND=tiny-skia`) | 9.2 / 9.1 MB | 10.5 MB | 0 ms (15 s) / 0 ms | 32 | same exe | | |
| **egui/eframe** 0.37.0, default (wgpu) | 148.2 / 124.7 MB | 453 to 481 MB | 109 / 109 ms (0.015%) | 41 | 10 155 008 B | 150.9 s | 174 / 176 |
| egui, wgpu with the same two variables | 302.8 MB | 395 MB | 0 ms (15 s) | 22 | same exe | | |
| egui/eframe 0.37.0 with `glow` (OpenGL), `default-features = false` | 31.3 / 30.4 MB | 44.5 to 46 MB | 94 / 78 ms | 12 | 4 957 184 B | 87 s | 108 / 121 |
| Reference: Tauri 2 minimal (dependencies.md 3.1) | 5.1 MB + 136 to 147 MB WebView2 | | ≤ 63 ms / 10 s | | 3 770 880 B | 129 s | 203 / 203 |

Resolution only (not built, not run): gpui 0.2.2: 419 / 430 crates; Freya 0.5.0: 204 / 203; Xilem 0.4.0: 173 / 156; Makepad (`makepad-widgets` 1.0.0): 93 / 80; `objc2-app-kit` 0.3.2 with default features: 16 on macOS; `accesskit_macos` 0.27.2: 28 on macOS.

What the numbers say:

- **wgpu is the memory problem, not the toolkit.** Both wgpu builds (iced, egui) loaded the Intel *and* the NVIDIA user-mode drivers for D3D11, D3D12, Vulkan and OpenGL (`nvwgf2umx`, `nvoglv64`, `vulkan-1`, `igvk64`... in the module list): wgpu enumerates every adapter on every backend, and iced 0.14 changed its default to `PowerPreference::HighPerformance` (iced 0.14.0 release notes, https://github.com/iced-rs/iced/releases/tag/0.14.0). That is 125 to 155 MB private, the same order as WebView2. The OpenGL and Direct2D builds loaded only the Intel driver and sat at 27 to 31 MB. Pinning wgpu to one backend and the low-power adapter cut iced to 55 MB, but the same variables made egui worse (303 MB); egui's code-level adapter choice was not tried (**UNPROVEN**).
- **Software renderers are the floor** (7.5 to 9.2 MB) but paint the whole UI on the CPU; for a mostly static shell that is cheap, for the viewer toolbar over 60 fps video it is not the path (the video never goes through them anyway).
- **No candidate redraws continuously at idle.** egui documents "1-2 ms per frame"; at 60 fps that would be 1.8 to 3.6 s of CPU per 30 s, and the worst measured was 0.22 s. Win32/D2D, WinUI and both software renderers woke up zero times; the GL/wgpu ones spent 62 to 219 ms per 30 s, at most 0.03% of the 24 cores (GUESS: swap-chain or event-loop housekeeping). The UI is not part of B6 either way.
- **Size and build time:** the hand-written Win32 shell is 129 KB and builds in 15 s with 16 crates; every Rust toolkit is 5 to 10 MB, 87 to 151 s and 108 to 245 crates. WinUI is small in crates but needs the Windows App Runtime 2 installed (or shipped beside the exe through `windows-reactor-setup`).
- **Default look:** in the screenshots Slint, iced, egui and WinUI all followed the system dark mode (dark title bar, dark widget palette) without code; the Win32 sample kept a light title bar (it needs `DwmSetWindowAttribute(DWMWA_USE_IMMERSIVE_DARK_MODE)` and a theme listener by hand). Mixing hard-coded light colours with a dark default palette made some Slint button labels and iced texts unreadable in the sample: with these toolkits the design tokens must drive every widget style, both themes.
- **Text at 150%:** DirectWrite (Win32, WinUI) gave the crispest Segoe UI; Slint/FemtoVG and iced/cosmic-text were clean but slightly softer; egui's bundled font rendered thin and light. At 100% scale, where hinting and subpixel rendering matter more, nothing was compared (**UNPROVEN**).

## The candidates

### A1. Windows: Win32 + DirectComposition + Direct2D/DirectWrite, custom-drawn (windows-rs)

- **License:** `windows` 0.62.2 (2025-10-06) MIT OR Apache-2.0; repository pushed 2026-11-07. Fits.
- **Maturity:** Microsoft's own projection, already in D4/D14 for every platform crate. Win32, DComp, D2D and DirectWrite are stable OS APIs.
- **Accessibility:** none by itself: a custom-drawn window is a blank to Narrator until it implements UI Automation. AccessKit's Windows adapter (`accesskit_windows` 0.35.2, 2026-10-25, MIT OR Apache-2.0) "implements UI Automation" (https://github.com/AccessKit/accesskit README); the adapters "don't yet support all types of UI elements or all of the properties in the schema" and no rich text. It adds 7 crates to the 16 (23 measured). Cost: the shell must build and update an AccessKit tree for every screen (GUESS: a few hundred lines once, plus a role/label per control).
- **IME and focus:** custom-drawn text fields need IMM32/TSF by hand. A cheaper hybrid: the few text inputs (device name, manual address, chat line) as native `EDIT` child controls (IME and UIA for free, styling limited to font, colours and a custom-drawn border). Keyboard focus order is the shell's own code. The terminal panel needs its own renderer under any non-WebView option (see "Shared consequences").
- **HiDPI:** per-monitor v2 is one call (`SetProcessDpiAwarenessContext`) plus `WM_DPICHANGED`; done in the sample (draws in DIPs, measured at 150%).
- **Dark mode:** by hand: read `UISettings`/registry, listen for the change, DWM attribute for the title bar.
- **Styling to the design:** complete freedom: rounded rectangles, borders, the toolbar's translucent dark panel and its shadow (through a D2D effect; not drawn in the sample, not read on a page today: **UNPROVEN**), any font. Instrument Sans is OFL 1.1 and ships as a variable font (`InstrumentSans[wdth,wght].ttf`, https://github.com/google/fonts/tree/main/ofl/instrumentsans); DirectWrite loads private font files through a custom font collection; variable-axis weights there are **UNPROVEN** in this sample.
- **Text quality:** DirectWrite, the system's own (best measured here).
- **Video:** the documented zero-copy path. The Media Foundation decoder writes NV12 into a D3D11 texture; a D3D11 video processor converts it into the back buffer of a composition swap chain on the same device; `IDCompositionVisual::SetContent` puts that swap chain into the window's visual tree ("Creates a swap chain that you can use to send Direct3D content into the DirectComposition API", https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgifactory2-createswapchainforcomposition). The floating dark toolbar is a second visual above it, drawn by D2D: one window, no child HWND, no airspace problem. Frame time through this path is **UNPROVEN** until stage 15.
- **Raw keys:** `WH_KEYBOARD_LL` is independent of the UI: "the thread that installed the hook must have a message loop"; "it should run the hooks on a dedicated thread"; a hook that misses `LowLevelHooksTimeout` (at most 1 000 ms since Windows 10 1709) "is silently removed" (https://learn.microsoft.com/en-us/windows/win32/winmsg/lowlevelkeyboardproc, page dated 2025-07-14). Same for every candidate: C3 `InputGrabber` already runs on its own thread.
- **Idle CPU:** 0 ms in 30 s (paints only on `WM_PAINT`).

### A2. Windows: WinUI 3 from Rust

- **The old path is dead:** `microsoft/windows-app-rs` was archived on 2022-08-24 ("too tied to .NET and Visual Studio").
- **The new path is a preview:** `windows-reactor` (in the windows-rs repository, `crates/libs/reactor`), 0.100.0 published 2026-09-03, MIT OR Apache-2.0: "Build native WinUI 3 applications with Rust components, state, and typed messages. Reactor is a preview; public APIs may change" (https://github.com/microsoft/windows-rs/tree/master/crates/libs/reactor, readme on master already says 0.101). It needs the **Windows App Runtime 2** framework package (`Microsoft.WindowsAppRuntime.2_8wekyb3d8bbwe`, read in its `bootstrap.rs`; it shows an install dialog when missing) or a self-contained build. This PC has 2.6.5.0 installed, so the sample ran.
- **Measured:** 52 to 55 MB private, 0 ms idle, 13 crates, 43 s build, 1.7 MB exe.
- **Accessibility, IME, focus, DPI, dark mode:** built into the WinUI controls (UI Automation peers, TextBox IME, Tab order, per-monitor DPI, theme following the system: seen in the screenshot).
- **Styling:** Fluent controls; matching the approved design needs lightweight resource overrides per control (`resource_overrides` exists on `Button` in 0.100) and custom templates; how far that reaches from Rust today is **UNPROVEN**.
- **Video:** `SwapChainPanel` exists in reactor 0.100 (`generated.rs`); WinUI documents it as "a hosting surface, where Microsoft DirectX swap chains provide content that can be rendered into a XAML UI", attached through `ISwapChainPanelNative::SetSwapChain`, and "can be used as a control anywhere in a visual tree" (https://learn.microsoft.com/en-us/windows/windows-app-sdk/api/winrt/microsoft.ui.xaml.controls.swapchainpanel, updated 2026-07-28). Whether reactor hands out the native panel object to call `SetSwapChain` is **UNPROVEN**.
- **Verdict:** interesting, but a preview API, a runtime the user must have, twice the memory of A1, and a Fluent look to be bent toward the design.

### A3. macOS: AppKit through objc2 (Rust, no Swift)

- **License:** `objc2` 0.6.5 (2026-10-05) MIT; `objc2-app-kit` 0.3.2 Zlib OR Apache-2.0 OR MIT. Both already approved (D4, D14: "the whole objc2 family"). 16 crates on macOS with default features.
- **Accessibility, IME, focus, dark mode, Retina:** native for standard AppKit controls (NSButton, NSTextField, NSSwitch, NSTableView); custom-drawn views need NSAccessibility by hand or `accesskit_macos` (0.27.2, 28 crates, "implements the NSAccessibility protocols").
- **Styling:** layer-backed views (`CALayer` corner radius, borders, shadows), custom fonts registered at runtime (CoreText), the dark toolbar as an `NSVisualEffectView` or a plain layer. Text: CoreText.
- **Video:** the planned path (macos.md 7c): `AVSampleBufferDisplayLayer` with `sampleBufferRenderer` (macOS 14+) and `DisplayImmediately`, fed the VideoToolbox `CVPixelBuffer` directly: zero-copy; the toolbar view sits above the layer in the same window. Raw keys: CGEventTap on its own run-loop thread (macos.md 3a, probe P9), independent of the UI.
- **Memory, CPU, build:** cannot be measured here (no Xcode, no Mac): **UNPROVEN**; GUESS: same order as A1 (20 to 40 MB) since AppKit is already mapped into every macOS app.
- **Swift instead (SwiftUI/AppKit in Swift):** better documentation and previews, but a second language, an FFI layer between the Rust core and Swift (C ABI by hand, or a generator such as UniFFI or swift-bridge, each a new dependency), and every build only on the MacBook. No measured gain over objc2 for this UI (GUESS). Not recommended.

### B1. Slint (one codebase)

- **License (the catch):** the crate is `GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0`. The Royalty-free license "permits use in proprietary desktop, mobile, and web applications ... as long as you disclose that you use Slint (for example with the AboutSlint widget or the Slint badge)"; "Use in embedded systems is excluded" (https://github.com/slint-ui/slint/blob/master/LICENSE.md). Its full text (https://slint.dev/terms-and-conditions) requires the AboutSlint widget in an About screen (or splash) **or** the Slint badge on the public download page; forbids distributing an application "that exposes the APIs ... of the Software"; standalone redistribution of Slint needs the GPLv3. Consequence for Swapdesk: the source can stay MIT OR Apache-2.0, but every distributed binary carries Slint under the royalty-free terms (attribution) or the GPLv3 (the binary as a whole then under GPL terms); forks inherit the condition, and a strict free-software packager may reject the royalty-free option (GUESS). It is not a permissive dependency like the rest of D14.
- **Maturity:** 1.18.1 (2026-09-21), 1.17.0 on 2026-06-24, repository pushed 2026-11-07, 24 k stars, a company behind it (SixtyFPS GmbH).
- **Accessibility:** AccessKit through the winit backend; `accesskit_windows` 0.34.0 and `accesskit_winit` are in the measured default tree (`accessibility` is a default feature). Properties `accessible-role` (mandatory before any other), `accessible-label`, `accessible-live-region`, actions (https://docs.slint.dev/latest/docs/slint/reference/common/). Which widgets of the std style expose full roles is **UNPROVEN**.
- **IME, focus, DPI, dark mode:** winit IME and per-monitor DPI; the std widgets followed the system dark mode in the sample.
- **Styling:** a declarative language close to the CSS of the design (`border-radius`, `border-width`, `border-color` and layouts used in the sample; drop shadows not tried: **UNPROVEN**); custom fonts by `import "font.ttf"` in a `.slint` file (https://docs.slint.dev/latest/docs/slint/guide/development/fonts/). Variable-font weights: **UNPROVEN**.
- **Text:** FemtoVG (own glyph rasteriser on OpenGL), Skia or Vello renderers optional.
- **Video:** two ways, both weaker than A1/A3. (1) The current C3 shape: a native child (HWND with its own swap chain, NSView with the sample-buffer layer) inside Slint's window through `raw-window-handle`; the child covers Slint's own drawing, so the floating toolbar must become a separate owned top-level window kept in place over the video (GUESS: workable, fiddly with moves, resizes, focus and full screen). (2) Texture import: `slint::wgpu_30` lets the app "convert an external `wgpu::Texture` into a `slint::Image`", only with the `unstable-wgpu-30` feature that "may be removed or changed in future minor releases" (https://docs.rs/slint/latest/slint/wgpu_30/index.html); getting the Media Foundation D3D11 texture into wgpu without a copy needs `create_texture_from_hal` (unsafe, `wgpu_core` feature) and a cross-API shared handle that wgpu does not document (https://docs.rs/wgpu/latest/wgpu/struct.Device.html, wgpu 30.0.1): **UNPROVEN**, and it brings wgpu back (the 125 to 155 MB problem above).
- **Measured:** 27 to 28 MB, ≤ 94 ms idle per 30 s, 238 crates, 9 MB exe, 120 s build. Software renderer 7.5 MB.

### B2. iced

- **License:** MIT. **Maturity:** 0.14.0 (2025-12-07) is the latest release, 11 months old; repository active (pushed 2026-11-01); pre-1.0, breaking changes between minors.
- **Accessibility: none shipped.** No AccessKit crate in the measured tree; the 0.14 release notes do not mention accessibility; PR #1849 "WIP: Iced accessibility" and #3111 "draft: Accesskit integration" are open, #3281 "Accessibility support" was closed without merge (last updated 2026-03-15), and issue #489 ("cannot focus most widgets ... or tab between widgets") is still open. This fails the second criterion.
- **IME:** "Input method support" landed in 0.14. Reactive rendering landed in 0.14 (idle measured ≤ 219 ms / 30 s).
- **Video:** a `shader` widget with custom wgpu pipelines; zero-copy from D3D11/VideoToolbox has the same unproven wgpu import problem as B1(2).
- **Measured:** 125 to 155 MB with wgpu (9 MB software), 154 crates, 7.3 MB, 123 s.

### B3. egui (eframe)

- **License:** MIT OR Apache-2.0. **Maturity:** 0.37.0 (2026-10-15), frequent releases, 32 k stars, pre-1.0.
- **Accessibility:** AccessKit "implements the native accessibility APIs on Windows and macOS" (https://github.com/emilk/egui README); `accesskit_windows` 0.35.2 is in the default tree.
- **Model:** immediate mode: "The main disadvantage of immediate mode is it makes layout more difficult ... a fundamental shortcoming"; styling "is not yet as powerful as say CSS" (same README). Matching the approved, carefully aligned design is the hardest here. No switch widget (the sample used a checkbox). Repaints "only ... when there is interaction ... or an animation".
- **Video:** `egui_wgpu::Renderer::register_native_texture` takes a wgpu texture that "must have the texture format `wgpu::TextureFormat::Rgba8Unorm`" (https://docs.rs/egui-wgpu/0.37.0/egui_wgpu/struct.Renderer.html): an NV12 decoder output would need a conversion pass plus the same unproven cross-API import.
- **Measured:** 125 to 148 MB with wgpu, **30 to 31 MB with glow** (108 crates, 5 MB, 87 s).

### B4. Others read, not measured

| Toolkit | Version (date) | License | Accessibility | Notes |
|---|---|---|---|---|
| gpui (Zed) | 0.2.2 (2025-10-22) on crates.io; Zed repo pushed 2026-11-07 | Apache-2.0 | AccessKit merged in Zed's repository on 2026-05-27 (PR #56065), after the last crates.io release, and still being fixed on Windows (PR #66631 open); not in the 0.2.2 tree | 419 / 430 crates; built for Zed, published irregularly; a git dependency would be needed |
| Freya | 0.5.0 (2026-11-05) | MIT | AccessKit in the tree (`accesskit_windows` 0.35.2) | Skia renderer from prebuilt binaries downloaded at build time (README credits); own reactive core since 0.4; 204 crates |
| Xilem / Masonry | 0.4.0 (2025-10-29) | Apache-2.0 | AccessKit | "An experimental Rust architecture for reactive UI" (README); Vello on wgpu (the wgpu memory issue); 173 crates |
| Makepad | `makepad-widgets` 1.0.0 (2025-05-13) | MIT OR Apache-2.0 | none found: issue #196 "On Accessibility" open | own shader UI; 93 crates |
| Floem (Lapce) | 0.2.0 (2024-11-14) | MIT | not checked | last release two years old |

None beats B1 or B3 on the criteria; gpui and Xilem would bring wgpu-class trees, Freya brings a downloaded Skia binary, Makepad has no accessibility.

## Shared consequences of dropping the WebView (every option)

- **Terminal:** xterm.js goes with the WebView. Every option needs a VT parser and a grid renderer in Rust (for example the `vte` parser, or `alacritty_terminal`; both new third-party crates for the owner's yes; not evaluated here: **UNPROVEN** size and fit). Toolkit add-ons exist (egui and iced terminal widgets) but were not read.
- **C6 is no longer an IPC API:** no `#[tauri::command]`, no JSON events, no `serde`/`serde_json` in `swapdesk-app`, no TypeScript DTO copies or JSON fixtures; the commands become plain Rust calls into a backend thread and the events a Rust enum posted to the UI thread. Message keys (pt-BR, en) are translated in Rust.
- **Dependencies leaving D4/D14:** Tauri 2 (203 crates), `tauri-cli`/NSIS bundler, xterm.js, TypeScript. The NSIS installer of F-SYS-01 needs another route (a hand-written NSIS script or WiX run as a tool in M8, **UNPROVEN** which).
- **Raw keys and the swap of keyboard modes** are untouched: `InputGrabber` stays a dedicated thread with `WH_KEYBOARD_LL` / CGEventTap; the UI only tells it when the viewer gains or loses focus (F-INPUT-04).

## Comparison

| | A: native per OS (Win32+DComp+D2D on Windows, AppKit via objc2 on macOS) | A': WinUI 3 (reactor) + AppKit | B1: Slint | B2: iced | B3: egui (glow) |
|---|---|---|---|---|---|
| Private memory, this PC | **27 to 28 MB** (Windows); Mac UNPROVEN | 52 to 55 MB | 27 to 28 MB (GL), 7.5 MB software | 125 to 155 MB (wgpu), 9 MB software | 30 to 31 MB (wgpu default: 125 to 148) |
| Idle CPU / 30 s | **0 ms** | 0 ms | 62 to 94 ms | 125 to 219 ms | 78 to 94 ms |
| Crates (Windows) | **16** (23 with AccessKit) | 13 + runtime package | 238 | 154 | 108 |
| Exe / clean build | **129 KB / 15 s** | 1.7 MB / 43 s | 9.1 MB / 120 s | 7.3 MB / 123 s | 5.0 MB / 87 s |
| License fit with MIT/Apache | yes | yes (preview crate) | conditional: attribution (royalty-free) or GPLv3 | yes | yes |
| Screen readers | AccessKit (Win, to build per screen); native AppKit (Mac) | native both | AccessKit, built in | **none** | AccessKit, built in |
| IME / focus | native EDIT for inputs, own focus order; native on Mac | native | winit IME, own focus | 0.14 IME, focus gaps (#489) | winit IME |
| Design match | full freedom, all by hand | Fluent to override | close to the CSS model | style closures, possible | hardest (immediate mode) |
| Video zero-copy + overlaid toolbar | **yes, documented on both OSs, one window** | SwapChainPanel (reactor access UNPROVEN) | child view + separate toolbar window, or unstable wgpu import (UNPROVEN) | wgpu import UNPROVEN | wgpu import UNPROVEN, RGBA only |
| Code across two OSs | **two shells** (~14 screens each, GUESS +40 to 60% UI effort over one toolkit) | two shells | one | one | one |
| Mac side provable here | no (MacBook) | no | Windows half yes | Windows half yes | Windows half yes |

## Recommendation: A, native per OS

Win32 + DirectComposition + Direct2D/DirectWrite with AccessKit on Windows; AppKit through objc2 on macOS; a shared, `unsafe`-free Rust crate for the view state, strings and design tokens.

1. **Performance first.** Tied lowest measured memory (27 to 28 MB against 140+ for WebView2 and 125 to 155 MB for the wgpu toolkits), the only GPU-drawn option with zero idle wake-ups, 16 crates instead of 108 to 245, a 129 KB exe built in 15 s. Above all, the video: the decoder's texture reaches the screen with no copy and the floating toolbar is composited above it in the same window, on both OSs, through the paths Microsoft and Apple document (DComp visual tree; `AVSampleBufferDisplayLayer` under an NSView). Every single-codebase toolkit either covers its own toolbar with a native child (airspace) or needs an unstable, undocumented cross-API texture import. B1 and B2 are on the video path of SPEC B1/B2; the shell is not.
2. **Accessibility second.** AppKit controls are accessible natively; on Windows, AccessKit gives UI Automation from a tree the shell describes (the same adapter Slint and egui use). The hybrid with native `EDIT` controls gives IME and UIA on text inputs without writing a TSF client. Cost: the Windows shell must keep its AccessKit tree complete; a Narrator pass per screen goes into stage 11's proof.
3. **Code cost third, and A's weak point.** Two shells. Mitigation: about 14 screens of mostly static layout with small state; the state machines, strings and tokens live once in Rust; each shell is drawing and event wiring. The Mac shell is written here and proven only on the MacBook (stage 23 grows).

**Second choice: B1 Slint**, if the owner prefers one codebase over the native video path: same memory, AccessKit built in, the closest styling model to the design, one shell to prove. Costs: 238 crates, a non-permissive dependency (AboutSlint in an About screen or the badge on the download page; or GPLv3 binaries), and the viewer toolbar as a separate overlay window (or an unstable wgpu import to prove in stage 15). **Not recommended:** iced (no screen reader support), egui (layout and styling far from the approved design; only acceptable with glow), WinUI 3 (preview crate, runtime dependency, 2× memory), the wgpu-based toolkits in general (memory), Swift on macOS (second language and FFI for no measured gain).

## What each choice changes

### In CONTRACTS.md

| | C3 `Renderer` | C3 `InputGrabber` | C6 |
|---|---|---|---|
| **A** | `attach` takes the shell's composition node instead of a child parent: Windows the DComp device and a parent `IDCompositionVisual` (plus the shared D3D11 device the decoder uses), macOS the parent `CALayer`; `NativeParent` becomes `NativeSurface`. Pointer, button and wheel events come from the shell's window (the video is no longer its own HWND/NSView), so the `pointer` sender moves to the shell, which normalizes over the video rectangle. `present`, `set_cursor`, `resize`, `detach` stay. | unchanged; the shell calls `start`/`stop` on focus changes | rewritten as an in-process Rust API: a `UiBackend` with the same commands as plain methods (blocking with timeouts, served by a backend thread that talks to the service pipe), a `UiEvent` enum for the events, delivered to the UI thread (`PostMessage` on Windows, the main dispatch queue on macOS). DTOs stay, without `serde`. New crates: `swapdesk-ui` (no `unsafe`: view state, strings pt-BR/en, design tokens, the AccessKit tree description), `swapdesk-shell-win`, `swapdesk-shell-mac` (`unsafe` allowed); `swapdesk-app` becomes the thin binary. |
| **B1** | shape kept (`NativeParent` from Slint's `raw-window-handle`), plus a toolbar overlay window owned by the viewer window; or, if stage 15 picks texture import, `present` hands a `wgpu::Texture` to the shell and the trait changes | unchanged | the same in-process `UiBackend`/`UiEvent` rewrite; events reach Slint through `slint::invoke_from_event_loop`; `.slint` files replace `ui/`; one `swapdesk-app` crate with no `unsafe`. |

Both: the "Tauri API" title, the `tauri::ipc::Channel` for terminal output (becomes a bounded channel into the terminal view), the JSON fixtures test and `ui/src/api/types.ts` go away; R7 ("a native render window inside the Tauri shell") is replaced by the overlay question of the chosen option.

### In the plan

| Stage | A | B1 |
|---|---|---|
| 11 UI shell on mock backend | the Windows shell only (Win32/DComp/D2D, AccessKit, EDIT inputs, both themes, pt-BR/en) on the C7 mock; done-when adds: Narrator reads every control of every screen, idle CPU 0 over 30 s, private memory measured against this file | the Slint shell for both OSs, checked on Windows here; done-when adds the Narrator pass and the About screen with AboutSlint (if royalty-free) |
| 15 Renderer spike + renderer-win | "D3D11 window vs WebCodecs" disappears; the spike is the DComp video visual under the toolbar visual, zero-copy from the MF decoder, measured against B1/B2 | spike: native child + overlay toolbar window vs `unstable-wgpu-30` texture import, by frame time and memory |
| 23 Mac viewer | grows: the whole AppKit shell (every screen, both themes, both languages) besides `AVSampleBufferDisplayLayer`, CGEventTap and TCC; GUESS: the longest stage of M5 | as planned, plus checking the Slint shell on macOS (renderer choice there: FemtoVG on OpenGL or Skia/Metal: UNPROVEN) |

D4/D14 lose Tauri, xterm.js and TypeScript in both; A adds `accesskit` + `accesskit_windows` (and possibly `accesskit_macos` for custom-drawn Mac views); B1 adds `slint`. Either way a terminal crate comes to the owner separately.

## The owner's decision, in plain words

How should Swapdesk's native interface be built?

1. **Native on each system (recommended).** Windows drawn directly with the system's graphics, Mac with the Mac's own controls. Lightest and fastest (about 28 MB, zero CPU when idle, the video goes from the decoder to the screen without copies), works with screen readers. Cost: two interfaces to write, and the Mac one is only tested on the MacBook.
2. **Slint, one interface for both.** Same memory on Windows, screen readers built in, one interface to write. Cost: about 240 libraries, and its license asks for a "Made with Slint" notice in an About screen or on the download page (or turns the binaries GPL); the viewer's floating toolbar becomes a separate little window over the video.
3. **egui or iced, one interface.** Rejected here: iced has no screen reader support; egui makes the approved design hard to reproduce; both use far more memory with their default graphics engine.
