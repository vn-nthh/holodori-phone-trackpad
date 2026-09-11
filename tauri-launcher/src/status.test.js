import assert from "node:assert/strict";
import test from "node:test";

import { statusPresentation } from "./status.js";

test("maps every stable host phase to launcher copy, tone, and link state", () => {
  assert.deepEqual(statusPresentation({ phase: "ready" }), {
    phase: "ready",
    label: "Ready",
    tone: "neutral",
    link: "idle",
    detail: "",
  });
  assert.equal(statusPresentation({ phase: "connected" }).tone, "success");
  assert.equal(statusPresentation({ phase: "connected" }).link, "connected");
  assert.equal(statusPresentation({ phase: "waiting" }).link, "searching");
  assert.equal(statusPresentation({ phase: "pairing" }).label, "Waiting for your phone…");
  assert.equal(statusPresentation({ phase: "recovering" }).tone, "warning");
  assert.equal(statusPresentation({ phase: "recovery-needs-admin" }).tone, "error");
  assert.equal(statusPresentation({ phase: "fatal" }).tone, "error");
  assert.equal(statusPresentation({ phase: "fatal" }).link, "off");
});

test("hides the native host's boilerplate phrases behind launcher copy", () => {
  const waiting = statusPresentation({ phase: "waiting", message: "Waiting for phone..." });
  assert.equal(waiting.label, "Waiting for your phone…");
  assert.equal(waiting.detail, "");
  const connected = statusPresentation({ phase: "connected", message: "Phone connected" });
  assert.equal(connected.label, "Connected");
  assert.equal(connected.detail, "");
});

test("surfaces a specific backend message as detail without weakening severity", () => {
  const fatal = statusPresentation({ phase: "fatal", message: "Native controller failed." });
  assert.equal(fatal.tone, "error");
  assert.equal(fatal.label, "Native controller failed.");
  assert.equal(fatal.detail, "Native controller failed.");

  const ready = statusPresentation({
    phase: "ready",
    message: "Ready. USB route cleanup is pending.",
  });
  assert.equal(ready.label, "Ready");
  assert.equal(ready.detail, "Ready. USB route cleanup is pending.");
});

test("treats unknown or missing phases as fatal", () => {
  assert.equal(statusPresentation({ phase: "surprise" }).phase, "fatal");
  assert.equal(statusPresentation({}).tone, "error");
  assert.equal(statusPresentation({}).label, "Something went wrong.");
});
