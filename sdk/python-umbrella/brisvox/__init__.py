"""Brisvox for Python: the agent SDK under the short name.

``pip install brisvox`` installs this package and ``brisvox-agent``, the SDK it re-exports; the two
are released together, so ``brisvox.X`` is ``brisvox_agent.X`` for every public name ``X``::

    from brisvox import Agent

    agent = Agent("luigi", system_prompt="You are Luigi's host.", greeting="Hi!")

    @agent.tool
    async def opening_hours(day: str) -> str:
        \"\"\"Opening hours of the restaurant on a weekday.\"\"\"
        return "12:00 to 22:00"

    agent.run(port=8765)

The server itself is a separate program (the ``brisvox-server`` binary or container image); see
https://bilal07karadeniz.github.io/brisvox/.
"""

import brisvox_agent as _sdk
from brisvox_agent import *  # noqa: F403  (the whole public API of the SDK)

# The version of this package, which is the version of the SDK it depends on exactly.
__version__ = "0.1.2"

__all__ = [*_sdk.__all__, "__version__"]
