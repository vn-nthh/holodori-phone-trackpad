import { invoke } from "@tauri-apps/api/core";
import "./styles.css";
import { statusPresentation } from "./status.js";
import { localOnlyTetherSelection, localOnlyTetherSupported } from "./platform.js";

const form = document.querySelector("#settings-form");
const keySlots = Array.from(document.querySelectorAll(".key-slot"));
const metricsInput = document.querySelector("#metrics");
const localOnlyTetherInput = document.querySelector("#local-only-tether");
const adminAction = document.querySelector("#admin-action");
const adminActionText = document.querySelector("#admin-action-text");
const restartAsAdminButton = document.querySelector("#restart-as-admin");
const status = document.querySelector("#status");
const startButton = document.querySelector("#start");
const stopButton = document.querySelector("#stop");
const LOCAL_ONLY_TETHER_PREFERENCE = "doritrack.localOnlyTether";

try {
  localOnlyTetherInput.checked = localStorage.getItem(LOCAL_ONLY_TETHER_PREFERENCE) === "true";
} catch {
  // The launcher can still work if WebView storage is unavailable.
}

const KEY_PATTERN = /^[a-zA-Z0-9]$/;
let launcherElevated;
let elevationModel;
let activeSlotIndex = 0;
let stopping = false;
let recoveryNeedsAdmin = false;

function setStatus(message, tone = "neutral", phase = "ready") {
  status.textContent = message;
  status.dataset.tone = tone;
  status.dataset.phase = phase;
}

function setRunning(running) {
  keySlots.forEach((slot) => {
    slot.disabled = running;
  });
  metricsInput.disabled = running;
  // Stays disabled regardless of `running` when the option is unsupported on
  // this platform (see `updateAdminAction`); otherwise it just follows
  // `running` like the other controls.
  localOnlyTetherInput.disabled = running || elevationModel === "unsupported";
  restartAsAdminButton.disabled = running;
  startButton.disabled = running || recoveryNeedsAdmin;
  stopButton.disabled = !running;
}

function updateAdminAction() {
  if (elevationModel !== undefined && !localOnlyTetherSupported(elevationModel)) {
    localOnlyTetherInput.checked = false;
    localOnlyTetherInput.disabled = true;
    restartAsAdminButton.hidden = true;
    adminActionText.textContent = "This option is Windows-only.";
    adminAction.hidden = false;
    return;
  }

  restartAsAdminButton.hidden = false;
  adminActionText.textContent = recoveryNeedsAdmin
    ? "Administrator access is required to recover USB-tether routes."
    : "Needs admin elevation.";
  const optionNeedsAdmin = localOnlyTetherInput.checked && launcherElevated !== true;
  adminAction.hidden = !recoveryNeedsAdmin && !optionNeedsAdmin;
}

function applyHostStatus(result) {
  recoveryNeedsAdmin = Boolean(result.recovery_needs_admin);
  stopping = Boolean(result.stopping);
  setRunning(Boolean(result.running));
  const presentation = statusPresentation(result);
  setStatus(presentation.label, presentation.tone, presentation.phase);
  updateAdminAction();
}

async function refreshElevation() {
  try {
    launcherElevated = await invoke("launcher_is_elevated");
  } catch {
    // If detection is unavailable, keep the recovery action available.
    launcherElevated = false;
  }
  updateAdminAction();
}

async function initElevation() {
  try {
    elevationModel = await invoke("elevation_model");
  } catch {
    elevationModel = "unsupported";
  }
  if (elevationModel === "launcher") {
    await refreshElevation();
  } else {
    updateAdminAction();
  }
}

function updateActiveSlot(index) {
  activeSlotIndex = Math.max(0, Math.min(index, keySlots.length - 1));
  keySlots.forEach((slot, slotIndex) => {
    slot.dataset.active = slotIndex === activeSlotIndex ? "true" : "false";
  });
}

function focusSlot(index) {
  updateActiveSlot(index);
  const slot = keySlots[activeSlotIndex];
  slot.focus({ preventScroll: true });
  slot.select();
}

function setSlotValue(index, value) {
  const slot = keySlots[index];
  slot.value = value.toLowerCase();
  slot.removeAttribute("aria-invalid");
}

function handleSlotInput(index) {
  const slot = keySlots[index];
  const value = slot.value.toLowerCase().match(/[a-z0-9]/)?.[0] ?? "";
  slot.value = value;
  slot.removeAttribute("aria-invalid");

  if (value && index < keySlots.length - 1) {
    focusSlot(index + 1);
  }
}

function handleSlotKeyDown(event, index) {
  if (event.ctrlKey || event.metaKey || event.altKey) return;

  if (KEY_PATTERN.test(event.key)) {
    event.preventDefault();
    setSlotValue(index, event.key);
    if (index < keySlots.length - 1) focusSlot(index + 1);
    return;
  }

  if (event.key === "Backspace") {
    event.preventDefault();
    if (keySlots[index].value) {
      setSlotValue(index, "");
    } else if (index > 0) {
      setSlotValue(index - 1, "");
      focusSlot(index - 1);
    }
    return;
  }

  if (event.key === "Delete") {
    event.preventDefault();
    setSlotValue(index, "");
    return;
  }

  if (event.key === "ArrowLeft") {
    event.preventDefault();
    focusSlot(index - 1);
    return;
  }

  if (event.key === "ArrowRight") {
    event.preventDefault();
    focusSlot(index + 1);
    return;
  }

  if (event.key === "Home") {
    event.preventDefault();
    focusSlot(0);
    return;
  }

  if (event.key === "End") {
    event.preventDefault();
    focusSlot(keySlots.length - 1);
  }
}

function handleSlotPaste(event, index) {
  const pastedKeys = event.clipboardData?.getData("text").toLowerCase().match(/[a-z0-9]/g) ?? [];
  if (!pastedKeys.length) {
    event.preventDefault();
    return;
  }

  event.preventDefault();
  const lastSlotIndex = Math.min(index + pastedKeys.length - 1, keySlots.length - 1);
  pastedKeys.slice(0, keySlots.length - index).forEach((key, offset) => {
    setSlotValue(index + offset, key);
  });
  focusSlot(Math.min(lastSlotIndex + 1, keySlots.length - 1));
}

function firstInvalidSlot() {
  let invalidIndex = -1;
  keySlots.forEach((slot, index) => {
    const valid = KEY_PATTERN.test(slot.value);
    if (valid) {
      slot.removeAttribute("aria-invalid");
    } else {
      slot.setAttribute("aria-invalid", "true");
    }
    if (!valid && invalidIndex === -1) invalidIndex = index;
  });
  return invalidIndex;
}

function serializedKeys() {
  return keySlots.map((slot) => slot.value.toLowerCase()).join(",");
}

async function refreshStatus() {
  try {
    const result = await invoke("host_status");
    applyHostStatus(result);
  } catch (error) {
    setStatus(String(error), "error");
  }
}

form.addEventListener("submit", async (event) => {
  event.preventDefault();
  if (recoveryNeedsAdmin) {
    setStatus("Restart as admin to recover USB-tether routes.", "error", "recovery-needs-admin");
    restartAsAdminButton.focus();
    return;
  }

  if (elevationModel === undefined) await initElevation();
  // Never send the option through on a platform where it is unsupported,
  // regardless of the checkbox's current DOM state.
  const localOnlyTether = localOnlyTetherSelection(elevationModel, localOnlyTetherInput.checked);

  if (localOnlyTether && elevationModel === "launcher") {
    if (launcherElevated === undefined) await refreshElevation();
    if (launcherElevated !== true) {
      setStatus("Restart as admin to use this option.", "error");
      restartAsAdminButton.focus();
      return;
    }
  }

  const invalidIndex = firstInvalidSlot();

  if (invalidIndex !== -1) {
    setStatus("Set one letter or number in each of the six key slots.", "error");
    focusSlot(invalidIndex);
    return;
  }

  const keys = serializedKeys();
  try {
    setStatus("Starting...");
    const result = await invoke("start_host", {
      keys,
      metrics: metricsInput.checked,
      localOnlyTether: localOnlyTether,
    });
    applyHostStatus(result);
  } catch (error) {
    setRunning(false);
    setStatus(String(error), "error");
  }
});

localOnlyTetherInput.addEventListener("change", () => {
  try {
    localStorage.setItem(LOCAL_ONLY_TETHER_PREFERENCE, String(localOnlyTetherInput.checked));
  } catch {
    // The current selection still applies for this session.
  }
  updateAdminAction();
});

restartAsAdminButton.addEventListener("click", async () => {
  try {
    restartAsAdminButton.disabled = true;
    setStatus("Restarting as admin...");
    await invoke("restart_as_admin");
  } catch (error) {
    restartAsAdminButton.disabled = false;
    setStatus(String(error), "error");
  }
});

keySlots.forEach((slot, index) => {
  slot.addEventListener("focus", () => updateActiveSlot(index));
  slot.addEventListener("keydown", (event) => handleSlotKeyDown(event, index));
  slot.addEventListener("input", () => handleSlotInput(index));
  slot.addEventListener("paste", (event) => handleSlotPaste(event, index));
});

async function fitWindowToContent() {
  // GTK's text-DPI scaling can render this layout far taller than the fixed
  // size chosen for Windows at 96 DPI; grow the window once to fit.
  //
  // This deliberately does not ask the backend to compare against
  // `window.inner_size()` / `window.scale_factor()`: on GTK/Wayland those
  // were measured to disagree with the webview's real content box by a
  // large, constant offset, which made the old check conclude the window
  // was already big enough when it visibly was not.
  // `document.documentElement`'s box model and `window.devicePixelRatio`
  // are what the webview itself actually uses to lay out and paint, so they
  // are trusted here instead: measure in CSS pixels, convert to physical
  // pixels with `devicePixelRatio` (verified to round-trip exactly through
  // `set_size(PhysicalSize)` on this stack), and only ask the backend to
  // resize when this measurement shows real overflow -- which keeps the
  // whole operation grow-only by construction.
  const docEl = document.documentElement;
  const currentWidth = docEl.clientWidth;
  const currentHeight = docEl.clientHeight;
  const wantedWidth = docEl.scrollWidth;
  const wantedHeight = docEl.scrollHeight + 8;

  if (wantedWidth <= currentWidth && wantedHeight <= currentHeight) {
    return;
  }

  const scale = window.devicePixelRatio || 1;
  try {
    await invoke("fit_window_to_content", {
      currentWidth: Math.round(currentWidth * scale),
      currentHeight: Math.round(currentHeight * scale),
      wantedWidth: Math.round(wantedWidth * scale),
      wantedHeight: Math.round(wantedHeight * scale),
      scale,
    });
  } catch {
    // Best-effort only; the launcher still works at its default size.
  }
}

updateActiveSlot(0);
focusSlot(0);
updateAdminAction();
initElevation().then(fitWindowToContent);

stopButton.addEventListener("click", async () => {
  if (stopping) return;
  try {
    stopping = true;
    setRunning(true);
    setStatus("Stopping safely...");
    await invoke("stop_host");
  } catch (error) {
    stopping = false;
    setStatus(String(error), "error");
  }
});

setInterval(refreshStatus, 250);
refreshStatus();
