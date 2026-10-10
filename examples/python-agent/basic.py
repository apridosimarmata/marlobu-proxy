"""Basic Marlobu session example."""

from marlobu import Marlobu

client = Marlobu(
    api_url="http://localhost:8080",
    proxy_host="localhost",
    proxy_port=5433,
)

# Create a session
session = client.create_session(project_id="example")
print(f"Created session: {session.id}")

# Connect to database through proxy
session.connect(database="mydb", user="postgres", password="postgres")

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
