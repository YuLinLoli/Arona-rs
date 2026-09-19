# Arona 插件开发指南

本项目已拆成「框架 + 插件」两部分。框架只提供 **OneBot 连接、管理面板(GUI)、群授权/黑名单、命令分发骨架** 与启动/关闭的生命周期编排；具体功能（抽卡、活动日历、攻略、塔罗……）一律以**插件**形式实现并插入框架。

## 1. 目录与版本

```
Cargo.toml                 # 虚拟 workspace（resolver=2）
plugins.toml               # 功能插件清单：host 编译期据此静态注册插件（build.rs 读取）
crates/arona/              # 框架库 crate：name=arona, version=1.0.0（发布版从 1.0.0 起）
crates/arona-host/         # 宿主可执行：name=arona-host, version=1.0.0, 产物 bin=arona-rs
plugins/bluearchive/       # 碧蓝档案功能插件：name=bluearchive-plugin, version=0.3.4, lib=bluearchive_plugin
```

版本约定：框架与 host 同为 `1.0.0`；功能插件 `BluearchivePlugin` 的版本号（`0.3.4`）接替拆分前本项目的版本号，随插件功能演进单独递增。

## 2. 职责边界（依赖方向）

**框架 `arona` 绝不依赖任何插件**，插件单向依赖框架：

```
arona-host  ──depends──▶  arona (框架)
     │                        ▲
     └──depends──▶ bluearchive-plugin
                            │
                            └──depends──▶ arona (框架)
```

框架反向调用插件只能通过 `arona::plugin` 提供的接口（注册表 + 分发器槽位 + 功能开关登记）
与 `arona::config::arona::ConfigSection`（插件配置区渲染器），实现**依赖倒置**。任何“框架里 `use bluearchive_plugin::…`”都是设计违规。

## 3. 生命周期（框架 `arona::run` → `run_bot` 的真实顺序）

宿主在调用 `arona::run(args)` **之前**注册插件（`register_plugins()` 由 `crates/arona-host/build.rs`
依据 `plugins.toml` 生成，见 §8）；`run` 内部按序驱动各阶段：

1. `plugin::install_all()` —— **早于 `arona.yml` 加载**，插件在此登记功能开关（`register_feature`）、
   自持有的配置区（`register_section`）与服务，使生成的配置模板认得这些顶层键并带完整功能清单。
2. 加载 `arona.yml`（业务配置，含热更新）与 `onebot.yml`（协议配置）。
3. `plugin::configure_all(&PluginContext)` —— 插件构建自己的 `SimpleCommandDispatcher` 并 `plugin::set_dispatcher(..)`。
4. `plugin::start_all()` —— 打开数据库、拉起数据预热与定时推送后台任务。
5. 框架用 `plugin::dispatcher()`（缺省空表兜底）装配 `StandaloneBusinessHandler` 并启动 OneBot 连接。
6. 运行中：`arona.yml` 每次热重载/写回后回调 `plugin::notify_config_reloaded()`（→ 各插件 `on_config_reload`）。
   首次加载不通知（那时插件 `start()` 会按配置建任务），插件自己在回调里判断是否需要重建任务。
7. 退出（Ctrl+C / GUI 关窗）：`application.stop()` → `plugin::stop_all()` → `quartz::pause_all()`。

## 4. 插件接口

实现 `arona::plugin::AronaPlugin`，方法都有默认实现，只需覆写关心的阶段：

```rust
pub trait AronaPlugin: Send + Sync + 'static {
    fn meta(&self) -> PluginMeta;                                   // 必填
    fn install(&self) -> Result<(), String> { .. }                  // 登记功能开关/服务
    fn configure(&self, ctx: &PluginContext) -> Result<(), String>  // 构建并 set_dispatcher
    fn start(&self) { .. }                                          // DB/预热/定时任务
    fn on_config_reload(&self) { .. }                               // arona.yml 热重载回调
    fn stop(&self) { .. }                                           // 关 DB/释放资源
}
```

### 硬性规范：插件必须有 name 与 version

`meta()` 返回的 `PluginMeta { name, version, description }` 里 **`name` 与 `version` 为强制项**（`description` 可留空串）。建议 `version` 直接取 `env!("CARGO_PKG_VERSION")`，与 crate 版本保持一致：

```rust
fn meta(&self) -> PluginMeta {
    PluginMeta {
        name: "BluearchivePlugin",
        version: env!("CARGO_PKG_VERSION"),
        description: "碧蓝档案功能插件",
    }
}
```

框架用 `plugin::metas()` 汇总所有已注册插件的元信息（GUI「关于」页 / 诊断）。

## 5. 群授权与功能开关接口（框架留作接口、插件消费）

权限骨架留在框架，插件通过下面三个接口接入：

- **功能清单**：`arona::admin::register_feature(Feature { key, name, description })`（等价于 `arona::runtime::config::register_feature`）。GUI「功能开关」页与 `arona.yml` 模板注释据此生成。在 `install()` 阶段调用。
- **命令归属**：注册命令时 `CommandRegistration::new(..).with_feature("gacha")` 绑定某功能 key；某群关闭该功能时，分发器把该命令视为“未匹配”，交给兜底逻辑。功能 key 用 `&'static str`，与 `register_feature` 的 `key` 对齐。
- **管理员/黑名单**：命令上下文 `CommandContext.is_admin`，以及 `runtime::config::is_manager/is_blacklisted` 由框架在分发前统一判定，插件无需自己实现群授权。

`Feature`、`CommandContext`、`SimpleCommandDispatcher` 等类型都在框架 `arona` 中导出（`arona::runtime::config::Feature`、`arona::runtime::dispatcher::*`）。

## 6. 注册命令（插件的典型做法）

在 `configure()` 里构造分发器，把命令表交给框架：

```rust
use arona::runtime::dispatcher::{
    CommandRegistration, SimpleCommandDispatcher, fallback, handler,
};

fn configure(&self, ctx: &PluginContext) -> Result<(), String> {
    let registrations = vec![
        CommandRegistration::new(
            vec!["/单抽".into(), "gacha_one".into()],
            "单抽一次, 可选服务器",
            handler(|context, arguments| async move { /* 返回 Some(OutgoingMessage) 或 None */ }),
        )
        .with_feature("gacha"),
    ];
    // 未匹配任何命令时的兜底（例如把纯数字回复解析成上一次模糊建议的选项）
    let fb = fallback(|context| async move { /* .. */ });
    let dispatcher = std::sync::Arc::new(SimpleCommandDispatcher::new(registrations, Some(fb)));
    arona::plugin::set_dispatcher(dispatcher);
    Ok(())
}
```

`configure()` 里可拿到 `ctx.onebot_config`（协议配置快照，构建分发器/需要 self_id 时用）与 `ctx.test_notify`（命令行是否带 `--test-notify`）。

## 7. 插件自己的配置区（arona.yml 顶层键）

框架只认 `groups` / `managers` / `global_blacklist` / `group_settings` 四个顶层键；
插件的业务配置以**原样 YAML 片段**存在 `AronaConfig.sections` 里，框架不理解内容，
靠插件登记的 `ConfigSection` 渲染器完成“认键 + 生成带注释模板”。

```rust
use arona::config::arona::ConfigSection;
use serde_yaml::Value;

/// notify 配置区渲染器：一个 unit struct 即可，无状态
struct NotifySection;

impl ConfigSection for NotifySection {
    fn key(&self) -> &'static str {
        "notify"                                   // arona.yml 的顶层键名
    }
    fn default_value(&self) -> Value {
        // 文件里缺这个键时，模板用这里渲染
        serde_yaml::to_value(NotifyConfig::default()).unwrap_or(Value::Null)
    }
    fn render(&self, value: &Value) -> String {
        // 通常先把 value 反序列化成自己的强类型配置（顺带过滤未知子键），再手写带注释片段
        let config = serde_yaml::from_value::<NotifyConfig>(value.clone()).unwrap_or_default();
        let mut out = String::new();
        out.push_str("# ==================== 每日推送 ====================\n");
        out.push_str("notify:\n");
        out.push_str(&format!("  # 是否启用每日推送\n  enable: {}\n", config.enable));
        out
        // 片段以 "key:" 开头、末尾不留空行
    }
    fn after_key(&self) -> Option<&'static str> {
        Some("managers")       // 写在 managers 下面；不实现则排到文件末尾
    }
}
```

`install()` 阶段登记（登记顺序 = 同一锚点内的先后顺序）：

```rust
fn install(&self) -> Result<(), String> {
    arona::config::arona::register_section(std::sync::Arc::new(NotifySection));
    Ok(())
}
```

读写自己的配置用框架的通用槽位（`arona::config::standalone`）：

```rust
/// 读：拿到原样 YAML 值，反序列化成强类型，缺失/格式错回退默认值
pub fn notify() -> NotifyConfig {
    arona::config::standalone::section_value("notify")
        .and_then(|value| serde_yaml::from_value::<NotifyConfig>(value).ok())
        .unwrap_or_default()
}

/// 写：序列化后写回，框架负责落盘 arona.yml 并热应用（会触发一次 on_config_reload）
pub fn set_notify(config: &NotifyConfig) -> Result<(), String> {
    let value = serde_yaml::to_value(config).map_err(|err| err.to_string())?;
    arona::config::standalone::set_section("notify", value)
}
```

约束：
- 键名要和插件功能对得上，且**不同插件之间不能撞 key**（`register_section` 撞名时保留先登记的）。
- `after_key` 只决定「落在框架四个键中的哪一个后面」：`managers` / `global_blacklist` /
  `group_settings` / `None`(文件末尾)，同一位置再按登记顺序排。安装包的 `defaults/arona.yml`
  里**只有框架那四项**，插件配置区由插件首次运行时补到声明的位置——插件加了新字段也会自动补齐，
  用户已有的值一律保留。
- `set_section` 之后框架必定回调 `on_config_reload`，需要即时生效的定时任务在那里重建。
- GUI/`/config` 只编辑框架自己的四个键；插件配置区由插件的指令或手改 YAML 维护。

## 8. 新增一个插件

1. 在 workspace 里建 crate，`Cargo.toml` 依赖框架（**务必 `default-features = false`**，见下节）：
   ```toml
   [dependencies]
   arona = { path = "../../crates/arona", version = "1.0.0", default-features = false }
   ```
2. 实现 `AronaPlugin`（`meta` 必填 name+version），并提供无参 `::new()`（注册代码要调它）。
3. 在仓库根目录 `plugins.toml` 的数组里加一行类型路径：
   ```toml
   plugins = [
       "bluearchive_plugin::BluearchivePlugin",
       "your_plugin::YourPlugin",
   ]
   ```
4. 在 host `crates/arona-host/Cargo.toml` 的 `[dependencies]` 里加上该 crate
   （同样 `default-features = false`）。
5. 把新 crate 加进根 `Cargo.toml` 的 `members`。

`crates/arona-host/build.rs` 读 `plugins.toml` 生成 `OUT_DIR/plugins.rs`（一个
`register_plugins()`，按清单顺序 `arona::plugin::register(..)`），`main.rs` 用 `include!`
引回并在 `arona::run(..)` 之前调用它 —— **不要再去 main.rs 里硬编码注册**。改了 `plugins.toml`
无需改任何 Rust 代码，build.rs 已 `rerun-if-changed` 该文件。

## 9. feature 统一陷阱：GUI 只在 host 打开

`arona` 的 `default = ["gui"]` 会拉进 `eframe`/`wgpu`。插件与 host 都以 `default-features = false` 依赖 `arona`，**只有 host 通过自身的 `gui = ["arona/gui"]` feature 打开 GUI**。这样插件不参与 GUI 编译、也不改变框架 GUI 开关的归属，避免 Cargo feature 统一把 `gui` 意外扩散。host 的 `default = ["gui"]` 决定了产物是否含管理面板。

## 10. 开发与调试

```bash
cargo run -p arona-host                 # 默认打开管理面板 GUI
cargo run -p arona-host -- --nogui      # 纯命令行模式（黑窗口）
cargo run -p arona-host -- --test-notify# 20 秒后跑一次每日推送，便于联调
cargo build -p arona-host --no-default-features   # 精简命令行版
```

或用 `.cargo/config.toml` 里的别名：`cargo build-release` / `cargo build-nogui` / `cargo gui` /
`cargo run-nogui` / `cargo dist`（整理交付产物）/ `cargo installer`（打安装包）/ `cargo smoke-*`（各链路自检）。

因为插件与框架在同一 workspace，断点可直接打在插件源码里。

GUI 相关的落地细节（改动前务必先读）：
- Windows 下含 GUI 的构建带 `#![windows_subsystem = "windows"]`，**不弹控制台**；命令行模式由
  `runtime/console.rs` 接回上级终端或另建控制台。核对产物可看 PE 头 subsystem：GUI 版应为 2，
  `--no-default-features` 版应为 3。
- 依赖全部内置，目标机不装任何东西也能启动：`+crt-static` 静态链接 MSVC 运行库，
  `d3dcompiler_47.dll` / `opengl32.dll` 走 `/DELAYLOAD`，wgpu 用 **DynamicDxc**（按需 LoadLibrary
  exe 同级的 `dxcompiler.dll` / `dxil.dll`，由 `scripts/fetch-dxc.ps1` 拉取、安装包与 zip 一并分发）
  或系统 FXC，**绝不用静态 DXC**（静态 DXC 会引用 ATL 的 `_AtlBaseModule`，逼人装 VS 的
  「ATL/MFC」组件——**不要走这条路**）。
- 渲染后端按候选阶梯**从高到低**逐个降级：`glow`(硬件 OpenGL) → `wgpu(DX12/Vulkan+随包 DXC)` →
  `wgpu(全部后端+DXC)` → `wgpu(+FXC)` → `glow(软件 OpenGL llvmpipe，即随包的 softgl\)`，
  exe 同级没有 DXC 两个 dll 时自动跳过 DXC 两档。服务器/无显卡环境靠这个兜底。
- 两层兜底都要留着：进程内的**卡死看门狗**（后端超时不出窗口就换下一个后端另起进程）+
  `catch_unwind`；以及**进程级的闪退看门狗**（父进程盯子进程退出码，被驱动直接干掉时改用 CPU
  软件渲染重启，用 `ARONA_GUI_GUARDED` 防止自己盯自己）。只留前者不够——硬闪退时什么都写不出来。
- 窗口尺寸是「逻辑点」，由 winit 按屏幕缩放比换算；前提是 `assets/arona.manifest` 声明了
  PerMonitorV2 DPI 感知，别删那几行。
