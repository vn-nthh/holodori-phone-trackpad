import test from "node:test";
import assert from "node:assert/strict";

import { localOnlyTetherSelection, localOnlyTetherSupported } from "./platform.js";

test("enables route control only for the Windows launcher model", () => {
  assert.equal(localOnlyTetherSupported("launcher"), true);
  assert.equal(localOnlyTetherSupported("unsupported"), false);
  assert.equal(localOnlyTetherSupported(undefined), false);
  assert.equal(localOnlyTetherSupported("unexpected"), false);
});

test("never forwards a stale checked preference on Linux or unknown platforms", () => {
  assert.equal(localOnlyTetherSelection("unsupported", true), false);
  assert.equal(localOnlyTetherSelection(undefined, true), false);
  assert.equal(localOnlyTetherSelection("unexpected", true), false);
  assert.equal(localOnlyTetherSelection("launcher", true), true);
  assert.equal(localOnlyTetherSelection("launcher", false), false);
});
