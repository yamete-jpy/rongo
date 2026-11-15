; rongo's Windows installer (NSIS 3, Unicode). Built by scripts/bundle-windows.sh,
; which passes -DVERSION (Cargo.toml's), -DEXE (rongo.exe), -DICON (assets/icon.ico)
; and -DOUTFILE, all as absolute paths, and runs makensis with -WX.
;
; Per user: no administrator rights, nothing outside the user's own profile.
;   program   %LOCALAPPDATA%\Programs\rongo      (rongo.exe, uninstall.exe)
;   shortcut  Start menu\rongo.lnk
;   uninstall HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\rongo
; Running the installer over an older version updates it in place. The user's
; data (connections, settings, vault, Collection History) is kept by the
; uninstaller unless its "Remove my data" box is ticked.

Unicode true
!ifndef VERSION
  !error "pass -DVERSION=<x.y.z> (scripts/bundle-windows.sh does)"
!endif
!ifndef EXE
  !error "pass -DEXE=<path to rongo.exe>"
!endif
!ifndef ICON
  !error "pass -DICON=<path to icon.ico>"
!endif
!ifndef OUTFILE
  !error "pass -DOUTFILE=<path of the installer to write>"
!endif

!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\rongo"

Name "rongo"
Caption "rongo ${VERSION} Setup"
BrandingText "rongo ${VERSION}"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\Programs\rongo"
; An earlier install made with /D= elsewhere is upgraded where it is (an explicit /D= still wins).
InstallDirRegKey HKCU "${UNINSTALL_KEY}" "InstallLocation"
RequestExecutionLevel user
SetCompressor /SOLID lzma
ManifestDPIAware true
ManifestSupportedOS {8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "rongo"
VIAddVersionKey "FileDescription" "rongo installer"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "MIT License"

!include MUI2.nsh
!include LogicLib.nsh
!include x64.nsh

!define MUI_ICON "${ICON}"
!define MUI_UNICON "${ICON}"
!define MUI_ABORTWARNING
!define MUI_UNABORTWARNING

!define MUI_WELCOMEPAGE_TEXT "This installs rongo ${VERSION} for your Windows account only. It needs no administrator rights.$\r$\n$\r$\nIf rongo is already installed, this updates it and keeps your connections and settings. Close rongo first if it is open.$\r$\n$\r$\nClick Install to continue."
!define MUI_FINISHPAGE_RUN "$INSTDIR\rongo.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Run rongo"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_COMPONENTS
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

; Windows lets nobody delete a running program, so a failed Delete of rongo.exe
; means rongo is open (renaming would succeed while it runs, so it can't tell).
; Retry loops until it is closed; Cancel (or silent mode, where nobody can
; answer) stops before anything has been changed, with exit code 5.
!macro CheckRunning un
Function ${un}CheckRunning
  ${Do}
    ClearErrors
    ${IfNot} ${FileExists} "$INSTDIR\rongo.exe"
      ${Break}
    ${EndIf}
    Delete "$INSTDIR\rongo.exe"
    ${IfNot} ${Errors}
      ${Break}
    ${EndIf}
    MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "rongo.exe can't be replaced. If rongo is open, close it, then click Retry." /SD IDCANCEL IDRETRY retry
    SetErrorLevel 5
    Abort
    retry:
  ${Loop}
FunctionEnd
!macroend
!insertmacro CheckRunning ""
!insertmacro CheckRunning "un."

Function .onInit
  ${IfNot} ${RunningX64}
    MessageBox MB_ICONSTOP|MB_OK "rongo needs 64-bit Windows 10 or 11." /SD IDOK
    SetErrorLevel 3
    Abort
  ${EndIf}
FunctionEnd

Section "-rongo" SecMain
  SetShellVarContext current
  Call CheckRunning
  SetOutPath "$INSTDIR"
  File "/oname=rongo.exe" "${EXE}"
  WriteUninstaller "$INSTDIR\uninstall.exe"
  CreateShortCut "$SMPROGRAMS\rongo.lnk" "$INSTDIR\rongo.exe" "" "$INSTDIR\rongo.exe" 0

  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "rongo"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\rongo.exe"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "Publisher" "rongo"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${UNINSTALL_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegStr HKCU "${UNINSTALL_KEY}" "URLInfoAbout" "https://github.com/yamete-jpy/rongo"
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1
  SectionGetSize ${SecMain} $0
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "EstimatedSize" $0
SectionEnd

Section "un.Uninstall rongo" UnMain
  SectionIn RO
  SetShellVarContext current
  Call un.CheckRunning
  Delete "$INSTDIR\rongo.exe"
  Delete "$INSTDIR\uninstall.exe"
  ; Not recursive: an install folder chosen with /D= may hold other files.
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\rongo.lnk"
  DeleteRegKey HKCU "${UNINSTALL_KEY}"
SectionEnd

; Both folders are rongo's own (the install folder, Programs\rongo, is a different
; path): %APPDATA%\rongo holds profiles.json, settings.json, the vault and the
; window state; %LOCALAPPDATA%\rongo holds Collection History.
Section /o "un.Remove my data (connections, settings, master-password vault, Collection History)" UnData
  SetShellVarContext current
  ${If} $APPDATA != ""
    RMDir /r "$APPDATA\rongo"
  ${EndIf}
  ${If} $LOCALAPPDATA != ""
    RMDir /r "$LOCALAPPDATA\rongo"
  ${EndIf}
SectionEnd

!insertmacro MUI_UNFUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${UnMain} "Removes rongo, its Start menu entry and its entry under Settings > Apps."
  !insertmacro MUI_DESCRIPTION_TEXT ${UnData} "Also deletes your saved connections, settings, master-password vault and Collection History for this Windows account. This cannot be undone. Leave it unticked to keep them for a later install."
!insertmacro MUI_UNFUNCTION_DESCRIPTION_END
