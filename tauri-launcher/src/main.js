import { invoke } from "@tauri-apps/api/core";
import { version } from "../package.json";
import "./styles.css";
import { mountLinkArt, setLinkArt } from "./art.js";
import {
  PAIR_COPY,
  canGoBack,
  deriveScreen,
  pairStage,
  pairingSkipped,
  transportName,
} from "./flow.js";
import {
  localOnlyTetherSelection,
  localOnlyTetherSupported,
  networkManagerCheckboxState,
  networkManagerPolicyUnresolved,
} from "./platform.js";
import { statusPresentation } from "./status.js";

const $ = (selector) => document.querySelector(selector);
const app = $("#app");
const backButton = $("#back");
const prefsOpenButton = $("#prefs-open");
const prefsCloseButton = $("#prefs-close");
const prefs = $("#prefs");
const transportCards = Array.from(document.querySelectorAll(".transport-card"));
const pairLead = $("#pair-lead");
const pairPattern = $("#pair-pattern");
const pairHint = $("#pair-hint");
const pairButton = $("#pair");
const approveButton = $("#approve-pairing");
const cancelPairingButton = $("#cancel-pairing");
const transportBadge = $("#transport-badge");
const transportNameLabel = $("#transport-name");
const startButton = $("#start");
const playLead = $("#play-lead");
const stopButton = $("#stop");
const notice = $("#notice");
const noticeText = $("#notice-text");
const noticeAdminButton = $("#restart-as-admin");
const noticeDismissButton = $("#notice-dismiss");
const keySlots = Array.from(document.querySelectorAll(".key-slot"));
const metricsInput = $("#metrics");
const legacyV4Input = $("#legacy-v4");
const localOnlyTetherInput = $("#local-only-tether");
const adminAction = $("#admin-action");
const adminActionText = $("#admin-action-text");
const prefsAdminButton = $("#prefs-restart-as-admin");
const refreshTetherPolicyButton = $("#refresh-tether-policy");
const pairedPref = $("#paired-pref");
const forgetDeviceButton = $("#forget-device");
const versionLabel = $("#version");

const PREF = {
  transport: "doritrack.transport",
  keys: "doritrack.keys",
  metrics: "doritrack.metrics",
  legacyV4: "doritrack.legacyV4",
  localOnlyTether: "doritrack.localOnlyTether",
};
const KEY_PATTERN = /^[a-zA-Z0-9]$/;
const DEFAULT_KEYS = ["s", "d", "f", "j", "k", "l"];

function readPref(key) {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function writePref(key, value) {
  try {
    localStorage.setItem(key, String(value));
  } catch {
    // The choice still applies for this session.
  }
}

const store = {
  transport: ["usb", "wifi"].includes(readPref(PREF.transport)) ? readPref(PREF.transport) : null,
  metrics: readPref(PREF.metrics) === "true",
  legacyV4: readPref(PREF.legacyV4) === "true",
  localOnlyTether: readPref(PREF.localOnlyTether) === "true",
};

let status = {};
let booted = false;
let choosingTransport = false;
let approvalSent = false;
let stopping = false;
let launcherElevated;
let elevationModel;
let linuxTetherPolicy;
let linuxTetherRequested = store.localOnlyTether;
let tetherPolicyBusy = false;
let activeSlotIndex = 0;
let noticeTimer;
let noticeSource = null;
let dismissedPhaseNotice = "";
let lastFitScreen = null;

const stageArt = Array.from(document.querySelectorAll('[data-art="stage"]')).map((el) =>
  mountLinkArt(el, store.transport ?? "usb", "idle"),
);
transportCards.forEach((card) => {
  mountLinkArt(card.querySelector(".card-art"), card.dataset.transport, "idle");
});
versionLabel.textContent = `v${version}`;

(readPref(PREF.keys) ?? DEFAULT_KEYS.join(",")).split(",").forEach((key, index) => {
  if (keySlots[index] && KEY_PATTERN.test(key)) keySlots[index].value = key.toLowerCase();
});
metricsInput.checked = store.metrics;
legacyV4Input.checked = store.legacyV4;
localOnlyTetherInput.checked = store.localOnlyTether;

function transport() {
  return store.transport ?? "usb";
}

function effectivePrefs() {
  return { transport: store.transport, legacyV4: store.legacyV4 };
}

// --- notices -------------------------------------------------------------

function showNotice(text, { sticky = false, admin = false, source = "action" } = {}) {
  if (source === "phase" && text === dismissedPhaseNotice) return;
  if (source !== "phase") dismissedPhaseNotice = "";
  clearTimeout(noticeTimer);
  noticeText.textContent = text;
  noticeAdminButton.hidden = !admin;
  notice.hidden = false;
  noticeSource = source;
  if (!sticky) noticeTimer = setTimeout(hideNotice, 2800);
}

function hideNotice() {
  clearTimeout(noticeTimer);
  notice.hidden = true;
  noticeSource = null;
}

function dismissNotice() {
  // A host-reported problem stays dismissed until the host says something new.
  if (noticeSource === "phase") dismissedPhaseNotice = noticeText.textContent;
  hideNotice();
}

function fail(error) {
  showNotice(String(error), { sticky: true });
}

// --- rendering -----------------------------------------------------------

function render() {
  if (!booted) return;
  const screen = deriveScreen(status, effectivePrefs(), { choosingTransport });
  const presentation = statusPresentation(status);
  const running = Boolean(status.running);
  const wifi = transport() === "wifi";

  app.dataset.screen = screen;
  app.dataset.transport = transport();
  const stage = pairStage(status, { approvalSent });
  backButton.hidden = !canGoBack(screen, stage, {
    running,
    hasTransport: Boolean(store.transport),
    choosingTransport,
  });
  prefsOpenButton.disabled = false;

  // Pair screen.
  const copy = PAIR_COPY[stage];
  pairLead.textContent = copy.lead;
  pairHint.textContent = copy.hint;
  pairHint.hidden = !copy.hint;
  const pattern = Array.isArray(status.pattern) ? status.pattern : [];
  const showPattern = (stage === "pattern" || stage === "approve") && pattern.length === 8;
  pairPattern.hidden = !showPattern;
  if (showPattern) {
    pairPattern.replaceChildren(
      ...pattern.map((lane) => {
        const item = document.createElement("li");
        item.textContent = String(lane);
        return item;
      }),
    );
  }
  pairButton.hidden = stage !== "start";
  approveButton.hidden = stage !== "approve";
  cancelPairingButton.hidden = stage === "start" || stage === "finishing";
  cancelPairingButton.disabled = stopping;
  approveButton.disabled = !status.can_approve;

  // Ready screen.
  transportNameLabel.textContent = pairingSkipped(effectivePrefs())
    ? `${transportName(transport())} · v4`
    : transportName(transport());
  const linuxPolicyUnresolved =
    !wifi &&
    elevationModel === "network-manager" &&
    networkManagerPolicyUnresolved(linuxTetherPolicy, linuxTetherRequested);
  startButton.disabled =
    running ||
    Boolean(status.recovery_needs_admin) ||
    (!wifi && tetherPolicyBusy) ||
    linuxPolicyUnresolved ||
    elevationModel === undefined;

  // Play screen.
  playLead.textContent = presentation.label;
  playLead.dataset.tone = presentation.tone;
  stopButton.disabled = stopping || !running;

  // Link illustration.
  let link = "idle";
  if (screen === "pair") {
    link = stage === "start" ? "idle" : stage === "waiting" ? "searching" : "connected";
  } else if (screen === "play") {
    link = presentation.link;
  }
  stageArt.forEach((svg) => setLinkArt(svg, transport(), link));

  // Preferences.
  keySlots.forEach((slot) => {
    slot.disabled = running;
  });
  metricsInput.disabled = running;
  legacyV4Input.disabled = running;
  pairedPref.hidden = !status.paired;
  forgetDeviceButton.disabled = running;
  renderTetherOption(running, wifi);

  // Phase-driven notices replace each other and clear when the phase moves on.
  if (presentation.phase === "recovery-needs-admin" || status.recovery_needs_admin) {
    showNotice(presentation.label, { sticky: true, admin: true, source: "phase" });
  } else if (presentation.phase === "fatal") {
    showNotice(presentation.label, { sticky: true, source: "phase" });
  } else if (presentation.detail && screen !== "pair") {
    showNotice(presentation.detail, { sticky: true, source: "phase" });
  } else {
    dismissedPhaseNotice = "";
    if (noticeSource === "phase") hideNotice();
  }

  if (screen !== lastFitScreen) {
    lastFitScreen = screen;
    fitWindowToContent();
  }
}

function renderTetherOption(running, wifi) {
  const supported = localOnlyTetherSupported(elevationModel);
  const tetherPref = localOnlyTetherInput.closest(".usb-only");
  tetherPref.hidden = elevationModel !== undefined && !supported;
  if (!supported) {
    adminAction.hidden = true;
    localOnlyTetherInput.disabled = true;
    return;
  }

  if (elevationModel === "network-manager") {
    prefsAdminButton.hidden = true;
    refreshTetherPolicyButton.hidden = false;
    refreshTetherPolicyButton.disabled = running || tetherPolicyBusy;
    if (!tetherPolicyBusy) {
      const checkbox = networkManagerCheckboxState(linuxTetherPolicy, linuxTetherRequested);
      localOnlyTetherInput.checked = checkbox.checked;
      localOnlyTetherInput.indeterminate = checkbox.indeterminate;
    }
    adminActionText.textContent = linuxTetherPolicy?.message ?? "Checking the USB link…";
    adminAction.hidden = wifi;
    localOnlyTetherInput.disabled =
      running || tetherPolicyBusy || wifi || !linuxTetherPolicy?.available;
    return;
  }

  localOnlyTetherInput.indeterminate = false;
  localOnlyTetherInput.disabled = running || wifi;
  refreshTetherPolicyButton.hidden = true;
  const needsAdmin = localOnlyTetherInput.checked && launcherElevated !== true;
  prefsAdminButton.hidden = !needsAdmin;
  prefsAdminButton.disabled = running;
  adminActionText.textContent = needsAdmin ? "Needs admin." : "";
  adminAction.hidden = wifi || !needsAdmin;
}

function applyHostStatus(result) {
  const wasPaired = Boolean(status.paired);
  const wasPairing = Boolean(status.pairing);
  status = result ?? {};
  stopping = Boolean(status.stopping);
  if (!status.pairing) approvalSent = false;
  if (wasPairing && !status.pairing && status.paired && !wasPaired) {
    showNotice("Paired.");
  }
  if (!booted) {
    booted = true;
    app.classList.add("booted");
  }
  render();
}

async function refreshStatus() {
  try {
    applyHostStatus(await invoke("host_status"));
  } catch (error) {
    if (!booted) return;
    fail(error);
  }
}

// --- elevation / Linux tether policy -------------------------------------

async function refreshElevation() {
  try {
    launcherElevated = await invoke("launcher_is_elevated");
  } catch {
    launcherElevated = false;
  }
  render();
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
    await refreshLinuxTetherPolicy({ quiet: true });
  } else {
    render();
  }
}

async function refreshLinuxTetherPolicy({ quiet = false } = {}) {
  if (elevationModel !== "network-manager" || tetherPolicyBusy) return;
  tetherPolicyBusy = true;
  render();
  try {
    linuxTetherPolicy = await invoke("linux_local_only_tether_status");
    if (linuxTetherPolicy.enabled || linuxTetherPolicy.configured) {
      linuxTetherRequested = true;
      writePref(PREF.localOnlyTether, true);
    }
  } catch (error) {
    linuxTetherPolicy = {
      available: false,
      enabled: false,
      configured: false,
      mixed: false,
      message: String(error),
    };
    if (!quiet) fail(error);
  } finally {
    tetherPolicyBusy = false;
    render();
  }
}

// --- key slots -----------------------------------------------------------

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
  keySlots[index].value = value.toLowerCase();
  keySlots[index].removeAttribute("aria-invalid");
  saveKeys();
}

function saveKeys() {
  writePref(PREF.keys, serializedKeys());
}

function handleSlotInput(index) {
  const slot = keySlots[index];
  const value = slot.value.toLowerCase().match(/[a-z0-9]/)?.[0] ?? "";
  slot.value = value;
  slot.removeAttribute("aria-invalid");
  saveKeys();
  if (value && index < keySlots.length - 1) focusSlot(index + 1);
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
  event.preventDefault();
  const pastedKeys = event.clipboardData?.getData("text").toLowerCase().match(/[a-z0-9]/g) ?? [];
  if (!pastedKeys.length) return;
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
    if (valid) slot.removeAttribute("aria-invalid");
    else slot.setAttribute("aria-invalid", "true");
    if (!valid && invalidIndex === -1) invalidIndex = index;
  });
  return invalidIndex;
}

function serializedKeys() {
  return keySlots.map((slot) => slot.value.toLowerCase()).join(",");
}

// --- preferences sheet ---------------------------------------------------

function openPrefs() {
  prefs.hidden = false;
  app.classList.add("prefs-open");
  prefsOpenButton.setAttribute("aria-expanded", "true");
  prefsCloseButton.focus();
}

function closePrefs() {
  prefs.hidden = true;
  app.classList.remove("prefs-open");
  prefsOpenButton.setAttribute("aria-expanded", "false");
  prefsOpenButton.focus();
}

// --- actions -------------------------------------------------------------

async function startHost() {
  const usb = transport() === "usb";
  if (usb && tetherPolicyBusy) {
    showNotice("Checking the USB link first.");
    return;
  }
  if (status.recovery_needs_admin) {
    showNotice("Admin access is needed to repair the USB link.", { sticky: true, admin: true });
    return;
  }
  const legacyV4 = pairingSkipped(effectivePrefs());
  if (!status.paired && !legacyV4) {
    render();
    return;
  }
  if (elevationModel === undefined) await initElevation();
  if (
    usb &&
    elevationModel === "network-manager" &&
    networkManagerPolicyUnresolved(linuxTetherPolicy, linuxTetherRequested)
  ) {
    showNotice(linuxTetherPolicy?.message ?? "Reconnect the phone, then check the USB link.", {
      sticky: true,
    });
    openPrefs();
    refreshTetherPolicyButton.focus();
    return;
  }
  // Never send the option through on an unimplemented platform, regardless
  // of a stale or manually altered checkbox state. The backend independently
  // verifies the NetworkManager profile on Linux before it starts the host.
  const localOnlyTether = localOnlyTetherSelection(
    elevationModel,
    elevationModel === "network-manager" ? linuxTetherRequested : localOnlyTetherInput.checked,
    transport(),
  );
  if (localOnlyTether && elevationModel === "launcher") {
    if (launcherElevated === undefined) await refreshElevation();
    if (launcherElevated !== true) {
      showNotice("Restart as admin to keep the phone's internet on the phone.", {
        sticky: true,
        admin: true,
      });
      return;
    }
  }
  const invalidIndex = firstInvalidSlot();
  if (invalidIndex !== -1) {
    showNotice("Give every lane one letter or number.", { sticky: true });
    openPrefs();
    focusSlot(invalidIndex);
    return;
  }

  hideNotice();
  startButton.disabled = true;
  try {
    applyHostStatus(
      await invoke("start_host", {
        keys: serializedKeys(),
        metrics: metricsInput.checked,
        localOnlyTether,
        transport: transport(),
        legacyV4,
      }),
    );
  } catch (error) {
    fail(error);
    render();
  }
}

async function stopHost() {
  if (stopping) return;
  try {
    stopping = true;
    render();
    applyHostStatus(await invoke("stop_host"));
  } catch (error) {
    stopping = false;
    fail(error);
  }
}

async function restartAsAdmin(button) {
  try {
    button.disabled = true;
    showNotice("Restarting as admin…");
    await invoke("restart_as_admin");
  } catch (error) {
    button.disabled = false;
    fail(error);
  }
}

// --- wiring --------------------------------------------------------------

transportCards.forEach((card) => {
  card.addEventListener("click", () => {
    store.transport = card.dataset.transport;
    writePref(PREF.transport, store.transport);
    choosingTransport = false;
    hideNotice();
    render();
  });
});

backButton.addEventListener("click", () => {
  // On the connect screen, back returns to where "Change" was pressed; on the
  // pair screen, back reopens the transport choice.
  choosingTransport = app.dataset.screen === "pair";
  render();
});

transportBadge.addEventListener("click", () => {
  if (status.running) return;
  choosingTransport = true;
  render();
});

pairButton.addEventListener("click", async () => {
  try {
    hideNotice();
    pairButton.disabled = true;
    applyHostStatus(await invoke("begin_pairing", { transport: transport() }));
  } catch (error) {
    fail(error);
  } finally {
    pairButton.disabled = false;
  }
});

approveButton.addEventListener("click", async () => {
  try {
    approveButton.disabled = true;
    approvalSent = true;
    applyHostStatus(await invoke("approve_pairing"));
  } catch (error) {
    approvalSent = false;
    fail(error);
    render();
  }
});

cancelPairingButton.addEventListener("click", stopHost);
startButton.addEventListener("click", startHost);
stopButton.addEventListener("click", stopHost);

forgetDeviceButton.addEventListener("click", async () => {
  try {
    applyHostStatus(await invoke("forget_device"));
    closePrefs();
    showNotice("Phone forgotten.");
  } catch (error) {
    fail(error);
  }
});

metricsInput.addEventListener("change", () => {
  store.metrics = metricsInput.checked;
  writePref(PREF.metrics, store.metrics);
});

legacyV4Input.addEventListener("change", () => {
  store.legacyV4 = legacyV4Input.checked;
  writePref(PREF.legacyV4, store.legacyV4);
  render();
});

localOnlyTetherInput.addEventListener("change", async () => {
  if (elevationModel === "network-manager") {
    if (status.running || tetherPolicyBusy) {
      render();
      return;
    }
    const enabled = localOnlyTetherInput.checked;
    localOnlyTetherInput.indeterminate = false;
    tetherPolicyBusy = true;
    render();
    try {
      linuxTetherPolicy = await invoke("set_linux_local_only_tether", { enabled });
      linuxTetherRequested = enabled;
      writePref(PREF.localOnlyTether, enabled);
    } catch (error) {
      fail(error);
      tetherPolicyBusy = false;
      await refreshLinuxTetherPolicy({ quiet: true });
      return;
    } finally {
      tetherPolicyBusy = false;
      render();
    }
    return;
  }
  store.localOnlyTether = localOnlyTetherInput.checked;
  writePref(PREF.localOnlyTether, store.localOnlyTether);
  render();
});

refreshTetherPolicyButton.addEventListener("click", () => refreshLinuxTetherPolicy());
noticeAdminButton.addEventListener("click", () => restartAsAdmin(noticeAdminButton));
prefsAdminButton.addEventListener("click", () => restartAsAdmin(prefsAdminButton));
noticeDismissButton.addEventListener("click", dismissNotice);
prefsOpenButton.addEventListener("click", openPrefs);
prefsCloseButton.addEventListener("click", closePrefs);
$("#prefs-form").addEventListener("submit", (event) => event.preventDefault());
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && !prefs.hidden) closePrefs();
});

keySlots.forEach((slot, index) => {
  slot.addEventListener("focus", () => updateActiveSlot(index));
  slot.addEventListener("keydown", (event) => handleSlotKeyDown(event, index));
  slot.addEventListener("input", () => handleSlotInput(index));
  slot.addEventListener("paste", (event) => handleSlotPaste(event, index));
});
updateActiveSlot(0);

async function fitWindowToContent() {
  // GTK's text-DPI scaling can render this layout far taller than the fixed
  // size chosen for Windows at 96 DPI; grow the window to fit the current
  // screen. The webview's own box model and devicePixelRatio are trusted
  // over backend inner-size queries, which disagree on GTK/Wayland by a
  // large constant offset. Grow-only by construction.
  const docEl = document.documentElement;
  const currentWidth = docEl.clientWidth;
  const currentHeight = docEl.clientHeight;
  const wantedWidth = docEl.scrollWidth;
  const wantedHeight = app.scrollHeight + 8;
  if (wantedWidth <= currentWidth && wantedHeight <= currentHeight) return;
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

initElevation();
setInterval(refreshStatus, 250);
refreshStatus();
