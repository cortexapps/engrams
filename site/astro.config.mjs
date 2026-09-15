import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";
import starlightLinksValidator from "starlight-links-validator";
import langHeader from "./scripts/ec-lang-header.mjs";

// The deploy target is parameterized by two build-time env vars so the same
// code serves every host the site will ever live on:
//
//   SITE_URL   absolute origin for canonical / sitemap / Open Graph URLs
//   BASE_PATH  the URL path prefix the site is served under
//
// Access-controlled GitHub Pages serve at the ROOT of a dedicated
// <random>.pages.github.io host, so BASE_PATH is "/" there. A public project
// page would be <owner>.github.io/<repo>/ → BASE_PATH=/<repo>. A custom domain
// is SITE_URL plus a public/CNAME file. When BASE_PATH is a subpath the build
// nests into dist/<base> so a static host serves it at that exact path.
const site = process.env.SITE_URL ?? "http://localhost:4321";
const base = process.env.BASE_PATH ?? "/";
const outDir = base === "/" ? "./dist" : `./dist${base.replace(/\/$/, "")}`;

export default defineConfig({
  site,
  base,
  outDir,
  trailingSlash: "always",
  output: "static",
  integrations: [
    starlight({
      title: "engrams",
      description: "Self-hosted sandboxes for AI coding agents, on Firecracker microVMs.",
      logo: { src: "./src/assets/engram-trace.svg", alt: "" },
      favicon: "/favicon.ico",
      head: [
        {
          tag: "link",
          attrs: {
            rel: "apple-touch-icon",
            href: `${base.replace(/\/$/, "")}/apple-touch-icon.png`,
          },
        },
      ],
      social: [{ icon: "github", label: "GitHub", href: "https://github.com/cortexapps/engrams" }],
      customCss: [
        "@fontsource-variable/jetbrains-mono",
        "@fontsource-variable/saira/wdth.css",
        "./src/styles/tokens.css",
        "./src/styles/starlight.css",
      ],
      editLink: {
        baseUrl: "https://github.com/cortexapps/engrams/edit/main/site/",
      },
      components: {
        ThemeProvider: "./src/components/starlight/ThemeProvider.astro",
        ThemeSelect: "./src/components/starlight/ThemeSelect.astro",
        Header: "./src/components/starlight/Header.astro",
        Sidebar: "./src/components/starlight/Sidebar.astro",
        PageTitle: "./src/components/starlight/PageTitle.astro",
      },
      // Code blocks use the dark cover palette,
      // the panel colors from tokens.css, a chamfer from starlight.css.
      expressiveCode: {
        themes: ["github-dark"],
        useStarlightDarkModeSwitch: false,
        useStarlightUiThemeColors: false,
        styleOverrides: {
          borderRadius: "0",
          borderWidth: "1px",
          borderColor: "color-mix(in oklch, var(--primary) 45%, transparent)",
          codeBackground: "var(--panel-code)",
          codeFontFamily: "var(--font-mono)",
          codeFontSize: "13px",
          codeLineHeight: "1.7",
          codePaddingBlock: "16px",
          codePaddingInline: "18px",
          uiFontFamily: "var(--font-mono)",
          frames: {
            frameBoxShadowCssValue: "none",
            editorBackground: "var(--panel-code)",
            terminalBackground: "var(--panel-code)",
            terminalTitlebarBackground: "transparent",
            terminalTitlebarBorderBottomColor: "rgba(255,255,255,.12)",
            terminalTitlebarForeground: "var(--sage-on-cover)",
            editorTabBarBackground: "transparent",
            editorTabBarBorderBottomColor: "rgba(255,255,255,.12)",
            editorTabBarBorderColor: "transparent",
            editorActiveTabBackground: "transparent",
            editorActiveTabForeground: "var(--sage-on-cover)",
            editorActiveTabIndicatorTopColor: "transparent",
            editorActiveTabIndicatorBottomColor: "var(--primary)",
            editorActiveTabBorderColor: "transparent",
            inlineButtonBackground: "transparent",
            inlineButtonBorder: "transparent",
            inlineButtonForeground: "var(--primary)",
          },
        },
        plugins: [langHeader()],
      },
      sidebar: [
        {
          label: "Getting started",
          items: [{ autogenerate: { directory: "docs/getting-started" } }],
        },
        { label: "Platform", items: [{ autogenerate: { directory: "docs/platform" } }] },
        { label: "Concepts", items: [{ autogenerate: { directory: "docs/concepts" } }] },
        { label: "Guides", items: [{ autogenerate: { directory: "docs/guides" } }] },
        { label: "Reference", items: [{ autogenerate: { directory: "docs/reference" } }] },
        {
          label: "API reference",
          collapsed: true,
          items: [{ autogenerate: { directory: "docs/api" } }],
        },
        { label: "Contributing", items: [{ autogenerate: { directory: "docs/contributing" } }] },
      ],
      plugins: [starlightLinksValidator({ errorOnRelativeLinks: false, errorOnLocalLinks: false })],
    }),
  ],
});
