# Python Agent Examples

Minimal examples showing how to integrate AI agents with Marlobu.

## Setup

```bash
# Install dependencies
pip install marlobu openai

# Start the Marlobu proxy (from repo root)
DATABASE_URL=postgres://user:pass@localhost/mydb \
./target/release/marlobu-proxy
```

## Examples

### Basic Session

```bash
python basic.py
```

Creates a session, executes some queries, and shows the staged changes.

### OpenAI Agent

```bash
OPENAI_API_KEY=sk-... python openai_agent.py
```

An agent that can query and modify the database. All writes are staged for approval.

### LangChain Agent

```bash
OPENAI_API_KEY=sk-... python langchain_agent.py
```

Same pattern using LangChain's tool abstraction.

## How it works

1. Agent connects through Marlobu proxy
2. Reads see production data
3. Writes go to isolated shadow tables
4. Session proposes changes for review
5. Human approves → atomic merge to production
