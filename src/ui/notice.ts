export function notice(message: string, kind = "status") {
  const node = document.querySelector<HTMLElement>("#toast");
  if (node) {
    node.textContent = message;
    node.dataset.kind = kind;
  }
}
