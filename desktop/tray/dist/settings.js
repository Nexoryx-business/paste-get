// SPDX-License-Identifier: GPL-3.0-or-later
// The settings window. It reads and saves through the tray's two commands, load_settings and
// save_settings (desktop/tray/src/main.rs), which check everything before it is written.
"use strict";

const $ = (id) => document.getElementById(id);
let notifyChoices = [];

function invoke(command, args) {
  const tauri = window.__TAURI__;
  if (!tauri) return Promise.reject("this page only works inside paste-get");
  return tauri.core.invoke(command, args);
}

function say(text, kind) {
  const status = $("status");
  status.className = "status" + (kind ? " " + kind : "");
  status.textContent = text;
}

function show(form) {
  $("device_name").value = form.device_name;
  for (const choice of notifyChoices) choice.checked = choice.value === form.notify;
  $("offer_to_open_links").checked = form.offer_to_open_links;
  $("toggle_pause").value = form.toggle_pause;
  $("send_clipboard").value = form.send_clipboard;
  const login = form.start_at_login;
  $("login_section").hidden = login === null || login === undefined;
  $("start_at_login").checked = !!login;
}

function read() {
  const notify = notifyChoices.find((choice) => choice.checked);
  return {
    device_name: $("device_name").value,
    notify: notify ? notify.value : "images",
    offer_to_open_links: $("offer_to_open_links").checked,
    toggle_pause: $("toggle_pause").value,
    send_clipboard: $("send_clipboard").value,
    start_at_login: $("login_section").hidden ? null : $("start_at_login").checked,
  };
}

// "Ctrl+Alt+Shift+P" from a key press, as the hotkey parser writes it; null if it's only a
// modifier, or a key hotkeys can't use.
function hotkeyOf(event) {
  const code = event.code || "";
  let key = null;
  if (/^Key[A-Z]$/.test(code)) key = code.slice(3);
  else if (/^Digit[0-9]$/.test(code)) key = code.slice(5);
  else if (/^F([1-9]|1[0-9]|2[0-4])$/.test(code)) key = code;
  if (!key) return null;
  const parts = [];
  if (event.ctrlKey) parts.push("Ctrl");
  if (event.altKey) parts.push("Alt");
  if (event.shiftKey) parts.push("Shift");
  if (event.metaKey) parts.push("Super");
  parts.push(key);
  return parts.join("+");
}

function captureHotkeys(input) {
  input.addEventListener("keydown", (event) => {
    if (event.key === "Tab") return;
    event.preventDefault();
    if ((event.key === "Backspace" || event.key === "Delete") && !event.ctrlKey && !event.altKey) {
      input.value = "";
      return;
    }
    const hotkey = hotkeyOf(event);
    if (hotkey) input.value = hotkey;
    else if (!["Control", "Alt", "Shift", "Meta", "AltGraph"].includes(event.key)) {
      say("Hotkeys use a letter, a digit or F1 to F24, with Ctrl, Alt or Super.", "bad");
    }
  });
  // Typing is replaced by key capture; pasting a hotkey's text still works.
  input.addEventListener("beforeinput", (event) => {
    if (event.inputType !== "insertFromPaste") event.preventDefault();
  });
}

document.addEventListener("DOMContentLoaded", () => {
  notifyChoices = Array.from(document.querySelectorAll('input[name="notify"]'));
  document.querySelectorAll(".hotkey").forEach(captureHotkeys);

  $("close").addEventListener("click", () => invoke("close_window").catch(() => {}));
  $("settings").addEventListener("submit", async (event) => {
    event.preventDefault();
    $("save").disabled = true;
    say("Saving…");
    try {
      say(await invoke("save_settings", { form: read() }), "ok");
    } catch (error) {
      say(String(error), "bad");
    } finally {
      $("save").disabled = false;
    }
  });

  invoke("load_settings")
    .then(show)
    .catch((error) => say(String(error), "bad"));
});
