import { invoke } from "@tauri-apps/api/core";
import "./styles.css";
import { statusPresentation } from "./status.js";
import {
  localOnlyTetherSelection,
  localOnlyTetherSupported,
  networkManagerCheckboxState,
  networkManagerPolicyUnresolved,
} from "./platform.js";

const form = document.querySelector("#settings-form");
const transportInputs = Array.from(document.querySelectorAll('input[name="transport"]'));
const pairButton = document.querySelector("#pair");
const approvePairingButton = document.querySelector("#approve-pairing");
const forgetDeviceButton = document.querySelector("#forget-device");
const pairedState = document.querySelector("#paired-state");
const pairPattern = document.querySelector("#pair-pattern");
const quality = document.querySelector("#quality");
const keySlots = Array.from(document.querySelectorAll(".key-slot"));
const metricsInput = document.querySelector("#metrics");
const legacyV4Input = document.querySelector("#legacy-v4");
const localOnlyTetherInput = document.querySelector("#local-only-tether");
const adminAction = document.querySelector("#admin-action");
const adminActionText = document.querySelector("#admin-action-text");
const restartAsAdminButton = document.querySelector("#restart-as-admin");
const refreshTetherPolicyButton = document.querySelector("#refresh-tether-policy");
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
let hostRunning = false;
let pairing = false;
let paired = false;
let canApprovePairing = false;
let linuxTetherPolicy;
let linuxTetherRequested = localOnlyTetherInput.checked;
let tetherPolicyBusy = false;

function selectedTransport() {
  return transportInputs.find((input) => input.checked)?.value ?? "usb";
}

function saveLocalOnlyTetherPreference(value) {
  try {
    localStorage.setItem(LOCAL_ONLY_TETHER_PREFERENCE, String(value));
  } catch {
    // The current selection still applies for this session.
  }
}

function setStatus(message, tone = "neutral", phase = "ready") {
  status.textContent = message;
  status.dataset.tone = tone;
  status.dataset.phase = phase;
}

function setRunning(running) {
  hostRunning = running;
  const wifi = selectedTransport() === "wifi";
  keySlots.forEach((slot) => {
    slot.disabled = running;
  });
  transportInputs.forEach((input) => {
    input.disabled = running;
  });
  metricsInput.disabled = running;
  legacyV4Input.disabled = running || wifi;
  const linuxProfileUnavailable =
    elevationModel === "network-manager" && !linuxTetherPolicy?.available;
  const linuxPolicyUnresolved =
    elevationModel === "network-manager" &&
    networkManagerPolicyUnresolved(linuxTetherPolicy, linuxTetherRequested);
  localOnlyTetherInput.disabled =
    running ||
    tetherPolicyBusy ||
    wifi ||
    !localOnlyTetherSupported(elevationModel) ||
    linuxProfileUnavailable;
  restartAsAdminButton.disabled = running;
  refreshTetherPolicyButton.disabled = running || tetherPolicyBusy;
  pairButton.disabled = running;
  approvePairingButton.disabled = !running || !pairing || !canApprovePairing;
  forgetDeviceButton.disabled = running || !paired;
  startButton.disabled =
    running ||
    (!paired && !legacyV4Input.checked) ||
    recoveryNeedsAdmin ||
    tetherPolicyBusy ||
    linuxPolicyUnresolved ||
    elevationModel === undefined;
  stopButton.disabled = !running;
}

function renderPairing(result) {
  pairing = Boolean(result.pairing);
  paired = Boolean(result.paired);
  canApprovePairing = Boolean(result.can_approve);
  pairedState.textContent = paired ? "Paired" : "Not paired";
  pairedState.dataset.paired = String(paired);
  const pattern = Array.isArray(result.pattern) ? result.pattern : [];
  pairPattern.replaceChildren(
    ...pattern.map((lane) => {
      const item = document.createElement("span");
      item.textContent = String(lane);
      return item;
    }),
  );
  pairPattern.hidden = pattern.length !== 8;
  quality.textContent = result.quality || "";
  quality.hidden = !result.quality;
}

function updateAdminAction() {
  if (selectedTransport() === "wifi") {
    localOnlyTetherInput.checked = false;
    localOnlyTetherInput.indeterminate = false;
    localOnlyTetherInput.disabled = true;
    adminAction.hidden = true;
    setRunning(hostRunning);
    return;
  }
  if (elevationModel !== undefined && !localOnlyTetherSupported(elevationModel)) {
    localOnlyTetherInput.checked = false;
    localOnlyTetherInput.disabled = true;
    restartAsAdminButton.hidden = true;
    refreshTetherPolicyButton.hidden = true;
    adminActionText.textContent = "This option is unavailable on this platform.";
    adminAction.hidden = false;
    setRunning(hostRunning);
    return;
  }

  if (elevationModel === "network-manager") {
    restartAsAdminButton.hidden = true;
    refreshTetherPolicyButton.hidden = false;
    if (!tetherPolicyBusy) {
      const checkbox = networkManagerCheckboxState(linuxTetherPolicy, linuxTetherRequested);
      localOnlyTetherInput.checked = checkbox.checked;
      localOnlyTetherInput.indeterminate = checkbox.indeterminate;
    }
    adminActionText.textContent =
      linuxTetherPolicy?.message ?? "Checking NetworkManager for an active RNDIS tether...";
    adminAction.hidden = false;
    setRunning(hostRunning);
    return;
  }

  restartAsAdminButton.hidden = false;
  refreshTetherPolicyButton.hidden = true;
  localOnlyTetherInput.indeterminate = false;
  adminActionText.textContent = recoveryNeedsAdmin
    ? "Administrator access is required to recover USB-tether routes."
    : "Needs admin elevation.";
  const optionNeedsAdmin = localOnlyTetherInput.checked && launcherElevated !== true;
  adminAction.hidden = !recoveryNeedsAdmin && !optionNeedsAdmin;
  setRunning(hostRunning);
}

function applyHostStatus(result) {
  recoveryNeedsAdmin = Boolean(result.recovery_needs_admin);
  stopping = Boolean(result.stopping);
  renderPairing(result);
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
  } else if (elevationModel === "network-manager") {
    await refreshLinuxTetherPolicy();
  } else {
    updateAdminAction();
  }
}

async function refreshLinuxTetherPolicy({ quiet = false } = {}) {
  if (elevationModel !== "network-manager" || tetherPolicyBusy) return;
  tetherPolicyBusy = true;
  updateAdminAction();
  try {
    linuxTetherPolicy = await invoke("linux_local_only_tether_status");
    if (linuxTetherPolicy.enabled || linuxTetherPolicy.configured) {
      linuxTetherRequested = true;
      saveLocalOnlyTetherPreference(true);
    }
  } catch (error) {
    linuxTetherPolicy = {
      available: false,
      enabled: false,
      configured: false,
      mixed: false,
      message: String(error),
    };
    if (!quiet) setStatus(String(error), "error");
  } finally {
    tetherPolicyBusy = false;
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
  if (tetherPolicyBusy) {
    setStatus("Wait for the NetworkManager update to finish.", "error");
    return;
  }
  if (recoveryNeedsAdmin) {
    setStatus("Restart as admin to recover USB-tether routes.", "error", "recovery-needs-admin");
    restartAsAdminButton.focus();
    return;
  }
  if (!paired && !legacyV4Input.checked) {
    setStatus("Pair the host and phone before starting protocol v5.", "error");
    pairButton.focus();
    return;
  }

  if (elevationModel === undefined) await initElevation();
  if (
    elevationModel === "network-manager" &&
    networkManagerPolicyUnresolved(linuxTetherPolicy, linuxTetherRequested)
  ) {
    setStatus(
      linuxTetherPolicy?.message ??
        "Reconnect and check the tether before starting with the requested policy.",
      "error",
    );
    refreshTetherPolicyButton.focus();
    return;
  }
  // Never send the option through on an unimplemented platform, regardless
  // of a stale or manually altered checkbox state. The backend independently
  // verifies the NetworkManager profile on Linux before it starts the host.
  const localOnlyTether = localOnlyTetherSelection(
    elevationModel,
    elevationModel === "network-manager" ? linuxTetherRequested : localOnlyTetherInput.checked,
  );

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
      transport: selectedTransport(),
      legacyV4: legacyV4Input.checked,
    });
    applyHostStatus(result);
  } catch (error) {
    setRunning(false);
    setStatus(String(error), "error");
  }
});

localOnlyTetherInput.addEventListener("change", async () => {
  if (elevationModel === "network-manager") {
    if (hostRunning || tetherPolicyBusy) {
      updateAdminAction();
      return;
    }
    const enabled = localOnlyTetherInput.checked;
    localOnlyTetherInput.indeterminate = false;
    tetherPolicyBusy = true;
    updateAdminAction();
    setStatus("Updating the NetworkManager tether profile...");
    try {
      linuxTetherPolicy = await invoke("set_linux_local_only_tether", { enabled });
      linuxTetherRequested = enabled;
      saveLocalOnlyTetherPreference(enabled);
      setStatus(linuxTetherPolicy.message);
    } catch (error) {
      setStatus(String(error), "error");
      tetherPolicyBusy = false;
      await refreshLinuxTetherPolicy({ quiet: true });
      return;
    } finally {
      tetherPolicyBusy = false;
      updateAdminAction();
    }
    return;
  }

  saveLocalOnlyTetherPreference(localOnlyTetherInput.checked);
  updateAdminAction();
});

transportInputs.forEach((input) => {
  input.addEventListener("change", () => {
    if (selectedTransport() === "wifi") {
      legacyV4Input.checked = false;
      localOnlyTetherInput.checked = false;
    }
    updateAdminAction();
    setRunning(hostRunning);
  });
});

legacyV4Input.addEventListener("change", () => {
  setRunning(hostRunning);
  setStatus(
    legacyV4Input.checked
      ? "Legacy v4 selected: unauthenticated USB migration mode."
      : paired
        ? "Protocol v5 ready."
        : "Pair before starting protocol v5.",
    legacyV4Input.checked ? "warning" : "neutral",
  );
});

pairButton.addEventListener("click", async () => {
  try {
    setStatus("Opening a 60-second pairing window...");
    const result = await invoke("begin_pairing", { transport: selectedTransport() });
    applyHostStatus(result);
  } catch (error) {
    setStatus(String(error), "error");
  }
});

approvePairingButton.addEventListener("click", async () => {
  try {
    approvePairingButton.disabled = true;
    const result = await invoke("approve_pairing");
    applyHostStatus(result);
  } catch (error) {
    setStatus(String(error), "error");
    setRunning(hostRunning);
  }
});

forgetDeviceButton.addEventListener("click", async () => {
  try {
    const result = await invoke("forget_device");
    applyHostStatus(result);
  } catch (error) {
    setStatus(String(error), "error");
  }
});

refreshTetherPolicyButton.addEventListener("click", async () => {
  await refreshLinuxTetherPolicy();
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
