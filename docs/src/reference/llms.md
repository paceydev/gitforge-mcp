# For LLMs

This site in plain markdown, for agents and tools that read documentation.

Following the [llms.txt](https://llmstxt.org) convention, the site serves:

| File | What it holds |
|---|---|
| [`llms.txt`](../llms.txt) | An index: a one-line summary of the project, then every page with a link to its markdown and one sentence on what it covers. |
| [`llms-full.txt`](../llms-full.txt) | Every page in one file, in table-of-contents order. Load it into an agent's context to answer questions about gitforge-mcp. |
| `<page>.md` | Each page's markdown beside its HTML, such as `guide/safety.md` next to `guide/safety.html`. |

All three come from the same source as the HTML, at the same build, so they
never disagree with it.

An agent working in a repository doesn't need these to use the server: the
server describes its own tools, and [`forge_context`](../tools/forge-context.md)
says what is available and why. They're for questions about the server
itself, such as how to configure it or what a version supports.
