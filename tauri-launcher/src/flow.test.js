import assert from "node:assert/strict";
import test from "node:test";

import {
  PAIR_COPY,
  canGoBack,
  deriveScreen,
  pairStage,
  pairingSkipped,
  transportName,
} from "./flow.js";

test("asks for a transport before anything else", () => {
  assert.equal(deriveScreen({ paired: false }, {}), "connect");
  assert.equal(deriveScreen({ paired: true }, { transport: null }), "connect");
  assert.equal(deriveScreen({ paired: true }, { transport: "usb" }, { choosingTransport: true }), "connect");
});

test("pairs before it offers Start, and never shows both", () => {
  assert.equal(deriveScreen({ paired: false }, { transport: "usb" }), "pair");
  assert.equal(deriveScreen({ paired: false }, { transport: "wifi" }), "pair");
  assert.equal(deriveScreen({ paired: true }, { transport: "usb" }), "ready");
  assert.equal(deriveScreen({ paired: true }, { transport: "wifi" }), "ready");
});

test("protocol v4 skips pairing over USB only", () => {
  assert.equal(pairingSkipped({ transport: "usb", legacyV4: true }), true);
  assert.equal(pairingSkipped({ transport: "wifi", legacyV4: true }), false);
  assert.equal(deriveScreen({ paired: false }, { transport: "usb", legacyV4: true }), "ready");
  assert.equal(deriveScreen({ paired: false }, { transport: "wifi", legacyV4: true }), "pair");
});

test("a running controller always shows the play screen", () => {
  assert.equal(deriveScreen({ running: true, paired: true }, { transport: "usb" }), "play");
  assert.equal(deriveScreen({ running: true, stopping: true, paired: true }, { transport: "usb" }), "play");
  assert.equal(
    deriveScreen({ running: true, paired: false }, { transport: "usb" }, { choosingTransport: true }),
    "play",
  );
});

test("an open pairing window stays on the pair screen", () => {
  assert.equal(deriveScreen({ running: true, pairing: true }, { transport: "usb" }), "pair");
  assert.equal(
    deriveScreen({ running: true, pairing: true }, { transport: "usb" }, { choosingTransport: true }),
    "pair",
  );
});

test("walks the desktop side of one pairing attempt in order", () => {
  assert.equal(pairStage({ pairing: false }), "start");
  assert.equal(pairStage({ pairing: true }), "waiting");
  assert.equal(pairStage({ pairing: true, pattern: [1, 2, 3] }), "waiting");
  assert.equal(pairStage({ pairing: true, pattern: [1, 2, 3, 4, 5, 6, 1, 2] }), "pattern");
  assert.equal(
    pairStage({ pairing: true, pattern: [1, 2, 3, 4, 5, 6, 1, 2], can_approve: true }),
    "approve",
  );
  assert.equal(
    pairStage({ pairing: true, pattern: [1, 2, 3, 4, 5, 6, 1, 2] }, { approvalSent: true }),
    "finishing",
  );
  for (const stage of ["start", "waiting", "pattern", "approve", "finishing"]) {
    assert.equal(typeof PAIR_COPY[stage].lead, "string");
  }
});

test("back only ever leads to the transport choice", () => {
  const ui = { running: false, hasTransport: true, choosingTransport: false };
  // A wrong transport pick can be undone before pairing starts.
  assert.equal(canGoBack("pair", "start", ui), true);
  // Not once an attempt is under way.
  assert.equal(canGoBack("pair", "waiting", ui), false);
  assert.equal(canGoBack("pair", "approve", ui), false);
  // "Change" opens connect with a way back; first launch has none.
  assert.equal(canGoBack("connect", "start", { ...ui, choosingTransport: true }), true);
  assert.equal(canGoBack("connect", "start", ui), false);
  assert.equal(canGoBack("connect", "start", { ...ui, hasTransport: false, choosingTransport: true }), false);
  // Ready and play have no back step.
  assert.equal(canGoBack("ready", "start", ui), false);
  assert.equal(canGoBack("play", "start", { ...ui, running: true }), false);
});

test("names transports for people, not protocols", () => {
  assert.equal(transportName("usb"), "USB cable");
  assert.equal(transportName("wifi"), "Wi-Fi");
});
