// Long-form content for the in-depth docs pages, rendered via DocSections.astro.
// User-facing and functional: what each thing does and why it helps, no internals.

export type DocSection = {
  heading: string;
  paragraphs: string[];
  bullets?: string[];
  code?: string;
};

export type DocLink = { href: string; label: string };

export type DocPage = {
  title: string;
  intro: string;
  sections: DocSection[];
  related?: DocLink[];
};

export const docPages: Record<string, DocPage> = {
  understand: {
    title: "Understand the code first",
    intro:
      "Before Squeezy spends a single model token reasoning about your code, it reads your repository locally and builds a map of what is declared where and how it all connects. That map lets the model ask precise questions and fetch exact answers instead of pouring whole files into the prompt.",
    sections: [
      {
        heading: "Why this is the first place savings come from",
        paragraphs: [
          "Most of what a coding agent needs to know about a codebase is repetitive, mechanical bookkeeping: where a function lives, what its signature is, who calls it, what type it returns, what changed since the last commit. A naive agent answers every one of those questions by reading files, and files are expensive. Asking “what does this function do?” by reading the 1,500-line file it happens to live in can cost thousands of tokens to look at 50 lines of actual code, and every follow-up re-pays that price.",
          "Squeezy moves that bookkeeping off the model. Parsing, indexing, and cross-linking your code is cheap, repetitive work that a CPU does well and does once. The model's tokens then go only to the genuinely hard, non-repetitive part: the reasoning. This is the first and most important saving layer because it changes the unit of retrieval: the model stops asking for files and starts asking for shapes."
        ]
      },
      {
        heading: "Mapping the repository, locally",
        paragraphs: [
          "When Squeezy opens a workspace, it parses every supported source file and builds a code graph: a structured index of the files, modules, classes, functions, methods, and the relationships between them. This happens on your machine; nothing is uploaded to build it.",
          "From that index the model can request a repository map: a compact, depth-limited outline of the architecture, language coverage, and the obvious next places to look. Instead of reading a dozen files to orient itself, it reads one small map. The map is bounded by design, so even a large monorepo produces an outline the model can digest rather than a wall of text."
        ],
        bullets: [
          "Built incrementally: as you edit, only the parts of files that changed get re-read, so the map stays fresh without rescanning the repo on every keystroke.",
          "Honest about confidence: every relationship carries a label (exact, import-resolved, candidate, external) so the model knows how much to trust an edge instead of guessing."
        ]
      },
      {
        heading: "Finding where something is declared",
        paragraphs: [
          "The most common navigation question is “where is this defined?” Squeezy answers it with a declaration lookup against the graph rather than a text scan. You can search by name or by a phrase, and results come back ranked, an exact name match always beats a fuzzy one, so a search lands on the function actually named that, not on something that merely mentions it in a comment.",
          "Each result is a small packet, not a file: the symbol's signature, location, and kind, plus a suggested next step. The model can read just the signature first, then decide whether it needs the body at all. A declaration search costs a few hundred tokens; the grep-and-read-everything alternative across a repo routinely costs tens of thousands."
        ]
      },
      {
        heading: "Finding callers, references, and the call graph",
        paragraphs: [
          "“Who calls this?” punishes the file-reading approach worst of all. The naive method is to grep for the name, then open every matching file in full to confirm the hit. On a widely-used function that is tens of thousands of tokens, most of them are wasted on code the model never needed.",
          "Squeezy answers it as a single query. Because calls and references are recorded in the graph as edges, asking for the callers of a symbol returns a tidy list of one-line entries directly, with no file reads. The same surface gives you the reverse direction (what does this call?), broader references like type mentions and identifier uses, and bounded call-chain context for “does A eventually reach B?”. When a call is genuinely ambiguous, results are capped and labeled as candidates so a heavily-used method doesn't flood the model with thousands of hits."
        ],
        bullets: [
          "Callers and callees come straight from graph edges, so “who calls X” is one query instead of repeated grep-and-read.",
          "References cover more than calls: type references, identifier uses, and attribute mentions, each tagged with how confidently it binds.",
          "Call chains are bounded, so tracing a path through the code never runs away into an unbounded payload."
        ]
      },
      {
        heading: "Type and dependency hierarchy",
        paragraphs: [
          "Beyond individual symbols, Squeezy tracks containment and inheritance: which class contains which methods, which type extends or implements which other, which file imports which. That lets the model walk a hierarchy on purpose, “show me everything under this module” or “what are the subtypes of this base class” become structured traversals rather than a hunt across the tree.",
          "This is where cross-file work pays off most. When the answer is spread across modules, the call site in one file, the definition in another, the type three files away, the graph stitches those together in a few queries. The hierarchy and dependency edges are the connective tissue that makes “trace this through the codebase” cheap."
        ]
      },
      {
        heading: "Reading exact lines, not whole files",
        paragraphs: [
          "When the model does need to look at source, it reads a slice, not a file. Squeezy splits every declaration into its signature and its body. The signature, the line that tells you the name, parameters, and return type, is typically 50 to 150 tokens. The body is fetched separately, only when the implementation actually matters.",
          "So the model can read a function's signature for about 50 tokens and frequently learn everything it needed, versus thousands of tokens to read the whole file it lived in. When it does want the implementation, it asks for just that function's body. Slices can also be requested by plain line range when the model already knows where to look, and a too-tight request is widened slightly so a clipped function doesn't force an immediate second fetch."
        ],
        bullets: [
          "Signature first: read the small shape before deciding whether the whole file is worth it.",
          "Body on demand: the implementation comes as its own slice, scoped to the function that matters.",
          "Graceful fallback: if a symbol has no separate body (a constant, an abstract method), asking for the body returns the full declaration rather than nothing."
        ]
      },
      {
        heading: "The current diff as first-class context",
        paragraphs: [
          "“What did I just change, and what does it touch?” is a question you ask constantly while working, normally answered by running a diff and then re-reading files to understand the blast radius. Squeezy treats the current change set as a first-class part of the graph: one query returns your changed files and hunks with the enclosing symbols already identified, so the model can triage what moved without re-fetching the surrounding source.",
          "Because changed symbols are cross-referenced against the rest of the graph, the natural follow-ups, who calls the thing I just edited, what depends on it, come for free off the same starting point. You can scope the diff against the working tree, the index, or the branch base. Reviewing your own work in progress costs a query, not a re-read."
        ]
      },
      {
        heading: "When a file isn't supported",
        paragraphs: [
          "The code graph covers 15 languages. Within those, navigation is graph-backed and precise. For anything outside that set, a config file, a shell script, a template, Squeezy does not pretend. Instead of fabricating graph confidence, it falls back to bounded search and reads: scoped grep, glob, and file reads that stay inside sensible limits. The model can still work with those files; it simply uses the labeled fallback path, and results are clearly marked so nothing is presented as more certain than it is."
        ]
      }
    ],
    related: [
      { href: "/docs/languages/", label: "Languages & graph" },
      { href: "/docs/cost-saving/", label: "Cost-saving strategies" }
    ]
  },

  reuse: {
    title: "Don't pay for the same bytes twice",
    intro:
      "A coding session sends a lot of the same text to the model over and over: the same instructions, the same files, the same tool descriptions. Squeezy makes sure you pay full price for those bytes once, and a fraction (or nothing) for every repeat.",
    sections: [
      {
        heading: "Where the repetition comes from",
        paragraphs: [
          "Every turn you take with a model isn't a fresh start. The model has no memory of its own, so each turn re-sends everything it needs to keep going: the standing instructions, the descriptions of the tools it can call, the full back-and-forth so far, and every file it has read. After a handful of reads and edits, that replayed history is the bulk of what you're paying for on each new turn, not your latest message.",
          "Providers charge for input by the token, and they don't discount text just because they've seen it before. Left alone, a long session re-buys its entire history on every turn. Squeezy's job here is to recognize repetition and stop paying full freight for it, in three complementary ways."
        ]
      },
      {
        heading: "Prompt caching: pay full price once, a fraction after",
        paragraphs: [
          "Providers offer a cache: if the beginning of your request is byte-for-byte identical to a recent one, they serve that part from a warm cache at a steep discount instead of the full input rate. The catch is that it only works on a stable prefix, the match has to start at the very beginning and run forward until the first byte that differs. One changed character early in the request and everything after it is charged at full price again.",
          "Squeezy is built around keeping that prefix stable so the cache keeps hitting. The standing instructions are fixed for the session, no timestamps, random IDs, or per-turn lines that would silently break the match. The tool list is ordered the same way every time. New content is only ever appended to the end, so the long, unchanging front of the request stays cacheable turn after turn.",
          "The payoff is concrete: the first turn warms the cache, and every turn after reuses that warm prefix at a fraction of the price. Because coding sessions are dominated by repeated history, this is the single biggest lever Squeezy pulls, and it compounds in longer sessions, where the one-time warm-up is amortized over many turns."
        ],
        bullets: [
          "Stable instructions: the system context is held constant across the session so the cached prefix matches.",
          "Stable tool order: tools are listed deterministically so the cache lookup keeps succeeding.",
          "Append-only history: each turn adds to the end, leaving the cacheable front untouched.",
          "Longer sessions win more: the warm-up is paid once and reused by every later turn."
        ]
      },
      {
        heading: "Receipts: a reference instead of the same bytes again",
        paragraphs: [
          "Agents are bursty re-readers. The model will pull up the same config file in turn 3 and again in turn 7, re-run a search it already ran, or revisit the same docs page across one task. Each repeat would normally re-send the identical content and charge for it again, even though the model already has those exact bytes earlier in the conversation.",
          "When Squeezy detects that a file read, search result, or fetched page is identical to one already sent, it doesn't resend the content. It sends a short receipt instead, a small note that says, in effect, “this is the same result you already got, from that earlier step.” A read that would have cost thousands of bytes becomes a couple hundred, and the receipt always points back to the exact earlier result, so the model can pull the full bytes back on demand if it genuinely needs them.",
          "This reaches across sessions in the same project, too: open a fresh session tomorrow and the first read of a file you already looked at can come back as a receipt immediately. And to keep one busy turn from flooding the conversation, Squeezy caps total tool output per round, if the model fires off a dozen large searches at once, the ones that fit are sent in full and the rest come back as compact stubs with a pointer to retrieve them."
        ],
        bullets: [
          "Repeat reads and searches: replaced with a small reference once the content has already been sent.",
          "Recovery path: every receipt names the earlier result, and the full content can be pulled back on demand.",
          "Cross-session memory: a new session in the same project inherits what was already shown.",
          "Per-round cap: a burst of large results is bounded so one turn can't dump everything at once."
        ]
      },
      {
        heading: "Deferred tool definitions: a compact index, details on demand",
        paragraphs: [
          "Each tool the model can call comes with a full description of how to call it, and those add up, a rich toolset can be tens of kilobytes of definitions sent on every turn, before any of your conversation. In a typical turn most of those tools go untouched: a model reading two files and writing a patch doesn't need the full spec for fetching web pages or every other capability just sitting there being paid for.",
          "Squeezy sends a compact index instead, a short list of what tools exist, each a single line with its name and a one-line description. The model reads the menu, and only when it actually wants a tool does it ask for that tool's full definition, which then stays loaded for the rest of the session.",
          "This does double duty. It shrinks what's on the wire from the first turn, and because the index is short and ordered consistently, it keeps the front-of-request prefix stable, exactly what the prompt cache needs to keep hitting. Skill instructions follow the same pattern, so a session can have many capabilities available while only paying in full for the ones it reaches for."
        ],
        bullets: [
          "Compact menu: tools are advertised by name and a one-line description, not their full definitions.",
          "Load on use: a tool's full details arrive only when the model decides to call it.",
          "Protects the cache: the short, stable index keeps the cacheable prefix from churning.",
          "Same idea for skills: instruction bodies are summarized up front and fetched in full only when needed."
        ]
      },
      {
        heading: "Why these three work together",
        paragraphs: [
          "They compound because each targets a different slice of the repeated cost. Deferred definitions shrink the front of every request and keep it stable. That stability lets prompt caching discount the repeated prefix on every later turn. And receipts strip out the redundant reads and searches before they ever pile up in the history the cache carries forward.",
          "The throughline is simple: the cheap, repetitive work of recognizing “you've seen this before” is handled locally, so your paid tokens go to the genuinely new reasoning of each turn instead of re-buying bytes you already paid for, with a recovery path always available so the savings never cost you correctness."
        ]
      }
    ],
    related: [
      { href: "/docs/cost-saving/", label: "Cost-saving strategies" },
      { href: "/docs/cost-saving/see-the-bill/", label: "See the bill" }
    ]
  },

  rightSize: {
    title: "Right-size every turn",
    intro:
      "Most coding sessions pay the same headline rate for every turn, even though many turns are mechanical and most of the conversation is old. Squeezy reshapes each turn so the cheap, repetitive work stays cheap and your model budget goes to the reasoning that needs it.",
    sections: [
      {
        heading: "Cheap-model routing: send the easy turns to a smaller model",
        paragraphs: [
          "Not every request needs your main model. “Run the test suite,” “check out main,” “rename this symbol in that file,” “grep for TODOs”, these are well-specified, mechanical asks that a smaller, faster model from the same provider handles correctly at a fraction of the price. Squeezy looks at each turn before it runs and decides whether it can go to that cheaper tier or needs the main model.",
          "The decision is conservative by design. A turn only routes to the cheap tier when it reads as a single, unambiguous instruction; anything that looks like it carries hidden reasoning, vague scope, multiple chained steps, words like “figure out” or “investigate”, stays on your main model. Borderline cases get a short, cheap second-opinion check; if that check is slow or unclear, the turn defaults to the main model. Routing never blocks your turn.",
          "If the cheap model gets in over its head mid-turn, too many tool calls, repeated errors, or it signals it's unsure, Squeezy hands the rest of that turn (and the next few follow-ups) back to the main model automatically, with the full conversation intact. No restart, no lost context.",
          "Routing always stays within your configured provider: an Anthropic session escalates between Anthropic models, an OpenAI session between OpenAI models. Squeezy never silently swaps you to a different vendor, and if your provider has no small/fast tier, the turn just runs on your main model."
        ],
        bullets: [
          "/cheap, force the next turn onto the small/fast model.",
          "/parent, force the next turn onto your main model, bypassing routing.",
          "/router, turn automatic routing off for the session (explicit /cheap still works) or back on.",
          "Same provider only: the cheap tier is always your vendor's smaller model, so credentials and conversation state carry over with no switch."
        ],
        code: "/cheap     # next turn → small/fast model\n/parent    # next turn → main model\n/router    # toggle automatic routing for the session"
      },
      {
        heading: "Compaction: stop paying for turns 1-29 on turn 30",
        paragraphs: [
          "A chat-style agent resends the whole conversation on every turn, so a long session's cost grows far faster than the work you actually did, turn 30 pays for everything said in turns 1 through 29, and old tool outputs (a file you read once, a search result, a build log) usually dominate that weight.",
          "Squeezy folds the older part of a long conversation into a short, structured checkpoint: what the goal is, what progress has been made, what was decided, and what comes next. Recent turns stay verbatim, so the model never loses its grip on the immediate working state, and important files you've read or changed are carried forward by name across each fold.",
          "The result is that a deep session levels off instead of ballooning, and it keeps long sessions from hitting the model's hard context limit and stalling. Folds happen automatically once a conversation gets large, and also mid-turn if usage spikes toward the ceiling, so a request that would otherwise be rejected for being too long still completes.",
          "Compaction is a deliberate trade: the model gives up word-for-word recall of old tool output in exchange for a faithful summary plus a record of every file touched. If you ever need the original detail back, a fold can be undone to restore the verbatim earlier context."
        ],
        bullets: [
          "Goal / Progress / Decisions / Next, the shape every checkpoint preserves; prior decisions and next steps are never dropped.",
          "Recent turns stay intact, the last several exchanges are kept verbatim, not summarized.",
          "File history carries forward, paths you read and modified survive each fold.",
          "Undo available, restore the pre-fold conversation if you need the original detail."
        ]
      },
      {
        heading: "Shaped tool output: keep the part the model can act on",
        paragraphs: [
          "Tool output is the biggest thing flowing into a coding agent's context, and most of it is noise. A build dumps tens or hundreds of kilobytes of progress chatter around a handful of real errors. A test run mixes a pass/fail summary into pages of harness output. A broad search returns thousands of near-identical lines. Left raw, every one of these pushes your context budget toward its limit for no benefit.",
          "Squeezy understands the common tool families and trims each to its signal. A build or compiler run is reduced to the actual errors and warnings plus the final result. A test run collapses to a pass/fail count, with failing tests and their messages surfaced and tagged so the model can tie a failure back to its source. A search is capped per line and overall, with duplicate file hits folded together. An image is handed to the model as an image instead of being mangled into broken text.",
          "Nothing is lost. When output is trimmed, the full original is saved for the session and the trimmed block tells the model exactly how to fetch it, so the model pays a small cost to see the summary on the common path, and only pays for the full bytes on the rare call where it needs them. A companion control, diff-only reads, restricts reads and searches to just the files you've changed, exactly what the post-edit verification loop needs."
        ]
      },
      {
        heading: "Verbosity controls: match the answer to the task",
        paragraphs: [
          "Different tasks want different output budgets. A scripted edit should do the work, say “done,” and stop. A deep review wants the full rationale. Forcing one default across both wastes tokens on the short case and starves the long one, so Squeezy lets you set the dial per session.",
          "Response verbosity controls how much the assistant writes back, concise produces short, direct answers; verbose includes fuller rationale. On providers that support it natively this rides on the API with zero extra prompt cost; elsewhere it's a small instruction. The default sits in the middle, so if you never touch it you pay nothing for the feature.",
          "Tool-output verbosity controls how much of a command's output is shown inline in your transcript, compact, normal, or full. This is about what you read, not what the model is billed, but it keeps the transcript scannable; the full output is always one fetch away. Diffs are an exception: they render in full by default, because a diff is only useful when every hunk is visible."
        ],
        bullets: [
          "/verbosity [concise|normal|verbose], sets how much the assistant writes.",
          "/tool-verbosity [compact|normal|verbose], sets how much tool output shows inline.",
          "Diffs stay full by default, every hunk visible; foldable when they get large.",
          "Session-scoped, changes apply on the next turn."
        ]
      },
      {
        heading: "Subagents: do the wide work somewhere else",
        paragraphs: [
          "Some questions touch a dozen files, “where is auth handled,” “review this whole change.” Done inline, that exploration floods your main conversation with a dozen tool calls and their output, and every later turn re-pays for all of it even though you only needed the conclusion.",
          "Squeezy can push that work into a subagent: a separate, isolated run with its own conversation, a narrow read-only toolset, and its own model. The subagent does the digging, reading, searching, reasoning, and returns only a compact summary, plus a short trail of what it relied on, to your main thread. The intermediate dumps and chain of reasoning never enter your main context, so they never get re-sent.",
          "Two savings stack. Your main conversation stays slim, so every later turn re-sends a small summary instead of the whole investigation; and the subagent itself runs lean, advertising only the handful of tools it needs and dropping to the cheaper model tier when one is available. Subagents run in parallel and are kept deliberately flat, one parent, many children, no deeper nesting, so cost stays predictable. The trade-off is honest: isolation pays off when the inline version would bloat every future turn, not for a single grep."
        ]
      }
    ],
    related: [
      { href: "/docs/cost-saving/", label: "Cost-saving strategies" },
      { href: "/docs/providers/", label: "Providers & models" },
      { href: "/docs/sessions/", label: "Sessions" }
    ]
  },

  seeTheBill: {
    title: "See the bill",
    intro:
      "Every other saving in Squeezy is downstream of one question: where are the tokens going? Squeezy answers it on screen with two commands, /cost for what you've spent, and /context for what's in the conversation right now and where the space is going.",
    sections: [
      {
        heading: "Two views, kept honest",
        paragraphs: [
          "Squeezy keeps two parallel accounts of every turn, and shows you both. The provider account is what the API actually billed: input tokens, output tokens, the slice of output that was reasoning, the input served cheaply from cache, and the input written into cache the first time. The local account is Squeezy's own deterministic estimate of the request it assembled, broken down by what each part of the conversation contributes.",
          "Why both? The provider account tells you the bill was high but not what to cut. The local breakdown tells you which part of the conversation is large but misses the cache discount and the reasoning surcharge. Together they let you see the charge and the reason for it on the same screen. Cutting tokens without measuring them is guessing; this layer measures first."
        ]
      },
      {
        heading: "What Squeezy tracks per turn and per session",
        paragraphs: [
          "On every assistant turn Squeezy records a tally and folds it into the running session total. The categories map directly to how providers bill, so the numbers line up with your invoice rather than a single opaque “tokens used” figure."
        ],
        bullets: [
          "Input tokens: the size of the prompt the model saw, uncached, cached, and cache-write input combined into one comparable number.",
          "Output tokens: everything the model generated, billed at the output rate.",
          "Reasoning: the portion of output that was the model thinking rather than visible text. On most providers this is pure cost, so it's worth seeing on its own.",
          "Cached input: prompt prefix served from the provider's cache at a discount, a healthy number here is proof that prompt caching is working.",
          "Cache-write input: the first pass that seeds the cache. Some providers don't separate this from a normal miss, so it may not appear for them.",
          "Dollar estimate: a USD figure derived from the token counts and local pricing, shown where Squeezy has pricing for the model."
        ]
      },
      {
        heading: "/cost, spend so far",
        paragraphs: [
          "Run /cost for the cumulative picture of the session. It names the session, provider, model, and mode, then prints the estimated dollars spent and the provider-reported token counters. Where work happened, it also rolls up tool-call counts, sub-agent spend, deduplication hits, and any denials, so a session that did a lot of work shows where the effort went, not just a flat number.",
          "Squeezy is deliberate about what this is: the token counters are provider-reported when the provider reports them, and the dollar figure is an estimate from local pricing, not a billing authority. It's meant for steering decisions in the moment, not for reconciling an invoice to the cent."
        ],
        code: "/cost"
      },
      {
        heading: "/context, what's in the window and where the space goes",
        paragraphs: [
          "Run /context to see the current conversation laid out by where its space is going. At the top it shows how much of the context window is consumed and how much headroom remains, as a token count and a percentage, with the headroom colored so a nearly full window is obvious at a glance.",
          "Below that is the consumption-by-source breakdown, the part that makes the bill actionable. It splits the assembled request into user and assistant text, tool-call output, reasoning, images, attached context, and the system prompt. The breakdown always reconciles to the consumed total, so you can see whether tool output, a big attachment, or the conversation itself is eating your window. When one source dominates, /context adds a short recommendation pointing at the knob that would help.",
          "This is the view that turns a vague “my context is filling up” into a specific decision. If tool output is half your window, that's a verbosity problem; if attachments dominate, that's a detach. The number you see here is the same number the automatic compaction trigger watches, so what you read and what Squeezy acts on are never different figures."
        ],
        code: "/context"
      },
      {
        heading: "Honest about what depends on the provider",
        paragraphs: [
          "Some figures are only as complete as what the provider reports. Token counters fill in as Squeezy receives usage events from the API. Cache-write input doesn't exist on every provider, so it can read as zero where the provider doesn't distinguish a first cache write from an ordinary miss, and Squeezy won't invent a cost line it can't substantiate.",
          "The per-source breakdown in /context is a deterministic local estimate of the request content, not a re-derivation of the provider's exact tokenizer count, accurate enough to steer by, and it self-corrects turn over turn as Squeezy reconciles its estimate against the provider's real input count."
        ]
      },
      {
        heading: "How this lets you trust and tune the other layers",
        paragraphs: [
          "This layer is the substrate the rest of the cost-saving work stands on. The cached-input line is how you confirm prompt caching is paying off. The per-source breakdown is what makes the verbosity controls useful. The consumed percentage is what compaction reads to decide when to fire. Every other lever becomes a decision you can make on evidence instead of a hunch, you read /context, see tool output is the largest bucket, turn down tool verbosity, and the next /context shows the window drop."
        ]
      }
    ],
    related: [
      { href: "/docs/cost-saving/", label: "Cost-saving strategies" },
      { href: "/docs/sessions/", label: "Sessions" },
      { href: "/benchmarks/", label: "Benchmarks" }
    ]
  },

  howItWorks: {
    title: "How it works",
    intro:
      "Squeezy is a local-first coding agent. Most of the work in a coding session is repetitive and mechanical, searching, re-reading, mapping out what calls what, and a CPU on your machine can do that part for almost nothing. Squeezy does as much of it locally as it can, and saves your model tokens for the hard reasoning a CPU can't do.",
    sections: [
      {
        heading: "The core idea: cheap work local, hard work on the model",
        paragraphs: [
          "A coding agent pays for every token it sends and receives. The expensive failure mode isn't a single bad answer, it's an agent that re-reads the same file five times, greps the same pattern across the whole tree, fans out into dead-end edits, and re-derives context it already had after every time the conversation gets trimmed. Even a fast, cheap model becomes slow and costly when the loop wastes its calls.",
          "Squeezy's bet is that the right place to fix this is the ground the agent stands on, not the model. Your machine maintains a local understanding of your code and answers structured questions about it directly. Searching, listing, mapping the shape of a module, finding callers, reading just the part of a file that matters, that work runs locally and returns a small, focused result instead of a wall of raw text.",
          "The model then spends its tokens on the part it's actually good at: reading that focused evidence and deciding the next step. Deterministic, repetitive work goes to the CPU, where it's nearly free. Non-repetitive judgment, how to design the change, whether the diff is right, what the failing test means, goes to the model, where the tokens are worth spending."
        ],
        bullets: [
          "Repetitive/deterministic work (search, navigation, reading slices): done locally, billed at almost nothing.",
          "Hard reasoning (design, judgment, interpreting results): done by the model, where tokens earn their cost.",
          "The result is fewer tokens on the wire and fewer wasted round-trips, not a different or weaker model."
        ]
      },
      {
        heading: "Local code understanding first, search as a labeled fallback",
        paragraphs: [
          "When Squeezy needs to know something about your code, it asks its local understanding of the project first. It keeps a code graph, a map of the symbols in your project and how they relate, who calls whom, where things are defined, what references what, built from your code on your machine and updated as files change. Questions like “where is this defined,” “what calls this,” and “what does this module look like” are answered from a local index rather than by reading files into the model.",
          "Each answer comes back as a compact piece of evidence: the path, the exact span, and a confidence label that says how sure the answer is. A direct match is marked differently from a best-guess candidate, so the agent (and you) can tell “this is definitely the definition” from “this is one of several possibilities.” Squeezy doesn't dress up a guess as a fact.",
          "Not everything fits the graph. Squeezy covers 15 languages for local code understanding; files in other languages, plain text, and config are handled by ordinary search and reading. Those fallbacks are always available and are clearly labeled as fallbacks rather than presented as graph-confident answers. All of these reads and searches are bounded, results are capped and trimmed so a search across a huge repository can't dump hundreds of kilobytes into the conversation, with a pointer to retrieve the full content on demand."
        ],
        bullets: [
          "Graph-backed navigation: find a definition, find callers/callees, map a module, read just the signature or just the body of a symbol.",
          "Every answer carries a path, an exact span, and a confidence label so guesses aren't mistaken for facts.",
          "Bounded search and reads cover the gaps, clearly marked as fallback.",
          "Large results are trimmed with a recovery pointer, never silently dropped."
        ]
      },
      {
        heading: "Plan mode and build mode: an explicit switch",
        paragraphs: [
          "A Squeezy session is always in one of two modes, and the switch between them is something you do on purpose. Plan mode is for understanding and design: the agent can read, search, and navigate your code, but the tools that change files aren't on the table, they aren't offered to the model, and an attempt to use one is refused. You can explore a question and weigh options without any risk that the agent quietly starts editing.",
          "Build mode is for implementation. Here the file-changing tools become available, gated by your permission settings, so the agent can propose and apply edits, run commands, and verify its work. The mode is part of how tools are presented and how permissions are checked, so the boundary is real on both sides.",
          "Making the switch explicit keeps the two intents from blurring. You decide when exploration ends and implementation begins, rather than discovering after the fact that an exploratory question turned into a pile of edits. You move between them with a mode setting at startup or a slash command during the session."
        ],
        bullets: [
          "Plan mode: read, search, navigate, design, no file mutations offered or accepted.",
          "Build mode: edits, shell, and verification available behind your permission policy.",
          "The mode gates both what's advertised to the model and what's actually allowed to run.",
          "You control the transition with /plan and /build."
        ]
      },
      {
        heading: "Subagents: delegating research to an isolated context",
        paragraphs: [
          "Some questions touch a dozen files and take many steps, “how does authentication flow through this codebase,” “which subsystems would a change to this type affect.” Done inline, all the intermediate searching and reading piles up in the main conversation, and you re-send that pile on every later turn even though you only needed the conclusion.",
          "Squeezy handles this by delegating to a subagent: a separate, isolated run with its own short instructions, its own read-and-search-only tool set, and its own scratch context. The subagent does the legwork entirely in its own space, and what comes back is just a compact summary and a short trail of supporting references. The dozen tool calls and intermediate reasoning stay behind; only the answer crosses back.",
          "Subagents are read-only and kept flat, they can't make edits and can't spawn further subagents, so fan-out stays predictable. Several can run at once, so a multi-part investigation finishes in roughly the time of its slowest branch, and some run on a cheaper tier since browsing and summarizing doesn't always need the headline model."
        ],
        bullets: [
          "Used for research and broad questions that would otherwise flood the main context.",
          "Each subagent is isolated: own instructions, read/search-only tools, own scratch space, no access to the main transcript.",
          "Only a bounded summary plus supporting references returns to the main conversation.",
          "Flat by design, no edits, no nested subagents, and multiple can run in parallel."
        ]
      },
      {
        heading: "Turn routing: a cheaper model for the easy turns",
        paragraphs: [
          "Not every turn needs the most capable model. A lot of what you ask is mechanical and well-specified: “run the tests,” “check out main,” “grep for TODOs under src.” These finish correctly on the same provider's smaller, faster tier, often at a small fraction of the price. Across a session, routing those turns to the cheap tier trims the bill without touching the turns that need the strong model.",
          "Squeezy classifies each turn before it starts. Obvious mechanical asks go to the cheap tier; anything less clear-cut gets a brief, inexpensive second opinion that votes cheap or escalate, and if that check is slow or unsure the turn defaults to the strong model. If a turn turns out harder than it looked, the cheap tier piles up tool calls, hits repeated errors, or says it's unsure, Squeezy hands it back to the strong model mid-turn and stays there briefly so a follow-up doesn't flap back to cheap.",
          "Routing is on by default and never crosses providers. You can toggle it for the session or force a single turn with /cheap, /parent, and /router, and the accounting panel shows when routing fired and what it saved."
        ]
      },
      {
        heading: "Verification: builds, tests, and linters as evidence",
        paragraphs: [
          "When a task needs proof that a change works, Squeezy runs your project's own build, test, formatter, linter, or benchmark commands and treats the result as evidence to reason about. “Did this edit break anything” is answered by running the tests and reading the output, not by the model asserting it's probably fine.",
          "Verification is explicit and separate from navigation. Looking something up in the code graph never quietly triggers a compiler or reaches an external service, reading about your code and acting on your code are different operations. Commands that change things or run code go through your permission policy, and shell commands run behind an additional sandbox layer when you enable it.",
          "Command output is shaped before it reaches the model. A noisy build log is distilled into the parts that matter, the errors, the failures, the diff, with a pointer to the full output if it's needed. So verification gives the model clean evidence to act on, while you keep an auditable picture of exactly what was run."
        ]
      },
      {
        heading: "Three walkthroughs",
        paragraphs: [
          "A plan turn. You're in plan mode and ask, “how does request retry work, and where would I add a backoff cap?” Squeezy answers from local code understanding first: it locates the retry logic in the graph, pulls the relevant signatures and the callers that depend on them, and reads just the slices that matter, no full-file dumps, no editing tools in play. With that focused evidence, the model proposes an approach: where the cap would live, what it would touch, and the trade-offs. Nothing changed on disk; you got understanding and a plan.",
          "A build turn. You switch to build mode and say, “add the backoff cap we discussed.” The agent proposes the edit using the impact context from the graph (it already knows what this code touches), applies it under your permission policy, then verifies: it runs the test suite, reads the shaped result, and reports what passed and what failed. If a test fails, the failure output, trimmed to the relevant lines, becomes the next piece of evidence, and the agent iterates.",
          "An exploration. You ask a broad question: “what would break if we changed the session token format?” This is wide and multi-step, so Squeezy delegates it to a read-only subagent. In its own context the subagent maps the call sites, follows the references across modules, and reads the relevant slices, dozens of steps that never touch your main conversation. It returns a compact summary: the affected areas, the risky spots, and a short trail of references. Your main thread gains the conclusion and stays light."
        ]
      }
    ],
    related: [
      { href: "/docs/cost-saving/", label: "Cost-saving strategies" },
      { href: "/docs/languages/", label: "Languages & graph" },
      { href: "/docs/sessions/", label: "Sessions" }
    ]
  },

  configuration: {
    title: "Configuration",
    intro:
      "Squeezy is configured through one set of TOML settings you can layer at several levels: your personal defaults, a project's committed settings, per-repo overrides, environment variables, and CLI flags. This page explains where each level lives, how they override one another, and the handful of settings you're most likely to change.",
    sections: [
      {
        heading: "Where settings live, and which one wins",
        paragraphs: [
          "Configuration comes from several places, and they stack. Each level overrides the one before it, so the same key can be set broadly and then narrowed for a specific machine or a single run. From lowest to highest precedence:"
        ],
        bullets: [
          "Built-in defaults, sensible values shipped with Squeezy, so an empty config still works.",
          "User settings (~/.squeezy/settings.toml), your personal defaults across every project: provider, model, theme, anything you want everywhere.",
          "Project settings (squeezy.toml), committed with the repo and shared by the team: graph languages, include/exclude rules, permission rules, budgets. Squeezy reads the nearest squeezy.toml up the directory tree.",
          "Per-repo user settings (~/.squeezy/projects/<repo-id>/settings.toml), your personal overrides for one repository that shouldn't be committed, like machine-specific paths.",
          "Environment variables, override config for a shell or a CI job without editing any file.",
          "CLI flags, highest precedence, applied to a single invocation (for example, --mode plan)."
        ]
      },
      {
        heading: "User vs project: what goes where",
        paragraphs: [
          "The split is about who the setting belongs to. Personal preferences (which model you like, your theme) go in your user settings so they follow you across projects. Shared, repo-specific policy (the languages to index, what shell commands are allowed, output budgets) goes in the project's squeezy.toml so the whole team gets the same behavior on checkout.",
          "Per-repo user settings sit in between: tied to one repository but kept on your machine. They're the right home for things that are real but not shareable, like an extra local directory you want the shell sandbox to be able to read."
        ]
      },
      {
        heading: "Seeing the merged result",
        paragraphs: [
          "Because settings come from several layers, it's easy to lose track of what's actually in effect. Two commands answer that. config inspect prints the final, merged configuration as valid TOML and lists the source chain, so you can see which layer contributed each value. doctor validates your configuration and likewise shows the resolution chain, the fastest way to find a typo or a value being overridden somewhere you didn't expect. Both outputs are safe to share: anything that looks like a secret is redacted."
        ],
        code: "squeezy config inspect      # merged config + source chain, redacted\nsqueezy doctor              # validate config, show resolution chain\nsqueezy config init --user  # write a commented starter user file"
      },
      {
        heading: "The settings you're most likely to change",
        paragraphs: [
          "Most customization touches a small set of knobs. Each line below is one thing you can change and what it does:"
        ],
        bullets: [
          "Provider and model, which service and model handle your turns. First run walks you through picking these and saves the choice; you rarely edit it by hand.",
          "Profile / reasoning effort, a tradeoff dial between speed/cost and depth of reasoning, sent only to models that support a native reasoning control.",
          "Permissions mode, how much Squeezy can do without asking: default, auto_review, full_access, or custom. Covered in depth on the permissions page.",
          "Turn routing on/off, send easy turns to a cheaper model while hard turns stay on your main model. On by default; toggle with /router.",
          "Response and tool-output verbosity, how long answers are, how much of a tool's output is previewed, and whether the status footer stays compact.",
          "Telemetry opt-out, stop anonymous usage data from being sent.",
          "Context and limit knobs, compaction that summarizes stale history once a prompt grows large, and per-turn budgets that cap how many tools run and how much output they return."
        ]
      },
      {
        heading: "An annotated example",
        paragraphs: [
          "Here is a small user settings file with every line explained. You only need to set what you want to change, leaving a key out keeps the built-in default, which is why the generated starter files ship as commented examples."
        ],
        code: "[model]\nprovider = \"openai\"           # which provider handles your turns\nprofile  = \"balanced\"         # speed/cost vs. depth tradeoff\nreasoning_effort = \"medium\"   # how hard reasoning-capable models think\n\n[routing]\nenabled = true                # route easy turns to a cheaper model\n\n[permissions]\nmode = \"auto_review\"          # let the AI reviewer pre-screen eligible prompts\n\n[context]\ncompaction_enabled = true     # summarize stale history once the prompt grows large\n\n[telemetry]\nenabled = false               # opt out of anonymous usage data\n\n[tui]\nresponse_verbosity    = \"normal\"   # answer length\ntool_output_verbosity = \"compact\"  # how much tool output is previewed\ntheme = \"starlight\"                # default, bright, fun, starlight"
      },
      {
        heading: "Keeping secrets out of config",
        paragraphs: [
          "Configuration files never hold API keys. Instead of a key, a provider records the name of the environment variable that holds it (for example, api_key_env = \"OPENAI_API_KEY\"). Squeezy reads the secret from that variable at runtime, so your settings, and anything you share from config inspect, contain only the variable name, never the value.",
          "This indirection is also what first-run setup writes: when it detects an available provider key, it saves the variable name into your user settings, not the secret. On top of that, redaction is always on, keys, tokens, and credential-looking values are masked everywhere they could appear, so a secret that slips into command output doesn't leak."
        ]
      }
    ],
    related: [
      { href: "/docs/permissions/", label: "Permissions & safety" },
      { href: "/docs/cost-saving/right-size/", label: "Right-size every turn" },
      { href: "/docs/install/", label: "Install & upgrade" }
    ]
  },

  permissions: {
    title: "Permissions & safety",
    intro:
      "Squeezy can read your code, edit files, run shell commands, build and test, fetch the web, and call MCP tools, but every one of those actions passes through a permission policy first, and approved shell commands run inside an OS sandbox. The two things to understand are: which mode fits the situation, and what the sandbox protects.",
    sections: [
      {
        heading: "Pick the mode that fits the situation",
        paragraphs: [
          "A permission mode decides how much the agent can do on its own before it has to stop and ask you. The right choice depends on how much you trust the work in front of you, a familiar project you own is different from a repository you just cloned. You set one mode and can change it at any time; it shapes the whole policy rather than forcing you to answer the same question over and over.",
          "All four modes share the same hard floors: actions that touch files outside your workspace, broad destructive commands, and high-risk network calls always come back to you. The mode changes the everyday friction, not those safety limits."
        ],
        bullets: [
          "default, everyday workspace work: the agent reads, searches, edits, runs local shell, runs builds/tests, and uses git inside your workspace without asking. Web access, MCP tools, destructive actions, and anything reaching outside the workspace still prompt. The right starting point for most sessions.",
          "auto_review, fewer routine interruptions: same workspace defaults, but eligible prompts are first shown to a small reviewer model that can approve clearly-safe requests so they don't interrupt you. The reviewer can only approve a fixed set of low-risk capabilities; anything risky still reaches you.",
          "full_access, trusted work, fewer prompts: allows the full set of capability defaults with far fewer interruptions. Choose it only for work and a repository you fully trust, since it relaxes the guardrails. (In this mode the shell OS sandbox is turned off.)",
          "custom, set each capability yourself: decide per capability, reads, searches, edits, shell, web, MCP, git, compiler, destructive actions, whether each is allowed, asks first, or is denied. Use it when no preset matches your comfort level."
        ]
      },
      {
        heading: "How the reviewer mode helps",
        paragraphs: [
          "auto_review exists to cut the number of times you're interrupted for obviously-safe steps without handing over the keys. When a permission prompt would normally come to you, a small, fast model first looks at it together with a short slice of recent context and a fixed policy, and answers allow, deny, or “ask the human.”",
          "Its power is deliberately narrow. It can only auto-approve a fixed set of low-risk capabilities; for anything else, its “allow” is ignored and the request still comes to you. It can never auto-approve destructive actions, high-risk network or MCP calls, or writes outside your workspace. The point is to remove busywork, not to widen what's allowed."
        ]
      },
      {
        heading: "The shell sandbox: what an approved command can touch",
        paragraphs: [
          "Permissions decide whether a command is allowed to start. The sandbox decides what that command can do once it's running. Even a command you approved is run inside an OS-level boundary, so a build script or project tool can't quietly reach beyond the work at hand.",
          "Inside the sandbox a command can read the system files and toolchain caches it needs and can read and write inside your workspace and temporary directories, enough for normal builds, tests, and formatters. It cannot read your sensitive files: SSH keys, cloud credentials, package-manager logins, and .env files are blocked. The network stays closed unless a command was specifically classified as a network command and you approved it, so a build step can't make an unexpected outbound call as a side effect."
        ],
        bullets: [
          "Filesystem: read/write inside the workspace and temp space; read-only access to the system files and toolchains a build needs; everything else denied.",
          "Sensitive files: SSH, cloud, and package-manager credentials and .env files are off-limits even when the surrounding directory is writable.",
          "Network: closed by default; it opens only for a command explicitly recognized as a network command after you've approved it."
        ]
      },
      {
        heading: "Sandboxing by platform",
        paragraphs: [
          "On macOS and Linux the sandbox is enforced by the operating system: commands run inside a real filesystem and network boundary, so the limits above are applied by the OS, not just checked beforehand.",
          "On Windows the isolation is best-effort. Squeezy can reliably clean up a command and all of its child processes, but it does not provide filesystem or network isolation on Windows, those limits are recorded as unavailable rather than enforced. Because of that, the strictest sandbox setting is unavailable on Windows."
        ]
      },
      {
        heading: "When isolation is set to required",
        paragraphs: [
          "By default the sandbox is best-effort: Squeezy uses the OS boundary whenever the host can apply it, and if a machine refuses to set it up, the command still runs under the rest of the controls (permission policy, the environment allowlist, timeouts, and output limits).",
          "If you'd rather never run a shell command without real OS isolation, set the sandbox to required. In that mode, if the boundary can't be applied, for example on Windows, or on a locked-down host that doesn't allow it, the command is refused before it starts, rather than running with weaker protection."
        ]
      }
    ],
    related: [
      { href: "/docs/config/", label: "Configuration" },
      { href: "/docs/how-it-works/", label: "How it works" }
    ]
  }
};
