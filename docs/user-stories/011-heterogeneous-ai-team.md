# User Story 011: Heterogeneous AI Team

**As the root agent, I should be able to orchestrate a diverse team of AI agents — different models, different specializations — working together on complex tasks.**

## Context

Today's session proved that TWO Claude instances collaborating beat one working alone. But the real power comes from mixing DIFFERENT models with different strengths, just like our oracle analysis showed: diversity of viewpoint matters more than depth of any single viewpoint.

## The Vision

```
Root Agent (Claude Opus — strategic planner)
  ├── Executor A: Claude Code (Sonnet — fast implementation)
  ├── Executor B: Codex CLI (OpenAI — different training, different biases)
  ├── Executor C: Shell (system monitoring, file management)
  ├── Executor D: aider + Gemini (yet another perspective)
  └── Executor E: Claude Code (Haiku — rapid prototyping/exploration)
```

Each model brings different:
- **Training data** → different knowledge about techniques
- **Reasoning style** → different approaches to the same problem
- **Speed/cost tradeoff** → use Haiku for quick experiments, Opus for deep analysis
- **Blind spots** → what one model misses, another catches

## Stories

### 011a: Model diversity for research

**Given** a complex research question
**When** I dispatch it to multiple different models simultaneously
**Then** I get genuinely diverse approaches that I can synthesize

**How this would have helped today:**
- Send "propose 5 approaches to oracle distillation" to Claude, Codex, and Gemini
- Each returns different ideas based on different training data
- I (the root agent) synthesize the best ideas from all three
- This is literally the "expert aggregation" problem we studied!

**The meta-irony:** We spent hours trying to combine predictions from diverse models for survival prediction. The same principle applies to combining RESEARCH IDEAS from diverse AI models.

### 011b: Speed-tiered execution

**Given** experiments vary in complexity
**When** I have access to models of different speeds
**Then** I should dispatch appropriately

**Tiering strategy:**
- **Quick exploration** (Haiku/fast model): "Does adding feature X improve baseline? Just run a quick 3-fold CV."
- **Careful implementation** (Sonnet/medium model): "Write a proper script with error handling, run full 5-fold CV."
- **Deep analysis** (Opus/slow model): "Analyze why the oracle gap is uniform. Consider statistical mechanics parallels."

**What I wished today:** Many of our experiments were simple "does this help?" tests that didn't need the full power of the executor's model. A quick Haiku test could have filtered out dead ends in 1 minute instead of 10.

### 011c: Debate and cross-check

**Given** one executor proposes a result or conclusion
**When** I want to verify it
**Then** I should be able to ask a different model to cross-check

**Example from our session:**
- Executor (Claude) said: "Oracle gap is uniform, ceiling is ~0.75-0.76"
- I could have asked a Codex executor: "Here are the oracle error patterns. Do you agree the gap is uniform? Do you see any subgroup structure Claude might have missed?"
- Different model = different analytical biases = genuine cross-validation of conclusions

### 011d: Specialized roles

**Given** different agents have different strengths
**When** assembling a research team
**Then** assign roles based on strengths

**Potential role assignments:**
- **Implementer** (Claude Code Sonnet): Write and run experiments
- **Critic** (different model): Review code for bugs, question assumptions
- **Literature searcher** (model with web access): Find relevant papers and methods
- **Monitor** (shell session): Watch resource usage, tail logs, check for errors
- **Summarizer** (Haiku): Quick summaries of long outputs to save root agent context

### 011e: Context window management across army

**Given** multiple executors are working
**When** the root agent's context window fills up
**Then** the orchestration should remain efficient

**Pain point from today:** My context window filled with hundreds of sleep/poll/screen-read cycles. With multiple executors, this would be N times worse.

**Solutions:**
- Executor agents summarize their own results (only final summary goes to root)
- Structured result format (JSON) instead of screen scraping for inter-agent communication
- The root agent maintains a task board, not a full conversation history
- Background notifications only for state changes, not periodic polls

## The Tango Metaphor Extended

"Takes Two To Tango" — but a research orchestra needs more than two dancers:

- **Conductor** (root agent): Sets the tempo, chooses the pieces, coordinates
- **First violin** (primary executor): Carries the main melody, does the heavy lifting
- **Second violin** (secondary executor): Harmonizes, provides counterpoint
- **Percussion** (shell monitor): Keeps the beat, alerts to timing issues
- **Audience** (human): Enjoys the performance, occasionally requests encores

The beauty is that the conductor doesn't need to play every instrument — just understand each one well enough to coordinate them effectively.
