; ============================================================================
;  Arona-rs 安装包脚本（Inno Setup 6）
;
;  编译方式（二选一）：
;    1) 项目内：cargo installer
;       （等价于 cargo test installer_build --release -- --ignored --nocapture）
;    2) 手动：  ISCC.exe /DAppVersion=0.2.3 /DHasSoftgl=1 arona-rs.iss
;
;  可选命令行宏（不传则用下面的默认值）：
;    /DAppVersion=0.2.3      版本号，默认 0.0.0
;    /DAppSourceDir=..\target\release     exe 与 softgl/ 所在目录
;    /DOutputDir=..\target\release        安装包输出目录
;    /DHasSoftgl=1                        包里带上 {#AppSourceDir}\softgl（CPU 软件渲染依赖）
;                                        不传则该组件不存在；传了也允许在「自定义安装」里取消勾选
;
;  安装/更新行为：
;    - 安装目录会写进注册表 HKA\Software\YuLinLoli\Arona-rs（InstallPath / Version / ExeName）
;    - 再次运行安装包（即“下载新版本再装一次”）时，会自动认到已有的安装目录并装回去，
;      同时跳过「程序简介」和「开源协议」两页，只需一路下一步
;    - 也可以用 setup.exe /UPDATE 显式声明这是一次更新安装
; ============================================================================

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

#ifndef AppSourceDir
  #define AppSourceDir "..\target\release"
#endif

#ifndef OutputDir
  #define OutputDir "..\target\release"
#endif

#define AppName "Arona-rs"
; 主程序固定叫 arona-rs.exe，不带版本号：升级/自动更新直接覆盖同名文件，
; 快捷方式与注册表里的 ExeName 不用跟着版本号变。版本号只体现在安装包名
; （OutputBaseFilename）与「应用和功能」里显示的版本上。
#define AppExeName "arona-rs.exe"
#define AppURL "https://github.com/YuLinLoli/Arona-rs"
#define AppAuthor "YuLinLoli"
#define RegSubKey "Software\YuLinLoli\Arona-rs"
; Inno 自己写的卸载项（更新安装时读它兜底找旧目录）
#define UninstallSubKey "SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{B7F3E0C2-5D4A-4E1B-9C7F-2A6D8E3140B5}_is1"


[Setup]
; AppId 固定不变：升级安装会识别为同一个程序，而不是装成两份
AppId={{B7F3E0C2-5D4A-4E1B-9C7F-2A6D8E3140B5}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppAuthor}
AppPublisherURL={#AppURL}
AppSupportURL={#AppURL}/issues
AppUpdatesURL={#AppURL}/releases
AppCopyright=Copyright (C) {#AppAuthor}. Licensed under GNU AGPLv3.
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
AllowNoIcons=yes
; 程序介绍（首次安装时展示；更新安装会自动跳过）
InfoBeforeFile=intro.txt
; 开源协议：AGPLv3 完整原文（首次安装时展示；更新安装会自动跳过）
LicenseFile=..\LICENSE
OutputDir={#OutputDir}
OutputBaseFilename=arona-rs-{#AppVersion}-setup-win-x64
SetupIconFile=..\assets\arona.ico
UninstallDisplayIcon={app}\{#AppExeName}
UninstallDisplayName={#AppName} {#AppVersion}
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
; 升级时沿用上一次的安装目录 / 安装范围（与下面的注册表读取是双保险）
UsePreviousAppDir=yes
UsePreviousPrivileges=yes
; 目标平台：64 位 Windows 7 SP1(6.1) 及以上（含 Windows Server 2008 R2 SP1+）
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=6.1sp1
; 默认「仅为我安装」，装在 %LOCALAPPDATA%\Programs 下，全程不需要管理员权限，
; 也避免装进 Program Files 后普通用户写不了数据目录（arona-standalone/）
;
; 注意：这里只管「安装程序」本身的权限；主程序 arona-rs.exe 启动时会自己申请
; 管理员权限（UAC 自提权，见 src/runtime/elevate.rs），与安装范围无关。
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
; 升级/卸载时检测正在运行的程序（数据目录里有 DLL 被占用时也能正确提示）
CloseApplications=yes
RestartApplications=no
VersionInfoVersion={#AppVersion}
VersionInfoProductName={#AppName}
VersionInfoCompany={#AppAuthor}
VersionInfoDescription={#AppName} 安装程序

[Languages]
Name: "chinesesimplified"; MessagesFile: "languages\ChineseSimplified.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Types]
Name: "full"; Description: "完整安装（推荐，含 CPU 软件渲染依赖）"
Name: "custom"; Description: "自定义安装（可去掉 CPU 软件渲染依赖）"; Flags: iscustom

[Components]
; 默认「完整安装」会带上 softgl；选「自定义安装」时可以把 softgl 取消掉节省 62MB
; （取消后服务器 / 无显卡机器上 GUI 会退回命令行模式，机器人功能不受影响）
Name: "main"; Description: "Arona-rs 主程序（必需）"; Types: full custom; Flags: fixed
Name: "softgl"; Description: "CPU 软件渲染依赖（约 62 MB，Mesa llvmpipe；服务器 / 无显卡驱动 / 无 DX12 的机器靠它打开管理面板）"; Types: full custom

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Registry]
; 记录安装目录：再次运行安装包（更新）时据此装回原目录，也是给外部工具/更新脚本用的入口
Root: HKA; Subkey: "{#RegSubKey}"; ValueType: string; ValueName: "InstallPath"; ValueData: "{app}"; Flags: uninsdeletekey
Root: HKA; Subkey: "{#RegSubKey}"; ValueType: string; ValueName: "Version"; ValueData: "{#AppVersion}"
Root: HKA; Subkey: "{#RegSubKey}"; ValueType: string; ValueName: "ExeName"; ValueData: "{#AppExeName}"
Root: HKA; Subkey: "{#RegSubKey}"; ValueType: string; ValueName: "UninstallString"; ValueData: "{uninstallexe}"
; 这里记录的是「本次是否真的装了」，自定义安装里取消勾选会写成 0
Root: HKA; Subkey: "{#RegSubKey}"; ValueType: dword;  ValueName: "SoftglInstalled"; ValueData: "{code:SoftglInstalledFlag}"

[Files]
; ---- 主程序：带版本号的 exe（与命令行版 cargo dist 的产物命名保持一致）----
Source: "{#AppSourceDir}\{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion; Components: main
; ---- 协议与说明 ----
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion; Components: main
Source: "..\LICENSE.zh-CN.md"; DestDir: "{app}"; Flags: ignoreversion; Components: main
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion; Components: main
Source: "intro.txt"; DestDir: "{app}"; DestName: "安装说明.txt"; Flags: ignoreversion; Components: main
; ---- 默认配置文件：只在缺失时释放，已存在（含用户改过的）一律不动 ----
;      uninsneveruninstall: 卸载时默认保留，是否删除由卸载流程单独询问
Source: "defaults\onebot.yml"; DestDir: "{app}\arona-standalone"; Flags: onlyifdoesntexist uninsneveruninstall; Components: main
Source: "defaults\arona.yml"; DestDir: "{app}\arona-standalone"; Flags: onlyifdoesntexist uninsneveruninstall; Components: main
Source: "defaults\trainer_config.yml"; DestDir: "{app}\arona-standalone"; Flags: onlyifdoesntexist uninsneveruninstall; Components: main
; ---- CPU 软件渲染依赖（大体积依赖与 exe 分开存储，安装时释放到 {app}\softgl）----
;      程序在 exe 同级目录寻找 softgl\，找不到就跳过软渲染，不影响机器人功能
#ifdef HasSoftgl
Source: "{#AppSourceDir}\softgl\*"; DestDir: "{app}\softgl"; Flags: ignoreversion recursesubdirs createallsubdirs; Components: softgl
#endif

[Icons]
; 快捷方式必须把工作目录设成安装目录：数据目录 arona-standalone/ 是相对工作目录创建的
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExeName}"; WorkingDir: "{app}"; IconFilename: "{app}\{#AppExeName}"
Name: "{autoprograms}\{#AppName} 数据目录"; Filename: "{app}\arona-standalone"; IconFilename: "{app}\{#AppExeName}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; WorkingDir: "{app}"; IconFilename: "{app}\{#AppExeName}"; Tasks: desktopicon

[InstallDelete]
; 旧版本升级时清掉上一份带版本号的 exe，避免目录里堆积多个版本
Type: files; Name: "{app}\arona-rs-*.exe"

[Run]
Filename: "{app}\{#AppExeName}"; Description: "{cm:LaunchProgram,{#AppName}}"; WorkingDir: "{app}"; Flags: nowait postinstall skipifsilent
Filename: "{app}\arona-standalone"; Description: "打开数据目录（onebot.yml / arona.yml 在这里）"; Flags: postinstall shellexec skipifsilent unchecked
Filename: "{app}\安装说明.txt"; Description: "查看安装说明"; Flags: postinstall shellexec skipifsilent unchecked

[Code]
var
  // 已经装过一次（检测到注册表里的安装目录）时，这是一次“更新安装”
  IsUpdateInstall: Boolean;
  // 注册表里记录的旧安装目录
  RecordedDir: String;

// 命令行里是否出现某个参数（前缀匹配，大小写不敏感）：/UPDATE、/DIR=...、/SILENT ...
function CmdLineParamExists(const Name: String): Boolean;
var
  I: Integer;
begin
  Result := False;
  for I := 1 to ParamCount do
  begin
    if CompareText(Copy(ParamStr(I), 1, Length(Name)), Name) = 0 then
    begin
      Result := True;
      Exit;
    end;
  end;
end;

// 去掉路径末尾的反斜杠（注册表里的 InstallLocation 可能带）
function TrimTrailingSlash(const Path: String): String;
begin
  Result := Path;
  while (Length(Result) > 3) and (Result[Length(Result)] = '\') do
    Delete(Result, Length(Result), 1);
end;

// 读注册表里记录的安装目录（先 HKLM 再 HKCU，兼容“为所有用户”与“仅为我”两种安装）
function ReadRecordedDir(): String;
var
  Value: String;
begin
  Result := '';
  if RegQueryStringValue(HKLM, '{#RegSubKey}', 'InstallPath', Value) and (Value <> '') and DirExists(Value) then
  begin
    Result := TrimTrailingSlash(Value);
    Exit;
  end;
  if RegQueryStringValue(HKLM32, '{#RegSubKey}', 'InstallPath', Value) and (Value <> '') and DirExists(Value) then
  begin
    Result := TrimTrailingSlash(Value);
    Exit;
  end;
  if RegQueryStringValue(HKCU, '{#RegSubKey}', 'InstallPath', Value) and (Value <> '') and DirExists(Value) then
  begin
    Result := TrimTrailingSlash(Value);
    Exit;
  end;
  // 兜底：Inno 自己写的卸载项
  if RegQueryStringValue(HKCU, '{#UninstallSubKey}', 'InstallLocation', Value) and (Value <> '') and DirExists(Value) then
  begin
    Result := TrimTrailingSlash(Value);
    Exit;
  end;
  if RegQueryStringValue(HKLM, '{#UninstallSubKey}', 'InstallLocation', Value) and (Value <> '') and DirExists(Value) then
  begin
    Result := TrimTrailingSlash(Value);
    Exit;
  end;
end;

// 供 [Registry] 使用：本次安装是否真的带上（并勾选了）CPU 软件渲染依赖
function SoftglInstalledFlag(Param: String): String;
begin
#ifdef HasSoftgl
  if WizardIsComponentSelected('softgl') then
    Result := '1'
  else
    Result := '0';
#else
  Result := '0';
#endif
end;

function InitializeSetup(): Boolean;
begin
  RecordedDir := ReadRecordedDir();
  IsUpdateInstall := (RecordedDir <> '') or CmdLineParamExists('/UPDATE');
  if IsUpdateInstall then
  begin
    Log('检测到已有安装，按更新方式安装');
    if RecordedDir <> '' then
      Log('安装目录取自注册表: ' + RecordedDir);
  end
  else
    Log('未检测到已有安装，按首次安装处理');
  Result := True;
end;

// 更新安装时不再重复展示「程序简介」和「开源协议」
function ShouldSkipPage(PageID: Integer): Boolean;
begin
  Result := IsUpdateInstall and ((PageID = wpInfoBefore) or (PageID = wpLicense));
  if Result then
  begin
    if PageID = wpLicense then
      Log('更新安装: 跳过「开源协议」页')
    else
      Log('更新安装: 跳过「程序简介」页');
  end;
end;

procedure InitializeWizard();
begin
  // 更新安装：默认装回注册表里记录的目录（命令行显式 /DIR= 优先）
  if (RecordedDir <> '') and (not CmdLineParamExists('/DIR=')) then
  begin
    WizardForm.DirEdit.Text := RecordedDir;
  end;
end;

const
  UninstallDataPrompt = '是否同时删除配置、数据库、日志与图片？' + #13#10 + #13#10 +
    '选择「是」会删除整个 arona-standalone 目录，其中包括你配置好的群、管理员、' +
    '黑名单、抽卡历史与备份，删除后无法恢复。' + #13#10 + #13#10 +
    '只想卸载程序、以后可能还回来用，请选择「否」。';

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  DataDir: String;
begin
  // 静默卸载(/VERYSILENT)不弹窗，一律保留用户数据，避免误删
  if UninstallSilent then
  begin
    Exit;
  end;
  if CurUninstallStep = usPostUninstall then
  begin
    DataDir := ExpandConstant('{app}\arona-standalone');
    if DirExists(DataDir) then
    begin
      if MsgBox(UninstallDataPrompt, mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
      begin
        DelTree(DataDir, True, True, True);
      end;
    end;
  end;
end;