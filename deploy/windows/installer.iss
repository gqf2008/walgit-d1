; deploy/windows/installer.iss — walgit Windows 安装程序(Inno Setup 6)。
; 构建方法与产物名见 deploy/windows/README.md(唯一出处);CI(release.yml
; 的 windows leg)以 tag 去掉 v 传 -DMyAppVersion。

#ifndef MyAppVersion
#define MyAppVersion "0.0.0-dev"
#endif

#define MyAppName "walgit"
#define MyAppExeName "walgit-tray.exe"

[Setup]
AppId={{B4776A83-9C52-4A9E-8F1D-0A5F3E2D1C74}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher=walgit
; 程序目录与状态目录分离：程序在 %LOCALAPPDATA%\Programs\walgit，
; 状态/配置在 %USERPROFILE%\.walgit（与 tray-rs 的 state_dir() 一致）。
DefaultDirName={%LOCALAPPDATA}\Programs\walgit
AppendDefaultDirName=no
; 旧版曾装在 %USERPROFILE%\walgit；升级必须切到新的程序目录，
; 状态配置另行迁移到 %USERPROFILE%\.walgit。
UsePreviousAppDir=no
DirExistsWarning=no
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
WizardStyle=modern
DefaultGroupName=walgit
Compression=lzma2/max
; 两个大 exe 的载荷,SolidCompression 无尺寸收益只有编译耗时
CloseApplications=no
OutputDir=Output
OutputBaseFilename=walgit-setup-{#MyAppVersion}-x64
UninstallDisplayName={#MyAppName}
UninstallDisplayIcon={app}\{#MyAppExeName}
MinVersion=10.0

[Languages]
; 中文语言包入库(官方 unofficial 翻译,Inno 不随基包分发);引用相对脚本目录
Name: "chinesesimplified"; MessagesFile: "ChineseSimplified.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[CustomMessages]
chinesesimplified.AutoStartTask=开机自动启动 walgit 托盘(&A)
english.AutoStartTask=Start the walgit tray at logon (&A)
chinesesimplified.LaunchTray=启动 walgit 托盘(服务请在托盘菜单「启动服务」)
english.LaunchTray=Launch the walgit tray (start the service from its menu)

[Tasks]
Name: "autostart"; Description: "{cm:AutoStartTask}"; GroupDescription: "{cm:AdditionalIcons}"
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Files]
Source: "..\..\target\release\walgit.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\target\release\walgit-tray.exe"; DestDir: "{app}"; Flags: ignoreversion
; 已有配置绝不覆盖;卸载也不删(用户数据)。写到状态目录而非程序目录。
Source: "walgit.toml.initial"; DestDir: "{%USERPROFILE}\.walgit"; DestName: "walgit.toml"; Flags: onlyifdoesntexist uninsneveruninstall

[Icons]
Name: "{group}\walgit 托盘"; Filename: "{app}\{#MyAppExeName}"
Name: "{group}\walgit 配置文件 walgit.toml"; Filename: "notepad.exe"; Parameters: """{%USERPROFILE}\.walgit\walgit.toml"""
Name: "{autodesktop}\walgit 托盘"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "walgit-tray"; ValueData: """{app}\walgit-tray.exe"""; Flags: uninsdeletevalue; Tasks: autostart

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchTray}"; Flags: nowait postinstall skipifsilent

[Code]
function LegacyProgramDir: String;
begin
  Result := ExpandConstant('{%USERPROFILE}\walgit');
end;

procedure MigrateLegacyState;
var
  NewDir, OldConfig, NewConfig: String;
begin
  NewDir := ExpandConstant('{%USERPROFILE}\.walgit');
  OldConfig := LegacyProgramDir + '\walgit.toml';
  NewConfig := NewDir + '\walgit.toml';
  if FileExists(OldConfig) and not FileExists(NewConfig) then
  begin
    ForceDirectories(NewDir);
    if CopyFile(OldConfig, NewConfig, False) then
      Log('migrated legacy walgit.toml to ' + NewConfig)
    else
      Log('WARNING: could not migrate ' + OldConfig);
  end;
end;

procedure StopWalgit;
var
  ResultCode: Integer;
  // AnsiString:LoadStringFromFile 的 var 参数是这个类型(Unicode string 会
  // 编译期 Type mismatch);pid 内容是 ASCII 数字,无损。
  pidbuf: AnsiString;
  pid: String;
begin
  // 换文件前结束新旧程序目录里的实例:
  // 1) 服务优先按 pidfile + 映像名**双过滤**精确杀——裸 /PID 会撞上 pid
  //    复用误杀无关进程(taskkill /FI 要求 PID 与 IMAGENAME 同时匹配);
  // 2) 清扫只按可执行文件路径圈定新旧安装目录,不碰机器上其他同名进程。
  // 失败一律忽略——多半本就没在跑。
  pidbuf := '';
  if not LoadStringFromFile(ExpandConstant('{%USERPROFILE}\.walgit\walgit.pid'), pidbuf) then
    LoadStringFromFile(LegacyProgramDir + '\walgit.pid', pidbuf);
  pid := Trim(pidbuf);
  if pid <> '' then
  begin
    Exec(ExpandConstant('{cmd}'),
      '/C taskkill /F /T /FI "PID eq ' + pid + '" /FI "IMAGENAME eq walgit.exe"',
      '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    DeleteFile(ExpandConstant('{%USERPROFILE}\.walgit\walgit.pid'));
    DeleteFile(LegacyProgramDir + '\walgit.pid');
  end;
  // 托盘升级管线留的备份:安装器换装后它已无意义,留着会在托盘某次升级
  // 健康检查失败时被回滚逻辑盖回旧版本——删。
  DeleteFile(ExpandConstant('{app}\walgit.bak-tray'));
  Exec(ExpandConstant('{cmd}'),
    '/C powershell -NoProfile -Command "Get-Process walgit,walgit-tray -ErrorAction SilentlyContinue | Where-Object { ($_.Path -like ''' +
    ExpandConstant('{app}') + '\*'') -or ($_.Path -like ''' +
    LegacyProgramDir + '\*'') } | Stop-Process -Force"',
    '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssInstall then
  begin
    MigrateLegacyState;
    StopWalgit;
  end;
  if CurStep = ssPostInstall then
    // 自启勾选承诺的是「部署开机可用」,不是只把托盘拉起来:写标记文件,
    // 托盘启动时发现它 + 服务未运行,就把服务一并拉起(tray-rs 读它)。
    if WizardIsTaskSelected('autostart') then
      SaveStringToFile(ExpandConstant('{%USERPROFILE}\.walgit\service.autostart'), '', False)
    else
      DeleteFile(ExpandConstant('{%USERPROFILE}\.walgit\service.autostart'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
  begin
    StopWalgit;
    DeleteFile(ExpandConstant('{%USERPROFILE}\.walgit\service.autostart'));
  end;
end;
