export function localOnlyTetherSupported(elevationModel) {
  return elevationModel === "launcher";
}

export function localOnlyTetherSelection(elevationModel, checked) {
  return localOnlyTetherSupported(elevationModel) && Boolean(checked);
}
