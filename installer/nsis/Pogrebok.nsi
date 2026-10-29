; Погребок — NSIS-установщик (exe). Семантика зеркалит MSI
; (installer/msi/Pogrebok.wxs, спека §11): полная установка в
; Program Files, machine PATH, служба маршрутов (pogrebok-svc --install),
; включение компонентов WSL (3010 «нужна перезагрузка» = успех),
; ярлык GUI в меню Пуск, один UAC (RequestExecutionLevel admin).
;
; Сборка (Linux/macOS через makensis 3.x):
;   makensis /DVERSION=0.3.0 /DINPUT_DIR=<каталог с pogrebok.exe,
;     pogrebok-daemon.exe, pogrebok-svc.exe, pogrebok-gui.exe,
;     appliance\pogrebok-rootfs.tar.gz>
;     /DOUTFILE=<путь выходного exe> Pogrebok.nsi

Unicode true
ManifestDPIAware true
SetCompressor /SOLID lzma
CRCCheck on
XPStyle on

!ifndef VERSION
  !define VERSION "0.3.0"
!endif
!ifndef INPUT_DIR
  !define INPUT_DIR "."
!endif
!ifndef OUTFILE
  !define OUTFILE "Pogrebok-${VERSION}-win-x64-setup.exe"
!endif

!include "MUI2.nsh"
!include "x64.nsh"
!include "StrFunc.nsh"
!include "LogicLib.nsh"

; Объявления функций StrFunc (паттерн: сначала объявить, затем вызывать;
; вариант с Un — для деинсталлятора).
${StrStr}
${StrRep}
${UnStrRep}

Name "Погребок"
OutFile "${OUTFILE}"
InstallDir "$PROGRAMFILES64\Pogrebok"
InstallDirRegKey HKLM "Software\Pogrebok" "InstallDir"
RequestExecutionLevel admin
BrandingText "Погребок ${VERSION}"

!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\Pogrebok"
!define ENV_KEY "SYSTEM\CurrentControlSet\Control\Session Manager\Environment"
!ifndef HWND_BROADCAST
  !define HWND_BROADCAST 0xFFFF
!endif
!ifndef WM_SETTINGCHANGE
  !define WM_SETTINGCHANGE 0x001A
!endif

!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_NOAUTOCLOSE
!define MUI_UNFINISHPAGE_NOAUTOCLOSE

!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_UNPAGE_FINISH

!insertmacro MUI_LANGUAGE "Russian"

; --- Уведомить систему об изменении переменных окружения ----------------------
!macro BroadcastEnvChange
  SendMessage ${HWND_BROADCAST} ${WM_SETTINGCHANGE} 0 "STR:Environment" /TIMEOUT=5000
!macroend

; --- Добавить $INSTDIR в machine PATH (если ещё нет) --------------------------
!macro AddToPath
  ReadRegStr $0 HKLM "${ENV_KEY}" "Path"
  StrCpy $1 ";$0;"
  ${StrStr} $2 $1 ";$INSTDIR;"
  ${If} ${Errors}
    ; Нет нашего каталога — дописываем в конец (REG_EXPAND_SZ сохраняем).
    StrCpy $3 "$0"
    StrLen $4 "$0"
    ${If} $4 > 0
      StrCpy $5 "$0" 1 -1
      ${If} $5 == ";"
        StrCpy $3 "$0"  ; уже заканчивается на ';'
      ${Else}
        StrCpy $3 "$0;"
      ${EndIf}
    ${EndIf}
    WriteRegExpandStr HKLM "${ENV_KEY}" "Path" "$3$INSTDIR"
    !insertmacro BroadcastEnvChange
  ${EndIf}
!macroend

; --- Убрать $INSTDIR из machine PATH ------------------------------------------
!macro RemoveFromPath
  ReadRegStr $0 HKLM "${ENV_KEY}" "Path"
  ${UnStrRep} $1 "$0" ";$INSTDIR" ""
  ${If} $1 != $0
    WriteRegExpandStr HKLM "${ENV_KEY}" "Path" "$1"
    !insertmacro BroadcastEnvChange
  ${EndIf}
!macroend

Function .onInit
  SetRegView 64
  ; Обновление: если старая версия установлена — тихо сносим её сначала.
  ReadRegStr $0 HKLM "${UNINSTALL_KEY}" "UninstallString"
  ${If} $0 != ""
    MessageBox MB_YESNO|MB_ICONQUESTION "Погребок уже установлен. Удалить предыдущую версию и продолжить?" /SD IDYES IDYES +2
      Abort
    ExecWait '$0 /S _?=$INSTDIR' $1
  ${EndIf}
FunctionEnd

Section "Погребок" SecMain
  SectionIn RO
  SetRegView 64
  SetShellVarContext all

  SetOutPath "$INSTDIR"
  File "${INPUT_DIR}/pogrebok.exe"
  File "${INPUT_DIR}/pogrebok-daemon.exe"
  File "${INPUT_DIR}/pogrebok-svc.exe"
  File "${INPUT_DIR}/pogrebok-gui.exe"
  SetOutPath "$INSTDIR\appliance"
  File "${INPUT_DIR}/appliance/pogrebok-rootfs.tar.gz"

  ; Ярлык GUI в меню Пуск (рядом с бинарниками — GUI резолвит их от себя).
  CreateDirectory "$SMPROGRAMS\Погребок"
  CreateShortcut "$SMPROGRAMS\Погребок\Погребок.lnk" "$INSTDIR\pogrebok-gui.exe" "" "$INSTDIR\pogrebok-gui.exe" 0 "" "" "Погребок — управление бутылками"

  ; PATH (machine).
  !insertmacro AddToPath

  ; Реестр: наш ключ + запись «Установка и удаление программ».
  WriteRegStr HKLM "Software\Pogrebok" "InstallDir" "$INSTDIR"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayName" "Погребок"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "Publisher" "Погребок"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegDWORD HKLM "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINSTALL_KEY}" "NoRepair" 1
  WriteUninstaller "$INSTDIR\Uninstall.exe"

  ; WSL: включение компонентов без дистрибутива. 3010 («нужна
  ; перезагрузка») переводится в успех; принудительную перезагрузку не
  ; назначаем — doctor и README объясняют. Отказ = отказ установки
  ; (паритет с MSI Return="check").
  ${DisableX64FSRedirection}
  nsExec::ExecToStack 'wsl.exe --status'
  Pop $0
  Pop $1
  ${If} $0 != 0
    nsExec::ExecToStack 'wsl.exe --install --no-distribution'
    Pop $0
    Pop $1
    ${If} $0 != 0
    ${AndIf} $0 != 3010
      ${EnableX64FSRedirection}
      MessageBox MB_ICONSTOP "Не удалось включить компоненты WSL (код $0). Включите WSL вручную (wsl --install) и повторите установку."
      Abort "WSL недоступен"
    ${EndIf}
  ${EndIf}
  ${EnableX64FSRedirection}

  ; Служба маршрутов (автозапуск, SYSTEM). Отказ = отказ установки;
  ; поверх ручной установки службы установщик откажет с внятным текстом
  ; (снимите службу: pogrebok-svc.exe --uninstall).
  nsExec::ExecToLog '"$INSTDIR\pogrebok-svc.exe" --install'
  Pop $0
  ${If} $0 != 0
    MessageBox MB_ICONSTOP "Не удалось установить службу маршрутов (код $0). Если служба уже существует — снимите её: pogrebok-svc.exe --uninstall, затем повторите установку."
    Abort "служба не установлена"
  ${EndIf}
SectionEnd

Section "Uninstall"
  SetRegView 64
  SetShellVarContext all

  ; Служба снимается до удаления файлов (зеркало MSI UninstallSvc,
  ; ошибки игнорируем — службы может и не быть).
  nsExec::ExecToLog '"$INSTDIR\pogrebok-svc.exe" --uninstall'
  Pop $0

  Delete "$SMPROGRAMS\Погребок\Погребок.lnk"
  RMDir "$SMPROGRAMS\Погребок"

  !insertmacro RemoveFromPath

  Delete "$INSTDIR\pogrebok.exe"
  Delete "$INSTDIR\pogrebok-daemon.exe"
  Delete "$INSTDIR\pogrebok-svc.exe"
  Delete "$INSTDIR\pogrebok-gui.exe"
  Delete "$INSTDIR\appliance\pogrebok-rootfs.tar.gz"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR\appliance"
  RMDir "$INSTDIR"

  DeleteRegKey HKLM "${UNINSTALL_KEY}"
  DeleteRegKey HKLM "Software\Pogrebok"
SectionEnd
