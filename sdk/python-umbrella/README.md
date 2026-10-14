# brisvox (Python)

```sh
pip install brisvox
```

The Python SDK of [brisvox](https://github.com/bilal07karadeniz/brisvox), a Rust engine for
real-time voice agents, under the short package name. `brisvox` depends on
[`brisvox-agent`](https://pypi.org/project/brisvox-agent/) (the same release, pinned exactly) and
re-exports its whole public API: `brisvox.Agent` is `brisvox_agent.Agent`. Install either name; the
docs and the SDK reference use `brisvox_agent`.

```python
from brisvox import Agent, Session, UserText

agent = Agent("luigi", system_prompt="You are Luigi's host.", greeting="Hi, Luigi's here!")

@agent.tool                                   # JSON schema from the type hints
async def opening_hours(day: str) -> str:
    """Opening hours of the restaurant on a weekday."""
    return "12:00 to 22:00"

@agent.on_user_text
async def heard(session: Session, event: UserText) -> None:
    if "bye" in event.text.lower():
        await session.end("Goodbye!")         # speaks the farewell, then ends the call

agent.run(port=8765)                          # then: BRISVOX_AGENT_URL=ws://127.0.0.1:8765
```

The brisvox server connects out to this agent over a WebSocket, one connection per call. The server
is a separate program: the prebuilt `brisvox-server` binary, the container image, or the `brisvox`
crate for a server with your own Rust tools. Python 3.11 or newer.

* [Documentation](https://bilal07karadeniz.github.io/brisvox/)
* [Python SDK reference](https://github.com/bilal07karadeniz/brisvox/tree/main/sdk/python)

## License

Licensed under either of Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE)) or the MIT
license ([LICENSE-MIT](LICENSE-MIT)) at your option.
