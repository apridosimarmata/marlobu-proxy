# Marlobu MCP Server

MCP (Model Context Protocol) server for Claude Code integration.

## Install

```bash
npm install -g @marlobu/mcp-server
```

## Configure Claude Code

Add to your MCP settings (`~/.claude/settings.json`):

```json
{
  "mcpServers": {
    "marlobu": {
      "command": "marlobu-mcp",
      "env": {
        "MARLOBU_API_URL": "http://localhost:8080",
        "MARLOBU_PROXY_HOST": "localhost",
        "MARLOBU_PROXY_PORT": "5433",
        "DATABASE_NAME": "mydb",
        "DATABASE_USER": "postgres",
        "DATABASE_PASSWORD": "pass"
      }
    }
  }
}
```

## Tools

| Tool | Description |
|------|-------------|
| `marlobu_query` | Execute SELECT queries |
| `marlobu_mutate` | Execute INSERT/UPDATE/DELETE (staged) |
| `marlobu_diff` | View pending changes |
| `marlobu_propose` | Submit for human review |

## Usage

Once configured, Claude Code can use the tools:

```
You: Find users who haven't logged in for 30 days and mark them inactive

Claude: [marlobu_query] SELECT id, email FROM users WHERE last_login < NOW() - INTERVAL '30 days'
        → Found 42 users

        [marlobu_mutate] UPDATE users SET status = 'inactive' WHERE last_login < NOW() - INTERVAL '30 days'
        → Staged 42 updates

        [marlobu_diff]
        → users: 42 rows modified

        Ready to propose when you want to submit for review.
```
