// An Expressive Code plugin that puts the block's language in the frame
// header, so every code block carries a "sh" / "toml" / "yaml" readout next to
// the copy button, the way the design draws them. It runs after the frames
// plugin, which is what builds the <figure class="frame"> and its header.
export default function langHeader() {
  return {
    name: "engrams-lang-header",
    hooks: {
      postprocessRenderedBlock: ({ codeBlock, renderData }) => {
        const figure = renderData.blockAst;
        if (figure?.type !== "element" || figure.tagName !== "figure") return;
        const header = figure.children.find(
          (c) => c.type === "element" && c.tagName === "figcaption",
        );
        if (!header) return;
        const lang = codeBlock.language || "text";
        header.children.push({
          type: "element",
          tagName: "span",
          properties: { className: ["lang"] },
          children: [{ type: "text", value: lang }],
        });
        figure.properties.className = [...(figure.properties.className ?? []), "has-lang"];
      },
    },
  };
}
