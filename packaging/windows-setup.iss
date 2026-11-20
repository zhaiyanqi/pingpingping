[Setup]
AppId={{866D7767-24DE-4D93-BE30-23951810358A}
AppName=PingPingPing
AppVersion={#AppVersion}
DefaultDirName={localappdata}\Programs\PingPingPing
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
DisableProgramGroupPage=yes
UninstallDisplayIcon={app}\pingpingping.exe
SetupIconFile=pingpingping.ico
Compression=lzma2/ultra
SolidCompression=yes
WizardStyle=modern
#ifdef SignWindowsPackage
SignTool=PingPingPing
SignedUninstaller=yes
#endif

[Files]
Source: "{#AppBinary}"; DestDir: "{app}"; Flags: ignoreversion
Source: "LICENSE-LUCIDE.txt"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\PingPingPing"; Filename: "{app}\pingpingping.exe"; WorkingDir: "{app}"

[Run]
Filename: "{app}\pingpingping.exe"; Description: "{cm:LaunchProgram,PingPingPing}"; Flags: nowait postinstall skipifsilent
