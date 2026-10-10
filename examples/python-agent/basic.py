"""Basic Marlobu session example."""

import os
from marlobu import Marlobu

client = Marlobu(
    api_url=os.getenv("MARLOBU_API_URL", "http://localhost:8080"),
    proxy_host=os.getenv("MARLOBU_PROXY_HOST", "localhost"),
    proxy_port=int(os.getenv("MARLOBU_PROXY_PORT", "5433")),
)

# Create a session
session = client.create_session(project_id="example")
print(f"Created session: {session.id}")

# Connect to database through proxy
session.connect(
    database=os.environ["DATABASE_NAME"],
    user=os.environ["DATABASE_USER"],
    password=os.environ["DATABASE_PASSWORD"],
)

# Read data (sees production)
users = session.execute("SELECT * FROM users LIMIT 5")
print(f"Users: {users}")

# Write data (goes to shadow table)
session.execute("UPDATE users SET status = 'active' WHERE id = 1")
print("Staged update")

# View what changed
diff = session.diff()
print(f"Diff: {diff}")

# Submit for review
session.propose()
print(f"Session status: {session.status}")

# In a real workflow, a human would review and then:
# session.approve()  # Apply to production
# session.reject()   # Discard changes
