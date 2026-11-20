; TrackCut — установщик для Inno Setup 6.
; Обычно запускается из pack.ps1, вручную:
;   ISCC.exe /DMyAppVersion=0.1.0 /DStageDir="<путь>\dist\TrackCut-0.1.0" installer\TrackCut.iss
;
; Установка идёт в профиль пользователя (%LOCALAPPDATA%\Programs\TrackCut),
; права администратора не нужны. Настройки программы остаются в %APPDATA%\TrackCut.

#ifndef MyAppVersion
  #define MyAppVersion "0.1.0"
#endif

#ifndef StageDir
  #error Не задан StageDir — укажите /DStageDir=<папка со сборкой>
#endif

#define MyAppName "TrackCut"
#define MyAppExeName "trackcut.exe"
#define MyAppPublisher "TrackCut"
#define MyAppURL "https://github.com/JetcocosRun/trackcut"

[Setup]
AppId={{7C4E2B1A-9D3F-4A6E-8B21-5F0C7A9D3E42}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
AppUpdatesURL={#MyAppURL}
VersionInfoVersion={#MyAppVersion}
VersionInfoCompany={#MyAppPublisher}
VersionInfoDescription=TrackCut {#MyAppVersion}
VersionInfoProductName={#MyAppName}
DefaultDirName={localappdata}\Programs\TrackCut
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
OutputDir=..\dist
OutputBaseFilename=TrackCut-{#MyAppVersion}-setup
SetupIconFile=..\crates\gui\assets\icon.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
UninstallDisplayName={#MyAppName} {#MyAppVersion}
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
MinVersion=10.0
ChangesAssociations=yes
CloseApplications=no

[Languages]
Name: "russian"; MessagesFile: "compiler:Languages\Russian.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[CustomMessages]
russian.TaskShellMenu=Добавить «Открыть в TrackCut» в меню Проводника
russian.TaskDesktopIcon=Создать ярлык на рабочем столе
russian.AdditionalIcons=Дополнительно:
russian.ExplorerMenu=Открыть в TrackCut
russian.LaunchProgram=Запустить TrackCut
english.TaskShellMenu=Add "Open in TrackCut" to the Explorer menu
english.TaskDesktopIcon=Create a desktop shortcut
english.AdditionalIcons=Additional shortcuts:
english.ExplorerMenu=Open in TrackCut
english.LaunchProgram=Launch TrackCut

[Tasks]
Name: "shellmenu"; Description: "{cm:TaskShellMenu}"; GroupDescription: "{cm:AdditionalIcons}"
Name: "desktopicon"; Description: "{cm:TaskDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
; portable.ini в установленную версию не попадает: настройки идут в %APPDATA%\TrackCut.
Source: "{#StageDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs; Excludes: "portable.ini"

[Icons]
Name: "{group}\TrackCut"; Filename: "{app}\{#MyAppExeName}"
Name: "{group}\README"; Filename: "{app}\README.md"
Name: "{group}\{cm:UninstallProgram,{#MyAppName}}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\TrackCut"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Registry]
; Те же ключи, что создаёт кнопка «Добавить в меню Проводника» в настройках программы.
Root: HKCU; Subkey: "Software\Classes\SystemFileAssociations\.mkv\shell\TrackCut"; ValueType: string; ValueName: ""; ValueData: "{cm:ExplorerMenu}"; Flags: uninsdeletekey; Tasks: shellmenu
Root: HKCU; Subkey: "Software\Classes\SystemFileAssociations\.mkv\shell\TrackCut"; ValueType: string; ValueName: "Icon"; ValueData: "{app}\{#MyAppExeName}"; Tasks: shellmenu
Root: HKCU; Subkey: "Software\Classes\SystemFileAssociations\.mkv\shell\TrackCut\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExeName}"" ""%1"""; Tasks: shellmenu
Root: HKCU; Subkey: "Software\Classes\SystemFileAssociations\.mp4\shell\TrackCut"; ValueType: string; ValueName: ""; ValueData: "{cm:ExplorerMenu}"; Flags: uninsdeletekey; Tasks: shellmenu
Root: HKCU; Subkey: "Software\Classes\SystemFileAssociations\.mp4\shell\TrackCut"; ValueType: string; ValueName: "Icon"; ValueData: "{app}\{#MyAppExeName}"; Tasks: shellmenu
Root: HKCU; Subkey: "Software\Classes\SystemFileAssociations\.mp4\shell\TrackCut\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExeName}"" ""%1"""; Tasks: shellmenu
Root: HKCU; Subkey: "Software\Classes\SystemFileAssociations\.mov\shell\TrackCut"; ValueType: string; ValueName: ""; ValueData: "{cm:ExplorerMenu}"; Flags: uninsdeletekey; Tasks: shellmenu
Root: HKCU; Subkey: "Software\Classes\SystemFileAssociations\.mov\shell\TrackCut"; ValueType: string; ValueName: "Icon"; ValueData: "{app}\{#MyAppExeName}"; Tasks: shellmenu
Root: HKCU; Subkey: "Software\Classes\SystemFileAssociations\.mov\shell\TrackCut\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExeName}"" ""%1"""; Tasks: shellmenu
Root: HKCU; Subkey: "Software\Classes\SystemFileAssociations\.webm\shell\TrackCut"; ValueType: string; ValueName: ""; ValueData: "{cm:ExplorerMenu}"; Flags: uninsdeletekey; Tasks: shellmenu
Root: HKCU; Subkey: "Software\Classes\SystemFileAssociations\.webm\shell\TrackCut"; ValueType: string; ValueName: "Icon"; ValueData: "{app}\{#MyAppExeName}"; Tasks: shellmenu
Root: HKCU; Subkey: "Software\Classes\SystemFileAssociations\.webm\shell\TrackCut\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExeName}"" ""%1"""; Tasks: shellmenu
Root: HKCU; Subkey: "Software\Classes\SystemFileAssociations\.avi\shell\TrackCut"; ValueType: string; ValueName: ""; ValueData: "{cm:ExplorerMenu}"; Flags: uninsdeletekey; Tasks: shellmenu
Root: HKCU; Subkey: "Software\Classes\SystemFileAssociations\.avi\shell\TrackCut"; ValueType: string; ValueName: "Icon"; ValueData: "{app}\{#MyAppExeName}"; Tasks: shellmenu
Root: HKCU; Subkey: "Software\Classes\SystemFileAssociations\.avi\shell\TrackCut\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExeName}"" ""%1"""; Tasks: shellmenu
Root: HKCU; Subkey: "Software\Classes\SystemFileAssociations\.ts\shell\TrackCut"; ValueType: string; ValueName: ""; ValueData: "{cm:ExplorerMenu}"; Flags: uninsdeletekey; Tasks: shellmenu
Root: HKCU; Subkey: "Software\Classes\SystemFileAssociations\.ts\shell\TrackCut"; ValueType: string; ValueName: "Icon"; ValueData: "{app}\{#MyAppExeName}"; Tasks: shellmenu
Root: HKCU; Subkey: "Software\Classes\SystemFileAssociations\.ts\shell\TrackCut\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExeName}"" ""%1"""; Tasks: shellmenu

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram}"; Flags: nowait postinstall skipifsilent
