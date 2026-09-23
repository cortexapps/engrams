// Fixed-size canvases (the hero graph, the architecture diagram) scale down
// to the column they sit in. A wrapper carries data-fit="<design width>"; the
// canvas never reaches past its column, so it keeps the page's right margin.
function fit(el: HTMLElement) {
  const width = Number(el.dataset.fit);
  const parent = el.parentElement;
  if (!width || !parent) return;
  el.style.transform = "";
  el.style.height = "";
  const s = Math.min(1, parent.clientWidth / width);
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
