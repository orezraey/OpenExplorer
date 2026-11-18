; Inno Setup script for the Windows installer. The release workflow builds it:
;   iscc /DAppVersion=0.1.0 /DSourceExe=path\to\openexplorer.exe /DOutputName=... installer\openexplorer.iss

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceExe
  #define SourceExe "..\target\release\openexplorer.exe"
#endif
#ifndef OutputName
  #define OutputName "OpenExplorer-setup"
#endif

[Setup]
; Never change AppId: it's how an update finds the installed copy.
AppId={{6F2B3C4E-8A1D-4F5B-9C7E-2D4A6B8E0F13}
AppName=Open Explorer
AppVersion={#AppVersion}
AppPublisher=Open Explorer
AppPublisherURL=https://github.com/orezraey/OpenExplorer
DefaultDirName={autopf}\OpenExplorer
DefaultGroupName=Open Explorer
DisableProgramGroupPage=yes
; Program Files, and the search service, need an administrator.
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\dist
OutputBaseFilename={#OutputName}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\openexplorer.exe
UninstallDisplayName=Open Explorer
; An update closes a running copy first.
CloseApplications=yes
RestartApplications=no
ShowLanguageDialog=auto

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "brazilianportuguese"; MessagesFile: "compiler:Languages\BrazilianPortuguese.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Open Explorer"; Filename: "{app}\openexplorer.exe"
Name: "{autodesktop}\Open Explorer"; Filename: "{app}\openexplorer.exe"; Tasks: desktopicon

[Run]
; An update brings the search service, a copy of the app, up to the new version.
Filename: "{app}\openexplorer.exe"; Parameters: "index install"; Flags: runhidden waituntilterminated; Check: SearchServiceInstalled
Filename: "{app}\openexplorer.exe"; Description: "{cm:LaunchProgram,Open Explorer}"; Flags: nowait postinstall skipifsilent runasoriginaluser

[UninstallRun]
; The search service goes with the app. It may have been installed after the app, so this runs
; whatever was there at install time; with no service it only fails quietly.
Filename: "{app}\openexplorer.exe"; Parameters: "index uninstall"; Flags: runhidden waituntilterminated; RunOnceId: "RemoveSearchService"

[Code]
function SearchServiceInstalled: Boolean;
begin
  Result := RegKeyExists(HKLM, 'SYSTEM\CurrentControlSet\Services\OpenExplorerIndex');
end;
