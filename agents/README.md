# agents/

The helper agents' hand-back notes (`<lane>.md`) and outputs (CSV tables of names, hashes and counts; never
game bytes). See [`../secondary-agent.md`](../secondary-agent.md).

**[`secondary-agents-updates.md`](secondary-agents-updates.md)**: the helper lanes' live status, their pull
requests, items "for the lead" and a log. The coordinating session keeps it; the lead reads it before every step.

**`agents/MEASURING`**: while this file exists in the lead's tree, a timed or GPU run is in progress. Helper
agents run no builds, tests or other heavy work until it is gone. It is never committed.
