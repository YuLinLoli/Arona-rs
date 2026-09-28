# arona-rs

Arona 框架库 crate（OneBot v11 连接、管理面板、群授权与黑名单、插件装载与命令分发骨架）。
具体玩法一律以**插件**形式实现：插件是各自独立的 cdylib crate，编译出 dll 后由用户放进运行目录的
`plugins/`，框架启动时扫描、握手、装载。

包名 `arona-rs` 只是为了在 crates.io 上占得到名字（`arona` 早年被别的项目占了）；
**库名仍是 `arona`**，所以源码里照旧 `use arona::...`，依赖写一行重命名即可：

```toml
[dependencies]
# 务必 default-features = false：gui 特性只在宿主那边打开，插件不该把 eframe 拖进 dll
arona = { package = "arona-rs", version = "1.1.0", default-features = false }
```

- 宿主可执行文件在 `crates/arona-host`（产物 `arona-rs.exe`）。
- 写插件请先读 **`PLUGIN_DEVELOPMENT.md`**（生命周期、命令与事件钩子、类型化配置、
  以及 §22 那份动态装载的工具链硬性要求——不符就拒载）。
- 仓库内 `plugins/hello` 是一份可整份抄走的最小示例。

框架本体与插件之间只有运行期的 dll 这一层关系：本 crate 不链接任何插件，
也不会在构建产物里带上插件。

许可证：AGPL-3.0-only。
