import assert from "node:assert/strict";
import test from "node:test";

import { LINK_STATES, linkArtMarkup } from "./art.js";

test("one illustration carries both transports so they can cross-fade", () => {
  const markup = linkArtMarkup("usb", "idle");
  assert.match(markup, /class="cable"/);
  assert.match(markup, /class="waves"/);
  assert.match(markup, /data-transport="usb"/);
  assert.match(markup, /data-state="idle"/);
  assert.equal((markup.match(/class="wave wave-phone"/g) ?? []).length, 3);
  assert.equal((markup.match(/class="wave wave-pc"/g) ?? []).length, 3);
});

test("uses only stroke geometry: no gradients, no text, no emoji", () => {
  const markup = linkArtMarkup("wifi", "connected");
  assert.doesNotMatch(markup, /gradient/i);
  assert.doesNotMatch(markup, /<text/);
  assert.doesNotMatch(markup, /[\u{1F300}-\u{1FAFF}\u{2600}-\u{27BF}]/u);
  assert.match(markup, /aria-hidden="true"/);
});

test("exposes every animation state the screens use", () => {
  assert.deepEqual(LINK_STATES, ["idle", "searching", "connected", "off"]);
});
