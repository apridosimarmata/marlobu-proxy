"""OpenAI agent with Marlobu database tools."""

import json
from openai import OpenAI
from marlobu import Marlobu

openai = OpenAI()
marlobu = Marlobu()

# Define database tools
tools = [
    {
        "type": "function",
        "function": {
            "name": "query",
            "description": "Execute a SELECT query on the database",
            "parameters": {
                "type": "object",
                "properties": {
                    "sql": {"type": "string", "description": "SQL SELECT query"}
                },
                "required": ["sql"]
            }
        }
    },
    {
        "type": "function",
        "function": {
            "name": "mutate",
            "description": "Execute UPDATE, INSERT, or DELETE. Changes are staged for review.",
            "parameters": {
                "type": "object",
                "properties": {
                    "sql": {"type": "string", "description": "SQL mutation query"}
                },
                "required": ["sql"]
            }
        }
    }
]


def run_agent(user_input: str):
    """Run the agent with a Marlobu session."""

    with marlobu.session(
        project_id="openai-agent",
        database="mydb",
        user="postgres",
        password="postgres",
    ) as session:

        def handle_tool(name: str, args: dict) -> str:
            if name == "query":
                result = session.execute(args["sql"])
                return json.dumps(result) if result else "No results"
            elif name == "mutate":
                session.execute(args["sql"])
                return "Mutation staged for review"
            return "Unknown tool"

        messages = [{"role": "user", "content": user_input}]

        # Agent loop
        while True:
            response = openai.chat.completions.create(
                model="gpt-4",
                messages=messages,
                tools=tools,
            )

            msg = response.choices[0].message
            messages.append(msg)

            if not msg.tool_calls:
                print(f"Agent: {msg.content}")
                break

            # Handle tool calls
            for tc in msg.tool_calls:
                result = handle_tool(tc.function.name, json.loads(tc.function.arguments))
                print(f"  [{tc.function.name}] {result[:100]}...")
                messages.append({
                    "role": "tool",
                    "tool_call_id": tc.id,
                    "content": result
                })

        # Show staged changes
        diff = session.diff()
        if diff:
            print(f"\nStaged changes: {json.dumps(diff, indent=2)}")

        # Session auto-proposes on exit
        print(f"Session {session.id} ready for review")
        return session.id


if __name__ == "__main__":
    import sys
    prompt = sys.argv[1] if len(sys.argv) > 1 else "Show me all users"
    run_agent(prompt)
