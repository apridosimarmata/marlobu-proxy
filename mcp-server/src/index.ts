#!/usr/bin/env node

import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import {
  CallToolRequestSchema,
  ListToolsRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";
import { Marlobu } from "marlobu";

const API_URL = process.env.MARLOBU_API_URL || "http://localhost:8080";
const PROXY_HOST = process.env.MARLOBU_PROXY_HOST || "localhost";
const PROXY_PORT = parseInt(process.env.MARLOBU_PROXY_PORT || "5433");
const DATABASE = process.env.DATABASE_NAME || "";
const USER = process.env.DATABASE_USER || "";
const PASSWORD = process.env.DATABASE_PASSWORD || "";

const client = new Marlobu({
  apiUrl: API_URL,
  proxyHost: PROXY_HOST,
  proxyPort: PROXY_PORT,
});

let session: any = null;

const server = new Server(
  { name: "marlobu", version: "0.1.0" },
  { capabilities: { tools: {} } }
);

server.setRequestHandler(ListToolsRequestSchema, async () => ({
  tools: [
    {
      name: "marlobu_query",
      description: "Execute a SELECT query on the database. Returns query results.",
      inputSchema: {
        type: "object",
        properties: {
          sql: { type: "string", description: "SQL SELECT query" },
        },
        required: ["sql"],
      },
    },
    {
      name: "marlobu_mutate",
      description: "Execute INSERT, UPDATE, or DELETE. Changes are staged for human review, not applied immediately.",
      inputSchema: {
        type: "object",
        properties: {
          sql: { type: "string", description: "SQL mutation (INSERT/UPDATE/DELETE)" },
        },
        required: ["sql"],
      },
    },
    {
      name: "marlobu_diff",
      description: "View all pending changes staged in the current session.",
      inputSchema: {
        type: "object",
        properties: {},
      },
    },
    {
      name: "marlobu_propose",
      description: "Submit all staged changes for human review. After proposing, no more changes can be made.",
      inputSchema: {
        type: "object",
        properties: {},
      },
    },
  ],
}));

server.setRequestHandler(CallToolRequestSchema, async (request) => {
  const { name, arguments: args } = request.params;

  try {
    // Ensure session exists
    if (!session) {
      session = await client.createSession({ projectId: "claude-code" });
      await session.connect({
        database: DATABASE,
        user: USER,
        password: PASSWORD,
      });
    }

    switch (name) {
      case "marlobu_query": {
        const sql = (args as { sql: string }).sql;
        if (!sql.trim().toUpperCase().startsWith("SELECT")) {
          return { content: [{ type: "text", text: "Error: Only SELECT queries allowed" }] };
        }
        const result = await session.execute(sql);
        return { content: [{ type: "text", text: JSON.stringify(result, null, 2) }] };
      }

      case "marlobu_mutate": {
        const sql = (args as { sql: string }).sql;
        const upper = sql.trim().toUpperCase();
        if (!upper.startsWith("INSERT") && !upper.startsWith("UPDATE") && !upper.startsWith("DELETE")) {
          return { content: [{ type: "text", text: "Error: Only INSERT/UPDATE/DELETE allowed" }] };
        }
        await session.execute(sql);
        return { content: [{ type: "text", text: "Mutation staged for review" }] };
      }

      case "marlobu_diff": {
        const diff = await session.diff();
        if (!diff || Object.keys(diff).length === 0) {
          return { content: [{ type: "text", text: "No pending changes" }] };
        }
        return { content: [{ type: "text", text: JSON.stringify(diff, null, 2) }] };
      }

      case "marlobu_propose": {
        await session.propose();
        return { content: [{ type: "text", text: `Session ${session.id} proposed for review` }] };
      }

      default:
        return { content: [{ type: "text", text: `Unknown tool: ${name}` }] };
    }
  } catch (error: any) {
    return { content: [{ type: "text", text: `Error: ${error.message}` }] };
  }
});

async function main() {
  const transport = new StdioServerTransport();
  await server.connect(transport);
  console.error("Marlobu MCP server running");
}

main().catch(console.error);
