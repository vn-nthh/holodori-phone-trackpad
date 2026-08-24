const PHASES = {
  ready: { label: "Ready", tone: "neutral" },
  waiting: { label: "Waiting for phone...", tone: "neutral" },
  connected: { label: "Phone connected", tone: "success" },
  recovering: { label: "Connection lost — recovering...", tone: "warning" },
  stopping: { label: "Stopping safely...", tone: "neutral" },
  "recovery-needs-admin": {
    label: "Administrator access is required to recover USB-tether routes.",
    tone: "error",
  },
  fatal: { label: "The controller stopped unexpectedly.", tone: "error" },
};

export function statusPresentation(result = {}) {
  const phase = Object.hasOwn(PHASES, result.phase) ? result.phase : "fatal";
  const presentation = PHASES[phase];
  return {
    phase,
    label: result.message || presentation.label,
    tone: presentation.tone,
  };
}
