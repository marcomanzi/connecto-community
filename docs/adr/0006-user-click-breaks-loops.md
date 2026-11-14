# ADR 0006: every relay hop needs a user click

**Context.** Acceptance requires no automatic output-triggered loops. Session output is untrusted.

**Decision.** Only a UI click creates a delivery. Captured text fills a panel and stops there. Policy denies `Deliver` from any non-user actor, and the hook crate cannot depend on the gate.

**Alternatives.** Auto-forward with limits (ARCHITECTURE 9.4) is needed eventually, but it adds loop risk before anything is proven.

**Consequences.** The loop is broken by construction rather than by tuning limits. Hop, rate and duplicate limits remain as defence in depth. The demo needs one click per hop. Agent-initiated messaging is a later slice.
