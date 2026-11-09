; Celerity app installer (per-user, no admin rights needed).
;
; Build:  makensis -DVERSION=0.1.0 -DBIN_DIR=<folder with celerity.exe> -DOUT_FILE=<setup.exe> celerity.nsi

Unicode true
!include "MUI2.nsh"
!include "FileFunc.nsh"

!ifndef VERSION
  !error "Pass -DVERSION=x.y.z"
!endif
!ifndef BIN_DIR
  !error "Pass -DBIN_DIR=<folder containing celerity.exe>"
!endif
!ifndef OUT_FILE
  !define OUT_FILE "Celerity-Setup-${VERSION}.exe"
!endif

!define APP "Celerity"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP}"

Name "${APP}"
OutFile "${OUT_FILE}"
InstallDir "$LOCALAPPDATA\Programs\${APP}"
InstallDirRegKey HKCU "Software\${APP}" "InstallDir"
RequestExecutionLevel user
SetCompressor /SOLID lzma
BrandingText "${APP} ${VERSION}"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${APP}"
VIAddVersionKey "FileDescription" "${APP} installer"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "MIT licensed"

!define MUI_ICON "..\icons\celerity.ico"
!define MUI_UNICON "..\icons\celerity.ico"
!define MUI_ABORTWARNING
!define MUI_WELCOMEPAGE_TEXT "This will install ${APP} ${VERSION}, a lightweight chat app with text and voice channels.$\r$\n$\r$\nYou'll need the server address and an invite code from whoever runs your server.$\r$\n$\r$\nClick Next to continue."
!define MUI_FINISHPAGE_RUN "$INSTDIR\${APP}.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Open ${APP} now"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Section "${APP}" SecApp
  SectionIn RO
  ; Close a running copy so its file can be replaced.
  nsExec::Exec 'taskkill /IM "${APP}.exe" /F'
  Sleep 300

  ; Celerity used to be called SynCord: take an installed SynCord's place. (Settings,
  ; themes and trusted servers move over when Celerity first starts.)
  nsExec::Exec 'taskkill /IM "SynCord.exe" /F'
  ReadRegStr $0 HKCU "Software\SynCord" "InstallDir"
  StrCmp $0 "" no_syncord
    Sleep 300
    Delete "$0\SynCord.exe"
    Delete "$0\Uninstall.exe"
    RMDir "$0"
  no_syncord:
  Delete "$SMPROGRAMS\SynCord.lnk"
  Delete "$DESKTOP\SynCord.lnk"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\SynCord"
  DeleteRegKey HKCU "Software\SynCord"

  SetOutPath "$INSTDIR"
  File "/oname=${APP}.exe" "${BIN_DIR}\celerity.exe"
  WriteUninstaller "$INSTDIR\Uninstall.exe"
  CreateShortcut "$SMPROGRAMS\${APP}.lnk" "$INSTDIR\${APP}.exe"

  WriteRegStr HKCU "Software\${APP}" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayName" "${APP}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINST_KEY}" "Publisher" "${APP}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\${APP}.exe"
  WriteRegStr HKCU "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINST_KEY}" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegStr HKCU "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\Uninstall.exe" /S'
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoRepair" 1
  WriteRegDWORD HKCU "${UNINST_KEY}" "EstimatedSize" 20000
SectionEnd

Section "Desktop shortcut" SecDesktop
  CreateShortcut "$DESKTOP\${APP}.lnk" "$INSTDIR\${APP}.exe"
SectionEnd

; Automatic updates run this installer with /S /RELAUNCH: reopen Celerity when done.
Function .onInstSuccess
  ${GetParameters} $0
  ClearErrors
  ${GetOptions} $0 "/RELAUNCH" $1
  IfErrors +2
    Exec '"$INSTDIR\${APP}.exe"'
FunctionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecApp} "The ${APP} app."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop} "Put a ${APP} icon on your desktop."
!insertmacro MUI_FUNCTION_DESCRIPTION_END

Section "Uninstall"
  nsExec::Exec 'taskkill /IM "${APP}.exe" /F'
  Sleep 300
  Delete "$INSTDIR\${APP}.exe"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\${APP}.lnk"
  Delete "$DESKTOP\${APP}.lnk"
  DeleteRegKey HKCU "${UNINST_KEY}"
  DeleteRegKey HKCU "Software\${APP}"
  ; Your settings and themes in %APPDATA%\celerity are left alone.
SectionEnd
