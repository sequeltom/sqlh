# Product vision

Owner direction recorded 2026-10-10. This document sets product priorities; [DESIGN.md](DESIGN.md) describes current behavior and technical proposals. When older plans conflict with this direction, update those plans before implementing them.

## Purpose and primary workflow

sqlharness is a fast, context-efficient SQL Server investigation tool for agents working through human-led DBA and developer questions. The human steers; the agent discovers, queries, interprets, and adjusts across several turns.

The primary user is an agent with limited context that makes several queries while answering a person's question. Each call should return enough useful information to choose the next step without filling the context window with repetitive metadata or excessive data. Focused schema discovery, concise diagnostics, and easy recovery from an unhelpful query support that workflow.

Large analytics processes and bulk data movement are secondary use cases. Their throughput and completeness requirements should not dictate the main interaction model. Human-led sessions do not imply an interactive CLI: the agent-facing tool should still have predictable, noninteractive execution, with the existing profile-setup exception.

## Speed and recovery

The goal is to make each turn faster. Evaluate the time to the next useful answer, the overhead of repeated calls, the amount of context consumed, and the time needed to cancel and start again.

Connection reuse is a product priority because the primary workflow makes repeated calls. MCP and persistent sessions are possible delivery mechanisms; their detailed implementation remains a design decision. Reuse must preserve execution policy, isolation between calls, and reliable cleanup.

Quick cancellation is essential. An agent must be able to bail out of a slow or misdirected query and try again promptly. Cancellation must remain dependable during connection setup, execution, output delivery, and cleanup.

## Safety

Database permissions should be configured appropriately and remain the enforcement boundary. The tool also needs its own checks because a DBA or developer may have granted an agent excessive privileges.

Read-only defaults and command checks should reduce accidental damage in that situation while making legitimate DBA investigation practical. These checks matter even when the connected login can write. They do not replace database permissions or establish a security boundary against a malicious principal with the same access.

The command categorization system and the use of allowlists or blocklists need further product and design work. The vision establishes the purpose of that system, not a finished taxonomy. Keep this work distinct from selecting parser rules or individual permitted commands.

## Limits and completeness

Concise, bounded responses are central to an agent with limited context. Fast cancellation takes priority in the turn-by-turn workflow.

The usefulness of exact total row counts is unresolved. Assess whether those counts help investigation enough to justify draining results after the shown-row limit. The current implementation drains and counts; this vision does not change that behavior or its output contract. Any future cutoff behavior must distinguish exact counts from partial counts and communicate incomplete results clearly.

## Compatibility

- **First release:** `sqlh.exe` runs on Windows x64 workstations, validated on Windows 11. It connects to SQL Server 2025 Standard, which is on a remote server in about 90% of use cases (typically Windows Server 2025 Standard). Running sqlh on the database server itself is not a first-release target. (Owner decision, 2026-10-10.)
- **Candidate expansion:** a Windows Server 2016 / SQL Server 2016 floor, provided supporting it does not introduce excessive technical debt.
- **Other targets:** SQL Server 2012/2014 and Azure targets appear in earlier plans, but are not first-release requirements. Assess additional support explicitly before making commitments.

Preserve the single-executable, zero-runtime-dependency distribution goal on supported platforms. Favor inexpensive portability where practical, but do not let speculative compatibility work delay the first release or complicate its core workflow.

## Delivery priorities

1. Establish correctness and trustworthy outcomes: protect input files, report failed output delivery, handle malformed input, and close the known read-only and compile-mode gaps.
2. Make cancellation and bounded output reliable for short, iterative investigations.
3. Provide focused schema discovery and reduce repeated connection/setup overhead, with tested cleanup and reuse behavior.
4. Develop command categorization using concrete DBA workflows and the risks of overprivileged connections.
5. Evaluate exact row counts and broader compatibility using evidence from real sessions.

This ordering expresses product priorities, not a commitment to particular flags, output shapes, pooling mechanisms, or release dates.
