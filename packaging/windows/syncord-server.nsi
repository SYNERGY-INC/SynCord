; SynCord Server installer: sets up the server to run in the background and
; start with Windows. Needs administrator rights.
;
; Build:  makensis -DVERSION=0.1.0 -DBIN_DIR=<folder with syncord-server.exe> -DOUT_FILE=<setup.exe> syncord-server.nsi

Unicode true
!include "MUI2.nsh"
!include "x64.nsh"

!ifndef VERSION
  !error "Pass -DVERSION=x.y.z"
!endif
!ifndef BIN_DIR
  !error "Pass -DBIN_DIR=<folder containing syncord-server.exe>"
!endif
!ifndef OUT_FILE
  !define OUT_FILE "SynCord-Server-Setup-${VERSION}.exe"
!endif

!define APP "SynCord Server"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\SynCordServer"
!define DATA "$DataDir"
Var DataDir
; Always use 64-bit PowerShell (the installer itself is 32-bit; see DisableX64FSRedirection below).
!define PS '"$WINDIR\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -File'

Name "${APP}"
OutFile "${OUT_FILE}"
InstallDir "$PROGRAMFILES64\${APP}"
RequestExecutionLevel admin
SetCompressor /SOLID lzma
BrandingText "${APP} ${VERSION}"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${APP}"
VIAddVersionKey "FileDescription" "${APP} installer"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "MIT licensed"

!define MUI_ICON "..\icons\syncord.ico"
!define MUI_UNICON "..\icons\syncord.ico"
!define MUI_ABORTWARNING
!define MUI_WELCOMEPAGE_TITLE "Set up a SynCord server"
!define MUI_WELCOMEPAGE_TEXT "Install this on the one PC that will host your SynCord server, ideally one that stays on.$\r$\n$\r$\nThe server will:$\r$\n  - run in the background and start automatically with Windows$\r$\n  - be allowed through Windows Firewall (port 7878)$\r$\n  - keep all messages in C:\ProgramData\SynCord Server$\r$\n$\r$\nYour friends install the regular SynCord app, not this."
!define MUI_FINISHPAGE_TITLE "SynCord Server is set up"
!define MUI_FINISHPAGE_TEXT "Friends connect by typing this PC's address into SynCord. On your home network that's its local IP (run 'ipconfig'); over the internet use Tailscale or forward port 7878 (TCP and UDP) on your router.$\r$\n$\r$\nThey'll also need the invite code from the settings file. The first account created becomes the server owner, so register yours first.$\r$\n$\r$\nFind 'SynCord Server' in the Start menu to change settings, restart it, or run it in a window to see what it's doing.$\r$\n$\r$\nFor GIF search, paste a free GIPHY or KLIPY key into the settings file (giphy_api_key or klipy_api_key) and restart the server."
!define MUI_FINISHPAGE_SHOWREADME
!define MUI_FINISHPAGE_SHOWREADME_TEXT "Show the invite code and settings"
!define MUI_FINISHPAGE_SHOWREADME_FUNCTION ShowSettings

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

; C:\ProgramData\SynCord Server (read from the environment so it works on any Windows language).
!macro SetDataDir
  ReadEnvStr $DataDir "PROGRAMDATA"
  StrCmp $DataDir "" 0 +2
    StrCpy $DataDir "C:\ProgramData"
  StrCpy $DataDir "$DataDir\SynCord Server"
!macroend

Function .onInit
  !insertmacro SetDataDir
FunctionEnd

Function un.onInit
  !insertmacro SetDataDir
FunctionEnd

Function ShowSettings
  Exec 'notepad.exe "${DATA}\syncord-server.toml"'
FunctionEnd

Section "Install"
  SetShellVarContext all
  ${DisableX64FSRedirection}
  ; Stop an older version that's already running.
  IfFileExists "$INSTDIR\server-remove.ps1" 0 +2
    nsExec::ExecToLog '${PS} "$INSTDIR\server-remove.ps1"'

  SetOutPath "$INSTDIR"
  File "${BIN_DIR}\syncord-server.exe"
  File "server-setup.ps1"
  File "server-remove.ps1"
  File "restart-server.ps1"
  WriteUninstaller "$INSTDIR\Uninstall.exe"

  DetailPrint "Starting the server and adding it to Windows startup..."
  nsExec::ExecToLog '${PS} "$INSTDIR\server-setup.ps1" -Exe "$INSTDIR\syncord-server.exe" -DataDir "${DATA}"'
  Pop $0
  ; $0 is the script's exit code: a sum of problems (see server-setup.ps1), or "error".
  StrCpy $1 ""
  StrCmp $0 "error" 0 +3
    StrCpy $1 "- PowerShell couldn't be started, so the server wasn't set up. Use 'Run server in a window' from the Start menu.$\r$\n"
    Goto report
  IntOp $2 $0 & 1
  IntCmp $2 0 +2
    StrCpy $1 "$1- The setup script stopped unexpectedly (code $0).$\r$\n"
  IntOp $2 $0 & 2
  IntCmp $2 0 +2
    StrCpy $1 "$1- Windows Firewall couldn't be set up (another firewall or antivirus may be in charge). If friends can't connect, allow syncord-server.exe in it.$\r$\n"
  IntOp $2 $0 & 4
  IntCmp $2 0 +2
    StrCpy $1 "$1- It couldn't be set to start with Windows. It's running now; after restarting the PC, use 'Restart server' from the Start menu.$\r$\n"
  IntOp $2 $0 & 8
  IntCmp $2 0 +2
    StrCpy $1 "$1- The server isn't answering. Use 'Run server in a window' from the Start menu to see the error.$\r$\n"
  report:
  StrCmp $1 "" +2
    MessageBox MB_ICONEXCLAMATION "SynCord Server was installed, but:$\r$\n$\r$\n$1$\r$\nDetails: ${DATA}\setup.log" /SD IDOK

  CreateDirectory "$SMPROGRAMS\${APP}"
  CreateShortcut "$SMPROGRAMS\${APP}\Server settings and invite code.lnk" "notepad.exe" '"${DATA}\syncord-server.toml"'
  CreateShortcut "$SMPROGRAMS\${APP}\Restart server.lnk" "powershell.exe" '-NoProfile -ExecutionPolicy Bypass -File "$INSTDIR\restart-server.ps1"' "$INSTDIR\syncord-server.exe"
  CreateShortcut "$SMPROGRAMS\${APP}\Run server in a window (troubleshooting).lnk" "$WINDIR\System32\cmd.exe" '/k ""$INSTDIR\syncord-server.exe" "${DATA}\syncord-server.toml""' "$INSTDIR\syncord-server.exe"
  CreateShortcut "$SMPROGRAMS\${APP}\Open data folder.lnk" "${DATA}"
  CreateShortcut "$SMPROGRAMS\${APP}\Uninstall ${APP}.lnk" "$INSTDIR\Uninstall.exe"

  WriteRegStr HKLM "${UNINST_KEY}" "DisplayName" "${APP}"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${UNINST_KEY}" "Publisher" "SynCord"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\syncord-server.exe"
  WriteRegStr HKLM "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "${UNINST_KEY}" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegStr HKLM "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\Uninstall.exe" /S'
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoRepair" 1
  WriteRegDWORD HKLM "${UNINST_KEY}" "EstimatedSize" 6000
SectionEnd

Section "Uninstall"
  SetShellVarContext all
  ${DisableX64FSRedirection}
  nsExec::ExecToLog '${PS} "$INSTDIR\server-remove.ps1"'
  Delete "$INSTDIR\syncord-server.exe"
  Delete "$INSTDIR\server-setup.ps1"
  Delete "$INSTDIR\server-remove.ps1"
  Delete "$INSTDIR\restart-server.ps1"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR"
  RMDir /r "$SMPROGRAMS\${APP}"
  DeleteRegKey HKLM "${UNINST_KEY}"
  IfSilent +2
    MessageBox MB_ICONINFORMATION "SynCord Server was removed.$\r$\n$\r$\nYour messages and settings are still in ${DATA}. Delete that folder if you don't need them."
SectionEnd
