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
; `checkedonce`:没有它 Inno 的任务默认是**不勾选**的——装完不会开机自启(README
; 一直写着「默认勾选自启」,代码却没兑现)。桌面图标不做成可选项:checkedonce 只在首次
; 安装生效,升级会沿用上次的选择,已经装过且当时没勾的机器永远补不上图标。
Name: "autostart"; Description: "{cm:AutoStartTask}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: checkedonce


[Files]
Source: "..\..\target\release\walgit.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\target\release\walgit-tray.exe"; DestDir: "{app}"; Flags: ignoreversion
; 已有配置绝不覆盖;卸载也不删(用户数据)。写到状态目录而非程序目录。
Source: "walgit.toml.initial"; DestDir: "{%USERPROFILE}\.walgit"; DestName: "walgit.toml"; Flags: onlyifdoesntexist uninsneveruninstall

[Icons]
; 顶层再放一个 `walgit`:只放 {group} 文件夹的话,开始菜单「所有应用」里出现的是
; 文件夹名;托盘图标一旦被收进溢出区就没有别的入口(macOS 那侧的 Dock 图标
; #197/#200 是同一个诉求)。
Name: "{userprograms}\walgit"; Filename: "{app}\{#MyAppExeName}"
Name: "{group}\walgit 托盘"; Filename: "{app}\{#MyAppExeName}"
Name: "{group}\walgit 配置文件 walgit.toml"; Filename: "notepad.exe"; Parameters: """{%USERPROFILE}\.walgit\walgit.toml"""
Name: "{autodesktop}\walgit 托盘"; Filename: "{app}\{#MyAppExeName}"

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
  Script: String;
begin
  // 换文件前结束服务，顺序按「谁真正持有进程」来（D48）：
  // 1) 服务归任务计划程序：先 /End 那个具名任务。失败忽略——多半本就没起。
  // 2) 再按**监听端口**清扫：旧形态的 pidfile 会被写坏（写进已死子进程的 pid），
  //    端口才是真凭据。端口从用户配置里读，自定义端口不会被漏掉；只杀进程名以
  //    walgit 开头的，绝不误伤别人的进程。
  // 3) 最后按安装目录圈定托盘与本体。
  Exec(ExpandConstant('{cmd}'),
    '/C schtasks /End /TN walgit >NUL 2>&1',
    '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Script :=
    '$t = Join-Path $env:USERPROFILE ''\.walgit\walgit.toml''; ' +
    '$port = ''''; ' +
    'if (Test-Path $t) { ' +
    '$m = Select-String -Path $t -Pattern ''^\s*listen\s*='' | Select-Object -First 1; ' +
    'if ($m) { $port = ($m.Line -split '':'' | Select-Object -Last 1) -replace ''\D'',''''; } }; ' +
    'if ($port) { Get-NetTCPConnection -LocalPort ([int]$port) -State Listen -ErrorAction SilentlyContinue | ForEach-Object { ' +
    '$p = Get-Process -Id $_.OwningProcess -ErrorAction SilentlyContinue; ' +
    'if ($p -and $p.ProcessName -like ''walgit*'') { Stop-Process -Id $p.Id -Force } } }; ' +
    'Get-Process walgit,walgit-tray -ErrorAction SilentlyContinue | Where-Object { ($_.Path -like ''' +
    ExpandConstant('{app}') + '\*'') -or ($_.Path -like ''' +
    LegacyProgramDir + '\*'') } | Stop-Process -Force';
  Exec(ExpandConstant('{cmd}'),
    '/C powershell -NoProfile -Command "' + Script + '"',
    '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  // 托盘升级管线留的备份:安装器换装后它已无意义,留着会在托盘某次升级
  // 健康检查失败时被回滚逻辑盖回旧版本——删。
  DeleteFile(ExpandConstant('{app}\walgit.bak-tray'));
  // 旧形态遗留的 pidfile：D48 之后 Windows 不再有它的位置。
  DeleteFile(ExpandConstant('{%USERPROFILE}\.walgit\walgit.pid'));
  DeleteFile(LegacyProgramDir + '\walgit.pid');
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
    // 任务本身也要注销：留着它，下次重装会指向一个已删除的 exe（D48）。
    Exec(ExpandConstant('{cmd}'),
      '/C schtasks /Delete /TN walgit /F >NUL 2>&1',
      '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    DeleteFile(ExpandConstant('{%USERPROFILE}\.walgit\service.autostart'));
  end;
end;
