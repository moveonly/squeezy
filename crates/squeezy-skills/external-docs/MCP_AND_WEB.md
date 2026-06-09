# MCP And Web Lookup

Squeezy can use configured MCP servers and permission-gated web tools, but these
surfaces are separate from built-in Squeezy help.

## Built-In Help Does Not Fetch

`/help <topic>` answers from embedded external docs and redacted local
configuration. It never fetches the network automatically.

Curated topics (those listed by `/help` with no argument) are answered entirely
locally — zero provider cost. The answer is labeled **local curated answer** in
the response. If the topic is not in the curated list but subagents are enabled,
Squeezy may invoke a small DocHelp subagent that reads from the same bundled
docs but uses a provider call; that path is labeled **doc-help model answer**.
Set `[subagents] help_strict_local = true` in settings to disable the DocHelp
path and always refuse unsupported topics with a pointer to the public website.

If a Squeezy topic is not covered locally, the answer points to the public
website and GitHub repo so the user can choose whether to perform external
lookup.

## MCP Servers

Manage MCP servers with:

```sh
squeezy mcp list
squeezy mcp add <name> --user --transport stdio --command <command>
squeezy mcp add <name> --project --transport http --url <url>
squeezy mcp enable <name> --user
squeezy mcp disable <name> --project
squeezy mcp remove <name> --user
```

Squeezy discovers MCP tools once per agent turn across enabled servers. MCP
tool names are namespaced by server, and each server can define a default
permission policy of `allow`, `ask`, or `deny`.

## Web Tools

`websearch` discovers current or external information. `webfetch` retrieves a
specific HTTP(S) URL. Both tools are permission-gated, include the target host
or query in approval summaries, and return bounded redacted text with source and
cache metadata.

Use web tools when the user asks for current public information, when local docs
say to check the public website or repository, or when an external source was
explicitly provided by the user. Do not use web tools to bypass Squeezy's local
permission policy or shell sandbox.
