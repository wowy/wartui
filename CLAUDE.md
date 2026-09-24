# CLAUDE.md

The guidance for this repository lives in [AGENTS.md](AGENTS.md). Read it.

## Delegating implementation

The main session plans; the [`rust-developer`](.claude/agents/rust-developer.md) agent writes the
code. It starts without this conversation's context, so hand it a self-contained plan: the files
to change, the behaviour intended, and the tests to add. Review its diff before committing.
