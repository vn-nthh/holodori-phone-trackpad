import assert from "node:assert/strict";
import test from "node:test";

import { statusPresentation } from "./status.js";

test("maps every stable host phase to a label and tone", () => {
  assert.deepEqual(statusPresentation({ phase: "ready" }), {
    phase: "ready",
    label: "Ready",
    tone: "neutral",
  });
  assert.equal(statusPresentation({ phase: "connected" }).tone, "success");
  assert.equal(statusPresentation({ phase: "pairing" }).label, "Pairing window open...");
  assert.equal(statusPresentation({ phase: "recovering" }).tone, "warning");
  assert.equal(statusPresentation({ phase: "recovery-needs-admin" }).tone, "error");
  assert.equal(statusPresentation({ phase: "fatal" }).tone, "error");
});

test("uses backend detail without weakening phase severity", () => {
  assert.deepEqual(
    statusPresentation({
      phase: "fatal",
      message: "Native controller failed.",
    }),
    {
      phase: "fatal",
      label: "Native controller failed.",
      tone: "error",
    },
  );
});

test("treats unknown or missing phases as fatal", () => {
  assert.equal(statusPresentation({ phase: "surprise" }).phase, "fatal");
  assert.equal(statusPresentation({}).tone, "error");
});
