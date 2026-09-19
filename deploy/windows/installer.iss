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
Source: "..\..\target\release\walgit-upgrade-helper.exe"; DestDir: "{app}"; Flags: ignoreversion
; CI 与安装器共用的任务归属探测。必须作为普通文件装到 {app}；Inno 的临时解压 API
; 注册为 sfNoUninstall，而卸载路径也会调用归属探测，不能在安装器里依赖它。
Source: "task-ownership.ps1"; DestDir: "{app}"; Flags: ignoreversion
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

// 路径要嵌进 PowerShell 的单引号字符串里，先按 PS 规则转义单引号。
function PsQuote(s: String): String;
begin
  // Inno 的 StringChangeEx 是就地修改的 procedure（不是返回新串的函数）——
  // 按函数用会得到 "Type mismatch"（这一条正是 windows leg 的 ISCC 步骤抓到的）。
  StringChangeEx(s, '''', '''''', True);
  Result := s;
end;

const
  TaskStateUnknown = -1;
  TaskStateAbsent = 0;
  TaskStateOurs = 1;
  TaskStateForeign = 2;

// 只回答“任务是否确定不存在”：NotFound -> 0，存在 -> 2，查询失败 -> 3。
// 这用于 ssInstall 阶段脚本尚未落盘时的安全旁路，不参与归属判断。
function TaskIsAbsent: Boolean;
var
  ResultCode: Integer;
begin
  Result := Exec(ExpandConstant('{cmd}'),
    '/C powershell -NoProfile -ExecutionPolicy Bypass -Command "' +
    '$ErrorActionPreference = ''Stop''; ' +
    'try { Get-ScheduledTask -TaskName ''walgit'' -ErrorAction Stop | Out-Null; exit 2 } ' +
    'catch { if ($_.FullyQualifiedErrorId -like ''CmdletizationQuery_NotFound*'') { exit 0 } else { exit 3 } }"',
    '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  if Result then
    Result := ResultCode = 0;
end;

// 只有确认这个 `walgit` 任务确实指向 walgit 二进制时，才允许 End/Delete：
// 名字撞车的别人的任务不能碰（和「按端口清扫只杀 walgit* 进程」同一条原则）。
// 查询失败与任务不存在必须分开：前者是未知，不能静默当成“无需删除”。
function TaskOwnership: Integer;
var
  ResultCode: Integer;
  OutFile, ScriptFile, Status: String;
  Lines: TArrayOfString;
begin
  Result := TaskStateUnknown;
  OutFile := ExpandConstant('{tmp}\walgit-task-query.txt');
  ScriptFile := ExpandConstant('{app}\task-ownership.ps1');
  DeleteFile(OutFile);
  if not FileExists(ScriptFile) then
  begin
    // ssInstall 发生在 [Files] 之前；首次安装没有旧任务时可以安全返回
    // Absent，任何无法证明不存在的情况都保持 Unknown，交给调用方询问用户。
    if TaskIsAbsent then
      Result := TaskStateAbsent
    else
      Log('ERROR: task-ownership.ps1 is missing and task absence cannot be proven');
    exit;
  end;
  if not Exec(ExpandConstant('{cmd}'),
    '/C powershell -NoProfile -ExecutionPolicy Bypass -File "' + ScriptFile +
    '" -TaskName walgit -OutFile "' + OutFile + '"',
    '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then
  begin
    Log('ERROR: could not run the walgit task-ownership probe');
    exit;
  end;
  if ResultCode <> 0 then
  begin
    Log('ERROR: walgit task-ownership probe exited ' + IntToStr(ResultCode));
    exit;
  end;
  if LoadStringsFromFile(OutFile, Lines) and (GetArrayLength(Lines) > 0) then
  begin
    Status := Trim(Lines[0]);
    if Status = 'ours' then
      Result := TaskStateOurs
    else if Status = 'absent' then
      Result := TaskStateAbsent
    else if Status = 'foreign' then
      Result := TaskStateForeign
    else
      Log('ERROR: walgit task-ownership probe returned: ' + Status);
  end
  else
    Log('ERROR: walgit task-ownership probe produced no status');
  DeleteFile(OutFile);
end;

// `/End` 只负责发起停止；真正的判据是任务不再处于 Running。查询失败也必须
// 让用户决定，不能把“没法确认”当成“已经停止”。
function TaskIsStopped: Boolean;
var
  ResultCode: Integer;
begin
  Result := Exec(ExpandConstant('{cmd}'),
    '/C powershell -NoProfile -ExecutionPolicy Bypass -Command "' +
    '$ErrorActionPreference = ''Stop''; ' +
    'try { $t = Get-ScheduledTask -TaskName ''walgit'' -ErrorAction Stop } ' +
    'catch { if ($_.FullyQualifiedErrorId -like ''CmdletizationQuery_NotFound*'') { exit 0 } else { exit 4 } }; ' +
    'if ($t.State -eq ''Ready'' -or $t.State -eq ''Disabled'') { exit 0 }; ' +
    'for ($i = 0; $i -lt 20; $i++) { Start-Sleep -Milliseconds 250; ' +
    'try { $t = Get-ScheduledTask -TaskName ''walgit'' -ErrorAction Stop } catch { exit 4 }; ' +
    'if ($t.State -eq ''Ready'' -or $t.State -eq ''Disabled'') { exit 0 } }; exit 3"',
    '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  if Result then
    Result := ResultCode = 0;
end;

procedure StopWalgit(AskAboutUnknown: Boolean);
var
  ResultCode: Integer;
  Script: String;
  Ownership: Integer;
  EndOk: Boolean;
begin
  // 换文件前结束服务，顺序按「谁真正持有进程」来（D48）：
  // 1) 服务归任务计划程序：先 /End 那个具名任务，再验证 State 不再是 Running；
  //    失败时让用户显式决定是否继续，不能把“无法确认”当作“已经停止”。
  // 2) 再按**监听端口**清扫：旧形态的 pidfile 会被写坏（写进已死子进程的 pid），
  //    端口才是真凭据。端口从用户配置里读，自定义端口不会被漏掉；只杀进程名以
  //    walgit 开头的，绝不误伤别人的进程。
  // 3) 最后按安装目录圈定托盘与本体。
  Ownership := TaskOwnership;
  if Ownership = TaskStateOurs then
  begin
    EndOk := Exec(ExpandConstant('{cmd}'),
      '/C schtasks /End /TN walgit >NUL 2>&1',
      '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    if not EndOk then
      Log('WARNING: could not run schtasks /End for the walgit task');
    if EndOk and (ResultCode <> 0) then
      Log('WARNING: schtasks /End exited ' + IntToStr(ResultCode));
    if not TaskIsStopped then
      if MsgBox('walgit: /End 后无法确认计划任务已经停止；任务可能仍在运行。' + #13#10 +
        '继续安装可能让下一次 start 被 IgnoreNew 挡住。仍要继续吗？',
        mbConfirmation, MB_YESNO) <> IDYES then
        Abort;
  end;
  if Ownership = TaskStateUnknown then
  begin
    if AskAboutUnknown then
      if MsgBox('walgit: 无法确认计划任务 walgit 是否属于本程序，安装器不会自动结束它。' + #13#10 +
        '是否继续安装/升级？', mbConfirmation, MB_YESNO) <> IDYES then
        Abort;
  end;
  Script :=
    '$t = Join-Path $env:USERPROFILE ''\.walgit\walgit.toml''; ' +
    '$q = [char]34; $port = '''' ; ' +
    'if (Test-Path $t) { ' +
    '$m = Select-String -Path $t -Pattern ''^\s*listen\s*='' | Select-Object -First 1; ' +
    'if ($m -and ($m.Line -match (''listen\s*=\s*'' + $q + ''([^'' + $q + '']+)'' + $q))) ' +
    '{ $port = ($Matches[1] -split '':'' | Select-Object -Last 1) } }; ' +
    'if (-not ($port -match ''^\d+$'')) { $port = ''8081'' }; ' +
    'Get-NetTCPConnection -LocalPort ([int]$port) -State Listen -ErrorAction SilentlyContinue | ForEach-Object { ' +
    '$p = Get-Process -Id $_.OwningProcess -ErrorAction SilentlyContinue; ' +
    'if ($p -and $p.ProcessName -match ''^(?i)(walgit|walgit-server)$'') { Stop-Process -Id $p.Id -Force } }; ' +
    'Get-Process walgit,walgit-tray,walgit-server -ErrorAction SilentlyContinue | Where-Object { ($_.Path -like ''' +
    PsQuote(ExpandConstant('{app}')) + '\*'') -or ($_.Path -like ''' +
    PsQuote(LegacyProgramDir) + '\*'') } | Stop-Process -Force; ' +
    // The sweeps above are best-effort (a quietly failing query used to look like
    // "nothing to kill"); the last word is a *verified* check, so any non-zero
    // exit means we could not prove the port is free and the caller must not
    // replace files over a live server.
    'if ($port -match ''^\d+$'') { $still = Get-NetTCPConnection -LocalPort ([int]$port) -State Listen -ErrorAction Stop; ' +
    'if ($still) { exit 3 } }';
  Exec(ExpandConstant('{cmd}'),
    '/C powershell -NoProfile -Command "' + Script + '"',
    '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  // The sweep's last step *proves* the port is free: a non-zero exit means it
  // could not (still listening, or the query itself failed). Replacing files over
  // a live server is the bug this whole path exists to prevent, so ask rather
  // than proceed silently.
  if ResultCode <> 0 then
  begin
    // 托盘升级使用 /SUPPRESSMSGBOXES；Inno 对 MB_YESNO 取默认 IDYES，
    // 所以这里不会卡住无人值守升级。真正的失败兜底是安装完成后的
    // `walgit.exe --version` 与 `/healthz` 双重校验：替换运行中文件、
    // 服务仍占端口等问题都会在那里失败，并触发旧安装器回滚。
    if MsgBox('walgit: 无法确认服务已停止（端口可能仍被占用）。' + #13#10 +
      '继续安装会替换正在运行的二进制，旧进程会继续占用端口。仍要继续吗？',
      mbConfirmation, MB_YESNO) <> IDYES then
      Abort;
  end;
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
    StopWalgit(True);
  end;
  if CurStep = ssPostInstall then
    // 安装目录标记:托盘据此识别自定义 {app}(默认目录另有路径判据)。这个
    // 标记是只读部署信息，不写用户状态。
    SaveStringToFile(ExpandConstant('{app}\.walgit-install'), '{#MyAppVersion}', False);
    // 自启勾选承诺的是「部署开机可用」,不是只把托盘拉起来:写标记文件,
    // 托盘启动时发现它 + 服务未运行,就把服务一并拉起(tray-rs 读它)。
    if WizardIsTaskSelected('autostart') then
      SaveStringToFile(ExpandConstant('{%USERPROFILE}\.walgit\service.autostart'), '', False)
    else
      DeleteFile(ExpandConstant('{%USERPROFILE}\.walgit\service.autostart'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  ResultCode: Integer;
  Ownership: Integer;
begin
  if CurUninstallStep = usUninstall then
  begin
    StopWalgit(False);
    // 任务本身也要注销：留着它，下次重装会指向一个已删除的 exe（D48）。
    // 同样先确认归属，别删掉别人的同名任务。
    Ownership := TaskOwnership;
    if Ownership = TaskStateOurs then
      Exec(ExpandConstant('{cmd}'),
        '/C schtasks /Delete /TN walgit /F >NUL 2>&1',
        '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    if Ownership = TaskStateUnknown then
    begin
      if MsgBox('walgit: 无法确认计划任务 walgit 是否属于本程序。' + #13#10 +
        '是否仍然删除同名任务？', mbConfirmation, MB_YESNO) <> IDYES then
        Abort;
      Exec(ExpandConstant('{cmd}'),
        '/C schtasks /Delete /TN walgit /F >NUL 2>&1',
        '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    end;
    DeleteFile(ExpandConstant('{%USERPROFILE}\.walgit\service.autostart'));
    DeleteFile(ExpandConstant('{app}\.walgit-install'));
  end;
end;
