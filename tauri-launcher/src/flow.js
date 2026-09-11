// Pure screen derivation. Every screen the launcher shows is a function of
// host status plus a few remembered choices, so the UI can never show Pair
// and Start at the same time or hide a running controller behind setup.

export const SCREENS = ["connect", "pair", "ready", "play"];

export function pairingSkipped(prefs = {}) {
  return prefs.transport === "usb" && Boolean(prefs.legacyV4);
}

export function deriveScreen(status = {}, prefs = {}, ui = {}) {
  if (status.running && status.pairing) return "pair";
  if (status.running) return "play";
  if (!prefs.transport || ui.choosingTransport) return "connect";
  if (!status.paired && !pairingSkipped(prefs)) return "pair";
  return "ready";
}

// Back always leads to the connect screen: from a connect screen opened via
// "Change", or from the pair screen before an attempt has begun. Nothing else
// has a back step, and a running controller is left alone.
export function canGoBack(screen, stage, ui = {}) {
  if (ui.running || !ui.hasTransport) return false;
  if (screen === "connect") return Boolean(ui.choosingTransport);
  return screen === "pair" && stage === "start";
}

// Where the desktop side is inside one pairing attempt.
export function pairStage(status = {}, ui = {}) {
  if (!status.pairing) return "start";
  if (ui.approvalSent) return "finishing";
  if (status.can_approve) return "approve";
  if (Array.isArray(status.pattern) && status.pattern.length === 8) return "pattern";
  return "waiting";
}

export const PAIR_COPY = {
  start: { lead: "Open Doritrack on your phone, then pair.", hint: "" },
  waiting: { lead: "Waiting for your phone…", hint: "Tap Pair on the phone too." },
  pattern: { lead: "Tap these lanes on your phone, in order.", hint: "" },
  approve: {
    lead: "Does your phone say Pattern matched?",
    hint: "Approve only if it does.",
  },
  finishing: { lead: "Finishing…", hint: "" },
};

export function transportName(transport) {
  return transport === "wifi" ? "Wi-Fi" : "USB cable";
}
