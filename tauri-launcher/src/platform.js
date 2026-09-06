export function localOnlyTetherSupported(elevationModel) {
  return elevationModel === "launcher" || elevationModel === "network-manager";
}

export function localOnlyTetherSelection(elevationModel, checked, transport = "usb") {
  return transport === "usb" && localOnlyTetherSupported(elevationModel) && Boolean(checked);
}

export function networkManagerCheckboxState(policy, requested = false) {
  const unresolvedRequest = Boolean(requested && !policy?.enabled);
  return {
    // Keep every ambiguous state internally checked so one click always
    // normalizes it to off. A user who wants on can then make that explicit.
    checked: Boolean(requested || policy?.enabled || policy?.configured || policy?.mixed),
    indeterminate: Boolean(policy?.mixed || unresolvedRequest),
  };
}

export function networkManagerPolicyUnresolved(policy, requested = false) {
  return Boolean(policy?.mixed || (requested && policy?.enabled !== true));
}
