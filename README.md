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

1. 从 [Releases](https://github.com/YuLinLoli/Arona-rs/releases) 下载 `arona-rs-<版本号>.exe`。
2. 在你想作为数据目录的位置运行一次，会在**当前工作目录**下自动生成 `arona-standalone/`（`arona.yml`、`onebot.yml`、`data/`、`logs/`、`images/`、`backups/`）。
3. 编辑 `onebot.yml` 填机器人账号和连接方式，编辑 `arona.yml` 填服务群和管理员。
4. 重新启动。

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

- `--config=<路径>`：指定 `onebot.yml` 路径
- `--arona-config=<路径>`：指定 `arona.yml` 路径
- `--test-notify`：启动 20 秒后立即执行一次活动推送（用于验证推送是否可用）
- `ARONA_CONSOLE_COLOR=1|0`：强制开关控制台颜色；另识别 `NO_COLOR`、`FORCE_COLOR`、`CLICOLOR_FORCE`
- `ARONA_CONSOLE_DEBUG=1`：打印控制台颜色模式，便于排查终端差异
- `ARONA_CONSOLE_EMOJI=0`：关闭控制台 emoji

## 构建

需要 Rust 1.85 或更新版本（`edition = "2024"`）。

```bash
cargo build --release      # 产物: target/release/arona-rs[.exe]
cargo dist                 # 发布构建并生成带版本号的产物: target/release/arona-rs-<版本号>.exe
cargo run --release        # 直接运行独立版
```

**程序图标**：[`build.rs`](build.rs) 会在编译时把 [`assets/arona.ico`](assets/arona.ico) 编译成 `.res` 并链接进 Windows 产物（源立绘见 `assets/source/`；调用 Windows SDK 的 `rc.exe`，查找顺序为 `ARONA_RC` 环境变量 → `WindowsSdkDir` → 注册表 → 常见安装路径 → `PATH`）。需要换图标时，改裁切参数后重新生成：

```powershell
pwsh -File scripts/make-icon.ps1     # 从 assets/source 的立绘取上半部分裁成正方形 -> assets/arona.ico + assets/arona.png
```

找不到 `rc.exe` 时只输出 `cargo:warning`，不影响编译（产物只是没有图标）；非 Windows 目标自动跳过。
推送 `v*` 标签会触发 [`.github/workflows/AutoUploadReleaseBuild.yml`](.github/workflows/AutoUploadReleaseBuild.yml)：在 Windows 上跑测试、构建带版本号的 exe 并自动创建 GitHub Release。

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
  data/         外部数据源：GameKee、SchaleDB 生日、kivo 学生、arona 云端图片库
  image/        纯 Rust 图片渲染：抽卡结果图、活动日历图、字体与绘图工具
  activity/     活动日历同步与推送 / 预警调度
  db/           SQLite 持久层
  quartz/       定时任务
assets/         程序图标（arona.ico）、预览图与源立绘（source/）
scripts/        辅助脚本（make-icon.ps1 生成图标）
build.rs        构建脚本：把图标嵌入 Windows 产物
```

## 移植声明与许可

本项目是 [diyigemt/arona](https://github.com/diyigemt/arona) 的 Rust 移植版：命令行为、配置项、文案与数据结构均移植自该项目的 AGPLv3 源码（含其独立运行模式），仅保留 OneBot 独立模式、移除全部 Mirai 相关实现。

- 上游版权：Copyright (C) 2020-2021 StageGuard / diyigemt，以 GNU AGPLv3 授权
- 本移植版版权：Copyright (C) 2026 YuLinLoli

依据 GNU AGPLv3 第 13 条，通过网络与本机器人交互的用户有权获取其对应源码，获取地址即本仓库：<https://github.com/YuLinLoli/Arona-rs>

本项目同样以 **GNU Affero General Public License v3.0** 授权发布，全文见 [LICENSE](LICENSE)；中文说明（非官方译本，仅供参考，以英文原文为准）见 [LICENSE.zh-CN.md](LICENSE.zh-CN.md)。你可以自由使用、修改、分发，但分发衍生作品或将其作为网络服务提供时，必须保留同样的许可与版权声明，并向使用者提供完整对应源码。