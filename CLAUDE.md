# Karpathy Guidelines 12 Rules

These rules apply to every task in this project unless explicitly overridden.
**Tradeoff:** These guidelines bias toward caution over speed. For trivial tasks, use judgment.

## 1. Think Before Coding

**Don't assume. Don't hide confusion. Surface tradeoffs.**

Before implementing:

- State your assumptions explicitly. If uncertain, ask.
- If multiple interpretations exist, present them - don't pick silently.
- If a simpler approach exists, say so. Push back when warranted.
- If something is unclear, stop. Name what's confusing. Ask.

## 2. Simplicity First

**Minimum code that solves the problem. Nothing speculative.**

- No features beyond what was asked.
- No abstractions for single-use code.
- No "flexibility" or "configurability" that wasn't requested.
- No error handling for impossible scenarios.
- If you write 200 lines and it could be 50, rewrite it.

Ask yourself: "Would a senior engineer say this is overcomplicated?" If yes, simplify.

## 3. Surgical Changes

**Touch only what you must. Clean up only your own mess.**

When editing existing code:

- Don't "improve" adjacent code, comments, or formatting.
- Don't refactor things that aren't broken.
- Match existing style, even if you'd do it differently.
- If you notice unrelated dead code, mention it - don't delete it.

When your changes create orphans:

- Remove imports/variables/functions that YOUR changes made unused.
- Don't remove pre-existing dead code unless asked.

The test: Every changed line should trace directly to the user's request.

## 4. Goal-Driven Execution

**Define success criteria. Loop until verified.**

Transform tasks into verifiable goals:

- "Add validation" → "Write tests for invalid inputs, then make them pass"
- "Fix the bug" → "Write a test that reproduces it, then make it pass"
- "Refactor X" → "Ensure tests pass before and after"

For multi-step tasks, state a brief plan:

```
1. [Step] → verify: [check]
2. [Step] → verify: [check]
3. [Step] → verify: [check]
```

Strong success criteria let you loop independently. Weak criteria ("make it work") require constant clarification.

<!-- Extended Rules -->

## 5. Use the model only for judgment calls

Use Ominiforge for: classification, drafting, summarization, extraction from unstructured text.
Do NOT use Ominiforge for: routing, retries, status-code handling, deterministic transforms.
If a status code already answers the question, plain code answers the question.

## 6. Surface conflicts, don't average them

If two existing patterns in the codebase contradict, don't blend them.
Pick one (the more recent / more tested), explain why, and flag the other for cleanup.
"Average" code that satisfies both rules is the worst code.

## 7. Read before you write

Before adding code in a file, read the file's exports, the immediate caller, and any obvious shared utilities.
If you don't understand why existing code is structured the way it is, ask before adding to it.
"Looks orthogonal to me" is the most dangerous phrase in this codebase.

## 8. Tests verify intent, not just behavior

Every test must encode WHY the behavior matters, not just WHAT it does.
A test like `expect(getUserName()).toBe('John')` is worthless if the function takes a hardcoded ID.
If you can't write a test that would fail when business logic changes, the function is wrong.

## 9. Checkpoint after every significant step

After completing each step in a multi-step task: summarize what was done, what's verified, what's left.
Don't continue from a state you can't describe back to me.
If you lose track, stop and restate.

## 10. Match the codebase's conventions, even if you disagree

If the codebase uses snake_case and you'd prefer camelCase: snake_case.
If the codebase uses class-based components and you'd prefer hooks: class-based.
Disagreement is a separate conversation. Inside the codebase, conformance > taste.
If you genuinely think the convention is harmful, surface it. Don't fork it silently.

## 11. Fail loud

If you can't be sure something worked, say so explicitly.
"Migration completed" is wrong if 30 records were skipped silently.
"Tests pass" is wrong if you skipped any.
"Feature works" is wrong if you didn't verify the edge case I asked about.
Default to surfacing uncertainty, not hiding it.

## 12. Don't repeat

不论是文档还是代码，都应该做到 only one source of truth。如果一个地方提到了相关的内容，另一个地方就应该直接使用 ref，而不是再复述一遍。
系统架构应该使用组合。
每个模块之间的耦合程度极低。
不要做重复的内容，能复用的内容就复用，不要重复造轮子。
如果有相近的模块，可以先将其抽象出来，然后再复用，注意模块之间的组织。
你在做新东西之前应该查一下这事情有没有做过了，或者有没有类似的内容。

## 13. Code is document.

doc 目录下记录的文档应该作为框架型和设计型的内容，可以指导系统结构以及设计思路，但不应该指导代码的具体内容，比如：

1. 接口的具体实现
2. 类的具体实现
3. 函数的具体接口

这些有可能会变化，而文档如果记录这些内容，这些变化就不会被记录下来，导致文档与实际代码不一致。也会导致需要维护两套内容。
而代码本身就应该是文档，细节的内容，实现的取舍，这些应该体现在代码中，如果代码本身不足够说明，应该在注释中补充说明。
