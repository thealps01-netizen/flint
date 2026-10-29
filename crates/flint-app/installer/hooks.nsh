; Flint's additions to Tauri's NSIS installer (tauri.conf.json
; bundle.windows.nsis.installerHooks). Tauri includes this file at the top
; of its script and inserts the macros into its install and uninstall
; sections. The installer runs as administrator (per-machine install), so
; the programs started here run as administrator too.
; Saved as UTF-8 with a byte-order mark: makensis reads a file without
; one in the machine's ANSI code page, which would garble the Turkish text.
;
; Install: Windows 10 22H2 or later; only into Program Files (the index
; service's program is run as administrator from the install folder, so
; that folder must be one only administrators can change); the program
; is copied next to flint.exe; full indexing (the FlintIndexer service)
; is set up when it was on before (an update or a reinstall), when the
; command line has /FULLINDEX, or when the user says yes to the question
; (asked only in an interactive install). A running service is stopped
; before files are replaced and started again if the install fails.
; Uninstall: the service and its index are removed (with Windows' own
; sc.exe if Flint's program cannot do it); with "Delete the application
; data" ticked, the user's Flint settings and history too.

; This file's folder, fixed here (inside a macro, __FILEDIR__ would name
; the generated script's folder).
!define FLINT_HOOKS_DIR "${__FILEDIR__}"
; Windows 10 22H2
!define FLINT_MIN_BUILD 19045
; LANG_TURKISH (the languages are loaded after this file)
!define FLINT_LANG_TR 1055

Var FlintHadService
Var FlintFull
Var FlintTrusted
Var FlintLen
Var FlintPart
Var FlintTries

; FlintTrusted = 1 when $INSTDIR is a folder inside Program Files
; (compared without case, as Windows does).
!macro FLINT_CHECK_TRUSTED
  StrCpy $FlintTrusted 0
  StrLen $FlintLen "$PROGRAMFILES64\"
  StrCpy $FlintPart "$INSTDIR" $FlintLen
  ${If} $FlintPart == "$PROGRAMFILES64\"
    StrLen $FlintPart "$INSTDIR"
    ${If} $FlintPart > $FlintLen
      StrCpy $FlintTrusted 1
    ${EndIf}
  ${EndIf}
!macroend

; Stops FlintIndexer with sc.exe and waits up to 20 s. sc.exe's exit
; code is the Windows error: 1062 = not running, 1060 = not installed,
; 1061 = busy starting or stopping. Leaves $0 = 0 when it has stopped.
!macro FLINT_STOP_BY_SC
  StrCpy $FlintTries 0
  ${Do}
    nsExec::ExecToStack 'sc.exe stop FlintIndexer'
    Pop $0
    Pop $1
    ${If} $0 = 1062
    ${OrIf} $0 = 1060
      StrCpy $0 0
      ${ExitDo}
    ${EndIf}
    IntOp $FlintTries $FlintTries + 1
    ${If} $FlintTries > 40
      StrCpy $0 1
      ${ExitDo}
    ${EndIf}
    Sleep 500
  ${Loop}
!macroend

!macro NSIS_HOOK_PREINSTALL
  StrCpy $FlintHadService 0
  ReadRegStr $0 HKLM "SOFTWARE\Microsoft\Windows NT\CurrentVersion" "CurrentBuildNumber"
  ${If} $0 < ${FLINT_MIN_BUILD}
    ${If} $LANGUAGE = ${FLINT_LANG_TR}
      StrCpy $1 "Flint, Windows 10 sürüm 22H2 ya da Windows 11 gerektirir."
    ${Else}
      StrCpy $1 "Flint needs Windows 10 version 22H2 or Windows 11."
    ${EndIf}
    ${IfNot} ${Silent}
      MessageBox MB_ICONSTOP|MB_OK "$1"
    ${EndIf}
    Abort "$1"
  ${EndIf}
  !insertmacro FLINT_CHECK_TRUSTED
  ${If} $FlintTrusted <> 1
    ${If} $LANGUAGE = ${FLINT_LANG_TR}
      StrCpy $1 "Flint yalnızca $PROGRAMFILES64 içindeki bir klasöre kurulabilir (seçilen: $INSTDIR). Dizin hizmeti yönetici olarak bu klasörden çalışır; başka bir klasörü diğer kullanıcılar değiştirebilir."
    ${Else}
      StrCpy $1 "Flint can only be installed in a folder inside $PROGRAMFILES64 (chosen: $INSTDIR). Its index service runs as administrator from that folder, and other users may be able to change a folder elsewhere."
    ${EndIf}
    ${IfNot} ${Silent}
      MessageBox MB_ICONSTOP|MB_OK "$1"
    ${EndIf}
    Abort "$1"
  ${EndIf}
  ; a running service holds its program open: stop it before the files
  ; are replaced (it starts again, on the new program, after the install)
  nsExec::ExecToStack 'sc.exe query FlintIndexer'
  Pop $0
  Pop $1
  ${If} $0 = 0
    StrCpy $FlintHadService 1
    DetailPrint "Stopping the Flint index service"
    StrCpy $0 1
    ${If} ${FileExists} "$PROGRAMFILES64\Flint\flint-indexer.exe"
      nsExec::ExecToLog '"$PROGRAMFILES64\Flint\flint-indexer.exe" stop-service'
      Pop $0
    ${EndIf}
    ${If} $0 <> 0
      !insertmacro FLINT_STOP_BY_SC
    ${EndIf}
    ${If} $0 <> 0
      ${If} $LANGUAGE = ${FLINT_LANG_TR}
        StrCpy $1 "Flint dizin hizmeti durdurulamadı; dosyaları değiştirilemez. Windows'u yeniden başlatıp kurulumu tekrar çalıştırın."
      ${Else}
        StrCpy $1 "The Flint index service could not be stopped, so its files cannot be replaced. Restart Windows and run the installer again."
      ${EndIf}
      ${IfNot} ${Silent}
        MessageBox MB_ICONSTOP|MB_OK "$1"
      ${EndIf}
      Abort "$1"
    ${EndIf}
  ${EndIf}
!macroend

; An install that stops after the service was stopped (cancelled, a file
; that could not be written) starts the old service again.
Function .onInstFailed
  ${If} $FlintHadService = 1
    nsExec::ExecToLog 'sc.exe start FlintIndexer'
    Pop $0
  ${EndIf}
FunctionEnd

!macro NSIS_HOOK_POSTINSTALL
  ; the index service's program, built by scripts/prepare-indexer.mjs
  SetOutPath "$INSTDIR"
  File "/oname=flint-indexer.exe" "${FLINT_HOOKS_DIR}\bin\flint-indexer.exe"

  StrCpy $FlintFull 0
  ${GetOptions} $CMDLINE "/FULLINDEX" $0
  ${IfNot} ${Errors}
    StrCpy $FlintFull 1
  ${ElseIf} $FlintHadService = 1
    StrCpy $FlintFull 1
  ${ElseIf} $UpdateMode <> 1
  ${AndIf} $PassiveMode <> 1
  ${AndIfNot} ${Silent}
    ${If} $LANGUAGE = ${FLINT_LANG_TR}
      StrCpy $1 "Tam dizinleme açılsın mı? (önerilir)$\r$\n$\r$\nFlint böylece tüm NTFS sürücülerdeki dosyaları bulur; küçük bir arka plan hizmeti dizini güncel tutar. Açılmazsa Flint yalnızca sizin klasörlerinizde arar.$\r$\n$\r$\nBunu daha sonra Flint'ten de açabilirsiniz."
    ${Else}
      StrCpy $1 "Turn on full indexing? (recommended)$\r$\n$\r$\nFlint then finds files on every NTFS drive; a small background service keeps the index up to date. Without it, Flint searches only your own folders.$\r$\n$\r$\nYou can also turn it on later from Flint."
    ${EndIf}
    MessageBox MB_YESNO|MB_ICONQUESTION "$1" /SD IDNO IDNO flint_no_full
    StrCpy $FlintFull 1
    flint_no_full:
  ${EndIf}

  ${If} $FlintFull = 1
    DetailPrint "Setting up the Flint index service"
    nsExec::ExecToLog '"$INSTDIR\flint-indexer.exe" install-service'
    Pop $0
    ${If} $0 <> 0
      DetailPrint "The Flint index service was not set up (exit code $0); Flint runs in basic mode"
      ${IfNot} ${Silent}
      ${AndIf} $PassiveMode <> 1
        ${If} $LANGUAGE = ${FLINT_LANG_TR}
          MessageBox MB_ICONEXCLAMATION|MB_OK "Tam dizinleme açılamadı (kod $0). Flint temel modda çalışır; tam dizinlemeyi Flint'ten yeniden deneyebilirsiniz."
        ${Else}
          MessageBox MB_ICONEXCLAMATION|MB_OK "Full indexing could not be turned on (code $0). Flint runs in basic mode; you can try again from Flint."
        ${EndIf}
      ${EndIf}
    ${EndIf}
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ${If} $UpdateMode <> 1
    DetailPrint "Removing the Flint index service and its index"
    StrCpy $0 1
    !insertmacro FLINT_CHECK_TRUSTED
    ; Flint's own program only from a folder only administrators change
    ${If} $FlintTrusted = 1
    ${AndIf} ${FileExists} "$INSTDIR\flint-indexer.exe"
      nsExec::ExecToLog '"$INSTDIR\flint-indexer.exe" uninstall-service'
      Pop $0
    ${EndIf}
    ${If} $0 <> 0
      ; with Windows' own tool: stop, unregister, remove the index
      !insertmacro FLINT_STOP_BY_SC
      nsExec::ExecToLog 'sc.exe delete FlintIndexer'
      Pop $0
      ; $APPDATA is ProgramData here (Tauri's uninstaller sets the
      ; all-users context); not %ALLUSERSPROFILE%, which a user can redefine
      SetShellVarContext all
      RMDir /r "$APPDATA\Flint"
      Delete "$PROGRAMFILES64\Flint\flint-indexer.exe"
      RMDir "$PROGRAMFILES64\Flint"
      nsExec::ExecToStack 'sc.exe query FlintIndexer'
      Pop $0
      Pop $1
      ${If} $0 <> 1060
      ${AndIfNot} ${Silent}
        ${If} $LANGUAGE = ${FLINT_LANG_TR}
          MessageBox MB_ICONEXCLAMATION|MB_OK "Flint dizin hizmeti tam kaldırılamadı. Windows'u yeniden başlatınca kaldırılır."
        ${Else}
          MessageBox MB_ICONEXCLAMATION|MB_OK "The Flint index service could not be removed completely. Windows removes it when it restarts."
        ${EndIf}
      ${EndIf}
    ${EndIf}
  ${EndIf}
  Delete "$INSTDIR\flint-indexer.exe"
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ; Tauri keeps the remembered install folder unless app data is deleted;
  ; an uninstall leaves no Flint key behind
  ${If} $UpdateMode <> 1
    DeleteRegKey SHCTX "${MANUPRODUCTKEY}"
    DeleteRegKey /ifempty SHCTX "${MANUKEY}"
  ${EndIf}
  ${If} $DeleteAppDataCheckboxState = 1
  ${AndIf} $UpdateMode <> 1
    ; the user's own folders (a per-machine uninstaller points the shell
    ; folders at all users unless told otherwise)
    SetShellVarContext current
    RMDir /r "$APPDATA\Flint"
    RMDir /r "$LOCALAPPDATA\Flint"
    SetShellVarContext all
  ${EndIf}
!macroend
