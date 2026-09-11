# Arona-rs

碧蓝档案（Blue Archive）QQ 机器人 **arona** 的 Rust 移植版，只保留 **OneBot 独立运行模式**，完全剥离 Mirai。

- 本项目仓库：<https://github.com/YuLinLoli/Arona-rs>
- 上游原版（Kotlin + Mirai）：<https://github.com/diyigemt/arona>
- 许可证：GNU AGPLv3（见文末「移植声明与许可」）

## 这是什么

原版 arona 是基于 Mirai 控制台的 Kotlin 插件，需要 JVM、Mirai 运行环境和插件安装流程。本项目把它的业务逻辑改写为 Rust：

- **单文件可执行**：产物只有一个 exe，没有 JVM、没有 Mirai 控制台、没有插件目录，双击或命令行启动即可。
- **只做 OneBot 11**：支持正向 WebSocket / 反向 WebSocket / 正向 HTTP / 反向 HTTP 四种连接，可直接对接 NapCat、Lagrange 等 OneBot 实现。
- **图片改为纯 Rust 渲染**：原版抽卡结果图、活动日历图依赖 Java AWT，本移植版用 `fontdue` + `image` 自己绘制（抽卡图 2340x1080 卡片网格、日历图白底圆角色块排版），不依赖 JVM，也不需要外部渲染进程；系统无中文字体时命令层自动回退为文本输出。塔罗牌图沿用原版方案，从 CDN 下载后随文本一起发送。
- **控制台彩色输出**：Windows 下走原生 `SetConsoleTextAttribute`，cmd.exe（经典 conhost）、PowerShell、Windows Terminal 都能正常上色，不依赖 VT；其他平台走 ANSI。检测到 `NO_COLOR` 或输出被重定向时自动降级为纯文本，不会漏出转义码。

## 功能

**命令**

| 命令 | 说明 |
| --- | --- |
| `/单抽` `/十连` `/狗叫` `/历史` | 抽卡、狗叫彩蛋、抽卡历史 |
| `/抽卡服务器 日服\|国服\|国际服` | 切换抽卡使用的服务器 |
| `/游戏名` `/谁是`（`/谁叫`） `/叫我` | 群名片 / 称呼记录与查询 |
| `/塔罗牌 [牌名]` | 抽塔罗牌（可选配图） |
| `/活动 [日服\|国服\|国际服]` | 当期活动日历，默认日服 |
| `/攻略 <关键词>` | 见下方说明 |
| `/arona status\|services\|service list\|version\|help` | 运行状态、连接、已注册服务、版本、帮助 |
| `/帮助` | 帮助 |
| `/紧急停止` | 投票停止服务 |
| `/config` | 查看 / 修改配置（管理员） |
| `/任务` `/备份` `/恢复` `/抽卡` | 定时任务与备份恢复、抽卡配置（管理员） |

`/攻略` 支持的关键词：

- `日服活动` / `国际服活动` / `国服活动`：从 GameKee 抓取当期活动攻略图并逐张发送
- `日程笔记`：抓取 GameKee 日程笔记，以合并转发发送
- `日服卡池` / `国服卡池` / `国际服卡池`：当期卡池角色（卡池图 + 角色名/所属/起止时间）合并转发
- 其它任意关键词：走 arona 云端图片库检索，精确命中直接发图，模糊命中列出候选并可用数字回复选择

**推送**

- 每天 `notify.every_day_hour` 点（默认 8 点）向目标群推送三服活动日历
- 活动结束前 5 小时、1 小时各提醒一次（维护预告同样处理）
- 学生生日并入活动日历一起展示，但不参与 5 小时 / 1 小时预警

**数据来源**

- [GameKee](https://www.gamekee.com/)：活动攻略、日程笔记、当期卡池
- [SchaleDB](https://github.com/SchaleDB/SchaleDB)：学生生日
- [kivo.wiki](https://kivo.wiki/)：学生数据（抽卡池构建）
- arona 云端图片库：`/攻略` 的图片检索

## 快速开始

### 方式一：安装包（推荐）

1. 从 [Releases](https://github.com/YuLinLoli/Arona-rs/releases) 下载 `arona-rs-<版本号>-setup-win-x64.exe` 并运行。
   安装向导会先展示程序介绍与 AGPLv3 许可证全文，同意后才能继续。
2. 默认的**完整安装**会一并装上 CPU 软件渲染依赖 `softgl\`（约 62 MB，Mesa llvmpipe）。
   服务器 / 虚拟机 / 没有显卡驱动或没有 DX12 的机器上，硬件渲染后端会全部失败，
   程序会自动切到这套 CPU 渲染把管理面板画出来，所以服务器上也能正常开 GUI；
   正常带显卡的机器会优先用硬件渲染，这份依赖平时不会被加载。
   只在正常带显卡的机器上用的话，安装时选「自定义安装」可以把这项取消，省下 62 MB。
3. 安装目录（默认 `%LOCALAPPDATA%\Programs\Arona-rs`）里会释放一份默认配置
   （`arona-standalone\onebot.yml`、`arona.yml`、`trainer_config.yml`）。
   这些文件**只在缺失时写入**，升级安装不会覆盖你改过的配置；万一缺失或损坏，
   程序启动时也会按内置默认值自动补齐。
4. 从开始菜单启动「Arona-rs」打开管理面板，填好 OneBot 连接与群配置即可。

安装包是**按用户安装**的，全程不需要管理员权限，也不会把数据写进 `Program Files`。
快捷方式已把工作目录设为安装目录，所以数据目录就是安装目录下的 `arona-standalone\`。
卸载时默认**保留** `arona-standalone\`（配置 / 数据库 / 日志 / 图片），会单独询问是否连数据一起删除。

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

### 方式二：便携版

1. 从 [Releases](https://github.com/YuLinLoli/Arona-rs/releases) 下载 `arona-rs-<版本号>-win-x64.zip`
   解压（里面是 `arona-rs.exe` 与 `softgl/`；裸 exe 不再单独发布，需要单文件就用安装包）。
2. 在你想作为数据目录的位置运行一次，会在**当前工作目录**下自动生成 `arona-standalone/`（`arona.yml`、`onebot.yml`、`data/`、`logs/`、`images/`、`backups/`）。
3. 编辑 `onebot.yml` 填机器人账号和连接方式，编辑 `arona.yml` 填服务群和管理员。
4. 重新启动。

### 管理员权限（UAC）

主程序（`arona-rs.exe`）**启动时会申请管理员权限**：机器人要写数据目录、监听端口、
需要时替换 `softgl\` 下的渲染 DLL，统一以管理员身份运行可以避免权限不足导致的
「启动没反应」「保存配置失败」「监听不了端口」等问题。

- 双击 exe / 从开始菜单启动时会弹 UAC 询问框，选「是」后程序才会真正开始运行；
  选「否」或提权失败时程序会打印一条提示并以**普通权限**继续运行，功能不受影响，
  只是可能因权限不足写不了数据目录或监听端口。
- 安装包本身仍是**按用户安装**（`PrivilegesRequired=lowest`），安装过程不需要管理员权限，
  提权只发生在主程序启动那一刻。
- 调试 / 自动化场景可以用环境变量跳过提权：

  ```powershell
  $env:ARONA_NO_ELEVATE = "1"    # 本次启动不再弹 UAC，直接以当前权限运行
  ```

- 不想弹出 UAC 又有管理员需求时，也可以手动右键 exe →「以管理员身份运行」；
  程序检测到自己已经提权后不会重复申请。

### onebot.yml

```yaml
self_id: 123456789          # 机器人 QQ 号
nickname: "Arona"

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

### arona.yml

```yaml
groups: []                  # 允许响应的群号，留空 = 所有群
managers: []                # 管理员 QQ 号

notify:
  enable: true              # 每日活动推送
  every_day_hour: 8
  jp: true
  global: true
  cn: true
  black_groups: []          # 不推送的群
  notify_text: "碧蓝档案预警"

trainer:
  tip_when_null: true       # 模糊命中时给出候选提示
  tip_revoke_time: 10       # 提示消息撤回秒数，0 = 不撤回
  tip_response_wait_time: 10 # 等待数字回复的秒数，0 = 关闭
  override: []              # /攻略 别名覆盖: type=IMAGE|RAW|CODE, name, value
```

两个配置文件保存后自动热重载；`onebot.yml` 的连接项需要重启生效。`arona-standalone/trainer_config.yml` 可单独维护 `/攻略` 别名覆盖。

### 启动参数与环境变量

- `--gui`（默认）：打开管理面板（群功能开关 / 群成员黑名单 / OneBot 连接配置与热重载）
- `--nogui`：不打开面板，只启动命令行(黑窗口)模式
- `--renderer=glow|wgpu`：只试指定的渲染后端（默认自动：glow → wgpu 多种配置 → 软件 OpenGL 兜底）
- `--softgl`：强制用 Mesa 软件 OpenGL(llvmpipe) 渲染（见下文「Windows Server / 虚拟机」）
- `ARONA_SOFTGL=1` / `ARONA_SOFTGL_DIR=<目录>`：等价开关 / 指定 softgl 目录
- `ARONA_LOG=info|debug|trace|off`：三方库(wgpu/glutin 等)的日志级别，默认 `debug` 但只对 wgpu* 生效
- `--config=<路径>`：指定 `onebot.yml` 路径
- `--arona-config=<路径>`：指定 `arona.yml` 路径
- `--test-notify`：启动 20 秒后立即执行一次活动推送（用于验证推送是否可用）
- `ARONA_CONSOLE_COLOR=1|0`：强制开关控制台颜色；另识别 `NO_COLOR`、`FORCE_COLOR`、`CLICOLOR_FORCE`
- `ARONA_CONSOLE_DEBUG=1`：打印控制台颜色模式，便于排查终端差异
- `ARONA_CONSOLE_EMOJI=0`：关闭控制台 emoji
- `ARONA_NO_ELEVATE=1`：跳过启动时的管理员权限申请（调试 / 自动化测试用）
- `--arona-elevated`：提权重启时由程序自己附加的内部标记，不需要手动加（防止无限重启）

## 构建

需要 Rust 1.85 或更新版本（`edition = "2024"`）。

```bash
cargo build --release      # 发布构建(默认含管理 GUI), 产物: target/release/arona-rs[.exe]
cargo dist                 # 发布构建 + 整理产物: target/release/arona-rs[.exe]
                           # （主程序固定不带版本号, 升级时直接覆盖同名文件；顺手清理旧的 arona-rs-<版本号>.exe）
cargo run --release        # 构建并运行(默认打开管理 GUI)
cargo run-nogui            # 以纯命令行(黑窗口)模式运行
cargo build-nogui          # 精简命令行版: 不含 GUI, 体积更小(--no-default-features)
cargo installer            # 发布构建 + 打 Windows 安装包: target/release/arona-rs-<版本号>-setup-win-x64.exe
```

安装包由 [Inno Setup 6](https://jrsoftware.org/isinfo.php) 编译，脚本见 [`installer/arona-rs.iss`](installer/arona-rs.iss)：

- `cargo installer` 会自行查找 `ISCC.exe`（`ARONA_ISCC` 环境变量 → `PATH` → 注册表 → 常见安装路径），
  找不到时会提示先执行 `winget install --id JRSoftware.InnoSetup`。
- 安装包内含程序介绍（[`installer/intro.txt`](installer/intro.txt)）、AGPLv3 全文与中文译本、
  默认配置模板（[`installer/defaults/`](installer/defaults)，`onlyifdoesntexist` 释放）。
- 约 62 MB 的 `softgl/`（CPU 软件渲染依赖）与 exe **分开存储**，安装时释放到安装目录的 `softgl\`
  （程序就在 exe 同级的 `softgl\` 找它）。安装类型默认「完整安装」会勾上它，
  选「自定义安装」则可以取消；仓库里没有 `softgl/` 时安装包会自动不含该组件（`/DHasSoftgl` 控制）。
- 安装目录写入 `HKCU\Software\YuLinLoli\Arona-rs`（`InstallPath` / `Version` / `ExeName` /
  `UninstallString` / `SoftglInstalled`），再次运行安装包会据此自动装回原目录并跳过简介与协议页。
- **主程序固定叫 `arona-rs.exe`（不带版本号）**：升级时覆盖同名文件即可，快捷方式、计划任务与
  注册表里的 `ExeName` 都不用跟着版本号改；版本号只体现在安装包名、「应用和功能」列表和
  面板的「关于」页面里。安装时会自动清掉老版本留下的 `arona-rs-<版本号>.exe`。

可选：拉取**软件 OpenGL 兜底依赖**（Mesa llvmpipe，约 62MB，不入库）——没有显卡驱动 / 没有 DX12 的服务器靠它打开管理面板，详见下文「Windows Server / 虚拟机」：

```powershell
powershell -ExecutionPolicy Bypass -File scripts/fetch-softgl.ps1   # 产出仓库根目录的 softgl\，cargo dist 会一并复制到产物目录
```

**程序图标**：[`build.rs`](build.rs) 会在编译时把 [`assets/arona.ico`](assets/arona.ico) 编译成 `.res` 并链接进 Windows 产物（源立绘见 `assets/source/`；调用 Windows SDK 的 `rc.exe`，查找顺序为 `ARONA_RC` 环境变量 → `WindowsSdkDir` → 注册表 → 常见安装路径 → `PATH`）。需要换图标时，改裁切参数后重新生成：

```powershell
pwsh -File scripts/make-icon.ps1     # 从 assets/source 的立绘取上半部分裁成正方形 -> assets/arona.ico + assets/arona.png
```

找不到 `rc.exe` 时只输出 `cargo:warning`，不影响编译（产物只是没有图标）；非 Windows 目标自动跳过。
推送 `v*` 标签会触发 [`.github/workflows/AutoUploadReleaseBuild.yml`](.github/workflows/AutoUploadReleaseBuild.yml)：在 Windows 上跑测试、拉取 `softgl`、构建主程序 exe，再用 Inno Setup 编译安装包，最终发布两样东西并自动创建 GitHub Release：

| 产物 | 说明 |
| --- | --- |
| `arona-rs-<版本号>-win-x64.zip` | 便携版：`arona-rs.exe`（主程序固定不带版本号）+ `softgl/` |
| `arona-rs-<版本号>-setup-win-x64.exe` | 安装包：程序介绍 + AGPLv3 全文 + 默认配置 + 可选 `softgl/` 组件 |

裸 `arona-rs.exe` 只是构建中间产物（本地 `cargo dist` 产出、安装包与 zip 都从它取材），
不再单独上传到 Release——否则 Release 里会躺着一个不带版本号、下载下来分不清版本的 exe。

## 自检

```bash
cargo test                 # 离线单元测试
cargo smoke-image          # 抽卡 / 活动日历图渲染冒烟自检（像素级断言）
cargo smoke-tenpull        # /十连 端到端（联网，出图到 arona-standalone/images）
cargo smoke-trainer        # /攻略 端到端（联网）
cargo smoke-notify         # 推送与预警调度逻辑（离线）
cargo smoke-notify-net     # 每日推送端到端（联网）
cargo smoke-birthday       # 学生生日数据流端到端（联网）
```

## 目录结构

```
src/
  onebot/       OneBot 11 协议：四种连接方式、事件分发、API 调用、消息发送
  runtime/      运行期基础设施：配置、日志、控制台颜色、命令分发、数据目录
  standalone/   独立模式：命令注册与各命令实现（抽卡/攻略/活动/塔罗/名字/管理）
  admin/        管理后端：群/成员查询、功能开关与黑名单读写、OneBot 配置读写与热重载
  gui/          管理面板（eframe/egui，默认启用，启动即打开；含群管理/OneBot 连接/实时日志）
  data/         外部数据源：GameKee、SchaleDB 生日、kivo 学生、arona 云端图片库
  image/        纯 Rust 图片渲染：抽卡结果图、活动日历图、字体与绘图工具
  activity/     活动日历同步与推送 / 预警调度
  db/           SQLite 持久层
  quartz/       定时任务
assets/         程序图标（arona.ico）、预览图与源立绘（source/）
scripts/        辅助脚本（make-icon.ps1 生成图标、fetch-softgl.ps1 拉取软件 OpenGL）
installer/      Windows 安装包：arona-rs.iss(Inno Setup 脚本)、intro.txt(程序介绍)、
                defaults/(默认配置模板)、languages/(简体中文向导翻译)
build.rs        构建脚本：把图标嵌入 Windows 产物
```

## 管理 GUI

本地管理面板基于 `eframe`/`egui`，**默认构建即包含、启动即打开**：

```bash
cargo build --release    # 默认含 GUI
target/release/arona-rs          # 启动 -> 打开管理面板
target/release/arona-rs --nogui  # 只启动命令行(黑窗口)模式
target/release/arona-rs --gui    # 显式打开面板（默认行为，等价于不加参数）
```

Windows 上含 GUI 的产物使用 windows 子系统，GUI 模式不会多出控制台窗口；`--nogui`
会自动附加上级终端（cmd/PowerShell）或新建控制台，日志与颜色照常输出。

渲染后端默认自动：先试 `glow`（OpenGL），失败再试 `wgpu`（DX12/Vulkan，含 WARP 软件渲染）。
可用 `--renderer=glow` / `--renderer=wgpu` 强制指定。

若需要不含 GUI 的精简命令行产物：`cargo build --release --no-default-features`（或 `cargo build-nogui`）。

面板顶部有四个标签页（关闭窗口 = 退出整个程序，GUI 与机器人一起结束）：

**群管理**
- 左栏：搜索群号/群名，`●`/`○` 标记该群是否启用，右侧显示已关闭的功能数量与黑名单人数；可点「刷新群列表」从 OneBot 拉取。
- 右栏：勾选「启用本群响应」；按功能 key 逐个开关（抽卡 / 名字 / 塔罗 / 活动 / 攻略 / 任务 / 备份 / 配置 / 紧急 / 帮助）。
- 「群成员黑名单」：点进群后自动拉取该群全部成员（管理员/群主/成员排序），每个成员有「本群」「全局」两个勾选框，可加入或移出黑名单，支持「只看黑名单」过滤。
- 「清空该群设置」：删除该群的 `disabled_features` 与 `blacklist`，恢复默认。

**OneBot 连接**
- 连接列表展示全部实例（同一类型可添加多个）。
- 「添加连接」下拉可选择 `ws-reverse`（反向 WebSocket）/ `ws`（正向 WebSocket）/ `http`（正向 HTTP）/ `http-post`（HTTP 上报）等类型，选中后点「添加」。
- 折叠面板可编辑 host / port / url / path / token / 心跳 / 重连等字段，以及 `self_id` / 昵称。
- 「保存并热重载」：写回 `onebot.yml` 后立即 `stop → start` 全部 WebSocket / HTTP 服务（含反向监听端口真正释放并重绑），无需重启程序。

**实时日志**
- 控制台（stdout/stderr）的一切输出都会同步进内存缓冲并在这里实时刷新（约每 0.5 秒），含启动横幅、机器人日志、群消息收发、告警/错误。
- **普通行跟随外观**：黑夜模式白色文字、白天模式黑色文字；特殊色沿用控制台规则
  （`[Arona]`/`[OneBot]` 绿、`WARNING`/`ERROR` 黄/红），并会按背景深浅换成对应深浅的版本，白色背景上同样看得清。
- 行首显示本机时间。
- 工具栏：`刷新`、`清空`、`自动滚动`、`只看告警/错误`、`最近 N 行`（200/500/1000/3000）、`打开日志目录`，以及关键字过滤。
- 缓冲上限 3000 行（只保留最新），完整历史仍按天落盘到 `arona-standalone/logs/arona-yyyy-MM-dd.log`。

**关于**
- 显示版本号、项目仓库（https://github.com/YuLinLoli/Arona-rs ）、鸣谢（原版作者
  https://github.com/diyigemt ）、开源协议，以及数据目录路径与快捷打开按钮。

**外观（白天 / 黑夜）**
- 右上角「外观」下拉可选 `跟随系统` / `白天模式` / `黑夜模式`，默认跟随系统。
- 白天模式整体亮色背景 + **黑色**文字，黑夜模式整体深色背景 + **白色**文字。
- 选择保存在数据目录的 `arona-standalone/gui.txt`，下次启动沿用（与机器人配置无关，删掉即恢复默认）。

### Windows Server / 虚拟机（没有可用显卡驱动）

管理面板的渲染后端会**逐个尝试**，第一个成功就停：

| 顺序 | 后端 | 说明 |
| --- | --- | --- |
| 1 | `glow`（OpenGL） | 桌面上最快；只有 OpenGL 1.1 的系统会失败 |
| 2 | `wgpu`（DX12/Vulkan） | DX12 之下有 **WARP 软件渲染**（Microsoft Basic Render Driver），无显卡驱动也能画 |
| 3 | `wgpu`（全部后端 / DX12+FXC） | 备用配置；万一静态 DXC 容器创建失败、或需要 GL 后端时兜底 |
| 4 | `glow` + 软件 OpenGL(llvmpipe) | 前三步都失败时**自动用 `--softgl` 重启自己**，改走 Mesa 的 CPU 软件渲染 |

- 强制指定：`--renderer=wgpu` / `--renderer=glow`（不加则按上表自动）。
- 日志里会打印全部候选适配器与最终选中的那个，例如
  `wgpu 适配器: Microsoft Basic Render Driver（Cpu, Dx12）` 说明正在用 WARP 软件渲染。

#### 软件 OpenGL 兜底（llvmpipe）

有些服务器既没有 OpenGL 2.0（只有 GDI 的 1.1），又没有可用的 DX12（例如系统缺少
`d3d12.dll`），这时前三个后端都会失败。为此本项目支持把 Mesa 的软件渲染版 `opengl32.dll`
放到 exe 同级的 `softgl\` 目录，用 CPU 把界面画出来：

```powershell
# 拉取依赖（约 62MB，不入库；需要时可重新跑）
powershell -ExecutionPolicy Bypass -File scripts/fetch-softgl.ps1
# 显式以软渲染启动（不加 --softgl 时，前三个后端全失败会自动切到它）
arona-rs.exe --softgl
```

- 目录查找顺序：`ARONA_SOFTGL_DIR` → exe 同级 `softgl\` → 工作目录 `softgl\` → `arona-standalone\softgl\`。
- 环境变量：`ARONA_SOFTGL=1` 等价于 `--softgl`；`ARONA_SOFTGL_DIR=<目录>` 直接指定目录。
- `cargo dist` 与 GitHub Actions 发布包会自动带上 `softgl/`（发布 zip 解压后 exe 与 `softgl\` 同级即生效）。

**运行库与着色器编译器（真正“免安装”）**：除了 Windows 自带的系统 DLL，产物不再依赖任何外部文件。

- **MSVC 运行库静态链接**：`.cargo/config.toml` 里的 `+crt-static` 已把 VCRUNTIME/UCRT 编进 exe，
  不再依赖 `VCRUNTIME140.dll`，服务器上**无需安装** VC++ Redistributable。
- **DX12 着色器编译器用静态 DXC**：`Cargo.toml` 里 `wgpu` 开了 `static-dxc`（`mach-dxcompiler-rs`），
  DXC 被直接编进 exe，因此**不需要**随包附带 `dxcompiler.dll` / `dxil.dll`。
- **`d3dcompiler_47.dll` 改成延迟加载**：`build.rs` 通过 `/DELAYLOAD:d3dcompiler_47.dll`
  把它从“启动即必需”降级为“用到才加载”。本项目默认走 StaticDxc，FXC 路径只在备用方案里才碰，
  所以即使系统里没有这个 DLL（部分 Server 精简安装就是这样），程序也能正常启动。
- **`opengl32.dll` 也改成延迟加载**：`build.rs` 用 `/DELAYLOAD:opengl32.dll`。`glutin_wgl_sys`
  是用 `#[link(name = "opengl32")]` 静态导入它的，不改成延迟加载的话进程一启动就会加载系统那份
  OpenGL 1.1，上面第 4 步就没机会换成 `softgl\` 里的 Mesa 了。
- 代价是体积：带 GUI 的发布产物约 **40 MB**（绝大部分是内嵌的 DXC）；`softgl/` 是额外的约 62MB 目录。

#### 出问题时怎么先看到原因

程序启动时会安装 `log` 门面，**wgpu / glutin / winit 的诊断都会写进控制台和日志文件**。
在这之前它们是被直接丢弃的，所以只会看到一句“没有可用的 wgpu 适配器”却不知道原因；
关键信息（如 `failed to create Dx12 backend: ...`、`failed to load d3d12.dll`）现在都在
`arona-standalone/logs/arona-yyyy-MM-dd.log` 里。`ARONA_LOG=info|debug|trace|off` 可调整级别。

窗口创建失败时程序**不会静默退出**，而是：

- 把 `GUI 启动失败: ...` 打到控制台（双击时会自动接回/新建控制台窗口）；
- 写入 `arona-standalone/logs/startup-error.log` 与当天的 `arona-yyyy-MM-dd.log`；
- 如果存在 `softgl\`，先自动用软件 OpenGL 重启一次；
- 仍然失败才**回退到命令行模式**继续运行机器人（下次可直接加 `--nogui` 跳过 GUI）。

排查清单：

- 直接跑 `arona-rs.exe --nogui`：能起机器人说明只是 GUI 的问题，用命令行模式即可；
- 面板起不来：先看日志里的后端失败原因；实在不行跑一次 `scripts/fetch-softgl.ps1` 走软渲染；
- 用远程桌面（RDP）登录通常也能直接开面板；
- Windows Server Core（无桌面体验）不支持 GUI，请只用 `--nogui`。
### 配置结构

GUI 的改动都落在 `arona.yml` 里，可手工编辑，程序与面板都会读取：

```yaml
# 全局用户黑名单：这些 QQ 在任何群/私聊都不触发机器人（管理员不受限）
global_blacklist: [10001]
# 分群设置：群号 -> 关闭的功能 / 群内成员黑名单
group_settings:
  "123456789":
    disabled_features: [tarot, activity]   # 可用 key 见 arona.yml 模板注释
    blacklist: [10002, 10003]
```

修改会在下次读取时生效；通过 GUI 保存的分群设置会立即写盘并应用。
## 移植声明与许可

本项目是 [diyigemt/arona](https://github.com/diyigemt/arona) 的 Rust 移植版：命令行为、配置项、文案与数据结构均移植自该项目的 AGPLv3 源码（含其独立运行模式），仅保留 OneBot 独立模式、移除全部 Mirai 相关实现。

- 上游版权：Copyright (C) 2020-2021 StageGuard / diyigemt，以 GNU AGPLv3 授权
- 本移植版版权：Copyright (C) 2026 YuLinLoli

依据 GNU AGPLv3 第 13 条，通过网络与本机器人交互的用户有权获取其对应源码，获取地址即本仓库：<https://github.com/YuLinLoli/Arona-rs>

本项目同样以 **GNU Affero General Public License v3.0** 授权发布，全文见 [LICENSE](LICENSE)；中文说明（非官方译本，仅供参考，以英文原文为准）见 [LICENSE.zh-CN.md](LICENSE.zh-CN.md)。你可以自由使用、修改、分发，但分发衍生作品或将其作为网络服务提供时，必须保留同样的许可与版权声明，并向使用者提供完整对应源码。