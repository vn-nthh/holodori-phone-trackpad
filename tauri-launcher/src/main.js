import { invoke } from "@tauri-apps/api/core";
import "./styles.css";

const form = document.querySelector("#settings-form");
const keysInput = document.querySelector("#keys");
const metricsInput = document.querySelector("#metrics");
const status = document.querySelector("#status");
const startButton = document.querySelector("#start");
const stopButton = document.querySelector("#stop");

let stopping = false;

function setStatus(message, tone = "neutral") {
  status.textContent = message;
  status.dataset.tone = tone;
}

function setRunning(running) {
  keysInput.disabled = running;
  metricsInput.disabled = running;
  startButton.disabled = running;
  stopButton.disabled = !running;
}

function validateKeys(value) {
  const lanes = value.split(",").map((lane) => lane.trim());
  return lanes.length >= 1
    && lanes.length <= 16
    && lanes.every((lane) => /^[a-zA-Z0-9]$/.test(lane));
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
  const keys = keysInput.value.trim();

  if (!validateKeys(keys)) {
    setStatus("Use one letter or number per lane, separated by commas.", "error");
    keysInput.focus();
    return;
  }
  try {
    setStatus("Starting...");
    const result = await invoke("start_host", {
      keys,
      metrics: metricsInput.checked,
    });
    setRunning(result.running);
    setStatus(result.message);
  } catch (error) {
    setRunning(false);
    setStatus(String(error), "error");
  }
});

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
