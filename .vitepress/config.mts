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
        text: "Tutorials",
        items: [{ text: "Your first package", link: "/tutorials/first-package" }],
      },
      {
        text: "How-to",
        items: [
          { text: "Install from a git repo", link: "/how-to/install-from-git" },
          { text: "Manage project dependencies", link: "/how-to/manage-deps" },
          { text: "Publish to a registry", link: "/how-to/publish-to-a-registry" },
          { text: "Provision and service", link: "/how-to/provision-a-service" },
          { text: "Write a driver", link: "/how-to/write-a-driver" },
        ],
      },
      {
        text: "Reference",
        items: [
          { text: "CLI", link: "/reference/cli" },
          { text: "Ref grammar", link: "/reference/refs" },
          { text: "Config", link: "/reference/config" },
          { text: "Scripting tamias", link: "/reference/scripting" },
          { text: "`tamias serve`", link: "/reference/serve" },
          { text: "Crate API", link: "/reference/crate-api" },
        ],
      },
      {
        text: "Explanation",
        items: [
          { text: "Architecture", link: "/explanation/architecture" },
          { text: "Content types", link: "/explanation/content-types" },
          { text: "Security model", link: "/explanation/security" },
          { text: "Boundaries", link: "/explanation/boundaries" },
        ],
      },
    ],
    socialLinks: [{ icon: "github", link: "https://github.com/9aia/tamias" }],
  },
});
