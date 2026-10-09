# Marlobu Proxy

Postgres wire protocol proxy for session-based database isolation.

## Architecture

```
Agent → Tool → Marlobu Proxy → Postgres
                    ↓
             Session Schema
             (shadow tables)
```

## Quick Start

```bash
# Start Postgres
make postgres

# Run migrations
DATABASE_URL=postgresql://postgres:postgres@localhost:5432/marlobu make migrate

# Start the proxy
RUST_LOG=debug cargo run
```

## Configuration

Environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `PROXY_ADDR` | `0.0.0.0:5433` | Proxy listen address |
| `API_ADDR` | `0.0.0.0:8080` | HTTP API address |
| `DATABASE_URL` | - | Backend Postgres connection string |
| `SESSION_TTL_SECONDS` | `3600` | Session TTL (1 hour) |
| `WEBHOOK_URL` | - | Webhook endpoint for events |
| `WEBHOOK_SECRET` | - | HMAC secret for webhook signatures |

## Connecting

Connect to the proxy like you would to Postgres:

```python
import psycopg2

# Include session ID in connection params
conn = psycopg2.connect(
    host="localhost",
    port=5433,
    database="mydb",
    user="myuser",
    password="mypass",
    options="-c marlobu_session=abc123"
)
```

Or create a session via the API first:

```bash
curl -X POST http://localhost:8080/sessions \
  -H "Content-Type: application/json" \
  -d '{"project_id": "my-project"}'
```

## API Endpoints

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/sessions` | Create new session |
| GET | `/sessions/:id` | Get session details |
| DELETE | `/sessions/:id` | Delete session |
| POST | `/sessions/:id/propose` | Submit for review |
| POST | `/sessions/:id/approve` | Approve and apply |
| POST | `/sessions/:id/reject` | Reject and discard |
| GET | `/sessions/:id/mutations` | Get staged mutations |
| GET | `/health` | Health check |

## How It Works

1. **Session Start**: Creates a Postgres schema (`session_{id}`)
2. **Reads**: Routed to views that merge shadow + prod data
3. **Writes**: Routed to shadow tables (copy-on-write)
4. **Propose**: Marks session for review
5. **Approve**: Applies changes atomically with conflict detection
6. **Reject**: Drops session schema, no changes to prod

## Development

```bash
# Run tests
make test

# Format code
make fmt

# Lint
make lint

# Full CI check
make ci
```

## License

MIT
