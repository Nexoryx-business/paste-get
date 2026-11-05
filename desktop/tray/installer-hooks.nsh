; SPDX-License-Identifier: GPL-3.0-or-later
;
; NSIS hooks for the Windows installer (tauri.bundle.conf.json). Uninstalling removes what
; paste-get registered for itself: its notification identity (desktop/src/notify.rs) and the
; icon it shows. Settings and paired devices, in %APPDATA%\paste-get, are kept.

!macro NSIS_HOOK_POSTUNINSTALL
  DeleteRegKey HKCU "Software\Classes\AppUserModelId\io.github.nexoryx.pasteget"
  Delete "$LOCALAPPDATA\paste-get\notification-icon.png"
  RMDir "$LOCALAPPDATA\paste-get"
!macroend
