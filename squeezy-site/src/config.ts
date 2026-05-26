export const SITE = {
  name: "Squeezy",
  url: "https://squeezyagent.com",
  description:
    "A terminal coding agent built on a local semantic graph. Answer navigation, reference, and impact questions before spending model tokens.",
  repoUrl: "https://github.com/esqueezy/squeezy",
  issuesUrl: "https://github.com/esqueezy/squeezy/issues",
  discussionsUrl: "https://github.com/esqueezy/squeezy/discussions",
  telemetryEndpoint: "https://squeezy-telemetry.esqueezy.workers.dev/v1/site",
  securityContactLabel: "Private disclosures: a dedicated channel is being set up."
};

export const DOCS_NAV = [
  {
    href: "/docs/install/",
    label: "Install",
    status: "setup"
  },
  {
    href: "/docs/semantic-navigation/",
    label: "Graph",
    status: "static analysis"
  },
  {
    href: "/docs/cost-receipts/",
    label: "Cost & receipts",
    status: "token budget"
  },
  {
    href: "/docs/config/",
    label: "Config",
    status: "settings"
  },
  {
    href: "/docs/permissions/",
    label: "Permissions",
    status: "policy"
  },
  {
    href: "/docs/languages/",
    label: "Languages",
    status: "coverage"
  },
  {
    href: "/docs/providers/",
    label: "Providers",
    status: "models"
  },
  {
    href: "/docs/troubleshooting/",
    label: "Support",
    status: "debug/report"
  }
];
