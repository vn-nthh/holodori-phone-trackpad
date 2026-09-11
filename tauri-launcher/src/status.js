// Screen copy for each host phase. The launcher owns the words users read;
// the native host's own default phrases are treated as boilerplate and only a
// backend message that says something specific (an error, a port conflict,
// a pending route repair) is surfaced as `detail`.

const PHASES = {
  ready: { label: "Ready", tone: "neutral", link: "idle" },
  waiting: { label: "Waiting for your phone…", tone: "neutral", link: "searching" },
  connected: { label: "Connected", tone: "success", link: "connected" },
  recovering: { label: "Reconnecting…", tone: "warning", link: "searching" },
  stopping: { label: "Stopping…", tone: "neutral", link: "idle" },
  pairing: { label: "Waiting for your phone…", tone: "neutral", link: "searching" },
  "recovery-needs-admin": {
    label: "Admin access is needed to repair the USB link.",
    tone: "error",
    link: "off",
  },
  fatal: { label: "Something went wrong.", tone: "error", link: "off" },
};

const BOILERPLATE = new Set([
  "",
  "Ready",
  "Waiting for phone...",
  "Phone connected",
  "Connection lost — recovering...",
  "Stopping safely...",
  "Pairing window open...",
  "Administrator access is required to recover USB-tether routes.",
  "The controller stopped unexpectedly.",
  "Replicate this pattern on the phone's six lanes.",
  "Phone reports Pattern matched. Confirm that on the real phone, then approve.",
  "Approval sent; finishing secure pairing...",
  "Pairing complete. Ready to start.",
  "Paired phone forgotten.",
]);

export function statusPresentation(result = {}) {
  const phase = Object.hasOwn(PHASES, result.phase) ? result.phase : "fatal";
  const presentation = PHASES[phase];
  const message = typeof result.message === "string" ? result.message.trim() : "";
  const detail = BOILERPLATE.has(message) ? "" : message;
  return {
    phase,
    label: phase === "fatal" && detail ? detail : presentation.label,
    tone: presentation.tone,
    link: presentation.link,
    detail,
  };
}
