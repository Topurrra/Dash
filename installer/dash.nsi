; Dash installer (Windows, per user — no admin prompt).
; Build: dash/installer/build.ps1 (release build + makensis).
; Installs dash.exe to %LOCALAPPDATA%\Programs\Dash with a Start menu
; shortcut and an uninstaller. Search index and clipboard history live in
; %LOCALAPPDATA%\Dash and are never removed here.
; Runtimes (installer\runtimes, see its README): PDFium (PDF text), Tesseract
; and its language files (OCR), the Visual C++ runtime Tesseract needs, and
; the EmbeddingGemma 2 model (search by meaning).
!define RUNTIMES "runtimes"

Unicode true
!include "MUI2.nsh"

!ifndef VERSION
  !define VERSION "1.0.0"
!endif
!ifndef OUTFILE
  !define OUTFILE "Dash_${VERSION}_x64-setup.exe"
!endif
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\Dash"
!define RUN_KEY "Software\Microsoft\Windows\CurrentVersion\Run"

Name "Dash"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\Programs\Dash"
InstallDirRegKey HKCU "${UNINSTALL_KEY}" "InstallLocation"
RequestExecutionLevel user
SetCompressor /SOLID lzma
VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "Dash"
VIAddVersionKey "FileDescription" "Dash — search and clipboard history"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "© Dash contributors"

!define MUI_ICON "dash.ico"
!define MUI_UNICON "dash.ico"
!define MUI_FINISHPAGE_RUN "$INSTDIR\dash.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Start Dash (Ctrl+Alt+F search, Ctrl+Alt+C clipboard)"

!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

; A running Dash (and its index workers) holds dash.exe open.
!macro StopDash
  nsExec::Exec 'taskkill /IM dash.exe /F'
  Pop $0
  Sleep 500
!macroend

Section "Dash" Main
  !insertmacro StopDash
  SetOutPath "$INSTDIR"
  File "..\target\release\dash.exe"
  File "dash.ico"
  File "..\LICENSE"
  File "..\THIRD-PARTY-NOTICES.md"
  File "${RUNTIMES}\pdfium-runtime\pdfium.dll"
  File "${RUNTIMES}\tesseract-runtime\tesseract.exe"
  File "${RUNTIMES}\tesseract-runtime\*.dll"
  File "${RUNTIMES}\msvc-runtime\*.dll"
  SetOutPath "$INSTDIR\tessdata"
  File "${RUNTIMES}\tessdata-runtime\*.traineddata"
  SetOutPath "$INSTDIR\embedding-runtime"
  File "${RUNTIMES}\embedding-runtime\tokenizer.json"
  SetOutPath "$INSTDIR\embedding-runtime\onnx"
  File "${RUNTIMES}\embedding-runtime\onnx\model_q4.onnx"
  File "${RUNTIMES}\embedding-runtime\onnx\model_q4.onnx_data"
  SetOutPath "$INSTDIR"
  WriteUninstaller "$INSTDIR\uninstall.exe"

  CreateShortcut "$SMPROGRAMS\Dash.lnk" "$INSTDIR\dash.exe" "" "$INSTDIR\dash.ico"

  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "Dash"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "Publisher" "Dash"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\dash.ico"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1
SectionEnd

; A silent install is Dash updating itself (app/src/update.rs, `/S`): it
; closed Dash, so start it again, in the tray.
Function .onInstSuccess
  IfSilent 0 +2
    Exec '"$INSTDIR\dash.exe" --background'
FunctionEnd

Section "Uninstall"
  !insertmacro StopDash
  Delete "$INSTDIR\dash.exe"
  Delete "$INSTDIR\dash.ico"
  Delete "$INSTDIR\LICENSE"
  Delete "$INSTDIR\THIRD-PARTY-NOTICES.md"
  Delete "$INSTDIR\tesseract.exe"
  Delete "$INSTDIR\*.dll"
  RMDir /r "$INSTDIR\tessdata"
  RMDir /r "$INSTDIR\embedding-runtime"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\Dash.lnk"
  ; "Start with Windows" from Dash's settings.
  DeleteRegValue HKCU "${RUN_KEY}" "Dash"
  DeleteRegKey HKCU "${UNINSTALL_KEY}"
SectionEnd
