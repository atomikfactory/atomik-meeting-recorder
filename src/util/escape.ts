export function escapeHtml(value: string) {
  const node = document.createElement("div");
  node.textContent = value;
  return node.innerHTML;
}

export function escapeAttr(value: string) {
  return escapeHtml(value).replaceAll('"', "&quot;");
}

export function setTextIfChanged(node: HTMLElement | null, value: string) {
  if (node && node.textContent !== value) node.textContent = value;
}
