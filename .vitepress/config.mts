import { defineConfig } from "vitepress";
import llmstxt from "vitepress-plugin-llms";

export default defineConfig({
  vite: {
    plugins: [llmstxt()],
  },
  title: "tamias",
  description:
    "package & content manager for the agent factory — fetch, store, cache, dispense",
  base: "/tamias/",
  srcExclude: ["README.md"],
  ignoreDeadLinks: [/\/api\//],
  themeConfig: {
    nav: [
      { text: "Getting Started", link: "/getting-started" },
      { text: "Explanation", link: "/explanation/architecture" },
      { text: "Reference", link: "/reference/cli" },
      {
        text: "LLMs",
        items: [
          { text: "llms.txt", link: "https://9aia.com/tamias/llms.txt", target: "_blank" },
          { text: "llms-full.txt", link: "https://9aia.com/tamias/llms-full.txt", target: "_blank" },
        ],
      },
    ],
    sidebar: [
      { text: "Getting Started", items: [{ text: "Install a package", link: "/getting-started" }] },
      {
        text: "Reference",
        items: [
          { text: "CLI", link: "/reference/cli" },
          { text: "Ref grammar", link: "/reference/refs" },
          { text: "Config", link: "/reference/config" },
        ],
      },
      {
        text: "Explanation",
        items: [
          { text: "Architecture", link: "/explanation/architecture" },
          { text: "Content types", link: "/explanation/content-types" },
          { text: "Boundaries", link: "/explanation/boundaries" },
        ],
      },
    ],
    socialLinks: [{ icon: "github", link: "https://github.com/9aia/tamias" }],
  },
});
