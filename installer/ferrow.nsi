; Ferrow setup: NSIS 3 Unicode, per-machine, x64. Build with scripts/package.ps1.
; /DVERSION /DVI_VERSION /DSRC /DOUTFILE; SRC holds the exact payload, and its
; parent holds the generated install-manifest.nsh and preflight-files.nsh.
; Modelled on the Corewright setup (docs/milestones/M4-installer.md).
!macro REQUIRE name
  !ifndef ${name}
    !error "Pass /D${name}=..."
  !endif
!macroend
!insertmacro REQUIRE VERSION
!insertmacro REQUIRE VI_VERSION
!insertmacro REQUIRE SRC
!insertmacro REQUIRE OUTFILE

Unicode true
ManifestDPIAware true
RequestExecutionLevel admin
SetCompressor /SOLID lzma
!include MUI2.nsh
!include LogicLib.nsh
!include x64.nsh
!include FileFunc.nsh
!include nsDialogs.nsh
!include TextFunc.nsh
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\Ferrow"
!define SERVICE "ferrowd"

Name "Ferrow"
OutFile "${OUTFILE}"
InstallDir "$PROGRAMFILES64\Ferrow"
BrandingText "Ferrow ${VERSION}"
VIProductVersion "${VI_VERSION}"
VIAddVersionKey "ProductName" "Ferrow"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "FileDescription" "Ferrow ${VERSION} setup"
VIAddVersionKey "LegalCopyright" "rennerdo30, MIT licence"
!define MUI_ICON "art\ferrow.ico"
!define MUI_UNICON "art\ferrow.ico"
!define MUI_WELCOMEFINISHPAGE_BITMAP "art\welcome.bmp"
!define MUI_UNWELCOMEFINISHPAGE_BITMAP "art\welcome.bmp"
!define MUI_HEADERIMAGE
!define MUI_HEADERIMAGE_RIGHT
!define MUI_HEADERIMAGE_BITMAP "art\header.bmp"
!define MUI_HEADERIMAGE_UNBITMAP "art\header.bmp"
!define MUI_ABORTWARNING
!define MUI_WELCOMEPAGE_TITLE "Install Ferrow ${VERSION}"
!define MUI_WELCOMEPAGE_TEXT "Ferrow finds any file on your PC by name, as fast as you type.$\r$\n$\r$\nSetup installs:$\r$\n$\r$\n  - the Ferrow app and the ferrow command line in $INSTDIR,$\r$\n  - optionally the Ferrow index service, which starts with Windows,$\r$\n  - a Start menu entry for all users.$\r$\n$\r$\nYour index and settings are kept when you update or uninstall."

!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_FUNCTION StartApp
!define MUI_FINISHPAGE_RUN_TEXT "Start Ferrow"
!insertmacro MUI_PAGE_WELCOME
Page custom OptionsPage OptionsLeave
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
UninstPage custom un.DataPage
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Var ServiceBox
Var WantService
Var ServiceState
Var ServicePid
Var ServiceWasRunning
Var ServiceRemoved
Var ScCommand
Var ScResult
Var TargetProcess
Var ProcessFound
Var Countdown
Var LogPath
Var IsUpgrade
Var SameUser
Var AppWasRunning
Var FilesReplaced
Var ManifestHandle
Var ManifestSize
Var MachineData
Var UserData

!macro LOG text
  FileOpen $9 "$LogPath" a
  FileSeek $9 0 END ; "a" keeps the contents but starts at the beginning
  FileWrite $9 "${text}$\r$\n"
  FileClose $9
!macroend

!macro FAIL text prefix
  ; Put a removed service back before any dialog or the failure page.
  Call ${prefix}RestoreService
  DetailPrint "ERROR: ${text}"
  !insertmacro LOG "ERROR: ${text}"
  MessageBox MB_OK|MB_ICONSTOP "${text}$\r$\nSee $LogPath." /SD IDOK
  SetErrorLevel 1
  Abort
!macroend

; Delete $INSTDIR\<file>; when it is running (the updater during an in-app
; update), rename it to <file>.old and remove that at the next restart.
!macro MoveAsideRunning file
  ${If} ${FileExists} "$INSTDIR\${file}"
    ClearErrors
    Delete "$INSTDIR\${file}"
    ${If} ${Errors}
      Delete "$INSTDIR\${file}.old"
      ClearErrors
      Rename "$INSTDIR\${file}" "$INSTDIR\${file}.old"
      ${IfNot} ${Errors}
        Delete /REBOOTOK "$INSTDIR\${file}.old"
      ${EndIf}
    ${EndIf}
  ${EndIf}
  Delete "$INSTDIR\${file}.old"
!macroend

!macro COMMON_FUNCTIONS prefix
; Ask every running app to close: the app waits on Global\Ferrow.Exit.S<session>
; in each session where it runs. No process is ever terminated.
; The stub is 32-bit: PROCESSENTRY32W is 556 bytes, th32ProcessID at offset 8,
; szExeFile at offset 36. The snapshot includes other sessions' processes.
Function ${prefix}CloseApps
  StrCpy $TargetProcess "ferrow-gui.exe"
  System::Call 'kernel32::CreateToolhelp32Snapshot(i 2, i 0) p.r0'
  ${If} $0 == -1
    Return
  ${EndIf}
  System::Alloc 556
  Pop $1
  System::Call '*$1(i 556)'
  System::Call 'kernel32::Process32FirstW(p r0, p r1) i.r2'
  ${DoWhile} $2 != 0
    IntOp $3 $1 + 36
    System::Call '*$3(&w260 .r4)'
    ${If} $4 == $TargetProcess
      System::Call '*$1(i, i, i.r6)'
      System::Call 'kernel32::ProcessIdToSessionId(i r6, *i .r7) i.r8'
      ${If} $8 != 0
        System::Call 'kernel32::OpenEventW(i 2, i 0, w "Global\Ferrow.Exit.S$7") p.r5'
        ${If} $5 != 0
          System::Call 'kernel32::SetEvent(p r5)'
          System::Call 'kernel32::CloseHandle(p r5)'
          !insertmacro LOG "Asked Ferrow to close in session $7."
        ${EndIf}
      ${EndIf}
    ${EndIf}
    System::Call 'kernel32::Process32NextW(p r0, p r1) i.r2'
  ${Loop}
  System::Free $1
  System::Call 'kernel32::CloseHandle(p r0)'
FunctionEnd

Function ${prefix}FindProcess
  StrCpy $ProcessFound 0
  System::Call 'kernel32::CreateToolhelp32Snapshot(i 2, i 0) p.r0'
  ${If} $0 == -1
    !insertmacro FAIL "Cannot enumerate running applications." "${prefix}"
  ${EndIf}
  System::Alloc 556
  Pop $1
  System::Call '*$1(i 556)'
  System::Call 'kernel32::Process32FirstW(p r0, p r1) i.r2'
  ${DoWhile} $2 != 0
    IntOp $3 $1 + 36
    System::Call '*$3(&w260 .r4)'
    ${If} $4 == $TargetProcess
      System::Call '*$1(i, i, i.r6)'
      ${If} $6 != $ServicePid
        StrCpy $ProcessFound 1
        ${ExitDo}
      ${EndIf}
    ${EndIf}
    System::Call 'kernel32::Process32NextW(p r0, p r1) i.r2'
  ${Loop}
  System::Free $1
  System::Call 'kernel32::CloseHandle(p r0)'
FunctionEnd

; SCM state by number (independent of the Windows display language):
; 0 = absent, 1 = stopped, 3 = stop pending, 4 = running, -1 = marked for deletion.
Function ${prefix}QueryService
  StrCpy $ServiceState 0
  StrCpy $ServicePid 0
  System::Call 'advapi32::OpenSCManagerW(p 0, p 0, i 1) p.r0'
  ${If} $0 == 0
    !insertmacro FAIL "Cannot open the Windows Service Control Manager." "${prefix}"
  ${EndIf}
  System::Call 'advapi32::OpenServiceW(p r0, w "${SERVICE}", i 4) p.r1 ?e'
  Pop $2
  ${If} $1 == 0
    System::Call 'advapi32::CloseServiceHandle(p r0)'
    ${If} $2 == 1072
      StrCpy $ServiceState -1
      Return
    ${EndIf}
    ${If} $2 != 1060
      !insertmacro FAIL "Cannot query the Ferrow service (Windows error $2)." "${prefix}"
    ${EndIf}
    Return
  ${EndIf}
  System::Alloc 36
  Pop $2
  System::Call 'advapi32::QueryServiceStatusEx(p r1, i 0, p r2, i 36, *i) i.r3'
  System::Call '*$2(i, i .r4, i, i, i, i, i, i .r5)'
  StrCpy $ServiceState $4
  StrCpy $ServicePid $5
  System::Free $2
  System::Call 'advapi32::CloseServiceHandle(p r1)'
  System::Call 'advapi32::CloseServiceHandle(p r0)'
  ${If} $3 == 0
    !insertmacro FAIL "Cannot read the Ferrow service state." "${prefix}"
  ${EndIf}
FunctionEnd

; Every sc.exe call is bounded and logged.
Function ${prefix}Sc
  DetailPrint "sc.exe $ScCommand"
  nsExec::ExecToLog /TIMEOUT=15000 '"$SYSDIR\sc.exe" $ScCommand'
  Pop $ScResult
  !insertmacro LOG "sc.exe $ScCommand => $ScResult"
FunctionEnd

; Stop (the service saves its index on stop), then delete the registration.
; `ferrow service install` refuses an existing registration, so an upgrade
; registers the new build from scratch.
Function ${prefix}RemoveService
  Call ${prefix}QueryService
  ${If} $ServiceState == 0
    Return
  ${EndIf}
  ${If} $ServiceState == 4
    StrCpy $ServiceWasRunning 1
  ${EndIf}
  ${If} $ServiceState != 1
  ${AndIf} $ServiceState != 3
  ${AndIf} $ServiceState != -1
    StrCpy $ScCommand 'stop ${SERVICE}'
    Call ${prefix}Sc
    ${If} $ScResult != 0
    ${AndIf} $ScResult != 1062
      !insertmacro FAIL "Could not stop the Ferrow service (sc.exe: $ScResult). Nothing was changed." "${prefix}"
    ${EndIf}
  ${EndIf}
  StrCpy $Countdown 60
  ${Do}
    Call ${prefix}QueryService
    ${If} $ServiceState == 1
    ${OrIf} $ServiceState == 0
    ${OrIf} $ServiceState == -1
      ${ExitDo}
    ${EndIf}
    Sleep 500
    IntOp $Countdown $Countdown - 1
  ${LoopWhile} $Countdown > 0
  ${If} $Countdown == 0
    !insertmacro FAIL "The Ferrow service did not stop within 30 seconds. Nothing was changed." "${prefix}"
  ${EndIf}
  ${If} $ServiceState == 1
    StrCpy $ScCommand 'delete ${SERVICE}'
    Call ${prefix}Sc
    ${If} $ScResult != 0
    ${AndIf} $ScResult != 1060
    ${AndIf} $ScResult != 1072
      !insertmacro FAIL "Could not remove the Ferrow service registration (sc.exe: $ScResult)." "${prefix}"
    ${EndIf}
  ${EndIf}
  StrCpy $ServiceRemoved 1
  StrCpy $Countdown 60
  ${Do}
    Call ${prefix}QueryService
    ${If} $ServiceState == 0
      Return
    ${EndIf}
    Sleep 500
    IntOp $Countdown $Countdown - 1
  ${LoopWhile} $Countdown > 0
  !insertmacro FAIL "The Ferrow service is still being removed. Close the Services window and retry." "${prefix}"
FunctionEnd

; Register with the installed CLI (same code path as `ferrow service install`:
; protected binary folder, protected data folder, restart on failure), then start.
Function ${prefix}InstallService
  DetailPrint "ferrow service install"
  nsExec::ExecToLog /TIMEOUT=60000 '"$INSTDIR\ferrow.exe" service install'
  Pop $0
  !insertmacro LOG "ferrow service install => $0"
  ${If} $0 != 0
    Push 1
    Return
  ${EndIf}
  StrCpy $ScCommand 'start ${SERVICE}'
  Call ${prefix}Sc
  ${If} $ScResult != 0
  ${AndIf} $ScResult != 1056
    Push 2
    Return
  ${EndIf}
  StrCpy $Countdown 60
  ${Do}
    Call ${prefix}QueryService
    ${If} $ServiceState == 4
      Push 0
      Return
    ${EndIf}
    Sleep 500
    IntOp $Countdown $Countdown - 1
  ${LoopWhile} $Countdown > 0
  Push 3
FunctionEnd

; After a failure: if this run removed a service, register the installed
; build again so the PC is not left without it. Never recurses into FAIL.
Function ${prefix}RestoreService
  ${If} $ServiceRemoved != 1
    Return
  ${EndIf}
  StrCpy $ServiceRemoved 0
  ${IfNot} ${FileExists} "$INSTDIR\ferrow.exe"
    Return
  ${EndIf}
  Push $0
  nsExec::ExecToLog /TIMEOUT=60000 '"$INSTDIR\ferrow.exe" service install'
  Pop $0
  ${If} $0 == 0
  ${AndIf} $ServiceWasRunning == 1
    nsExec::ExecToLog /TIMEOUT=15000 '"$SYSDIR\sc.exe" start ${SERVICE}'
    Pop $0
  ${EndIf}
  !insertmacro LOG "Service restore after failure => $0"
  Pop $0
FunctionEnd

; Close the app in every session, then make sure no payload file is in use.
Function ${prefix}PrepareFiles
  Call ${prefix}CloseApps
  StrCpy $TargetProcess "ferrow-gui.exe"
  retry_apps:
  StrCpy $Countdown 30
  ${Do}
    Call ${prefix}FindProcess
    ${If} $ProcessFound == 0
      ${ExitDo}
    ${EndIf}
    Sleep 500
    IntOp $Countdown $Countdown - 1
  ${LoopWhile} $Countdown > 0
  ${If} $ProcessFound == 1
    MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "Ferrow is still open in this or another signed-in session. Close it there, then choose Retry." /SD IDCANCEL IDRETRY retry_apps
    !insertmacro FAIL "Ferrow is still open. Nothing was changed." "${prefix}"
  ${EndIf}
  Call ${prefix}CheckFileLocks
FunctionEnd

Function ${prefix}CheckFileLocks
  retry_locks:
  !include "${SRC}\..\preflight-files.nsh"
  Return
  locked:
  MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "Ferrow files are in use (for example by a running ferrow command). Close them, then choose Retry." /SD IDCANCEL IDRETRY retry_locks
  !insertmacro FAIL "Ferrow files are in use. Nothing was changed." "${prefix}"
FunctionEnd
!macroend
!insertmacro COMMON_FUNCTIONS ""
!insertmacro COMMON_FUNCTIONS "un."

!macro INIT
  ${IfNot} ${RunningX64}
    MessageBox MB_ICONSTOP "Ferrow requires 64-bit Windows." /SD IDOK
    SetErrorLevel 1
    Abort
  ${EndIf}
  ${If} ${IsNativeARM64}
    MessageBox MB_ICONSTOP "This Ferrow setup is for x64 PCs." /SD IDOK
    SetErrorLevel 1
    Abort
  ${EndIf}
  SetRegView 64
  SetShellVarContext current
  StrCpy $UserData "$LOCALAPPDATA\ferrow"
  SetShellVarContext all
  StrCpy $MachineData "$APPDATA\Ferrow"
  ; Fixed location: no /D, no other folder.
  StrCpy $INSTDIR "$PROGRAMFILES64\Ferrow"
  StrCpy $LogPath "$TEMP\Ferrow-installer-${VERSION}.log"
  StrCpy $FilesReplaced 0
  StrCpy $ServiceRemoved 0
  StrCpy $ServiceWasRunning 0
  ; Per-user steps only when the elevated account is the signed-in user
  ; (a different administrator may have approved UAC).
  StrCpy $SameUser 0
  System::Call 'wtsapi32::WTSQuerySessionInformationW(p 0, i -1, i 5, *p .r0, *i .r1) i.r2'
  ${If} $2 != 0
    System::Call '*$0(&w256 .r3)'
    System::Call 'wtsapi32::WTSFreeMemory(p r0)'
    System::Call 'advapi32::GetUserNameW(w .r4, *i 257) i.r5'
    ${If} $5 != 0
    ${AndIf} $3 != ""
    ${AndIf} $3 == $4
      StrCpy $SameUser 1
    ${EndIf}
  ${EndIf}
  ${If} $SameUser != 1
    !insertmacro LOG "Per-user steps skipped: the elevated account is not the signed-in user (or it could not be checked)."
  ${EndIf}
!macroend

Function .onInit
  !insertmacro INIT
  StrCpy $IsUpgrade 0
  ReadRegStr $0 HKLM "${UNINSTALL_KEY}" "InstallLocation"
  ${IfNot} ${Errors}
    StrCpy $IsUpgrade 1
  ${EndIf}
  ClearErrors
  ; Fresh installs get the service; upgrades keep what is there.
  ; /SERVICE adds it, /NOSERVICE leaves it out (or removes it).
  StrCpy $WantService ${BST_CHECKED}
  ${If} $IsUpgrade == 1
    Call QueryService
    ${If} $ServiceState == 0
      StrCpy $WantService ${BST_UNCHECKED}
    ${EndIf}
  ${EndIf}
  ${GetParameters} $0
  ClearErrors
  ${GetOptions} $0 "/NOSERVICE" $1
  ${IfNot} ${Errors}
    StrCpy $WantService ${BST_UNCHECKED}
  ${EndIf}
  ClearErrors
  ${GetOptions} $0 "/SERVICE" $1
  ${IfNot} ${Errors}
    StrCpy $WantService ${BST_CHECKED}
  ${EndIf}
  ClearErrors
  ; Was the app open in this session? Then reopen it after an upgrade.
  StrCpy $AppWasRunning 0
  StrCpy $TargetProcess "ferrow-gui.exe"
  StrCpy $ServicePid 0
  Call FindProcess
  StrCpy $AppWasRunning $ProcessFound
FunctionEnd

Function .onInstSuccess
  ; Interactive upgrades reopen the app if it was open. Silent runs from the
  ; in-app updater leave that to the updater, which starts it as the desktop user.
  IfSilent done
  ${If} $IsUpgrade == 1
  ${AndIf} $AppWasRunning == 1
    Call StartApp
  ${EndIf}
  done:
FunctionEnd

Function un.onInit
  !insertmacro INIT
FunctionEnd

Function .onInstFailed
  Call RestoreService
FunctionEnd

Function un.onUninstFailed
  Call un.RestoreService
FunctionEnd

Function OptionsPage
  !insertmacro MUI_HEADER_TEXT "Ferrow options" "What Ferrow installs."
  nsDialogs::Create 1018
  Pop $0
  ${NSD_CreateCheckbox} 0 0 100% 14u "Install the Ferrow index service"
  Pop $ServiceBox
  ${NSD_SetState} $ServiceBox $WantService
  ${NSD_CreateLabel} 12u 15u -12u 44u "A Windows service named ferrowd runs as SYSTEM and starts with Windows. It keeps a file index that the ferrow command line searches; its index is readable by administrators only. The Ferrow app keeps its own index in your profile, so the app works with or without it."
  Pop $0
  ${NSD_CreateLabel} 0 70u 100% 24u "Installs to $INSTDIR. Setup changes nothing in your profile; Ferrow's own settings stay where they are."
  Pop $0
  nsDialogs::Show
FunctionEnd

Function OptionsLeave
  ${NSD_GetState} $ServiceBox $WantService
FunctionEnd

; Uninstall never deletes the index or settings (owner rule: data is never lost).
Function un.DataPage
  !insertmacro MUI_HEADER_TEXT "Index and settings" "Your index and settings are kept."
  nsDialogs::Create 1018
  Pop $0
  ${NSD_CreateLabel} 0 0 100% 70u "Ferrow keeps its index and settings, so a later install picks them up again:$\r$\n$\r$\n$MachineData$\r$\n$UserData$\r$\n$\r$\nDelete these folders yourself if you no longer need them."
  Pop $0
  nsDialogs::Show
FunctionEnd

; Install appends each version's exact payload. Keeping older entries means
; uninstall also removes files that a later version no longer ships.
Function un.ValidateManifest
  ClearErrors
  FileOpen $ManifestHandle "$INSTDIR\install-manifest.txt" r
  ${If} ${Errors}
    !insertmacro FAIL "The installed file list cannot be read. Nothing was removed." "un."
  ${EndIf}
  FileSeek $ManifestHandle 0 END $ManifestSize
  FileSeek $ManifestHandle 0 SET
  ${If} $ManifestSize == 0
    !insertmacro FAIL "The installed file list is empty. Nothing was removed." "un."
  ${EndIf}
  ${Do}
    ClearErrors
    FileRead $ManifestHandle $0
    ${If} ${Errors}
      FileSeek $ManifestHandle 0 CUR $1
      ${If} $1 != $ManifestSize
        !insertmacro FAIL "The installed file list could not be read completely." "un."
      ${EndIf}
      ${ExitDo}
    ${EndIf}
    ${TrimNewLines} $0 $0
    StrCpy $4 $0 2
    StrCpy $0 $0 "" 2
    ${If} $4 != "F|"
    ${AndIf} $4 != "D|"
      !insertmacro FAIL "The installed file list has an invalid entry." "un."
    ${EndIf}
    System::Call 'shlwapi::PathIsRelativeW(w r0) i.r2'
    ${If} $2 == 0
      !insertmacro FAIL "The installed file list has an absolute path." "un."
    ${EndIf}
    ; A quote cannot occur in a Windows file name; the prefix check below
    ; catches everything that escapes $INSTDIR.
    System::Call 'shlwapi::StrPBrkW(w r0, w "*?:<>|") p.r2'
    ${If} $2 != 0
      !insertmacro FAIL "The installed file list contains invalid path characters ($0)." "un."
    ${EndIf}
    GetFullPathName $1 "$INSTDIR\$0"
    StrCpy $2 "$INSTDIR\"
    StrLen $3 $2
    StrCpy $3 $1 $3
    ${If} $3 != $2
      !insertmacro FAIL "The installed file list contains an unsafe path." "un."
    ${EndIf}
  ${Loop}
  FileSeek $ManifestHandle 0 SET
  ClearErrors
FunctionEnd

Function un.RemovePayload
  ${Do}
    ClearErrors
    FileRead $ManifestHandle $0
    ${If} ${Errors}
      ${ExitDo}
    ${EndIf}
    ${TrimNewLines} $0 $0
    StrCpy $4 $0 2
    StrCpy $0 $0 "" 2
    GetFullPathName $1 "$INSTDIR\$0"
    ${If} $4 == "F|"
      ${IfNot} ${FileExists} "$1"
        ${Continue}
      ${EndIf}
      ClearErrors
      Delete "$1"
      ${If} ${Errors}
        !insertmacro FAIL "Could not remove $1. Close Ferrow and retry." "un."
      ${EndIf}
    ${ElseIf} $4 == "D|"
      RMDir "$1" ; folders with unknown files stay
    ${EndIf}
  ${Loop}
  FileClose $ManifestHandle
  Delete /REBOOTOK "$INSTDIR\ferrow-update.exe.old"
  ClearErrors
  Delete "$INSTDIR\install-manifest.txt"
  ${If} ${Errors}
    !insertmacro FAIL "Could not remove the installed file list." "un."
  ${EndIf}
FunctionEnd

; Explorer hands the start to the running desktop shell, so the app gets
; the signed-in user's normal token, never this elevated one.
Function StartApp
  ${If} $SameUser != 1
    !insertmacro LOG "App not started: the elevated account is not the signed-in user."
    Return
  ${EndIf}
  Exec '"$WINDIR\explorer.exe" "$INSTDIR\ferrow-gui.exe"'
FunctionEnd

Section "Ferrow"
  Call PrepareFiles
  Call RemoveService
  SetOutPath "$INSTDIR"
  ; An in-app update runs this setup from ferrow-update.exe: move it aside.
  !insertmacro MoveAsideRunning "ferrow-update.exe"
  SetRebootFlag false
  StrCpy $FilesReplaced 1
  ClearErrors
  File /r "${SRC}\*.*"
  ${If} ${Errors}
    !insertmacro FAIL "Could not copy the Ferrow files." ""
  ${EndIf}
  ClearErrors
  WriteUninstaller "$INSTDIR\uninstall.exe"
  ${If} ${Errors}
    !insertmacro FAIL "Could not write the uninstaller." ""
  ${EndIf}
  FileOpen $8 "$INSTDIR\install-manifest.txt" a
  ${If} ${Errors}
    !insertmacro FAIL "Could not write the installed file list." ""
  ${EndIf}
  FileSeek $8 0 END
  !include "${SRC}\..\install-manifest.nsh"
  ${If} ${Errors}
    FileClose $8
    !insertmacro FAIL "Could not save the installed file list." ""
  ${EndIf}
  FileClose $8

  ${If} $WantService == ${BST_CHECKED}
    Call InstallService
    Pop $0
    ${If} $0 != 0
      !insertmacro FAIL "Could not install or start the Ferrow service (step $0)." ""
    ${EndIf}
    StrCpy $ServiceRemoved 0
  ${EndIf}

  ClearErrors
  CreateShortcut "$SMPROGRAMS\Ferrow.lnk" "$INSTDIR\ferrow-gui.exe" "" "$INSTDIR\ferrow.ico"
  ${If} ${Errors}
    !insertmacro FAIL "Could not create the Start menu entry." ""
  ${EndIf}
  WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayName" "Ferrow"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "Publisher" "rennerdo30"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "URLInfoAbout" "https://github.com/rennerdo30/ferrow"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\ferrow.ico"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKLM "${UNINSTALL_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKLM "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINSTALL_KEY}" "NoRepair" 1
  ${If} ${Errors}
    !insertmacro FAIL "Could not register Ferrow in Installed apps." ""
  ${EndIf}
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  WriteRegDWORD HKLM "${UNINSTALL_KEY}" "EstimatedSize" $0
  !insertmacro LOG "Installed Ferrow ${VERSION}."
SectionEnd

Section "Uninstall"
  Call un.ValidateManifest
  Call un.PrepareFiles
  Call un.RemoveService
  StrCpy $ServiceRemoved 0 ; uninstalling: never put the service back
  Call un.RemovePayload
  ; The service binary lives in its own protected folder (ferrow service install).
  ClearErrors
  ${If} ${FileExists} "$INSTDIR\service\ferrowd.exe"
    Delete "$INSTDIR\service\ferrowd.exe"
    ${If} ${Errors}
      !insertmacro FAIL "Could not remove the service program." "un."
    ${EndIf}
  ${EndIf}
  RMDir "$INSTDIR\service"
  ClearErrors
  ${If} ${FileExists} "$INSTDIR\uninstall.exe"
    Delete "$INSTDIR\uninstall.exe"
    ${If} ${Errors}
      !insertmacro FAIL "Could not remove the uninstaller." "un."
    ${EndIf}
  ${EndIf}
  RMDir "$INSTDIR"
  ClearErrors
  ${If} ${FileExists} "$SMPROGRAMS\Ferrow.lnk"
    Delete "$SMPROGRAMS\Ferrow.lnk"
    ${If} ${Errors}
      !insertmacro FAIL "Could not remove the Start menu entry." "un."
    ${EndIf}
  ${EndIf}
  System::Call 'advapi32::RegDeleteKeyExW(p 0x80000002, w "${UNINSTALL_KEY}", i 0x100, i 0) i.r0'
  ${If} $0 != 0
  ${AndIf} $0 != 2
    !insertmacro FAIL "Could not remove Ferrow from Installed apps (Windows error $0)." "un."
  ${EndIf}
  SetErrorLevel 0
SectionEnd
