// Fixed-size canvases (the hero graph, the architecture diagram) scale down
// to the column they sit in. A wrapper carries data-fit="<design width>" and
// an optional data-fit-spill="<px>" for how far past its column it may reach
// before it starts shrinking, the way the hero graph spills into the gutter
// at 1440.
function fit(el: HTMLElement) {
  const width = Number(el.dataset.fit);
  const spill = Number(el.dataset.fitSpill ?? 0);
  const parent = el.parentElement;
  if (!width || !parent) return;
  el.style.transform = "";
  el.style.height = "";
  const s = Math.min(1, (parent.clientWidth + spill) / width);
  el.style.transformOrigin = "top left";
  el.style.transform = `scale(${s})`;
  el.style.height = `${el.scrollHeight * s}px`;
}

export function fitAll() {
  document.querySelectorAll<HTMLElement>("[data-fit]").forEach(fit);
}

fitAll();
window.addEventListener("resize", fitAll);
document.fonts?.ready.then(fitAll);
