# User Story 006: Multi-Executor Parallel Experiments

**As the root agent, I need to run multiple experiments simultaneously across different executor sessions.**

## Context

Our entire 8-hour session was bottlenecked by sequential execution. The research had many naturally parallel tasks:
- Oracle deep analysis (Phase 1) could have run alongside noise characterization
- FFORMA, GBMCI, and Super Learner are completely independent
- Sleeping Experts and META-DES don't depend on each other
- The permutation test (2+ hours) could have run alongside other experiments

With parallel execution, the session could have been 3-4 hours instead of 8+.

## Stories

### 006a: Launch parallel experiments

**Given** I have 3 independent experiment ideas
**When** I send each to a different executor
**Then** all three should run simultaneously
**And** I should be able to monitor each independently

**Workflow I wish I had:**
```
# Launch three executors
session_A = launch("claude", working_dir="/path")
session_B = launch("claude", working_dir="/path")
session_C = launch("claude", working_dir="/path")

# Send different experiments to each
send(session_A, "Run oracle deep analysis...")
send(session_B, "Run noise characterization...")
send(session_C, "Search literature for expert combination methods...")

# Monitor all three
while any_running([session_A, session_B, session_C]):
    for s in [session_A, session_B, session_C]:
        if has_new_output(s):
            screen = get_screen(s)
            # Process results
```

**Test cases:**
- Launch 3 sessions, send different tasks, verify independent execution
- Monitor 3 sessions without missing results from any
- Handle one session completing while others still run
- Handle one session erroring while others continue

### 006b: Resource management with parallel sessions

**Given** 3 executor Claudes are running simultaneously
**When** each launches compute-intensive experiments
**Then** the system should remain responsive
**And** experiments should not interfere with each other

**Concern from real usage:** Our single executor ran scripts at 100-155% CPU. Three simultaneous experiments would be 300-450% CPU. This could:
- Slow each experiment significantly
- Cause memory pressure
- Make screen updates laggy

**Test cases:**
- Run 3 CPU-intensive processes simultaneously
- Verify screen reading remains responsive under load
- Handle memory-constrained scenario
- Test with experiments that produce large output files

### 006c: Coordinated results collection

**Given** multiple parallel experiments complete
**When** I need to synthesize results
**Then** I should be able to collect and compare results across executors

**What I would have wanted:** After parallel experiments, I need to:
1. Read each executor's final results
2. Compare scores across approaches
3. Decide which to pursue further
4. Potentially feed results from executor A into executor B's next task

**Test cases:**
- Collect results from 3 completed experiments
- Handle experiments completing in different order
- Pass results from one executor as input to another
- Handle partial completion (2 of 3 done, decide to proceed)

### 006d: Git coordination across parallel executors

**Given** multiple executors may want to commit results
**When** they try to commit simultaneously
**Then** there should be no git conflicts

**Pain point anticipated:** If three executors each write a script and try to git commit, they'll conflict. Options:
1. Only one executor commits (designated "committer")
2. Sequential commits with automatic conflict resolution
3. Each executor works in a git worktree (the `own_worktree` sandbox profile!)

**The `own_worktree` sandbox profile from the architecture doc is EXACTLY the right solution for this.** Each executor gets its own branch/worktree, commits freely, and the root agent merges when ready.

**Test cases:**
- Two executors write different files, both commit successfully
- Two executors modify the same file — conflict detected
- Worktree isolation prevents conflicts
- Root agent can merge worktrees after parallel experiments
