import test from "node:test";
import assert from "node:assert/strict";

import {
  localOnlyTetherSelection,
  localOnlyTetherSupported,
  networkManagerCheckboxState,
  networkManagerPolicyUnresolved,
} from "./platform.js";

test("enables local-only control for Windows and NetworkManager", () => {
  assert.equal(localOnlyTetherSupported("launcher"), true);
  assert.equal(localOnlyTetherSupported("network-manager"), true);
  assert.equal(localOnlyTetherSupported("unsupported"), false);
  assert.equal(localOnlyTetherSupported(undefined), false);
  assert.equal(localOnlyTetherSupported("unexpected"), false);
});

test("forwards selections only for implemented platform models", () => {
  assert.equal(localOnlyTetherSelection("unsupported", true), false);
  assert.equal(localOnlyTetherSelection(undefined, true), false);
  assert.equal(localOnlyTetherSelection("unexpected", true), false);
  assert.equal(localOnlyTetherSelection("launcher", true), true);
  assert.equal(localOnlyTetherSelection("launcher", false), false);
  assert.equal(localOnlyTetherSelection("network-manager", true), true);
  assert.equal(localOnlyTetherSelection("network-manager", false), false);
});

test("keeps a pending NetworkManager policy checked so one click turns it off", () => {
  assert.deepEqual(
    networkManagerCheckboxState({ enabled: false, configured: true, mixed: true }),
    { checked: true, indeterminate: true },
  );
  assert.deepEqual(
    networkManagerCheckboxState({ enabled: false, configured: false, mixed: true }),
    { checked: true, indeterminate: true },
  );
});

test("keeps a requested policy latched while the tether is unavailable", () => {
  assert.deepEqual(
    networkManagerCheckboxState(
      { available: false, enabled: false, configured: false, mixed: false },
      true,
    ),
    { checked: true, indeterminate: true },
  );
  assert.equal(
    networkManagerPolicyUnresolved(
      { available: false, enabled: false, configured: false, mixed: false },
      true,
    ),
    true,
  );
  assert.equal(networkManagerPolicyUnresolved({ enabled: true, mixed: false }, true), false);
});
