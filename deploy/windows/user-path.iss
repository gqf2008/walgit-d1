// deploy/windows/user-path.iss —— 用户级 PATH 的增删(installer.iss 的 [Code] 片段)。
//
// 这是一段**片段**(没有区段头),由 installer.iss 用 #include 引入。单独成文件是为了让
// 本机探针 include 同一份源码——被测代码即随包代码(2026-10-07,线程 win-installer-user-path)。
//
// 契约(前两条由 CI windows leg 的静默安装步骤对真实 setup.exe 断言):
//   1. 安装:把 {app} 追加进用户 PATH;幂等——大小写、结尾反斜杠、带引号的等价写法算同一条。
//   2. 卸载:只摘掉自己那一条,其余条目**逐字节**不变(含以 ';' 结尾的空条目);摘空则删掉
//      整个值(摘掉后只剩下空条目/空串 → 整值删除:安装前"存在但是空串"的,卸载后是"不存在",
//      两者语义相同);原值是 REG_SZ 的,写入后按 REG_EXPAND_SZ 存(文本不变,类型变)。
//   3. 一律按 REG_EXPAND_SZ 的**原文**读写:用户 PATH 里别人的 %USERPROFILE% 之类不许被展开成
//      字面量(展开写回会永久改掉别人的条目),自己写回去的也仍是 REG_EXPAND_SZ。
//   4. 写失败只记日志,不让安装/卸载失败——少一条 PATH 不该让整次安装回滚(静默安装里
//      也没有可以问的人)。
//
// 为什么是用户级(HKCU\Environment):安装器 PrivilegesRequired=lowest,不碰 HKLM;Windows
// 在建进程环境时把用户 Path 接在系统 Path 之后,所以这里只写自己那一段。
//
// 追加在**末尾**(不抢先):别人的 walgit 在前就还是他们的,我们只保证"能用"而不是"用我们的"。
// 用户自己早就手加过同一条 → 安装是 no-op,卸载会把它摘掉——它指向的正是将被删除的程序目录。
// 已知边界:判据只有当前 {app}。装了 A 目录、又改到 B 目录(UsePreviousAppDir=no 允许)之后,
// A 那条会留在 PATH 里(它仍指向旧程序目录);要清得手动删。换目录不是常态,不引入"上次注册目录"状态。
//
// 为什么走 [Code] 而不是 [Registry] + {olddata}:卸载侧 Inno 只能整值删除,表达不了
// "只摘掉自己那一条、其余原样留着"。
// 为什么通知环境变化的是 [Setup] 的 ChangesEnvironment 而不是这里的 SendMessageTimeout:
// Inno 自带这条(安装结束广播 WM_SETTINGCHANGE),少一个需要手写的外部函数。

const
  UserEnvSubKey = 'Environment';
  UserEnvPathValue = 'Path';

// 判等形态:去首尾空白、去成对引号、去结尾反斜杠。PATH 里 "C:\dir"、C:\dir\、C:\DIR
// 说的是同一个目录;漏判会让安装写出重复条目、卸载摘不干净。
function NormalizeUserPathEntry(const Entry: String): String;
var
  S: String;
begin
  S := Trim(Entry);
  // '"' 在 Windows 目录名里非法,所以引号一律去掉:PATH 里带引号的条目(不推荐但合法)与不带
  // 引号说的是同一个目录;只去"成对"引号会漏掉 "C:\dir\" 这种写法。
  StringChangeEx(S, '"', '', True);
  while (Length(S) > 0) and (S[Length(S)] = '\') do
    Delete(S, Length(S), 1);
  Result := S;
end;

function SameUserPathEntry(const A, B: String): Boolean;
begin
  Result := CompareText(NormalizeUserPathEntry(A), NormalizeUserPathEntry(B)) = 0;
end;

// 按 ';' 切分并**保留空条目**:切分再拼回是恒等变换,所以"其余条目逐字节不变"成立
// (连续分号、结尾分号、开头的空条目都不会被顺手清掉——那不是我们的东西)。
function SplitUserPathValue(const Value: String): TArrayOfString;
var
  Rest, Entry: String;
  Sep: Integer;
begin
  SetArrayLength(Result, 0);
  Rest := Value;
  repeat
    Sep := Pos(';', Rest);
    if Sep = 0 then
    begin
      Entry := Rest;
      Rest := '';
    end
    else
    begin
      Entry := Copy(Rest, 1, Sep - 1);
      Rest := Copy(Rest, Sep + 1, Length(Rest) - Sep);
    end;
    SetArrayLength(Result, GetArrayLength(Result) + 1);
    Result[GetArrayLength(Result) - 1] := Entry;
  until (Sep = 0) and (Rest = '');
end;

function JoinUserPathValue(const Entries: TArrayOfString): String;
var
  I: Integer;
begin
  Result := '';
  for I := 0 to GetArrayLength(Entries) - 1 do
  begin
    if I > 0 then
      Result := Result + ';';
    Result := Result + Entries[I];
  end;
end;

// 读原文。Exists=False 表示这个值根本不存在(与"存在但是空串"区别对待:卸载要还原到
// 安装前的形态)。读失败(返回值 False)与不存在是两回事,调用方不能混。
function ReadUserPathValue(const SubKey: String; var Value: String; var Exists: Boolean): Boolean;
begin
  Value := '';
  Exists := RegValueExists(HKEY_CURRENT_USER, SubKey, UserEnvPathValue);
  if not Exists then
  begin
    Result := True;
    exit;
  end;
  Result := RegQueryStringValue(HKEY_CURRENT_USER, SubKey, UserEnvPathValue, Value);
end;

// 追加(幂等)。True = 现在 PATH 里确实有 Dir(本来就有的也算)。
function AddDirToUserPath(const SubKey, Dir: String): Boolean;
var
  Value, NewValue: String;
  Entries: TArrayOfString;
  Exists: Boolean;
  I: Integer;
begin
  Result := False;
  if Trim(Dir) = '' then
  begin
    Log('ERROR: user-path: empty directory, nothing to register');
    exit;
  end;
  if not ReadUserPathValue(SubKey, Value, Exists) then
  begin
    Log('ERROR: user-path: cannot read HKCU\' + SubKey + '\' + UserEnvPathValue);
    exit;
  end;
  Entries := SplitUserPathValue(Value);
  for I := 0 to GetArrayLength(Entries) - 1 do
    if SameUserPathEntry(Entries[I], Dir) then
    begin
      Log('user-path: already on PATH: ' + Dir);
      Result := True;
      exit;
    end;
  // 只追加,一个字节都不改写别人的:值非空就无条件再补一个分隔符。
  // (值以 ';' 结尾时,那个分号本身是一个**空条目**;直接拼 Dir 会把它吃掉,卸载就还原不回原样。)
  if Value = '' then
    NewValue := Dir
  else
    NewValue := Value + ';' + Dir;
  if not RegWriteExpandStringValue(HKEY_CURRENT_USER, SubKey, UserEnvPathValue, NewValue) then
  begin
    Log('ERROR: user-path: cannot write HKCU\' + SubKey + '\' + UserEnvPathValue);
    exit;
  end;
  Log('user-path: registered on PATH: ' + Dir);
  Result := True;
end;

// 只摘掉与 Dir 等价的条目(全部,重复的也摘)。True = 现在 PATH 里没有 Dir。
function RemoveDirFromUserPath(const SubKey, Dir: String): Boolean;
var
  Value, NewValue: String;
  Entries, Kept: TArrayOfString;
  Exists, Found: Boolean;
  I: Integer;
begin
  Result := False;
  if Trim(Dir) = '' then
  begin
    Log('ERROR: user-path: empty directory, nothing to remove');
    exit;
  end;
  if not ReadUserPathValue(SubKey, Value, Exists) then
  begin
    Log('ERROR: user-path: cannot read HKCU\' + SubKey + '\' + UserEnvPathValue);
    exit;
  end;
  if not Exists then
  begin
    Log('user-path: no user PATH value; nothing to remove');
    Result := True;
    exit;
  end;
  Entries := SplitUserPathValue(Value);
  SetArrayLength(Kept, 0);
  Found := False;
  for I := 0 to GetArrayLength(Entries) - 1 do
  begin
    // 空条目永远不是"我们的那一",原样留下。
    if (Trim(Entries[I]) <> '') and SameUserPathEntry(Entries[I], Dir) then
      Found := True
    else
    begin
      SetArrayLength(Kept, GetArrayLength(Kept) + 1);
      Kept[GetArrayLength(Kept) - 1] := Entries[I];
    end;
  end;
  NewValue := JoinUserPathValue(Kept);
  if not Found then
  begin
    Log('user-path: not on PATH: ' + Dir);
    Result := True;
    exit;
  end;
  if Trim(NewValue) = '' then
  begin
    // 摘空 = 安装前就没有这个值:删掉它,而不是留下一个空 PATH。
    if not RegDeleteValue(HKEY_CURRENT_USER, SubKey, UserEnvPathValue) then
    begin
      Log('ERROR: user-path: cannot delete HKCU\' + SubKey + '\' + UserEnvPathValue);
      exit;
    end;
    Log('user-path: removed ' + Dir + '; the empty user PATH value was deleted');
  end
  else
  begin
    if not RegWriteExpandStringValue(HKEY_CURRENT_USER, SubKey, UserEnvPathValue, NewValue) then
    begin
      Log('ERROR: user-path: cannot write HKCU\' + SubKey + '\' + UserEnvPathValue);
      exit;
    end;
    Log('user-path: removed from PATH: ' + Dir);
  end;
  Result := True;
end;

// installer.iss 只经这两个入口调用;判据是 {app}(程序目录里就是 walgit.exe)。
function AddAppToUserPath(): Boolean;
begin
  Result := AddDirToUserPath(UserEnvSubKey, ExpandConstant('{app}'));
end;

function RemoveAppFromUserPath(): Boolean;
begin
  Result := RemoveDirFromUserPath(UserEnvSubKey, ExpandConstant('{app}'));
end;
