export const SITE = {
  name: "Squeezy",
  url: "https://squeezyagent.com",
  description:
    "A coding agent that does the repetitive work on your machine and spends model tokens only where they're needed.",
  repoUrl: "https://github.com/esqueezy/squeezy",
  issuesUrl: "https://github.com/esqueezy/squeezy/issues",
  discussionsUrl: "https://github.com/esqueezy/squeezy/discussions",
  telemetryEndpoint: "https://squeezy-telemetry.esqueezy.workers.dev/v1/site",
  securityContactLabel: "Private disclosures: a dedicated channel is being set up."
};

export const DOCS_NAV = [
  {
    href: "/docs/install/",
    label: "Install & upgrade",
    status: "setup"
  },
  {
    href: "/docs/how-it-works/",
    label: "How it works",
    status: "concepts"
  },
  {
    href: "/docs/cost-saving/",
    label: "Cost-saving strategies",
    status: "cost",
    children: [
      { href: "/docs/cost-saving/understand/", label: "Understand the code first" },
      { href: "/docs/cost-saving/reuse/", label: "Don't pay for the same bytes twice" },
      { href: "/docs/cost-saving/right-size/", label: "Right-size every turn" },
      { href: "/docs/cost-saving/see-the-bill/", label: "See the bill" }
    ]
  },
  {
    href: "/docs/languages/",
    label: "Languages & graph",
    status: "coverage"
  },
  {
    href: "/docs/providers/",
    label: "Providers & models",
    status: "models"
  },
  {
    href: "/docs/config/",
    label: "Configuration",
    status: "settings"
  },
  {
    href: "/docs/permissions/",
    label: "Permissions & safety",
    status: "policy"
  },
  {
    href: "/docs/sessions/",
    label: "Sessions",
    status: "resume"
  },
  {
    href: "/docs/help/",
    label: "Help & troubleshooting",
    status: "support"
  }
];
