"""LangChain agent with Marlobu database tools."""

import json
import os
from langchain.tools import tool
from langchain_openai import ChatOpenAI
from langchain.agents import create_openai_tools_agent, AgentExecutor
from langchain_core.prompts import ChatPromptTemplate
from marlobu import Marlobu

marlobu = Marlobu(
    api_url=os.getenv("MARLOBU_API_URL", "http://localhost:8080"),
    proxy_host=os.getenv("MARLOBU_PROXY_HOST", "localhost"),
    proxy_port=int(os.getenv("MARLOBU_PROXY_PORT", "5433")),
)
session = None  # Set when running


@tool
def query(sql: str) -> str:
    """Execute a SELECT query on the database."""
    result = session.execute(sql)
    return json.dumps(result) if result else "No results"


@tool
def mutate(sql: str) -> str:
    """Execute UPDATE, INSERT, or DELETE. Changes are staged for review."""
    session.execute(sql)
    return "Mutation staged for review"


def run_agent(user_input: str):
    """Run the LangChain agent with a Marlobu session."""
    global session

    llm = ChatOpenAI(model="gpt-4")
    tools = [query, mutate]

    prompt = ChatPromptTemplate.from_messages([
        ("system", "You are a database assistant. Use the tools to help the user."),
        ("human", "{input}"),
        ("placeholder", "{agent_scratchpad}"),
    ])

    agent = create_openai_tools_agent(llm, tools, prompt)
    executor = AgentExecutor(agent=agent, tools=tools, verbose=True)

    with marlobu.session(
        project_id="langchain-agent",
        database=os.environ["DATABASE_NAME"],
        user=os.environ["DATABASE_USER"],
        password=os.environ["DATABASE_PASSWORD"],
    ) as sess:
        session = sess

        result = executor.invoke({"input": user_input})
        print(f"\nAgent: {result['output']}")

        # Show staged changes
        diff = sess.diff()
        if diff:
            print(f"\nStaged changes: {json.dumps(diff, indent=2)}")

        # Session auto-proposes on exit
        print(f"Session {sess.id} ready for review")
        return sess.id


if __name__ == "__main__":
    import sys
    prompt = sys.argv[1] if len(sys.argv) > 1 else "Show me all users"
    run_agent(prompt)
