# create-webclaw

Set up [webclaw](https://webclaw.io) MCP server for AI agents in one command.

## Usage

```bash
npx create-webclaw
```

## What it does

1. Detects installed AI tools (Claude Desktop, Claude Code, Cursor, Windsurf, VS Code + Continue)
2. Downloads the `webclaw-mcp` binary for your platform
3. Asks for your API key (optional — works locally without one)
4. Configures MCP in each detected tool

## Supported tools

| Tool | Config location |
|------|----------------|
| Claude Desktop | `~/Library/Application Support/Claude/claude_desktop_config.json` |
| Claude Code | `~/.claude.json` |
| Cursor | `.cursor/mcp.json` |
| Windsurf | `~/.codeium/windsurf/mcp_config.json` |
| VS Code (Continue) | `~/.continue/config.json` |

## MCP tools provided

After setup, your AI agent has access to:

- **scrape** — extract content from any URL
- **crawl** — recursively crawl a website
- **search** — web search + parallel scrape
- **map** — discover URLs from sitemaps
- **batch** — extract multiple URLs in parallel
- **extract** — LLM-powered structured extraction
- **summarize** — content summarization
- **diff** — track content changes
- **brand** — extract brand identity
