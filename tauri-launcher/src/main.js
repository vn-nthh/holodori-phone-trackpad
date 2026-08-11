import { invoke } from "@tauri-apps/api/core";
import "./styles.css";

const form = document.querySelector("#settings-form");
const keySlots = Array.from(document.querySelectorAll(".key-slot"));
const metricsInput = document.querySelector("#metrics");
const localOnlyTetherInput = document.querySelector("#local-only-tether");
const adminAction = document.querySelector("#admin-action");
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
let activeSlotIndex = 0;
let stopping = false;

function setStatus(message, tone = "neutral") {
  status.textContent = message;
  status.dataset.tone = tone;
}

function setRunning(running) {
  keySlots.forEach((slot) => {
    slot.disabled = running;
  });
  metricsInput.disabled = running;
  localOnlyTetherInput.disabled = running;
  restartAsAdminButton.disabled = running;
  startButton.disabled = running;
  stopButton.disabled = !running;
}

function updateAdminAction() {
  adminAction.hidden = !localOnlyTetherInput.checked || launcherElevated !== false;
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
    if (result.running) {
      setRunning(true);
      setStatus(result.stopping ? "Stopping safely..." : "Running");
      stopping = result.stopping;
    } else {
      setRunning(false);
      stopping = false;
      if (result.message) setStatus(result.message);
    }
  } catch (error) {
    setStatus(String(error), "error");
  }
}

form.addEventListener("submit", async (event) => {
  event.preventDefault();
  if (localOnlyTetherInput.checked) {
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
      localOnlyTether: localOnlyTetherInput.checked,
    });
    setRunning(result.running);
    setStatus(result.message);
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

updateActiveSlot(0);
focusSlot(0);
updateAdminAction();
refreshElevation();

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
