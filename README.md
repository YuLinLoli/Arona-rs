# Arona-rs

**Arona 框架**的 Rust 实现：一个只对接 **OneBot 11** 协议、完全剥离 Mirai 的 QQ 机器人框架。
框架负责连上 OneBot 实现、认权限、分命令、管插件、开管理面板；**具体功能一律来自「功能插件」**。

- 本项目仓库：<https://github.com/YuLinLoli/Arona-rs>
- 上游原版（Kotlin + Mirai 插件）：<https://github.com/diyigemt/arona>
- 插件开发文档：[PLUGIN_DEVELOPMENT.md](PLUGIN_DEVELOPMENT.md)，最小可编译样例：[`plugins/hello/`](plugins/hello)
- 许可证：GNU AGPLv3（见文末「移植声明与许可」）

本 README 只讲框架。插件是**独立编译的 dll**，由用户放进运行目录的 `plugins/`，框架启动时扫描装载 ——
本仓库不收录任何功能插件的源码，也不链接它们；某个插件的命令、玩法、数据来源与配置项都不属于框架，
本文一律不列。想知道插件怎么写、能拿到哪些接口、对工具链有什么硬性要求，
看 [PLUGIN_DEVELOPMENT.md](PLUGIN_DEVELOPMENT.md)。

## 框架与插件的分工

```
arona-host（宿主 exe）──▶ arona（框架库）◀── 功能插件（独立 crate，编译成 dll，实现 AronaPlugin）
                                              │
                              启动前放进 plugins/，框架扫描 + ABI 握手后装载
```

| 归框架 | 归插件 |
| --- | --- |
| OneBot 11 四种连接、断线重连、心跳、token 鉴权、热重载 | 用哪些命令、命令做什么 |
| 收发的全谱消息段（文本/@/引用/图片/表情/语音/视频/文件/戳一戳/位置/XML/JSON/合并转发） | 生成什么内容、调什么数据源、画什么图 |
| 群白名单、管理员名单、全局与分群黑名单、分群功能开关 | 声明自己提供哪些功能开关 |
| 命令表分发（别名、优先级裁决、前缀匹配、类型化参数与校验、群身份门控） | 登记命令与处理器 |
| 事件总线（监听优先级、按事件子类订阅、出站前改写/拦停、panic 隔离与自动停用） | 订阅事件、改写消息 |
| 聊天记录留档（`data/arona/chatlog.db`）、过期清理内核 | 拿它做引用还原与按存储 id 撤回 |
| 插件生命周期（登记 → 装配 → 启动 → 停止）与按归属回收 | 实现四个阶段，只关自己打开的句柄 |
| 配置文件：缺失生模板、改动热重载、旧键位搬迁 | 声明自己的配置区并读写 |
| 定时任务内核（每天/间隔/单次/延时） | 排自己的任务 |
| 管理面板 GUI、日志、提权、渲染后端降级 | ——（不参与 GUI） |

框架**不依赖任何插件**，插件单向依赖框架，产物永远是同一个 exe：装了几个插件、装的是哪几个，
只取决于运行目录的 `plugins/` 里放了哪些 dll。一个插件都没有时，连上 QQ 之后收得到消息，
但没有任何命令会被响应（连 `/帮助` 都没有）。

## 能力细节

- **只做 OneBot 11**：正向 WebSocket / 反向 WebSocket / 正向 HTTP / 反向 HTTP 四种连接，
  同一类型可配多个实例，可直接对接 NapCat、Lagrange、LLOWeb 等实现端。
- **插件放在目录里就行**：没有 JVM、没有 Mirai 控制台；启动前把插件 dll 丢进运行目录的 `plugins/`，
  框架扫描、做工具链握手、装载装配，和 mirai 的 `plugins/` 用法一致。功能包的增删**不重新编译框架**。
- **聊天记录留档**：QQ 的引用只认实现端缓存里的近期消息，久了就引用不到。框架因此自己记一份
  聊天数据（图片只存原链接），供插件做引用回复与按存储 id 的撤回；保留天数与清理间隔可配。
- **异常隔离**：某个插件的命令或钩子 panic 不会带崩机器人；连续失败达到阈值由框架自动停用该插件，
  其余插件照常服务。
- **控制台彩色输出**：Windows 下走原生 `SetConsoleTextAttribute`，cmd.exe（经典 conhost）、PowerShell、
  Windows Terminal 都能正常上色，不依赖 VT；其他平台走 ANSI。检测到 `NO_COLOR` 或输出被重定向时
  自动降级为纯文本，不会漏出转义码。
- **免安装依赖**：MSVC 运行库静态链接进 exe，`d3dcompiler_47.dll` / `opengl32.dll` 走延迟加载，
  着色器编译器用随包的 DXC（没有就降回系统 FXC），目标机什么都不用装。

## 快速开始

需要自行准备一个 OneBot 11 实现（如 NapCat、Lagrange）与 QQ 账号。

### 方式一：安装包（推荐）

Releases 里的安装包**只交付框架本体**（功能插件由各自的作者分发）：

| 文件 | 内容 |
| --- | --- |
| `arona-rs-<版本号>-setup-win-x64.exe` | 框架 + 渲染运行库；安装时会建好空的 `plugins\`，功能插件 dll 由你放进去 |

1. 下载并运行，向导会先展示程序介绍与 AGPLv3 全文，同意后才能继续。
2. 默认的**完整安装**会一并装上渲染运行库：DXC 着色器编译器（`dxcompiler.dll` + `dxil.dll`，约 30 MB）
   与 CPU 软件渲染依赖 `softgl\`（约 62 MB，Mesa llvmpipe）。服务器 / 虚拟机 / 没有显卡驱动或没有
   DX12 的机器上硬件后端会全部失败甚至**静默卡住或闪退**，程序会自动沿降级链切到 CPU 渲染把管理面板
   画出来，所以服务器上也能正常开 GUI；带显卡的机器优先用硬件渲染，`softgl\` 那份平时不会被加载。
   只在正常带显卡的机器上用时，选「自定义安装」可以取消 `softgl\` 这项，省下 62 MB。
3. 安装目录（默认 `%LOCALAPPDATA%\Programs\Arona-rs`）的 `config\` 里会释放默认配置
   （`config\onebot.yml`、`config\arona.yml`）。这些文件**只在缺失时写入**，升级安装不会覆盖你改过的；
   万一缺失或损坏，程序启动时也会按内置默认值自动补齐。
4. **装上你要的功能插件**：把插件作者给的 dll 放进安装目录的 `plugins\`（装载后框架会在旁边建出
   同名目录，那是插件的身份目录，不用手改）。框架本身不带任何功能，一个插件都没有时连 `/帮助` 都没有。
5. 从开始菜单启动「Arona-rs」打开管理面板：在「OneBot 连接」页加连接并「保存并热重载」，
   在「群管理」页确认要服务的群（群白名单留空表示所有群都认）。

安装包是**按用户安装**的，全程不需要管理员权限，也不会把数据写进 `Program Files`。
快捷方式已把工作目录设为安装目录，所以配置、数据、日志就在安装目录的 `config\`、`data\`、`logs\` 下。
卸载时默认**保留**这几个目录，`plugins\`（含你放进去的插件 dll）更是不会被卸载程序碰，
会单独询问是否连数据一起删除。

#### 更新

安装程序会把安装目录写进注册表：

```
HKEY_CURRENT_USER\Software\YuLinLoli\Arona-rs      # 选「为所有用户安装」时在 HKLM 下
  InstallPath        安装目录
  Version            已安装版本
  ExeName            主程序文件名
  UninstallString    卸载程序路径
  SoftglInstalled    本次安装是否装了 CPU 软件渲染依赖（自定义安装取消勾选则为 0）
```

下载新版本的安装包直接运行即可：程序会读注册表**自动装回原安装目录**（不用重新选路径），
并**自动跳过「程序简介」与「开源协议」两页**，一路「下一步」完成更新；配置、数据库、日志、图片全部保留。
也可以用 `/UPDATE` 显式声明这是更新安装，或用 `/VERYSILENT /NORESTART` 做全静默更新（脚本 / 自动更新用）。

主程序固定叫 `arona-rs.exe`（**不带版本号**）：升级就是覆盖同名文件，快捷方式、计划任务与注册表里的
`ExeName` 都不跟着版本号变；版本号只体现在安装包名、「应用和功能」列表和面板的「关于」页里。

### 方式二：便携版

1. 从 [Releases](https://github.com/YuLinLoli/Arona-rs/releases) 下载 `arona-rs-<版本号>-win-x64.zip`
   解压（里面是 `arona-rs.exe`、`dxcompiler.dll`、`dxil.dll` 与 `softgl/`，渲染运行库全部随包；
   裸 exe 不再单独发布，需要单文件就用安装包）。解压后这些文件保持同级即可。
2. 在你想作为运行目录的位置运行一次，会在**当前工作目录**下自动生成 `config/`、`data/`、`logs/`
   与 `plugins/`（见下文「运行目录」）。
3. 把插件 dll 放进这个 `plugins/`：框架本体不带任何功能，机器人能干什么全看这里装了什么。
4. 编辑 `config/onebot.yml` 填机器人账号和连接方式，编辑 `config/arona.yml` 填服务群和管理员。
5. 重新启动。

### 管理员权限（UAC）

主程序（`arona-rs.exe`）**启动时会申请管理员权限**：机器人要写数据目录、监听端口、
需要时替换 `softgl\` 下的渲染 DLL，统一以管理员身份运行可以避免权限不足导致的
「启动没反应」「保存配置失败」「监听不了端口」等问题。

- 双击 exe / 从开始菜单启动时会弹 UAC 询问框，选「是」后程序才会真正开始运行；
  选「否」或提权失败时程序会打印一条提示并以**普通权限**继续运行，功能不受影响，
  只是可能因权限不足写不了数据目录或监听端口。
- 安装包本身仍是**按用户安装**（`PrivilegesRequired=lowest`），安装过程不需要管理员权限，
  提权只发生在主程序启动那一刻。
- 调试 / 自动化场景可以用环境变量跳过提权：`ARONA_NO_ELEVATE=1`。
- 不想弹 UAC 又有管理员需求时，也可以手动右键 exe →「以管理员身份运行」；
  程序检测到自己已经提权后不会重复申请。

## 配置

### onebot.yml

```yaml
self_id: 123456789          # 机器人 QQ 号
nickname: "Arona"

# 插件生成的本地图片（结果图、日历图…）改用 file:// 路径直传 OneBot 实现，
# 不再内嵌十几 MB 的 base64；仅当 OneBot 实现与机器人同机部署时开启
send_image_as_file: false

connections:
  # 反向 WebSocket：本程序监听，OneBot 实现（如 NapCat）连过来
  ws-reverse:
    enable: true
    host: "0.0.0.0"
    port: 6701
    url: ""
    path: "/onebot/v11"
    token: ""               # 与 OneBot 端保持一致，留空表示不校验
```

`ws-forward`（主动连 OneBot 端）、`http`、`http-reverse` 三种方式同理，按需 `enable`。
连接的**类型由 `type:` 字段决定**，键名随意（`ws-reverse-2`、`my-bot` 都行），同一类型可以配多个实例。

> `onebot.yml` 只放 `self_id` / `nickname` / `send_image_as_file` / `connections` 四项。
> 授权与开关类设置属于 `arona.yml`，写到 `onebot.yml` 会被忽略，
> 程序会在日志里逐条提示「存在无法识别的配置项」。

`send_image_as_file` 也可以在管理面板「OneBot 连接」页的「发送设置」里勾选：
勾选后立即写入 `onebot.yml` 并热生效（不需要点「保存并热重载」，也不用重启）。

### arona.yml

框架这份配置只管「谁能用、谁被停掉」和框架自己的行为：

```yaml
groups: []                  # 允许响应的群号，留空 = 所有群
managers: []                # 管理员 QQ 号
global_blacklist: []        # 全局用户黑名单：任何群/私聊都不触发（管理员不受限）
disabled_plugins: []        # 整体停用的插件 id：不装配、不接收任何事件

# 分群设置：群号 -> 关闭的功能 / 停用的插件 / 群内成员黑名单
group_settings:
  "123456789":
    disabled_features: [example]        # 功能 key 由各插件在登记阶段声明，框架只按 key 存取
    disabled_plugins: [my-plugin]
    blacklist: [10001]

# 框架行为（对位 mirai 的 MiraiInstance.new { }，改完保存即热生效）
framework:
  panic_disable_threshold: 5     # 同一插件连续 panic 多少次就自动隔离停用（0 = 只记日志、不停用）
  prefix_match_by_default: false # 未声明 with_prefix_match 的命令是否也允许最短前缀匹配

# 聊天记录缓存：插件做「引用回复」与「按存储 id 撤回」的依据（data/arona/chatlog.db）
chatlog:
  enable: true              # 关掉后不再记账，引用还原与撤回时的 id 换算一起停
  keep_days: 2              # 本地留几天的聊天消息
  purge_interval_days: 4    # 每隔几天清一次过期记录
  quote_ttl_minutes: 30     # 多少分钟内的引用仍由实现端直接挂原生引用，超过才本地还原
```

功能插件的玩法参数**不住在这里**：每个插件各有 `config/<插件>/arona.yml`，模板由框架按插件登记的
配置区生成（带逐行注释），框架把它当原样 YAML 片段保存，不认识里面的键。旧版本把插件配置写在框架
`arona.yml` 顶层的，升级时框架会自动搬进插件自己那份文件。

两份文件保存后都自动热重载；`onebot.yml` 的连接项需要在面板里点「保存并热重载」。

### 启动参数与环境变量

- `--gui`（默认）：打开管理面板（插件管理 / 群功能开关 / 群成员黑名单 / OneBot 连接配置与热重载）
- `--nogui`：不打开面板，只启动命令行(黑窗口)模式
- `--renderer=glow|wgpu|softgl`：只试指定的渲染后端（默认自动：**从高到低** —— 硬件 `glow` →
  `wgpu` + 随包 DXC → `wgpu` + 系统 FXC → 软件 OpenGL 兜底）
- `--softgl`：强制用 Mesa 软件 OpenGL(llvmpipe) 渲染（见下文「Windows Server / 虚拟机」）
- `--config=<路径>`：指定 `onebot.yml` 路径
- `--arona-config=<路径>`：指定 `arona.yml` 路径
- `ARONA_SOFTGL=1` / `ARONA_SOFTGL_DIR=<目录>`：等价开关 / 指定 softgl 目录
- `ARONA_RENDERER=glow|wgpu|softgl`：等价于 `--renderer=`（方便在服务器上固定后端）
- `ARONA_GUI_TIMEOUT=<秒>`：某个渲染后端多久没出窗口就判定为卡死并换下一个（支持小数；
  默认 wgpu 6 秒、glow/软渲染 20 秒，设 `0` 关闭看门狗）
- `ARONA_GUI_GUARDED`：由闪退看门狗自动设置，标记「这份进程已经被盯着了」，别自己盯自己
  （一般不用手动设）
- `ARONA_LOG=info|debug|trace|off`：三方库(wgpu/glutin 等)的日志级别，默认 `debug` 但只对 wgpu* 生效
- `ARONA_CONSOLE_COLOR=1|0`：强制开关控制台颜色；另识别 `NO_COLOR`、`FORCE_COLOR`、`CLICOLOR_FORCE`
- `ARONA_CONSOLE_DEBUG=1`：打印控制台颜色模式，便于排查终端差异
- `ARONA_CONSOLE_EMOJI=0`：关闭控制台 emoji
- `ARONA_NO_ELEVATE=1`：跳过启动时的管理员权限申请（调试 / 自动化测试用）
- `--arona-elevated`：提权重启时由程序自己附加的内部标记，不需要手动加（防止无限重启）

还有一个**转交给插件**的开关，框架自己不用：`--test-notify` 会原样放进 `PluginContext::test_notify`，
由插件决定要不要安排一次自检。

### 运行目录

程序按**当前工作目录**摆放运行期文件（取路径的唯一入口是 `crates/arona/src/runtime/paths.rs`，
插件不自己拼路径，用框架给的 `ctx.config_dir()` / `ctx.data_dir()` / `ctx.image_dir()`）：

```
<运行目录>/
  config/
    arona.yml              框架配置：群授权 / 管理员 / 功能开关 / 插件开关 / 框架行为 / 聊天记录缓存
    onebot.yml             OneBot 连接
    gui.txt                面板外观偏好
    <插件>/arona.yml        插件自己的配置（框架按登记表生成带注释的模板；
                            插件升级新增的配置区会自动补齐，用户改过的值不动）
    <插件>/…               插件附带的其它配置文件
  data/
    arona/chatlog.db        框架的聊天记录缓存（引用回复与按存储 id 撤回的依据，图片只存原链接）
    <插件>/                 插件数据（数据库、图片缓存、备份…）
  logs/                    按天滚动的日志
  plugins/
    <插件>.dll              功能插件本体：启动前放这里（放进一级子目录也认），框架扫描装载
    _…                      以 `_` 开头的 dll 当作第三方运行库跳过，不会被当插件装载
    <插件>/plugin.yml       插件清单（id / 名称 / 版本 / 依赖），装载时由框架按插件上报的元信息写出
```

`plugins/` 是**装载目录**，`plugins/<插件>/` 是插件的**身份目录**：框架每装载一个 dll，就按它上报的
元信息写出或刷新 `plugins/<id>/plugin.yml`，并顺手备好 `config/<id>/` 与 `data/<id>/`，
用户数据一律按 id 归到这两处。删掉 dll 只是下次启动不再装载，身份目录与用户数据不会跟着消失。

早期版本把这一切放在工作目录的 `arona-standalone/` 下：升级后首次启动会把框架认识的
`arona.yml` / `onebot.yml` / `gui.txt` / `logs/` **复制**进上面的新布局（旧目录原样保留，不删不改）。

## 管理 GUI

本地管理面板基于 `eframe`/`egui`，**默认构建即包含、启动即打开**：

```bash
cargo build --release -p arona-host    # 默认含 GUI
target/release/arona-rs                # 启动 -> 打开管理面板
target/release/arona-rs --nogui        # 只启动命令行(黑窗口)模式
target/release/arona-rs --gui          # 显式打开面板（默认行为，等价于不加参数）
```

Windows 上含 GUI 的产物使用 windows 子系统，GUI 模式不会多出控制台窗口；`--nogui`
会自动附加上级终端（cmd/PowerShell）或新建控制台，日志与颜色照常输出。

面板顶部有五个标签页（关闭窗口 = 退出整个程序，GUI 与机器人一起结束）：

**群管理**
- 左栏：搜索群号/群名，`●`/`○` 标记该群是否启用，右侧显示已关闭的功能数量与黑名单人数；可点「刷新群列表」从 OneBot 拉取。
- 右栏：勾选「机器人在此群启用」；「插件开关」按插件逐个停用（本群不响应它名下的命令与事件，在「插件管理」页整体停用的插件这里显示为灰色）。
- 「功能开关」按**提供它的插件分组**，每组默认收起，点插件名（或它左边的三角）展开/收起。组头的「全部」勾选框一次开关该插件在本群的全部功能；展开后可逐项勾选。清单只列**当前用得上**的 key：提供它的插件被整体停用时不再显示，插件只在本群被停用时整组灰掉并注明原因。
- 「群成员黑名单」：点进群后自动拉取该群全部成员（管理员/群主/成员排序），每个成员有「本群」「全局」两个勾选框，支持「只看黑名单」过滤。
- 「清空该群设置」：删除该群的 `disabled_features`、`disabled_plugins` 与 `blacklist`，恢复默认。

**插件管理**
- 卡片列的是**本次启动装载成功**的插件：`plugins/` 里的 dll 过了工具链握手才会出现在这里，
  被拒载的那些只在日志里留原因，面板不给占位。
- 每个插件一张卡片：名称 / 版本 / 启用勾选框、说明、插件 id、它提供的功能开关、订阅的 OneBot 事件钩子（出站改写单独列成「出站消息」）、配置文件与数据目录（带「打开」按钮直接调资源管理器）。
- 取消勾选即整体停用：写进 `config/arona.yml` 的 `disabled_plugins`，热生效 —— 插件 `stop()`（定时任务一并取消）、不再装配、不接收任何消息与事件；配置和数据原样保留，重新勾选就恢复。
- 只想在某个群里停用：用「群管理」页的「插件开关」。

**OneBot 连接**
- 连接列表展示全部实例（同一类型可添加多个）。「添加连接」下拉可选 `ws-reverse` / `ws` / `http` / `http-post` 等类型。
- 折叠面板可编辑 host / port / url / path / token / 心跳 / 重连等字段，以及 `self_id` / 昵称。
- 「保存并热重载」：写回 `onebot.yml` 后立即 `stop → start` 全部 WebSocket / HTTP 服务（含反向监听端口真正释放并重绑），无需重启程序。

**实时日志**
- 控制台（stdout/stderr）的一切输出都会同步进内存缓冲并在这里实时刷新（约每 0.5 秒），含启动横幅、机器人日志、群消息收发、告警/错误。
- **普通行跟随外观**：黑夜模式白字、白天模式黑字；特殊色沿用控制台规则（`[Arona]`/`[OneBot]` 绿、`WARNING`/`ERROR` 黄/红），并按背景深浅换成对应版本。
- 行首显示本机时间。工具栏：`刷新`、`清空`、`自动滚动`、`只看告警/错误`、`最近 N 行`、`打开日志目录`，以及关键字过滤。
- 缓冲上限 3000 行（只保留最新），完整历史仍按天落盘到 `logs/arona-yyyy-MM-dd.log`。

**关于**
- 显示版本号、项目仓库、鸣谢（原版作者 <https://github.com/diyigemt>）、开源协议，以及运行目录路径与快捷打开按钮。

**外观（白天 / 黑夜）**
- 右上角「外观」下拉可选 `跟随系统` / `白天模式` / `黑夜模式`，默认跟随系统；选择保存在 `config/gui.txt`，
  下次启动沿用（与机器人配置无关，删掉即恢复默认）。

### Windows Server / 虚拟机（没有可用显卡驱动）

渲染后端**从高档到低档逐个尝试**，第一个成功就停（可用 `--renderer=` 强制指定）：

| 顺序 | 后端 | 说明 |
| --- | --- | --- |
| 1 | `glow`（硬件 OpenGL） | 桌面上最快；注册表里没有 OpenGL ICD 时跳过（只有 GDI 的 1.1 必然失败） |
| 2 | `wgpu(DX12/Vulkan+DXC)` | 默认这条：显卡渲染 + **随包附带**的 DXC 编译着色器。DX12 之下还有 **WARP 软件渲染**，无显卡驱动也能画 |
| 3 | `wgpu(全部后端+DXC)` | 备用配置：PRIMARY 挑不到适配器、或机器只有 GL/Metal 后端时兜底 |
| 4 | `wgpu(DX12/Vulkan+FXC)` | 换成系统自带的 FXC（`d3dcompiler_47.dll`）编译着色器；exe 同级没有 DXC 两个 dll 时，这条就是第一档 |
| 5 | `wgpu(全部后端+FXC)` | 最后一档 wgpu |
| 6 | `glow(软件 OpenGL llvmpipe)` | 前面几步**卡死、失败或闪退**时自动换到它，改走 Mesa 的 CPU 软件渲染（安装包必带 `softgl\`） |

- **卡死看门狗**：每个后端最多等一段时间（wgpu 默认 6 秒、glow/软渲染 20 秒，`ARONA_GUI_TIMEOUT` 可调）。
  超时即判定卡死 —— 卡死在 wgpu 里的线程没法在进程内干掉，所以程序会**另起一个进程**换下一个后端
  （子进程继承管理员令牌，不会二次弹 UAC）。全部后端都不行时退回命令行模式重启，机器人继续跑。
- **闪退看门狗**：高档后端有时不是返回错误，而是**直接把进程干掉**（显卡驱动炸了，什么都来不及写）。
  所以双击启动（没带 `--renderer=` / `--softgl` / `--nogui`）且本地有 `softgl\` 时，会由一份父进程盯着
  子进程的退出码：异常退出则父进程接着跑，并把 CPU 软件渲染提到降级链最前面。
- 软件 OpenGL 兜底：把 Mesa 的软件渲染版 `opengl32.dll` 放到 exe 同级的 `softgl\` 即可，
  查找顺序为 `ARONA_SOFTGL_DIR` → exe 同级 `softgl\` → 工作目录 `softgl\` → `data\softgl\`。
  拉取：`powershell -ExecutionPolicy Bypass -File scripts/fetch-softgl.ps1`（约 62MB，不入库）。

## 构建与交付

需要 Rust 1.85 或更新版本（`edition = "2024"`）。

```bash
cargo build-release      # 发布构建(默认含管理 GUI), 产物: target/release/arona-rs[.exe]
cargo dist               # 发布构建 + 整理产物（主程序固定不带版本号；顺手清掉旧的 arona-rs-<版本号>.exe）
cargo run-release        # 构建并运行(默认打开管理 GUI)
cargo run-nogui          # 以纯命令行(黑窗口)模式运行
cargo build-nogui        # 精简命令行版: 不含 GUI(--no-default-features)
cargo installer          # 发布构建 + 打 Windows 安装包
cargo build-plugin       # 构建仓库里的示例插件: target/release/hello_plugin.dll
```

这些是 `.cargo/config.toml` 里的别名，框架侧的都固定打在 `-p arona-host` 上，也可以直接写全命令。

### 插件怎么进来

框架产物里**永远不含功能插件**，所以没有「带不带插件」两种构建。插件是独立 crate，
编译成 dll 后由用户放进运行目录的 `plugins/`，框架启动时扫描并装载：

```bash
# 1. 构建框架（一次即可，之后换插件不用重编）
cargo build-release

# 2. 构建示例插件（或任何第三方插件 crate）
cargo build-plugin                                       # → target/release/hello_plugin.dll

# 3. 把 dll 放进运行目录的 plugins/，再启动框架
mkdir -p target/release/plugins
cp target/release/hello_plugin.dll target/release/plugins/
cargo run-release
```

日志里会依次出现「已装载动态插件: <名字> <版本> (<文件名>)」与「插件已启动: …」；
工具链对不上时会打印「跳过插件 …」并说明差在哪一项。
**插件必须与框架用同一套工具链构建**（rustc 版本、目标三元组、profile、CRT 链接方式、框架 feature 一致），
其中 rustc 版本已经钉在仓库根的 `rust-toolchain.toml` 里：`cargo build` 会自动选中它，
本地和 CI 不再各凭当天的 stable。详细要求和发布前自检清单见 [PLUGIN_DEVELOPMENT.md §22](PLUGIN_DEVELOPMENT.md)。

### 安装包

由 [Inno Setup 6](https://jrsoftware.org/isinfo.php) 编译，脚本见 [`installer/arona-rs.iss`](installer/arona-rs.iss)：

- `cargo installer` 会自行查找 `ISCC.exe`（`ARONA_ISCC` 环境变量 → `PATH` → 注册表 → 常见安装路径），
  找不到时会提示先执行 `winget install --id JRSoftware.InnoSetup`。
- 包内含程序介绍（[`installer/intro.txt`](installer/intro.txt)，里面写清了「装完还缺什么」——
  要往 `plugins\` 放插件 dll）、
  AGPLv3 全文与中文译本、默认配置模板（[`installer/defaults/`](installer/defaults)，`onlyifdoesntexist` 释放）。
  `defaults/arona.yml` 里只有框架自己的项，插件的配置模板由框架在装载该插件时自动生成，
  **安装包不需要跟着插件改**。
- 约 62 MB 的 `softgl/` 与 exe **分开存储**，安装时释放到安装目录的 `softgl\`；仓库里没有 `softgl/`
  时安装包自动不含该组件（`/DHasSoftgl`）。DXC 同理（`/DHasDxc`），缺失时程序降到系统 FXC。

可选：拉取**渲染运行库**（都不入库，`cargo dist` / `cargo installer` 会自动复制到产物目录）：

```powershell
powershell -ExecutionPolicy Bypass -File scripts/fetch-dxc.ps1      # dxc\dxcompiler.dll + dxil.dll（约 30MB）
powershell -ExecutionPolicy Bypass -File scripts/fetch-softgl.ps1   # softgl\（约 62MB，Mesa llvmpipe）
```

**程序图标**：[`crates/arona-host/build.rs`](crates/arona-host/build.rs) 编译时把
[`assets/arona.ico`](assets/arona.ico) 与 [`assets/arona.manifest`](assets/arona.manifest) 编成 `.res`
链接进 Windows 产物（调用 Windows SDK 的 `rc.exe`，查找顺序为 `ARONA_RC` → `WindowsSdkDir` → 注册表 →
常见安装路径 → `PATH`；找不到只输出 `cargo:warning`，非 Windows 目标自动跳过）。
这个脚本不涉及功能插件 —— 插件不编进宿主。换图标：

```powershell
pwsh -File scripts/make-icon.ps1     # 从 assets/source 的立绘取上半部分裁成正方形
```

推送 `v*` 标签会触发 [`.github/workflows/AutoUploadReleaseBuild.yml`](.github/workflows/AutoUploadReleaseBuild.yml)：
在 Windows 上跑测试、拉取 `dxc` 与 `softgl`、构建主程序 exe，再用 Inno Setup 编译安装包，
最终发布三样东西并自动创建 GitHub Release：

| 产物 | 说明 |
| --- | --- |
| `arona-rs-<版本号>-win-x64.zip` | 便携版（框架本体）：`arona-rs.exe` + DXC 两个 dll + `softgl/`，插件 dll 由你自己放进 `plugins/` |
| `arona-rs-<版本号>-setup-win-x64.exe` | 安装包：介绍 + AGPLv3 全文 + 默认配置 + DXC（必装）+ 可选 `softgl/`，额外建好空的 `plugins\` |

裸 `arona-rs.exe` 只是构建中间产物（本地 `cargo dist` 产出、安装包与 zip 都从它取材），
不单独上传到 Release。体积参考：含 GUI 的主程序 exe 约 19 MB，`--no-default-features` 的精简命令行版约 10 MB。

### 验证

```bash
cargo test --workspace                 # 离线单测（动态装载与 ABI 握手/生命周期门控/配置迁移/钩子/隔离实例）
cargo test --release --workspace       # CI 口径
cargo clippy --workspace --all-targets # 框架侧 error 级必须清零
cargo fmt --all
cargo build-plugin                     # 示例插件编成 dll，产物 target/release/hello_plugin.dll
```

## 仓库结构

Cargo workspace（`resolver = "2"`）：

```
Cargo.toml               workspace 清单（成员 + 三方依赖版本集中声明）
crates/arona/            框架库
  onebot/    OneBot 11 协议、四种连接、全量强类型动作接口、入站/出站事件总线
  runtime/   命令表与参数类型系统、事件优先级、配置、日志、聊天记录缓存、清理内核、消息模型
  config/    框架配置 arona.yml、协议配置 onebot.yml、插件配置区与模板生成
  plugin/    插件契约：生命周期编排、登记交接面、作用域回收、健康度
             abi/（导出符号与工具链指纹）dynamic/（扫描 plugins/ 装载 dll）
  framework/ 注册表的宿主实例（对齐 mirai 的 MiraiInstance）
  container/ 插件间共享能力的服务容器
  quartz/    定时任务内核   services/ 服务开关表   admin/ 管理与诊断接口   gui/ 管理面板
crates/arona-host/       宿主可执行：产物 bin = arona-rs，里面没有任何功能插件
  build.rs   只把图标与清单编进 Windows 产物（不再生成插件注册代码）
  src/main.rs  一行 arona::run(args)
plugins/hello/           插件开发示例（编译成 dll）：命令 / 事件钩子 / 出站钩子 / 类型化配置 / 每日任务
assets/                  程序图标（arona.ico）、预览图与源立绘（source/）、arona.manifest
scripts/                 make-icon.ps1、fetch-dxc.ps1、fetch-softgl.ps1
installer/               Windows 安装包：arona-rs.iss、intro.txt、defaults/（默认配置模板）、languages/
```

`plugins/hello/` 是**跟框架同仓存放的开发样例**，不被宿主依赖、不进安装包；
真实的功能插件由各自的仓库分发，交付物就是编译好的 dll。

## 写一个插件

插件是一个**独立 crate**，编译成 dll 交付。以 `arona` 为依赖（依赖声明里**务必**带上
`default-features = false`，别让 GUI 顺着依赖扩散），实现 `AronaPlugin` 并用一个宏导出三个入口符号：

```toml
# Cargo.toml
[lib]
crate-type = ["cdylib"]        # 产物就是插件 dll

[dependencies]
arona = { path = "../Arona-rs/crates/arona", default-features = false }
```

```rust
use arona::plugin::{AronaPlugin, PluginContext, PluginMeta, PluginRegistrar};

struct MyPlugin;
impl AronaPlugin for MyPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("my-plugin", "MyPlugin", env!("CARGO_PKG_VERSION"), "我的功能")
    }
    fn install(&self, _reg: &PluginRegistrar) -> Result<(), String> { Ok(()) }
    fn configure(&self, _ctx: &PluginContext) -> Result<(), String> { Ok(()) }
}

arona::export_arona_plugin!(MyPlugin::new());
```

编译出的 dll 放进框架运行目录的 `plugins/`，下次启动框架就会扫描、握手并装载它 ——
**框架不需要重新编译，也不需要知道你的插件存在**。命令怎么登记、事件怎么按子类订阅、
配置区怎么声明、停用后框架回收哪些东西、工具链必须对齐哪几项，逐条写在
[PLUGIN_DEVELOPMENT.md](PLUGIN_DEVELOPMENT.md)；`plugins/hello/` 是这些接口的可编译样例。

## 疑难排查

- 直接跑 `arona-rs.exe --nogui`：能起机器人说明只是 GUI 的问题，用命令行模式即可。
- 双击毫无反应：看 `logs/startup-error.log`，配置/渲染/panic 的原因都在里面。窗口创建失败时程序
  **不会静默退出**：把原因打到控制台、写进日志，有 `softgl\` 就先试软渲染，仍失败才退回命令行模式继续跑机器人。
- 配置写错也**不会静默退出**：YAML 语法错误会写进当天的 `arona-yyyy-MM-dd.log` 与 `startup-error.log`，
  GUI 模式再弹一个「Arona 配置错误」消息框；`arona.yml` 坏掉时按默认配置继续运行，
  `onebot.yml` 坏掉时连不上任何实现端，提示后退出并保留现场。软错误（不认识的配置项等）只记 `WARNING`。
- 三方库（wgpu / glutin / winit）的诊断会写进控制台与日志文件，`ARONA_LOG` 可调级别。
- 远程桌面（RDP）通常能直接开面板；Windows Server Core（无桌面体验）不支持 GUI，请只用 `--nogui`。
- 群里发命令没人回应：先确认插件 dll 在不在运行目录的 `plugins/` 里、日志有没有「已装载动态插件」
  （一条都没有就是根本没装进来；「跳过插件 …」会点名工具链差在哪一项），再到「群管理」页看
  该群/该功能/该插件有没有被关掉，最后看日志里有没有「功能已关闭」或 panic 隔离的记录。

## 移植声明与许可

本项目是 [diyigemt/arona](https://github.com/diyigemt/arona) 的 Rust 移植版：命令行为、配置项、文案与数据结构均移植自该项目的 AGPLv3 源码，取的是它 `standalone` 模块（不依赖 Mirai 的那一条实现路径）里的行为，只移植 OneBot 11 一侧、移除全部 Mirai 相关实现。

- 上游版权：Copyright (C) 2020-2021 StageGuard / diyigemt，以 GNU AGPLv3 授权
- 本移植版版权：Copyright (C) 2026 YuLinLoli

依据 GNU AGPLv3 第 13 条，通过网络与本机器人交互的用户有权获取其对应源码，获取地址即本仓库：<https://github.com/YuLinLoli/Arona-rs>

本项目同样以 **GNU Affero General Public License v3.0** 授权发布，全文见 [LICENSE](LICENSE)；中文说明（非官方译本，仅供参考，以英文原文为准）见 [LICENSE.zh-CN.md](LICENSE.zh-CN.md)。你可以自由使用、修改、分发，但分发衍生作品或将其作为网络服务提供时，必须保留同样的许可与版权声明，并向使用者提供完整对应源码。
