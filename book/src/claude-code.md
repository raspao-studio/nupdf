# Claude Code and MCP

There are two ways to give an agent nupdf: as a skill, where the agent runs the CLI from its shell, or as an MCP server, where each command is a tool. Both run the same commands and return the same JSON. Install `nupdf` first ([Install](install.md)).

## As a Claude Code skill

The skill is the guide (`nupdf guide`) with the name and description Claude Code reads to decide when to use it: whenever a task involves a PDF. The repository has it at [`skills/nupdf/SKILL.md`](https://github.com/raspao-studio/nupdf/blob/main/skills/nupdf/SKILL.md), and the binary prints the same file, so the skill always matches the version installed:

```bash
mkdir -p ~/.claude/skills/nupdf
nupdf guide --skill > ~/.claude/skills/nupdf/SKILL.md
```

That installs it for every project. For one project only, write it to `.claude/skills/nupdf/SKILL.md` in that project instead. Claude Code then runs `nupdf` through its shell when a task calls for it. Regenerate the file after upgrading nupdf.

## As an MCP server

`nupdf mcp` is a [Model Context Protocol](https://modelcontextprotocol.io) server over stdin and stdout. Add it to Claude Code with:

```bash
claude mcp add nupdf -- nupdf mcp
```

or, to share it with a project, check in a `.mcp.json` at the project's root (`claude mcp add --scope project` writes one):

```json
{
  "mcpServers": {
    "nupdf": {
      "command": "nupdf",
      "args": ["mcp"]
    }
  }
}
```

Other MCP clients take the same command and arguments.

What the server offers:

- **A tool for each command** but `mcp`, `batch` and `guide`, its arguments the command's options by name (`file`, `pages`, `format`, `max_chars`, ...), read from the CLI's own definition so they never fall out of step. The global options come with every tool: `password`, the limits, and a `timeout` in seconds.
- **The guide as its instructions.** A client gets it when it connects, so the model knows where to start without calling anything.
- **Many files in one call.** Give `file` a list of paths, directories or globs and the tool runs on each, one line per file with its report, then a summary.
- **The CLI's reports, byte for byte.** A result carries the report as text and, for JSON reports, as `structuredContent`; a failure is a tool error carrying nupdf's error report.
- **Bounded calls.** Each call stops after `--call-timeout` seconds (300 by default) or its own `timeout`, whichever is sooner, and a cancelled call stops where its work next checks; nothing it would have written is written. A report over `--max-output-bytes` (8 MB by default) fails with `limit.output` and points to the budgets (`pages`, `max_chars`, `max_items`). Set either on the command line: `claude mcp add nupdf -- nupdf mcp --call-timeout 60`.

Files are paths on the machine the server runs on. The server reads and hashes each file once while it stays the same, so repeated calls on one file are cheap.

## Which to use

The [agent eval](eval/REPORT.md) runs both on the same tasks: both passed every task, and the MCP server took fewer tokens to do it. The skill suits an agent that already works in a shell; the server suits a client without one, or a setup where tool calls are easier to permit and audit than shell commands.
