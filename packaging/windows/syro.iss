#ifndef MyAppVersion
  #define MyAppVersion "0.1.0"
#endif
#ifndef MyArch
  #define MyArch "x64"
#endif
#ifndef MySource
  #define MySource "..\..\target\release\syro.exe"
#endif

[Setup]
AppName=syro
AppVersion={#MyAppVersion}
AppPublisher=osdodo
DefaultDirName={autopf}\syro
DefaultGroupName=syro
UninstallDisplayIcon={app}\syro.exe
OutputDir=..\..\dist
OutputBaseFilename=syro-{#MyAppVersion}-windows-{#MyArch}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed={#MyArch}
ArchitecturesInstallIn64BitMode={#MyArch}

[Files]
Source: "{#MySource}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\syro"; Filename: "{app}\syro.exe"
