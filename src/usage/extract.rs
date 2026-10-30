//! The remote extractor: one POSIX `sh` + awk script per machine (sent on
//! stdin to `sh -s`, like the telemetry sampler) that finds the agents' own
//! session logs, reads only the bytes added since last time and prints compact
//! TSV records — ids, timestamps, model/effort, numbers and the folder, never
//! message text. Offsets, sizes and inodes from the last run are embedded in
//! the script; a truncated or replaced file is read again from the start.
//!
//! Budgets: the script renices itself (19, idle I/O where `ionice` exists),
//! starts no new file after `max_seconds` and reads at most `max_mb` per run;
//! files left over are listed in a `B` line and read next time. Newest files
//! go first, so today's numbers arrive with the first run of a backfill.
//!
//! Output (tab-separated; record lines are described in `parse.rs`):
//!
//! | tag | fields |
//! |---|---|
//! | `H` | hostname, `$HOME`, epoch, stat flavour, `zstd` if present, `sqlite3` if present |
//! | `A` | agent, reader, log folder (`-`: none found) |
//! | `C` | agent, files listed, unchanged, to read, `.zst` files skipped (no zstd) |
//! | `F` | agent, reader, path, device, inode, size, mtime, start offset — a file's records follow |
//! | `u` `t` `l` | records (`parse::from_wire`, `parse::limit_from_wire`) |
//! | `P` | agent, then an `l` record's fields and the account's id: a plan's windows from the agent's own settings, not a log (Claude Code's `~/.claude.json`) |
//! | `Q` | agent: it isn't installed here, so no plan of its own (one read before is dropped) |
//! | `f` | path, next offset, carried state, lines read, records — the file is done (commit point) |
//! | `R` | path: truncated or replaced, read from the start |
//! | `M` | old path, new path: moved (same inode) |
//! | `V` | path: gone (or older than the backfill window) |
//! | `x` | agent, path, why it was skipped (needs sqlite3, an error) |
//! | `W` | the listing is complete |
//! | `B` | `time` / `bytes`, files left, bytes left: the budget ran out |
//! | `E` | epoch, files read, bytes read |

use super::parse;
use std::collections::HashMap;

/// Agents with a reader, in the order they're listed.
pub const READERS: &[&str] = &[
    "claude", "codex", "pi", "omp", "opencode", "gemini", "agy", "qwen",
];

/// The reader awk program (one per log line or file; see `READ_AWK` users in
/// the script). Single quotes are not allowed in here: it is embedded in
/// `'…'` in the shell script.
const READ_AWK: &str = r##"
# aleph usage reader. K = reader (claude codex pi qwen geminil gemini ocjson
# ocdb), OFF = byte offset of the first line, LIM = bytes to read (-1: all).
# ENVIRON: FP = path, SZ = size, CTX = state carried from the last run.
# Prints ids, timestamps, models, numbers and folders; never message text.
# Steps (the timeline): p prompt, ks / ke the start / end, c a
# compaction, e an error, sl the lane of a subagent — ids, tool names and
# numbers only.
BEGIN {
  HOME = ENVIRON["HOME"]; FP = ENVIRON["FP"]; SZ = ENVIRON["SZ"]; CTX = ENVIRON["CTX"]
  US = sprintf("%c", 31)
  if (CTX == "-") CTX = ""
  JL = (K == "claude" || K == "codex" || K == "pi" || K == "qwen" || K == "geminil" || K == "agyt")
  # Gemini keeps chats under tmp/<project>/chats/: the project, unless it is a hash
  i = index(FP, "/chats/"); PROJ = (i > 0) ? substr(FP, 1, i - 1) : ""; sub(/.*\//, "", PROJ)
  if (PROJ ~ /^[0-9a-f]+$/ && length(PROJ) >= 32) PROJ = ""
  D = 0; POS = 0; SC = ""; NREC = 0; NLINES = 0; NTC = 0; NG = 0; MAXW = ""
  if (K == "ocdb") FS = "\t"
  setup()
  # the own file of a Claude subagent: its lane, and (read from the start) what started it
  LANE = ""
  if (K == "claude" && FP ~ /\/subagents\/agent-[^\/]*\.jsonl$/) {
    LANE = FP; sub(/.*\/agent-/, "", LANE); sub(/\.jsonl$/, "", LANE)
    if (OFF + 0 == 0) clmeta()
  }
}
# the .meta.json of a subagent: the tool call that started it and its type (never its description)
function clmeta(   m, l, s, se, call, ty) {
  m = FP; sub(/\.jsonl$/, ".meta.json", m); s = ""
  while ((getline l < m) > 0) s = s l
  close(m)
  if (s == "") return
  call = sval(s, "toolUseId", 1); ty = sval(s, "agentType", 1)
  se = FP; sub(/\/subagents\/.*/, "", se); sub(/.*\//, "", se)
  print "sl\t\t" cl(se) "\t" cl(LANE) "\t" cl(call) "\t" word(ty)
}
# a string value of a key in raw JSON: the first (or the last); ids and times only
function sval(s, k, first,   p, i, r) {
  p = "\"" k "\":\""; r = ""
  while ((i = index(s, p)) > 0) { s = substr(s, i + length(p)); r = substr(s, 1, index(s, "\"") - 1); if (first) return r }
  return r
}
# a name kept: a tool, a type — letters, digits and . _ : - only, 48 at most
function word(v) { gsub(/[^A-Za-z0-9_.:-]/, "", v); return substr(v, 1, 48) }
# what kind of tool call a name is
function tclass(n) {
  if (n ~ /^mcp__/) return "mcp"
  if (n ~ /^(WebSearch|WebFetch|web_search|web\.run|web\.search|google_web_search|web_fetch)$/) return "web"
  if (n ~ /^(Agent|Task|task|spawn_agent|collaboration\.spawn_agent)$/) return "agent"
  return "tool"
}
function setup(   n, c) {
  n = split(CTX, c, US)
  if (K == "claude") {
    w("type subtype timestamp sessionId cwd isSidechain requestId effort uuid durationMs costUSD message.model message.id")
    w("message.content.#.type message.content.#.id message.content.#.name isApiErrorMessage compactMetadata.preTokens compactMetadata.postTokens")
    w("message.usage.input_tokens message.usage.output_tokens message.usage.cache_read_input_tokens message.usage.cache_creation_input_tokens")
    w("message.usage.cache_creation.ephemeral_5m_input_tokens message.usage.cache_creation.ephemeral_1h_input_tokens")
    w("message.usage.output_tokens_details.thinking_tokens message.usage.server_tool_use.web_search_requests message.usage.speed message.usage.inference_geo")
  } else if (K == "codex") {
    w("type timestamp payload.type payload.id payload.cwd payload.model payload.effort payload.response_id payload.turn_id payload.duration_ms")
    w("payload.name payload.namespace payload.call_id payload.status payload.item.type payload.item.id payload.item.status payload.item.exit_code payload.started_at_ms payload.completed_at_ms payload.reason")
    w("payload.source.subagent.thread_spawn.parent_thread_id payload.source.subagent.thread_spawn.agent_role")
    w("payload.parent_thread_id payload.source.subagent.other payload.thread_source")
    tu("payload.usage."); tu("payload.info.last_token_usage.")
    w("payload.info.total_token_usage.total_tokens payload.rate_limits.limit_id payload.rate_limits.plan_type payload.creator_account_id")
    rl("payload.rate_limits.primary."); rl("payload.rate_limits.secondary.")
    cS = c[1]; cM = c[2]; cE = c[3]; cC = c[4]; NEWF = c[5] + 0; cT = c[6]; cA = c[7]
    FID = FP; sub(/\.zst$/, "", FID); sub(/\.jsonl$/, "", FID)
    FID = (length(FID) >= 36) ? substr(FID, length(FID) - 35) : ""
    if (FID !~ /^[0-9a-fA-F-]+$/) FID = ""
  } else if (K == "pi") {
    w("type id timestamp cwd modelId thinkingLevel message.role message.model message.provider")
    w("message.content.#.type message.content.#.id message.content.#.name message.toolCallId message.toolName message.isError message.stopReason tokensBefore")
    w("message.usage.input message.usage.output message.usage.cacheRead message.usage.cacheWrite message.usage.cost.total")
    cS = c[1]; cM = c[2]; cE = c[3]; cC = c[4]
    # <timestamp>_<session id>.jsonl, for a header that says less
    FID = FP; sub(/.*\//, "", FID); sub(/\.jsonl$/, "", FID)
    i = index(FID, "_"); FID = (i > 0) ? substr(FID, i + 1) : ""
    if (cS == "") cS = FID
  } else if (K == "qwen") {
    w("type uuid sessionId timestamp cwd model usageMetadata.promptTokenCount usageMetadata.candidatesTokenCount")
    w("usageMetadata.cachedContentTokenCount usageMetadata.thoughtsTokenCount usageMetadata.toolUsePromptTokenCount")
  } else if (K == "geminil") {
    w("sessionId kind id timestamp type model tokens.input tokens.output tokens.cached tokens.thoughts tokens.tool")
    w("toolCalls.#.id toolCalls.#.name toolCalls.#.status toolCalls.#.timestamp")
    cS = c[1]; cF = c[2]
    FID = FP; sub(/.*\//, "", FID); sub(/\.jsonl$/, "", FID)
    if (FID ~ /^session-/) FID = substr(FID, length(FID) - 7)
  } else if (K == "agyt") {
    # numbers, times, kinds and tool names: never content, thinking or the arguments of a call
    w("type step_index source status created_at input_tokens output_tokens cache_read_tokens tool_calls.#.name")
    AGP = c[1]; AGPI = c[2]; AGG = c[3]; AGW = c[4]; AGLM = c[5]
    # brain/<conversation>/.system_generated/logs/transcript.jsonl
    AGS = FP; sub(/\/\.system_generated\/.*$/, "", AGS); sub(/.*\//, "", AGS)
  } else if (K == "gemini") {
    w("sessionId messages.#.id messages.#.timestamp messages.#.type messages.#.model")
    w("messages.#.toolCalls.#.id messages.#.toolCalls.#.name messages.#.toolCalls.#.status messages.#.toolCalls.#.timestamp")
    w("messages.#.tokens.input messages.#.tokens.output messages.#.tokens.cached messages.#.tokens.thoughts messages.#.tokens.tool")
    # the newest message read last time: a rewritten chat gives only what came after
    GTS = c[1]; MAXTS = GTS
  } else if (K == "ocjson") {
    w("id sessionID role modelID variant path.cwd cost time.created providerID")
    w("tokens.input tokens.output tokens.reasoning tokens.cache.read tokens.cache.write")
  } else if (K == "ocdb") {
    # the timeline: the parts and sessions read so far (their own watermarks)
    OCP = c[1]; OCS = c[2]
  }
}
function w(s,   a, n, i) { n = split(s, a, " "); for (i = 1; i <= n; i++) W[a[i]] = 1 }
function tu(p) { w(p "input_tokens " p "cached_input_tokens " p "cache_write_input_tokens " p "output_tokens " p "reasoning_output_tokens") }
function rl(p) { w(p "used_percent " p "window_minutes " p "resets_at " p "resets_in_seconds") }

# ---- a streaming JSON scanner: escaped quotes and backslashes are dropped,
# then the line splits on quotes into outside / inside-a-string pieces. Keys
# build dotted paths (`#` for array items); only whitelisted paths are kept.
function path() { return (P[D] == "") ? KY[D] : P[D] "." KY[D] }
function flush() { if (SC != "") { if (D > 0) val(path(), SC); SC = "" } }
function val(p, v) { if (p in W) V[p] = v }
function jline(s,   n, seg, i, t, j, ch, c1, m) {
  if (index(s, "\\")) { gsub(/\\\\/, "", s); gsub(/\\"/, "", s) }
  n = split(s, seg, "\"")
  for (i = 1; i <= n; i++) {
    t = seg[i]
    if (i % 2 == 0) {
      c1 = seg[i + 1]; sub(/^[ \t\r]+/, "", c1)
      if (substr(c1, 1, 1) == ":") KY[D] = t
      else if (D > 0) val(path(), t)
      continue
    }
    m = length(t)
    for (j = 1; j <= m; j++) {
      ch = substr(t, j, 1)
      if (ch == "{" || ch == "[") {
        flush(); c1 = (D == 0) ? "" : path()
        D++; P[D] = c1; TY[D] = ch; KY[D] = (ch == "[") ? "#" : ""
      } else if (ch == "}" || ch == "]") {
        flush()
        if (D > 0) { endc(P[D], D); D-- }
        if (D > 0 && TY[D] == "[") KY[D] = "#"
      } else if (ch == ",") flush()
      else if (ch != ":" && ch != " " && ch != "\t" && ch != "\r") SC = SC ch
    }
  }
}
function endc(p, d) {
  if (K == "agyt" && p == "tool_calls.#") { if (V["tool_calls.#.name"] != "") AGC[++NAGC] = word(V["tool_calls.#.name"]); clr("tool_calls.#."); return }
  if (K == "geminil" && p == "toolCalls.#") { glcall("toolCalls.#."); clr("toolCalls.#."); return }
  if (K == "gemini" && p == "messages.#.toolCalls.#") { gmcall(); clr("messages.#.toolCalls.#."); return }
  if (K == "pi" && p == "message.content.#") { picall(); clr("message.content.#."); return }
  if (JL) { if (d == 1) row() }
  else if (K == "gemini") { if (p == "messages.#") { gmrow(); clr("messages.#.") } }
  else if (K == "ocjson") { if (d == 1) ocjrow() }
}
function clr(pre,   k, ks, n, i) {
  n = 0; for (k in V) if (index(k, pre) == 1) ks[++n] = k
  for (i = 1; i <= n; i++) delete V[ks[i]]
}
function want() {
  if (K == "claude") return index($0, "\"usage\"") || index($0, "\"turn_duration\"") || index($0, "\"compact_boundary\"")
  if (K == "codex") return index($0, "\"token_count\"") || index($0, "\"token_usage_record\"") || index($0, "\"turn_context\"") || index($0, "\"session_meta\"") || index($0, "\"task_complete\"") || index($0, "\"task_started\"") || index($0, "_call\"") || index($0, "\"item_completed\"") || index($0, "\"turn_aborted\"") || index($0, "\"compacted\"")
  if (K == "pi") return index($0, "\"usage\"") || index($0, "_change\"") || index($0, "\"session\"") || index($0, "\"cwd\"") || index($0, "\"role\":\"user\"") || index($0, "\"toolResult\"") || index($0, "\"compaction\"")
  if (K == "qwen") return index($0, "\"usageMetadata\"")
  if (K == "agyt") return index($0, "\"step_index\"")
  if (K == "geminil") return index($0, "\"tokens\"") || index($0, "\"sessionId\"") || index($0, "\"toolCalls\"") || index($0, "\"type\":\"user\"")
  return 1
}

# ---- helpers: numbers go out through %.0f (old mawk prints big sums as 1e+09)
function num(v) { return (v ~ /^[0-9]+$/) ? v + 0 : 0 }
function f0(x) { return sprintf("%.0f", x) }
function cl(s) { gsub(/[\t\r\n]/, " ", s); return s }
function tl(s,   h) {
  s = cl(s); h = length(HOME)
  if (HOME != "" && HOME != "/" && index(s, HOME) == 1 && (length(s) == h || substr(s, h + 1, 1) == "/")) s = "~" substr(s, h + 1)
  return s
}
# the provider a request went through (OpenCode, pi): its account there, per machine
function prov(v) { return (v ~ /^[A-Za-z0-9_.-]+$/) ? "p:" v : "" }
function cost(v) { return (v ~ /^[0-9]*\.?[0-9]+([eE][-+]?[0-9]+)?$/) ? v : "" }
function emit(s) { print s; NREC++ }
function row() {
  if (K == "claude") clrow()
  else if (K == "codex") cxrow()
  else if (K == "pi") pirow()
  else if (K == "qwen") qwrow()
  else if (K == "geminil") glrow()
  else if (K == "agyt") agyrow()
}

# ---- Claude Code: assistant lines (one per content block, same message id:
# the last one wins) and turn_duration system lines
function clrow(   c5, c1, cw, fl, rec, k, mo, ts) {
  ts = cl(V["timestamp"])
  if (V["type"] == "assistant" && V["message.content.#.type"] == "tool_use" && V["message.content.#.id"] != "") {
    k = word(V["message.content.#.name"])
    print "ks\t" ts "\t" cl(V["sessionId"]) "\t" LANE "\t" cl(V["message.content.#.id"]) "\t" k "\t" tclass(k)
  }
  if (V["type"] == "assistant" && V["isApiErrorMessage"] == "true") print "e\t" ts "\t" cl(V["sessionId"]) "\t" LANE "\t" cl(V["uuid"]) "\tapi"
  if (V["type"] == "system" && V["subtype"] == "compact_boundary") {
    print "c\t" ts "\t" cl(V["sessionId"]) "\t" LANE "\t" cl(V["uuid"]) "\t" f0(num(V["compactMetadata.preTokens"])) "\t" f0(num(V["compactMetadata.postTokens"]))
    return
  }
  if (V["type"] == "assistant") {
    mo = V["message.model"]
    if (V["message.id"] == "" || mo == "" || mo == "<synthetic>") return
    if (!("message.usage.input_tokens" in V) && !("message.usage.output_tokens" in V)) return
    cw = num(V["message.usage.cache_creation_input_tokens"])
    c5 = num(V["message.usage.cache_creation.ephemeral_5m_input_tokens"])
    c1 = num(V["message.usage.cache_creation.ephemeral_1h_input_tokens"])
    if (!("message.usage.cache_creation.ephemeral_5m_input_tokens" in V) && !("message.usage.cache_creation.ephemeral_1h_input_tokens" in V)) c5 = cw
    else if (cw > c5 + c1) c5 = cw - c1
    fl = ""
    if (V["isSidechain"] == "true") fl = fl "s"
    if (V["message.usage.speed"] == "fast") fl = fl "f"
    if (V["message.usage.inference_geo"] == "us") fl = fl "u"
    k = cl(V["message.id"] ":" V["requestId"])
    rec = "u\t" cl(V["timestamp"]) "\t" cl(V["sessionId"]) "\t" k "\t" cl(mo) "\t" cl(V["effort"]) "\t" tl(V["cwd"])
    rec = rec "\t" f0(num(V["message.usage.input_tokens"])) "\t" f0(num(V["message.usage.output_tokens"])) "\t" f0(num(V["message.usage.cache_read_input_tokens"]))
    rec = rec "\t" f0(c5) "\t" f0(c1) "\t" f0(num(V["message.usage.output_tokens_details.thinking_tokens"])) "\t" cost(V["costUSD"]) "\t" fl "\t" f0(num(V["message.usage.server_tool_use.web_search_requests"]))
    if (LANE != "") rec = rec "\t\t" LANE
    if (k != PK && PK != "") emit(PR)
    PK = k; PR = rec
  } else if (V["type"] == "system" && V["subtype"] == "turn_duration" && num(V["durationMs"]) > 0) {
    if (PK != "") { emit(PR); PK = "" }
    emit("t\t" cl(V["timestamp"]) "\t" cl(V["sessionId"]) "\t" cl(V["uuid"]) "\t" f0(num(V["durationMs"])))
  }
}

# ---- Claude Code user lines, without the scanner (the result of a tool can be
# megabytes): the end of a tool call (its id, an error, a denial), else a prompt
# and who sent it (origin.kind). The quotes of the raw line are its keys: a
# string inside it has its quotes escaped.
function cluser(   ts, se, n, parts, i, id, st, o, by, k) {
  ts = sval($0, "timestamp", 1); se = sval($0, "sessionId", 0)
  if (index($0, "\"tool_use_id\":\"")) {
    n = split($0, parts, "\"tool_use_id\":\"")
    for (i = 2; i <= n; i++) {
      id = substr(parts[i], 1, index(parts[i], "\"") - 1)
      st = index(parts[i], "\"is_error\":true") ? "err" : "ok"
      if (index($0, "\"permissionDecision\":{\"decision\":\"reject\"") || index($0, "\"toolDenialKind\":\"")) st = "denied"
      if (id != "") print "ke\t" cl(ts) "\t" cl(se) "\t" LANE "\t" cl(id) "\t" st "\t0"
    }
    return
  }
  if (index($0, "\"isMeta\":true") || index($0, "\"isCompactSummary\":true")) return
  o = ""; if (match($0, /"origin":\{"kind":"[a-z-]*"/)) { o = substr($0, RSTART + 18, RLENGTH - 19) }
  by = "u"; if (o == "task-notification" || o == "peer") by = "a"; else if (o != "" && o != "human") by = "s"
  if (LANE != "") by = "a"
  k = sval($0, "promptId", 1); if (k == "") k = sval($0, "uuid", 1)
  if (k != "") print "p\t" cl(ts) "\t" cl(se) "\t" LANE "\t" cl(k) "\t" by
}

# ---- Codex: per-response token_usage_record lines when the log has them,
# else token_count events (repeats of the running total are dropped);
# turn_context carries the model and effort, task_complete the turn time
function cxrow(   t, pt, k) {
  t = V["type"]; pt = V["payload.type"]
  if (t == "session_meta") {
    if (V["payload.id"] != "") cS = cl(V["payload.id"])
    if (V["payload.cwd"] != "") cC = cl(V["payload.cwd"])
    # whose session it is: the account id, as the app server says it
    if (V["payload.creator_account_id"] ~ /^[A-Za-z0-9_-]+$/) cA = V["payload.creator_account_id"]
    # a thread another started (a subagent): its lane in that session
    if (V["payload.source.subagent.thread_spawn.parent_thread_id"] != "") print "sl\t" cl(V["timestamp"]) "\t" cl(V["payload.source.subagent.thread_spawn.parent_thread_id"]) "\t" sid() "\t\t" word(V["payload.source.subagent.thread_spawn.agent_role"])
    # Codex 0.164: the parent at the top, the kind of helper beside it (a guardian review)
    else if (V["payload.parent_thread_id"] != "") print "sl\t" cl(V["timestamp"]) "\t" cl(V["payload.parent_thread_id"]) "\t" sid() "\t\t" word(V["payload.source.subagent.other"] != "" ? V["payload.source.subagent.other"] : V["payload.thread_source"])
  } else if (t == "turn_context") {
    if (V["payload.model"] != "") cM = cl(V["payload.model"])
    if ("payload.effort" in V) cE = cl(V["payload.effort"])
    if (V["payload.cwd"] != "") cC = cl(V["payload.cwd"])
  } else if (t == "token_usage_record") {
    if (!NEWF) { NEWF = 1; NTC = 0 }
    if (V["payload.response_id"] != "") emit(cxrec(V["payload.response_id"], "payload.usage."))
  } else if (t == "event_msg" && pt == "token_count") {
    lim()
    k = V["payload.info.total_token_usage.total_tokens"]
    if (NEWF || k == "" || k == cT) return
    cT = k
    TC[++NTC] = cxrec(sid() ":" k, "payload.info.last_token_usage.")
  } else if (t == "event_msg" && pt == "task_complete" && num(V["payload.duration_ms"]) > 0) {
    emit("t\t" cl(V["timestamp"]) "\t" sid() "\t" cl(V["payload.turn_id"]) "\t" f0(num(V["payload.duration_ms"])))
  } else if (t == "event_msg" && pt == "task_started") {
    # a task is a prompt to it
    print "p\t" cl(V["timestamp"]) "\t" sid() "\t\t" cl(V["payload.turn_id"]) "\tu"
  } else if (t == "response_item" && (pt == "function_call" || pt == "custom_tool_call" || pt == "local_shell_call")) {
    k = word((V["payload.namespace"] != "" ? V["payload.namespace"] "." : "") (V["payload.name"] != "" ? V["payload.name"] : pt))
    if (V["payload.call_id"] != "") print "ks\t" cl(V["timestamp"]) "\t" sid() "\t\t" cl(V["payload.call_id"]) "\t" k "\t" tclass(k)
  } else if (t == "response_item" && pt == "web_search_call") {
    k = (V["payload.id"] != "") ? V["payload.id"] : sid() ":" V["timestamp"]
    print "ks\t" cl(V["timestamp"]) "\t" sid() "\t\t" cl(k) "\tweb_search\tweb"
    print "ke\t" cl(V["timestamp"]) "\t" sid() "\t\t" cl(k) "\t" (V["payload.status"] == "failed" ? "err" : "ok") "\t0"
  } else if (t == "event_msg" && pt == "item_completed" && V["payload.item.id"] != "") {
    k = (V["payload.item.status"] == "failed" || (V["payload.item.exit_code"] != "" && V["payload.item.exit_code"] != "0")) ? "err" : "ok"
    print "ke\t" cl(V["timestamp"]) "\t" sid() "\t\t" cl(V["payload.item.id"]) "\t" k "\t" f0(num(V["payload.completed_at_ms"]) - num(V["payload.started_at_ms"]))
  } else if (t == "event_msg" && pt == "turn_aborted") {
    print "e\t" cl(V["timestamp"]) "\t" sid() "\t\t" cl(V["payload.turn_id"]) "\tinterrupted"
  } else if (t == "compacted") {
    print "c\t" cl(V["timestamp"]) "\t" sid() "\t\t" cl(V["timestamp"]) "\t0\t0"
  }
}
# the output line of a Codex tool call, without the scanner (it can be long): the end of the call
function cxout(   id) {
  id = sval($0, "call_id", 1)
  if (id != "") print "ke\t" cl(sval($0, "timestamp", 1)) "\t" sid() "\t\t" cl(id) "\tok\t0"
}
function sid() { return (cS != "") ? cS : FID }
function cxrec(k, p,   i, cr, cw) {
  i = num(V[p "input_tokens"]); cr = num(V[p "cached_input_tokens"]); cw = num(V[p "cache_write_input_tokens"])
  i = i - cr - cw; if (i < 0) i = 0
  return "u\t" cl(V["timestamp"]) "\t" sid() "\t" cl(k) "\t" cM "\t" cE "\t" tl(cC) "\t" f0(i) "\t" f0(num(V[p "output_tokens"])) "\t" f0(cr) "\t" f0(cw) "\t0\t" f0(num(V[p "reasoning_output_tokens"])) "\t\t\t0\t" cA
}
function lim() {
  if (!("payload.rate_limits.primary.used_percent" in V) && !("payload.rate_limits.secondary.used_percent" in V)) return
  LMT = "l\t" cl(V["timestamp"]) "\t" cl(V["payload.rate_limits.limit_id"]) "\t" cl(V["payload.rate_limits.plan_type"]) "\t" rw("payload.rate_limits.primary.") "\t" rw("payload.rate_limits.secondary.")
}
function rw(p,   u) {
  u = V[p "used_percent"]; if (u !~ /^[0-9.]+$/) u = ""
  return u "\t" f0(num(V[p "window_minutes"])) "\t" f0(num(V[p "resets_at"])) "\t" f0(num(V[p "resets_in_seconds"]))
}

# ---- Pi / OMP: a session header, model and thinking-level changes, assistant
# messages with usage (and the cost Pi worked out)
function pirow(   t, mo, i, o, cr, cw) {
  t = V["type"]
  if (t == "session" || (t == "" && V["cwd"] != "")) { if (V["id"] != "") cS = cl(V["id"]); if (V["cwd"] != "") cC = cl(V["cwd"]) }
  else if (t == "model_change") { if (V["modelId"] != "") cM = cl(V["modelId"]) }
  else if (t == "thinking_level_change") cE = cl(V["thinkingLevel"])
  else if (t == "message" && V["message.role"] == "user") print "p\t" cl(V["timestamp"]) "\t" cS "\t\t" cl(V["id"]) "\tu"
  else if (t == "message" && V["message.role"] == "toolResult" && V["message.toolCallId"] != "")
    print "ke\t" cl(V["timestamp"]) "\t" cS "\t\t" cl(V["message.toolCallId"]) "\t" (V["message.isError"] == "true" ? "err" : "ok") "\t0"
  else if (t == "compaction") print "c\t" cl(V["timestamp"]) "\t" cS "\t\t" cl(V["id"]) "\t" f0(num(V["tokensBefore"])) "\t0"
  else if (t == "message" && V["message.role"] == "assistant") {
    if (V["message.stopReason"] == "error" || V["message.stopReason"] == "aborted") print "e\t" cl(V["timestamp"]) "\t" cS "\t\t" cl(V["id"]) "\t" (V["message.stopReason"] == "aborted" ? "interrupted" : "api")
    if (!("message.usage.input" in V) && !("message.usage.output" in V)) return
    mo = cl(V["message.model"]); if (mo == "") mo = cM
    i = num(V["message.usage.input"]); o = num(V["message.usage.output"]); cr = num(V["message.usage.cacheRead"]); cw = num(V["message.usage.cacheWrite"])
    if (i + o + cr + cw == 0) return
    emit("u\t" cl(V["timestamp"]) "\t" cS "\t" cS ":" cl(V["id"]) "\t" mo "\t" cE "\t" tl(cC) "\t" f0(i) "\t" f0(o) "\t" f0(cr) "\t" f0(cw) "\t0\t0\t" cost(V["message.usage.cost.total"]) "\t\t0\t" prov(V["message.provider"]))
  }
}

# a pi tool call (one item of the content of an assistant message)
function picall(   k) {
  if (V["message.content.#.type"] != "toolCall" || V["message.content.#.id"] == "") return
  k = word(V["message.content.#.name"])
  print "ks\t" cl(V["timestamp"]) "\t" cS "\t\t" cl(V["message.content.#.id"]) "\t" k "\t" tclass(k)
}

# ---- Qwen Code (JSONL chats, unverified): Gemini-style usageMetadata
function qwrow(   i, o, c, th, k) {
  if (!("usageMetadata.promptTokenCount" in V) && !("usageMetadata.candidatesTokenCount" in V)) return
  k = V["uuid"]; if (k == "") return
  i = num(V["usageMetadata.promptTokenCount"]); c = num(V["usageMetadata.cachedContentTokenCount"]); th = num(V["usageMetadata.thoughtsTokenCount"])
  o = num(V["usageMetadata.candidatesTokenCount"]) + th
  i = i - c + num(V["usageMetadata.toolUsePromptTokenCount"]); if (i < 0) i = 0
  emit("u\t" cl(V["timestamp"]) "\t" cl(V["sessionId"]) "\t" cl(k) "\t" cl(V["model"]) "\t\t" tl(V["cwd"]) "\t" f0(i) "\t" f0(o) "\t" f0(c) "\t0\t0\t" f0(th) "\t\t\t0")
}

# ---- Gemini CLI chats (JSONL): a metadata line with the session id, then
# whole message lines; a gemini message is appended again once its tokens are
# in (the last line per id wins). $set / $patch / $rewindTo lines are skipped.
function glrow(   k, i, c, th, o, s) {
  if (V["sessionId"] != "") { cS = cl(V["sessionId"]); if (V["kind"] == "subagent") cF = "s"; return }
  if (V["type"] == "user" && V["id"] != "") { s = (cS != "") ? cS : FID; print "p\t" cl(V["timestamp"]) "\t" s "\t\t" cl(V["id"]) "\tu"; return }
  if (V["type"] != "gemini") return
  if (!("tokens.input" in V) && !("tokens.output" in V)) return
  k = V["id"]; if (k == "") return
  s = (cS != "") ? cS : FID
  i = num(V["tokens.input"]); c = num(V["tokens.cached"]); th = num(V["tokens.thoughts"])
  o = num(V["tokens.output"]) + th
  i = i - c + num(V["tokens.tool"]); if (i < 0) i = 0
  emit("u\t" cl(V["timestamp"]) "\t" s "\t" s ":" cl(k) "\t" cl(V["model"]) "\t\t" PROJ "\t" f0(i) "\t" f0(o) "\t" f0(c) "\t0\t0\t" f0(th) "\t\t" cF "\t0")
}

# a Gemini tool call (one item of the toolCalls of a message): no duration in the log
function glcall(pre,   s, k) {
  if (V[pre "id"] == "") return
  s = (cS != "") ? cS : FID; k = word(V[pre "name"])
  print "ks\t" cl(V[pre "timestamp"]) "\t" s "\t\t" cl(V[pre "id"]) "\t" k "\t" tclass(k)
  print "ke\t" cl(V[pre "timestamp"]) "\t" s "\t\t" cl(V[pre "id"]) "\t" (V[pre "status"] ~ /error|cancel/ ? "err" : "ok") "\t0"
}
function gmcall(   ts, k) {
  ts = V["messages.#.toolCalls.#.timestamp"]; if (GTS != "" && ts < GTS) return
  k = word(V["messages.#.toolCalls.#.name"])
  if (V["messages.#.toolCalls.#.id"] == "") return
  GS[++NGS] = "ks\t" cl(ts) "\t@S@\t\t" cl(V["messages.#.toolCalls.#.id"]) "\t" k "\t" tclass(k)
  GS[++NGS] = "ke\t" cl(ts) "\t@S@\t\t" cl(V["messages.#.toolCalls.#.id"]) "\t" (V["messages.#.toolCalls.#.status"] ~ /error|cancel/ ? "err" : "ok") "\t0"
}

# ---- Gemini CLI before JSONL (and the first Qwen Code chats): one JSON document per chat,
# rewritten as it grows; gemini messages carry tokens (input includes cached)
function gmrow(   i, o, c, th, k, ts) {
  if (V["messages.#.type"] == "user" && V["messages.#.id"] != "" && (GTS == "" || V["messages.#.timestamp"] >= GTS))
    GS[++NGS] = "p\t" cl(V["messages.#.timestamp"]) "\t@S@\t\t" cl(V["messages.#.id"]) "\tu"
  if (V["messages.#.type"] != "gemini") return
  if (!("messages.#.tokens.input" in V) && !("messages.#.tokens.output" in V)) return
  k = V["messages.#.id"]; if (k == "") return
  ts = V["messages.#.timestamp"]
  if (GTS != "" && ts < GTS) return
  if (ts > MAXTS) MAXTS = ts
  i = num(V["messages.#.tokens.input"]); c = num(V["messages.#.tokens.cached"]); th = num(V["messages.#.tokens.thoughts"])
  o = num(V["messages.#.tokens.output"]) + th
  i = i - c + num(V["messages.#.tokens.tool"]); if (i < 0) i = 0
  GR[++NG] = "\t" cl(V["messages.#.timestamp"]) "\t@S@\t@S@:" cl(k) "\t" cl(V["messages.#.model"]) "\t\t" PROJ "\t" f0(i) "\t" f0(o) "\t" f0(c) "\t0\t0\t" f0(th) "\t\t\t0"
}

# ---- Antigravity CLI: a conversation is brain/<id>/.system_generated/logs/transcript.jsonl
# (a step a line: the prompt, each request with its tokens and the tools it called, each
# step after one) beside conversations/<id>.db, whose protobuf rows give each request its
# model and thinking tokens and the conversation its folder (PBH, PBW lines ahead of the log)
function agyrow(   s, si, ts, i, o, c, th, m, j) {
  s = AGS; si = V["step_index"]; ts = cl(V["created_at"])
  if (si !~ /^[0-9]+$/ || s == "") { NAGC = 0; return }
  if (V["type"] == "USER_INPUT") {
    agend(AGL)
    print "p\t" ts "\t" s "\t\t" s ":" si "\t" ((V["source"] ~ /USER/) ? "u" : "s")
    AGP = ts; AGPI = si; AGL = ts; NAGC = 0; NAGO = 0; return
  }
  if (ts != "") AGL = ts
  if (V["type"] == "PLANNER_RESPONSE" && (("input_tokens" in V) || ("output_tokens" in V))) {
    # output_tokens counts thinking already (thinking + the answer, in its row)
    i = num(V["input_tokens"]); o = num(V["output_tokens"]); c = num(V["cache_read_tokens"])
    m = (si in AGM) ? AGM[si] : AGLM; if (m != "") AGLM = m
    th = (si in AGT) ? AGT[si] : 0; if (th > o) th = o
    emit("u\t" ts "\t" s "\t" s ":" si "\t" cl(m) "\t\t" tl(AGW) "\t" f0(i) "\t" f0(o) "\t" f0(c) "\t0\t0\t" f0(th) "\t\t\t0")
    for (j = 1; j <= NAGC; j++) {
      print "ks\t" ts "\t" s "\t\t" s ":" si ":" j "\t" AGC[j] "\t" tclass(AGC[j])
      AGO[++NAGO] = s ":" si ":" j
    }
    # an answer that calls no tool: the prompt is done
    if (NAGC == 0) agend(ts)
  } else if (NAGO > 0) {
    # the step after a request: the tools it called ran, done here
    for (j = 1; j <= NAGO; j++) print "ke\t" ts "\t" s "\t\t" AGO[j] "\t" ((V["status"] ~ /ERROR|FAIL|CANCEL/) ? "err" : "ok") "\t0"
    NAGO = 0
  }
  NAGC = 0
}
# the prompt going on ends at `ts`: a turn record (how long it took)
function agend(ts,   d) {
  if (AGP == "" || ts == "") return
  d = isots(ts) - isots(AGP)
  if (d > 0) emit("t\t" ts "\t" AGS "\t" AGS ":" AGPI "\t" f0(d * 1000))
  AGP = ""; AGPI = ""
}
# 2026-10-26T19:49:00Z (UTC) → unix seconds (0: not one)
function isots(s,   y, mo, d, yy, m2) {
  if (s !~ /^[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]/) return 0
  y = substr(s, 1, 4) + 0; mo = substr(s, 6, 2) + 0; d = substr(s, 9, 2) + 0
  yy = (mo <= 2) ? y - 1 : y; m2 = (mo <= 2) ? mo + 9 : mo - 3
  return (365 * yy + int(yy / 4) - int(yy / 100) + int(yy / 400) + int((153 * m2 + 2) / 5) + d - 719469) * 86400 + substr(s, 12, 2) * 3600 + substr(s, 15, 2) * 60 + substr(s, 18, 2)
}
# protobuf, from hex: a byte, a varint (PVI: where it ended), printable text (else empty)
function hx(h, i) { return index("0123456789ABCDEF", substr(h, i, 1)) - 1 }
function pbyte(h, i) { return hx(h, 2 * i - 1) * 16 + hx(h, 2 * i) }
function pvar(h, i,   r, m, c) { r = 0; m = 1; do { c = pbyte(h, i); i++; r += (c % 128) * m; m *= 128 } while (c >= 128 && i <= PBL); PVI = i; return r }
function pstr(h, i, n,   t, j, c) { t = ""; for (j = 0; j < n && j < 96; j++) { c = pbyte(h, i + j); if (c < 32 || c > 126) return ""; t = t sprintf("%c", c) }; return t }
# one level of a message (bytes s to e - 1): varints into PF, length-delimited ones into
# PS / PN (where, how long), by level and field; the key-value entries of the request (1.20)
# give the step it belongs to (last_step_index)
function pwalk(h, s, e, lvl,   i, k, f, wt, n) {
  i = s
  while (i < e) {
    k = pvar(h, i); i = PVI; f = int(k / 8); wt = k % 8
    if (wt == 0) { PF[lvl, f] = pvar(h, i); i = PVI }
    else if (wt == 1) i += 8
    else if (wt == 5) i += 4
    else if (wt == 2) { n = pvar(h, i); i = PVI; PS[lvl, f] = i; PN[lvl, f] = n; if (lvl == 1 && f == 20) agkv(h, i, i + n); i += n }
    else return
  }
}
function agkv(h, s, e,   i, k, n, key, value) {
  i = s; key = ""; value = ""
  while (i < e) {
    k = pvar(h, i); i = PVI; if (k % 8 != 2) return
    n = pvar(h, i); i = PVI
    if (int(k / 8) == 1) key = pstr(h, i, n); else if (int(k / 8) == 2) value = pstr(h, i, n)
    i += n
  }
  if (key == "last_step_index" && value ~ /^[0-9]+$/) GSTEP = value + 0
}
# a PBH line (a request: 1.19 its model, 1.4.9 its thinking tokens) or PBW (the conversation:
# its workspace, file:///…)
function agmeta(   h, t, p, j, c, m, x, n) {
  x = substr($0, 5); h = x; sub(/^[^\t]*\t/, "", h); sub(/\t.*$/, "", x)
  PBL = length(h) / 2
  if (substr($0, 1, 4) == "PBW\t") {
    p = index(h, "66696C653A2F2F"); if (p == 0 || p % 2 == 0) return
    # a string: its length is the byte before it (a path under 128 bytes), else up to
    # the first byte that is not text
    j = (p + 1) / 2; n = (j > 1) ? pbyte(h, j - 1) : 0; if (n < 8 || n > 127) n = PBL
    t = ""; for (c = 0; c < n && j + c <= PBL; c++) { m = pbyte(h, j + c); if (m < 33 || m > 126) break; t = t sprintf("%c", m) }
    sub(/^file:\/\//, "", t); gsub(/%20/, " ", t)
    if (t ~ /^\//) AGW = t
    return
  }
  split("", PF); split("", PS); split("", PN); GSTEP = ""
  pwalk(h, 1, PBL + 1, 0)
  # the next read starts after the newest row seen (two back: one written meanwhile)
  if (x ~ /^[0-9]+$/ && x + 1 > AGG) AGG = x + 1
  if (!((0, 1) in PS)) return
  pwalk(h, PS[0, 1], PS[0, 1] + PN[0, 1], 1)
  m = ((1, 19) in PS) ? pstr(h, PS[1, 19], PN[1, 19]) : ""
  if ((1, 4) in PS) pwalk(h, PS[1, 4], PS[1, 4] + PN[1, 4], 2)
  if (GSTEP == "") return
  # the request is the step after the last one it saw
  if (m != "") AGM[GSTEP + 1] = m
  if ((2, 9) in PF) AGT[GSTEP + 1] = PF[2, 9]
}

# ---- OpenCode: a message file (older versions) or rows from opencode.db;
# output there excludes reasoning
function ocjrow(   i, o, rs, cr, cw) {
  if (V["role"] != "assistant") return
  i = num(V["tokens.input"]); o = num(V["tokens.output"]); rs = num(V["tokens.reasoning"]); cr = num(V["tokens.cache.read"]); cw = num(V["tokens.cache.write"])
  emit("u\t" cl(V["time.created"]) "\t" cl(V["sessionID"]) "\t" cl(V["id"]) "\t" cl(V["modelID"]) "\t" cl(V["variant"]) "\t" tl(V["path.cwd"]) "\t" f0(i) "\t" f0(o + rs) "\t" f0(cr) "\t" f0(cw) "\t0\t" f0(rs) "\t" cost(V["cost"]) "\t\t0\t" prov(V["providerID"]))
}
function ocrow(   i, o, rs, cr, cw, k, fz) {
  if ($0 ~ /^(Error|Parse error|Runtime error)/) { print "x\topencode\t" FP "\t" substr(cl($0), 1, 160); return }
  if (substr($0, 1, 2) == "x\t") { print; return }
  # the timeline: a prompt, a tool call (its start and end), a subagent (a child session)
  if ($1 == "U" && NF >= 5) {
    if ($5 ~ /^[0-9]+$/ && (MAXW == "" || $5 + 0 > MAXW + 0)) MAXW = $5
    print "p\t" $4 "\t" cl($3) "\t\t" cl($2) "\t" (($6 ~ /^[as]$/) ? $6 : "u"); return
  }
  # OpenCode 2: a prompt ended (its time since the prompt; not succeeded: an error)
  if ($1 == "I" && NF >= 5) {
    if ($5 ~ /^[0-9]+$/ && (MAXW == "" || $5 + 0 > MAXW + 0)) MAXW = $5
    if ($4 ~ /^[0-9]+$/ && $6 ~ /^[0-9]+$/ && $4 - $6 > 0) emit("t\t" $4 "\t" cl($3) "\t" cl($2) "\t" f0($4 - $6))
    if ($7 != "" && $7 != "succeeded") print "e\t" $4 "\t" cl($3) "\t\t" cl($2) "\t" (($7 ~ /abort|cancel|interrupt/) ? "interrupted" : "api")
    return
  }
  if ($1 == "C" && NF >= 5) {
    if ($5 ~ /^[0-9]+$/ && (MAXW == "" || $5 + 0 > MAXW + 0)) MAXW = $5
    print "c\t" $4 "\t" cl($3) "\t\t" cl($2) "\t0\t0"; return
  }
  if ($1 == "T" && NF >= 9) {
    if ($9 ~ /^[0-9]+$/ && $9 + 0 > OCP + 0) OCP = $9
    k = ($5 != "") ? $5 : $2
    if ($7 ~ /^[0-9]+$/) print "ks\t" $7 "\t" cl($3) "\t\t" cl(k) "\t" word($4) "\t" tclass(word($4))
    if ($8 ~ /^[0-9]+$/) print "ke\t" $8 "\t" cl($3) "\t\t" cl(k) "\t" ($6 == "error" ? "err" : "ok") "\t" f0($8 - $7)
    return
  }
  if ($1 == "S" && NF >= 5) {
    if ($5 ~ /^[0-9]+$/ && $5 + 0 > OCS + 0) OCS = $5
    print "sl\t" $4 "\t" cl($3) "\t" cl($2) "\t" cl($7) "\t" word($6); return
  }
  if (NF < 14 || $1 == "" || $3 !~ /^[0-9]+$/) return
  if ($4 ~ /^[0-9]+$/ && (MAXW == "" || $4 + 0 > MAXW + 0)) MAXW = $4
  i = num($9); o = num($10); rs = num($11); cr = num($12); cw = num($13)
  # the models of OpenCode itself (provider opencode), priced by it: $0 is free
  fz = ($6 == "opencode" && cost($14) != "" && $14 + 0 == 0) ? "z" : ""
  emit("u\t" $3 "\t" cl($2) "\t" cl($1) "\t" cl($5) "\t" cl($7) "\t" tl($8) "\t" f0(i) "\t" f0(o + rs) "\t" f0(cr) "\t" f0(cw) "\t0\t" f0(rs) "\t" cost($14) "\t" fz "\t0\t" prov($6))
}

{
  NLINES++
  if (K == "ocdb") { ocrow(); next }
  if (K == "agyt" && (substr($0, 1, 4) == "PBH\t" || substr($0, 1, 4) == "PBW\t")) { NLINES--; agmeta(); next }
  len = length($0) + 1
  if (LIM >= 0 && POS + len > LIM) { NLINES--; exit }
  POS += len
  if (K == "claude" && index($0, "\"type\":\"user\"")) { cluser(); next }
  if (K == "codex" && (index($0, "\"function_call_output\"") || index($0, "\"custom_tool_call_output\""))) { cxout(); next }
  if (JL) { D = 0; SC = ""; split("", V); if (!want()) next }
  jline($0)
}
END {
  if (K == "claude" && PK != "") emit(PR)
  if (K == "codex" && !NEWF) for (j = 1; j <= NTC; j++) emit(TC[j])
  if (K == "gemini") for (j = 1; j <= NG; j++) { s = GR[j]; gsub(/@S@/, cl(V["sessionId"]), s); emit("u" s) }
  if (K == "gemini") for (j = 1; j <= NGS; j++) { s = GS[j]; gsub(/@S@/, cl(V["sessionId"]), s); print s }
  if (LMT != "") print LMT
  if (K == "ocdb") nx = (MAXW == "") ? OFF : MAXW
  else if (LIM < 0) nx = SZ
  else nx = OFF + POS
  c = ""
  if (K == "codex") c = cS US cM US cE US cC US NEWF US cT US cA
  else if (K == "ocdb") c = OCP US OCS
  else if (K == "pi") c = cS US cM US cE US cC
  else if (K == "gemini") c = MAXTS
  else if (K == "geminil") c = cS US cF
  else if (K == "agyt") c = AGP US AGPI US AGG US AGW US AGLM
  print "f\t" FP "\t" f0(nx) "\t" ((c == "") ? "-" : cl(c)) "\t" NLINES "\t" NREC
}
"##;

/// The listing → work list join. Known files (K lines) come first, then the
/// listing (A, L, x lines). Unchanged files are counted, changed ones go out
/// as `1 <mtime> …` work lines (sorted newest first by the script), the rest
/// as `0 <line>` (passed straight through).
const JOIN_AWK: &str = r##"
BEGIN { FS = "\t"; OFS = "\t" }
$1 == "K" {
  kp[$2] = 1; kd[$2] = $3; ki[$2] = $4; ks[$2] = $5; km[$2] = $6; ko[$2] = $7; kc[$2] = $8
  if ($4 != "0") bi[$3 ":" $4] = $2
  next
}
$1 == "L" {
  ag = $2; k = $3; dv = $4; ino = $5; sz = $6; mt = $7; p = $8
  if (p == "" || (p in seen)) next
  seen[p] = 1; nl[ag]++
  zst = (p ~ /\.zst$/)
  whole = (k == "gemini" || k == "ocjson" || zst)
  if (zst && Z == "") { nz[ag]++; next }
  if (k == "ocdb" && Q == "") { print "0", "x", ag, p, "needs sqlite3"; next }
  q = p
  if (!(q in kp) && ino != "0" && ((dv ":" ino) in bi)) {
    q = bi[dv ":" ino]
    if (!(q in seen)) { seen[q] = 1; print "0", "M", q, p }
  }
  st = 0; c = "-"
  if (q in kp) {
    same = (ino == "0" || (ki[q] == ino && kd[q] == dv))
    if (k == "ocdb") { st = ko[q]; c = kc[q] }
    else if (whole) {
      if (same && ks[q] == sz && km[q] == mt) { nu[ag]++; next }
      # a rewritten chat: read it all, but its reader knows what it gave before
      if (k == "gemini") c = kc[q]
    }
    else if (same && sz + 0 == ko[q] + 0) { nu[ag]++; next }
    else if (same && sz + 0 > ko[q] + 0) { st = ko[q]; c = kc[q] }
    else print "0", "R", p
  }
  if (c == "") c = "-"
  nt[ag]++
  print "1", mt, ag, k, st, sz, dv, ino, c, p
  next
}
$1 == "A" || $1 == "N" || $1 == "x" { print "0", $0; next }
END {
  for (p in kp) if (!(p in seen)) print "0", "V", p
  for (a in nl) print "0", "C", a, nl[a], nu[a] + 0, nt[a] + 0, nz[a] + 0
  print "0", "W"
}
"##;

/// `stat` output (`dev ino size mtime path`) → L lines.
const STAT_AWK: &str = r##"{ if (!match($0, /^[0-9]+ [0-9]+ [0-9]+ [0-9]+ /)) next; split($0, a, " "); print "L\t" ag "\t" k "\t" a[1] "\t" a[2] "\t" a[3] "\t" a[4] "\t" substr($0, RLENGTH + 1) }"##;

const SCRIPT: &str = r##"# aleph usage extractor (`aleph usage script` prints it). POSIX sh + awk.
# It reads the coding agents' own session logs on this machine and prints one
# tab-separated line per API request: ids, timestamps, model, effort, token
# counts, cost and the folder — never any message text. Only bytes added since
# the last run are read (the offsets at the end), within a time and size
# budget; what's left waits for the next run. Nothing is written here.
export LC_ALL=C
TAB=$(printf '\t')
US=$(printf '\037')
TMAX=@TMAX@
BMAX=@BMAX@
DAYS=@DAYS@
AGENTS=" @AGENTS@ "
CROOTS='@CROOTS@'
XHOMES='@XHOMES@'
T0=$(date +%s)
RD='@READ_AWK@'
JOIN='@JOIN_AWK@'
STATAWK='@STAT_AWK@'

on() { case $AGENTS in *" $1 "*) return 0 ;; esac; return 1; }

# the login environment: PATH (sqlite3, zstd) and where agents keep their logs
loginenv() {
  E=$("${SHELL:-/bin/sh}" -lc env </dev/null 2>/dev/null)
  lenv() { printf '%s\n' "$E" | sed -n "s/^$1=//p" | head -n 1; }
  [ -n "$CLAUDE_CONFIG_DIR" ] || CLAUDE_CONFIG_DIR=$(lenv CLAUDE_CONFIG_DIR)
  [ -n "$CODEX_HOME" ] || CODEX_HOME=$(lenv CODEX_HOME)
  [ -n "$PI_CODING_AGENT_DIR" ] || PI_CODING_AGENT_DIR=$(lenv PI_CODING_AGENT_DIR)
  [ -n "$XDG_DATA_HOME" ] || XDG_DATA_HOME=$(lenv XDG_DATA_HOME)
  p=$(lenv PATH); [ -n "$p" ] && PATH="$PATH:$p"
  PATH="$PATH:$HOME/.local/bin:$HOME/bin:/opt/homebrew/bin:/usr/local/bin"; export PATH
}

# lst <reader> <agent> <folder> <find tests…>: one L line per log file
lst() {
  k=$1; ag=$2; d=$3; shift 3
  [ -d "$d" ] || return 1
  d=$(cd "$d" 2>/dev/null && pwd -P) || return 1
  printf 'A\t%s\t%s\t%s\n' "$ag" "$k" "$d"
  case $ST in
    gnu) find "$d" "$@" -type f $MT -exec stat -c '%d %i %s %Y %n' {} + ;;
    bsd) find "$d" "$@" -type f $MT -exec stat -f '%d %i %z %m %N' {} + ;;
    *) find "$d" "$@" -type f $MT -print | while IFS= read -r f; do printf '0 0 %s 0 %s\n' "$(wc -c <"$f" | tr -d ' ')" "$f"; done ;;
  esac 2>/dev/null | awk -v k="$k" -v ag="$ag" "$STATAWK"
  return 0
}
statf() {
  case $ST in
    gnu) stat -c '%d %i %s %Y %n' "$3" ;;
    bsd) stat -f '%d %i %z %m %N' "$3" ;;
    *) printf '0 0 %s 0 %s\n' "$(wc -c <"$3" | tr -d ' ')" "$3" ;;
  esac 2>/dev/null | awk -v k="$1" -v ag="$2" "$STATAWK"
}

# one folder per line: ~ expanded, empty ones dropped
dirs() { tr ',' '\n' | while IFS= read -r d; do case $d in "~"*) d="$HOME${d#\~}" ;; esac; [ -n "$d" ] && printf '%s\n' "$d"; done; }

# Claude Code's config folders: $CLAUDE_CONFIG_DIR (a comma list), the
# machine's claude_roots, the defaults, and any ~/.claude-* or ~/.config/claude-*
# beside them (a second login, started as CLAUDE_CONFIG_DIR=… from an alias)
croots() {
  { printf '%s\n' "$CLAUDE_CONFIG_DIR"; printf '%s\n' "$CROOTS"; } | dirs
  printf '%s\n' "$HOME/.config/claude" "$HOME/.claude"
  find "$HOME" -maxdepth 1 -type d -name '.claude?*' 2>/dev/null
  find "$HOME/.config" -maxdepth 1 -type d -name 'claude?*' 2>/dev/null
}
# Codex's homes: $CODEX_HOME (else ~/.codex), the machine's codex_homes, any ~/.codex-*
xhomes() {
  printf '%s\n' "${CODEX_HOME:-$HOME/.codex}"
  printf '%s\n' "$XHOMES" | dirs
  find "$HOME" -maxdepth 1 -type d -name '.codex?*' 2>/dev/null
}

# whose a Claude config folder is ($1, its logs at $2): the oauthAccount in its
# .claude.json, i.e. the account's id, email, organisation and plan (no token
# is kept there). None: an API key or a cloud provider.
cacct() {
  a="$1/.claude.json"
  if [ ! -f "$a" ]; then
    case $1 in "$HOME/.claude"|"$HOME/.config/claude") a="$HOME/.claude.json" ;; esac
  fi
  if [ ! -f "$a" ]; then printf 'N\tclaude\t%s\n' "$2"; return; fi
  awk -v r="$2" '
    function val(o, k,   v) {
      if (!match(o, "\"" k "\": *\"[^\"]*\"")) return ""
      v = substr(o, RSTART, RLENGTH); sub(/^[^:]*: *"/, "", v); sub(/"$/, "", v); gsub(/[\t]/, "", v); return v
    }
    { s = s $0 }
    END {
      # the oauthAccount object alone (its braces counted), so no other key is taken for its own
      j = index(s, "\"oauthAccount\""); o = ""
      if (j > 0) {
        k = j + 14; while (substr(s, k, 1) ~ /[ :\t]/) k++
        if (substr(s, k, 1) == "{") {
          d = 0
          for (i = k; i <= length(s) && i < k + 8000; i++) {
            ch = substr(s, i, 1)
            if (ch == "{") d++
            else if (ch == "}") { d--; if (d == 0) break }
          }
          o = substr(s, k, i - k + 1)
        }
      }
      t = val(o, "organizationRateLimitTier"); if (t == "") t = val(o, "userRateLimitTier")
      printf "N\tclaude\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n", r, val(o, "accountUuid"), val(o, "emailAddress"), val(o, "organizationName"), val(o, "organizationType"), val(o, "billingType"), t
    }' "$a" 2>/dev/null
}

listing() {
  OIFS=$IFS; NL='
'
  if on claude; then
    f=0; seen=
    IFS=$NL
    for c in $(croots); do
      IFS=$OIFS
      r=$(cd "$c/projects" 2>/dev/null && pwd -P) || continue
      case $seen in *"|$r|"*) continue ;; esac
      seen="$seen|$r|"
      lst claude claude "$r" -name '*.jsonl' && f=1 && cacct "$c" "$r"
    done
    IFS=$OIFS
    [ $f = 1 ] || printf 'A\tclaude\tclaude\t-\n'
  fi
  if on codex; then
    f=0; seen=
    IFS=$NL
    for cx in $(xhomes); do
      IFS=$OIFS
      h=$(cd "$cx" 2>/dev/null && pwd -P) || continue
      case $seen in *"|$h|"*) continue ;; esac
      seen="$seen|$h|"
      for s in sessions archived_sessions; do
        lst codex codex "$h/$s" \( -name 'rollout-*.jsonl' -o -name 'rollout-*.jsonl.zst' \) && f=1
      done
    done
    IFS=$OIFS
    [ $f = 1 ] || printf 'A\tcodex\tcodex\t-\n'
  fi
  if on pi; then
    lst pi pi "${PI_CODING_AGENT_DIR:-$HOME/.pi/agent}/sessions" -name '*.jsonl' || printf 'A\tpi\tpi\t-\n'
  fi
  if on omp; then
    lst pi omp "$HOME/.omp/agent/sessions" -name '*.jsonl' || printf 'A\tomp\tpi\t-\n'
  fi
  if on opencode; then
    oc=${XDG_DATA_HOME:-$HOME/.local/share}/opencode; f=0
    if [ -f "$oc/opencode.db" ]; then
      f=1; printf 'A\topencode\tocdb\t%s\n' "$oc"
      statf ocdb opencode "$oc/opencode.db"
    fi
    lst ocjson opencode "$oc/storage/message" -name '*.json' && f=1
    [ $f = 1 ] || printf 'A\topencode\tocdb\t-\n'
  fi
  # Gemini CLI and Qwen Code: tmp/<project>/chats/, JSONL now, one JSON per chat before
  if on gemini; then
    lst geminil gemini "$HOME/.gemini/tmp" -path '*/chats/*' -name '*.jsonl' || printf 'A\tgemini\tgemini\t-\n'
    lst gemini gemini "$HOME/.gemini/tmp" -path '*/chats/*' -name '*.json'
    # whose requests they are: settings.json says how it signs in (security.auth.selectedType;
    # older: selectedAuthType). An API key or Vertex AI bills the key, an account of its own
    # on this machine (the key is never read); a Google sign-in, the email google_accounts.json
    # holds (the tokens live elsewhere, never read)
    r=$(cd "$HOME/.gemini/tmp" 2>/dev/null && pwd -P) && {
      ga=$(awk '{ s = s $0 } END { if (match(s, /"(selectedType|selectedAuthType)": *"[^"]*"/)) { v = substr(s, RSTART, RLENGTH); sub(/^[^:]*: *"/, "", v); sub(/"$/, "", v); gsub(/[^A-Za-z0-9_.-]/, "", v); print v } }' "$HOME/.gemini/settings.json" 2>/dev/null)
      case $ga in
        *api-key*|*vertex*) printf 'N\tgemini\t%s\t\t\t\t\t%s\t\n' "$r" "$ga" ;;
        *) [ -f "$HOME/.gemini/google_accounts.json" ] &&
             awk -v r="$r" -v ga="$ga" '{ s = s $0 } END { if (match(s, /"active": *"[^"]*"/)) { v = substr(s, RSTART, RLENGTH); sub(/^[^:]*: *"/, "", v); sub(/"$/, "", v); gsub(/[\t]/, "", v); if (v != "") printf "N\tgemini\t%s\t%s\t%s\t\t\t%s\t\n", r, v, v, ga } }' "$HOME/.gemini/google_accounts.json" 2>/dev/null ;;
      esac
    }
  fi
  if on agy; then
    lst agyt agy "$HOME/.gemini/antigravity-cli/brain" -path '*/.system_generated/logs/transcript.jsonl' || printf 'A\tagy\tagyt\t-\n'
    # it signs in with Google (its tokens in the keyring, never read): the email
    # google_accounts.json holds, shared with Gemini CLI
    r=$(cd "$HOME/.gemini/antigravity-cli/brain" 2>/dev/null && pwd -P) && [ -f "$HOME/.gemini/google_accounts.json" ] &&
      awk -v r="$r" '{ s = s $0 } END { if (match(s, /"active": *"[^"]*"/)) { v = substr(s, RSTART, RLENGTH); sub(/^[^:]*: *"/, "", v); sub(/"$/, "", v); gsub(/[\t]/, "", v); if (v != "") printf "N\tagy\t%s\t%s\t%s\t\t\tgoogle\t\n", r, v, v } }' "$HOME/.gemini/google_accounts.json" 2>/dev/null
  fi
  if on qwen; then
    lst qwen qwen "$HOME/.qwen/tmp" -path '*/chats/*' -name '*.jsonl' || printf 'A\tqwen\tqwen\t-\n'
    lst gemini qwen "$HOME/.qwen/tmp" -path '*/chats/*' -name '*.json'
  fi
}

# opencode.db rows changed after the watermark (ms), assistant messages only
ocdb() {
  w=$2; [ "$w" -gt 0 ] 2>/dev/null || w=$CUT
  t=$($SQL -readonly -batch -noheader "$1" "select group_concat(name) from sqlite_master where type='table' and name in ('message','session_message');" 2>&1 </dev/null)
  case ",$t," in
    *,session_message,*) ocdb2 "$@"; return ;;
    *,message,*) ;;
    *) printf 'x\topencode\t%s\tno message table: %s\n' "$1" "$(printf '%s' "$t" | tr '\t\n' '  ' | cut -c1-160)"
       return ;;
  esac
  $SQL -readonly -batch -noheader -separator "$TAB" -cmd '.timeout 3000' "$1" "select m.id, m.session_id, m.time_created, m.time_updated, json_extract(m.data,'\$.modelID'), json_extract(m.data,'\$.providerID'), json_extract(m.data,'\$.variant'), json_extract(m.data,'\$.path.cwd'), json_extract(m.data,'\$.tokens.input'), json_extract(m.data,'\$.tokens.output'), json_extract(m.data,'\$.tokens.reasoning'), json_extract(m.data,'\$.tokens.cache.read'), json_extract(m.data,'\$.tokens.cache.write'), json_extract(m.data,'\$.cost') from message m where m.time_updated > $w and json_extract(m.data,'\$.role') = 'assistant' order by m.time_updated limit 20000;" 2>&1 </dev/null
  # the timeline: prompts (user messages), tool calls (parts), subagents (child sessions);
  # watermarks of their own in the ctx (parts, sessions), and no complaint where a table is missing
  pw=$(printf '%s' "$3" | cut -d"$US" -f1); [ "$pw" -gt 0 ] 2>/dev/null || pw=$CUT
  sw=$(printf '%s' "$3" | cut -d"$US" -f2); [ "$sw" -gt 0 ] 2>/dev/null || sw=$CUT
  $SQL -readonly -batch -noheader -separator "$TAB" -cmd '.timeout 3000' "$1" "select 'U', m.id, m.session_id, m.time_created, m.time_updated from message m where m.time_updated > $w and json_extract(m.data,'\$.role') = 'user' order by m.time_updated limit 20000;" 2>/dev/null </dev/null
  $SQL -readonly -batch -noheader -separator "$TAB" -cmd '.timeout 3000' "$1" "select 'T', p.id, p.session_id, json_extract(p.data,'\$.tool'), json_extract(p.data,'\$.callID'), json_extract(p.data,'\$.state.status'), json_extract(p.data,'\$.state.time.start'), json_extract(p.data,'\$.state.time.end'), p.time_updated from part p where p.time_updated > $pw and json_extract(p.data,'\$.type') = 'tool' order by p.time_updated limit 20000;" 2>/dev/null </dev/null
  $SQL -readonly -batch -noheader -separator "$TAB" -cmd '.timeout 3000' "$1" "select 'S', s.id, s.parent_id, s.time_created, s.time_updated from session s where s.parent_id is not null and s.time_updated > $sw order by s.time_updated limit 5000;" 2>/dev/null </dev/null
}

# OpenCode 2: sessions in session_v2, their messages in session_message, a type per
# row (user; assistant = one request, its tool calls inside; idle = a prompt ended;
# compaction). Old messages were copied over with their ids, so a record read from
# the old tables is the same record. Rows print in the old reader's columns.
ocdb2() {
  w=$2; [ "$w" -gt 0 ] 2>/dev/null || w=$CUT
  sw=$(printf '%s' "$3" | cut -d"$US" -f2); [ "$sw" -gt 0 ] 2>/dev/null || sw=$CUT
  $SQL -readonly -batch -noheader -separator "$TAB" -cmd '.timeout 3000' "$1" "select m.id, m.session_id, m.time_created, m.time_updated, json_extract(m.data,'\$.model.id'), json_extract(m.data,'\$.model.providerID'), json_extract(m.data,'\$.model.variant'), s.directory, json_extract(m.data,'\$.tokens.input'), json_extract(m.data,'\$.tokens.output'), json_extract(m.data,'\$.tokens.reasoning'), json_extract(m.data,'\$.tokens.cache.read'), json_extract(m.data,'\$.tokens.cache.write'), json_extract(m.data,'\$.cost') from session_message m left join session_v2 s on s.id = m.session_id where m.type = 'assistant' and m.time_updated > $w order by m.time_updated limit 20000;" 2>&1 </dev/null
  $SQL -readonly -batch -noheader -separator "$TAB" -cmd '.timeout 3000' "$1" "select 'U', m.id, m.session_id, m.time_created, m.time_updated, case when m.type = 'synthetic' then 's' when s.parent_id is not null then 'a' else 'u' end from session_message m left join session_v2 s on s.id = m.session_id where m.type in ('user','synthetic') and m.time_updated > $w order by m.time_updated limit 20000;" 2>/dev/null </dev/null
  $SQL -readonly -batch -noheader -separator "$TAB" -cmd '.timeout 3000' "$1" "select 'T', json_extract(c.value,'\$.id'), m.session_id, json_extract(c.value,'\$.name'), json_extract(c.value,'\$.id'), json_extract(c.value,'\$.state.status'), json_extract(c.value,'\$.time.created'), json_extract(c.value,'\$.time.completed'), m.time_updated from session_message m, json_each(m.data,'\$.content') c where m.type = 'assistant' and m.time_updated > $w and json_extract(c.value,'\$.type') = 'tool' order by m.time_updated limit 50000;" 2>/dev/null </dev/null
  $SQL -readonly -batch -noheader -separator "$TAB" -cmd '.timeout 3000' "$1" "select 'I', i.id, i.session_id, i.time_created, i.time_updated, (select max(u.time_created) from session_message u where u.session_id = i.session_id and u.type = 'user' and u.seq < i.seq), json_extract(i.data,'\$.outcome') from session_message i where i.type = 'idle' and i.time_updated > $w order by i.time_updated limit 20000;" 2>/dev/null </dev/null
  $SQL -readonly -batch -noheader -separator "$TAB" -cmd '.timeout 3000' "$1" "select 'C', c.id, c.session_id, c.time_created, c.time_updated from session_message c where c.type = 'compaction' and c.time_updated > $w order by c.time_updated limit 5000;" 2>/dev/null </dev/null
  $SQL -readonly -batch -noheader -separator "$TAB" -cmd '.timeout 3000' "$1" "select 'S', s.id, s.parent_id, s.time_created, s.time_updated, s.agent, (select json_extract(c.value,'\$.id') from session_message m, json_each(m.data,'\$.content') c where m.session_id = s.parent_id and m.type = 'assistant' and json_extract(c.value,'\$.state.metadata.sessionId') = s.id limit 1) from session_v2 s where s.parent_id is not null and s.time_updated > $sw order by s.time_updated limit 5000;" 2>/dev/null </dev/null
}

# Antigravity CLI: the conversation database beside a transcript, as hex for the reader —
# the requests not matched yet (each one's model and thinking tokens) and, once, the
# conversation (its folder); read only, never its text
agymeta() {
  [ -n "$SQL" ] || return 0
  id=${1%/.system_generated/*}; id=${id##*/}
  db="$HOME/.gemini/antigravity-cli/conversations/$id.db"
  [ -f "$db" ] || return 0
  # the carried state (`-`: none; cut -s, so a state of one field reads as none either)
  c=$2; [ "$c" = - ] && c=
  g=$(printf '%s' "$c" | cut -s -d"$US" -f3); [ "$g" -gt 2 ] 2>/dev/null || g=2
  q="select 'PBH', idx, hex(data) from gen_metadata where idx >= $((g - 2)) order by idx;"
  [ -n "$(printf '%s' "$c" | cut -s -d"$US" -f4)" ] || q="$q select 'PBW', 0, hex(data) from trajectory_metadata_blob;"
  # read only; a database with no reader open has no -shm to share: as it is on disk then
  for m in 'mode=ro' 'mode=ro&immutable=1'; do
    o=$($SQL -batch -noheader -separator "$TAB" -cmd '.timeout 2000' "file:$db?$m" "$q" 2>/dev/null </dev/null) && { [ -n "$o" ] && printf '%s\n' "$o"; return 0; }
  done
}

# the bytes of a log from an offset (compressed rollouts whole)
src() {
  case $1 in
    *.zst) $ZSTD -dc "$1" 2>/dev/null ;;
    *) if [ "$2" -gt 0 ]; then tail -c +$(($2 + 1)) "$1" 2>/dev/null; else cat "$1" 2>/dev/null; fi ;;
  esac
}

readall() {
  bud=$BMAX; nf=0; nb=0; df=0; dfb=0; why=
  while IFS= read -r line; do
    case $line in
      "0$TAB"*) printf '%s\n' "${line#0"$TAB"}"; continue ;;
    esac
    OIFS=$IFS; IFS=$TAB; set -- $line; IFS=$OIFS
    mt=$2; ag=$3; k=$4; st=$5; sz=$6; dv=$7; ino=$8; ctx=$9; p=${10}
    whole=0; case $k in gemini|ocjson) whole=1 ;; esac; case $p in *.zst) whole=1 ;; esac
    want=$((sz - st)); [ $whole = 1 ] && want=$sz; [ "$k" = ocdb ] && want=0
    if [ -z "$why" ] && [ $(($(date +%s) - T0)) -ge "$TMAX" ]; then why=time; fi
    if [ -z "$why" ] && [ "$k" != ocdb ] && [ "$bud" -le 0 ]; then why=bytes; fi
    if [ -z "$why" ] && [ $whole = 1 ] && [ "$nb" -gt 0 ] && [ "$want" -gt "$bud" ]; then why=bytes; fi
    if [ -n "$why" ]; then df=$((df + 1)); dfb=$((dfb + want)); continue; fi
    lim=$want; [ "$lim" -gt "$bud" ] && lim=$bud; [ $whole = 1 ] && lim=-1
    printf 'F\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$ag" "$k" "$p" "$dv" "$ino" "$sz" "$mt" "$st"
    if [ "$k" = ocdb ]; then
      ocdb "$p" "$st" "$ctx" | CTX=$ctx FP=$p SZ=$sz awk -v K=ocdb -v OFF="$st" -v LIM=-1 "$RD"
    elif [ "$k" = agyt ]; then
      { agymeta "$p" "$ctx"; src "$p" "$st"; } | CTX=$ctx FP=$p SZ=$sz awk -v K=agyt -v OFF="$st" -v LIM="$lim" "$RD"
    else
      src "$p" "$st" | CTX=$ctx FP=$p SZ=$sz awk -v K="$k" -v OFF="$st" -v LIM="$lim" "$RD"
    fi
    nf=$((nf + 1))
    if [ $whole = 1 ]; then nb=$((nb + want)); bud=$((bud - want)); else nb=$((nb + lim)); bud=$((bud - lim)); fi
  done
  [ -n "$why" ] && printf 'B\t%s\t%s\t%s\n' "$why" "$df" "$dfb"
  printf 'E\t%s\t%s\t%s\n' "$(date +%s)" "$nf" "$nb"
}

# Claude Code's plan windows as it cached them in .claude.json (the 5-hour
# and weekly use, when they reset, the plan tier, whose account they are):
# numbers and the account's id only (Aleph keeps a hash of it)
plans() {
  on claude || return 0
  # only where Claude Code itself is installed: an editor extension's copy
  # (~/.vscode-server/…) leaves a .claude.json behind but isn't an agent here
  if ! command -v claude >/dev/null 2>&1 && [ ! -x "$HOME/.local/bin/claude" ] && [ ! -x "$HOME/.claude/local/claude" ]; then
    printf 'Q\tclaude\n'; return 0
  fi
  seen=
  for c in $(printf '%s' "$CLAUDE_CONFIG_DIR" | tr ',' ' ') "$HOME"; do
    f="$c/.claude.json"
    [ -f "$f" ] || continue
    case $seen in *"|$f|"*) continue ;; esac
    seen="$seen|$f|"
    awk '
      function val(o, k, q,   v) {
        if (!match(o, "\"" k "\": *" (q ? "\"[^\"]*\"" : "[0-9.]+"))) return ""
        v = substr(o, RSTART, RLENGTH); sub(/^[^:]*: */, "", v); gsub(/"/, "", v); return v
      }
      function win(u, k, m,   j, o) {
        j = index(u, "\"" k "\""); if (j == 0) return "\t\t\t"
        o = substr(u, j + length(k) + 2); sub(/^[ :]*/, "", o)
        if (substr(o, 1, 1) != "{") return "\t\t\t"
        o = substr(o, 1, index(o, "}"))
        return val(o, "utilization", 0) "\t" m "\t" val(o, "resets_at", 1) "\t"
      }
      { s = s $0 }
      END {
        i = index(s, "\"cachedUsageUtilization\"")
        # no reading of its own yet: still say whose account and plan (so machines on one account show together)
        if (i == 0) {
          j = index(s, "\"oauthAccount\""); if (j == 0) exit
          o = substr(s, j)
          printf "P\tclaude\t0\tclaude\t%s\t\t\t\t\t\t\t\t\t%s\n", val(o, "organizationRateLimitTier", 1), val(o, "accountUuid", 1)
          exit
        }
        u = substr(s, i)
        printf "P\tclaude\t%s\tclaude\t%s\t%s\t%s\t%s\n", val(u, "fetchedAtMs", 0), val(s, "organizationRateLimitTier", 1), win(u, "five_hour", 300), win(u, "seven_day", 10080), val(u, "accountUuid", 1)
      }' "$f" 2>/dev/null
  done
}

known() {
cat <<'ALEPH_KNOWN'
@KNOWN@
ALEPH_KNOWN
}

main() {
  set -f
  loginenv
  renice -n 19 -p $$ >/dev/null 2>&1
  command -v ionice >/dev/null 2>&1 && ionice -c 3 -p $$ >/dev/null 2>&1
  ZSTD=; command -v zstd >/dev/null 2>&1 && ZSTD=zstd
  SQL=; command -v sqlite3 >/dev/null 2>&1 && SQL=sqlite3
  ST=
  if stat -c '%d %i %s %Y %n' / >/dev/null 2>&1; then ST=gnu
  elif stat -f '%d %i %z %m %N' / >/dev/null 2>&1; then ST=bsd; fi
  MT=; CUT=0
  if [ "$DAYS" -gt 0 ]; then MT="-mtime -$DAYS"; CUT=$(((T0 - DAYS * 86400) * 1000)); fi
  printf 'H\t%s\t%s\t%s\t%s\t%s\t%s\n' "$(uname -n 2>/dev/null)" "$HOME" "$T0" "${ST:-none}" "${ZSTD:+zstd}" "${SQL:+sqlite3}"
  plans
  { known; listing; } | awk -v Z="$ZSTD" -v Q="$SQL" "$JOIN" | sort -t "$TAB" -k1,1 -k2,2nr | readall
}
main </dev/null
"##;

/// What the script needs to know about a file from last time.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Known {
    pub path: String,
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime: u64,
    /// next byte to read (opencode.db: the `time_updated` watermark, ms)
    pub off: u64,
    /// reader state carried between runs (Codex: session, model, effort, …)
    pub ctx: String,
}

/// Knobs for one run.
#[derive(Clone, Debug)]
pub struct Opts {
    /// agents to look for (the reader ids in [`READERS`])
    pub agents: Vec<String>,
    /// files changed in the last N days (0: everything)
    pub days: u64,
    /// don't start another file after this many seconds
    pub max_seconds: u64,
    /// read at most this many bytes
    pub max_bytes: u64,
    /// more Claude Code config folders and Codex homes there (`~/…` allowed)
    pub claude_roots: Vec<String>,
    pub codex_homes: Vec<String>,
}

impl Default for Opts {
    fn default() -> Opts {
        Opts {
            agents: READERS.iter().map(|s| s.to_string()).collect(),
            days: 90,
            max_seconds: 20,
            max_bytes: 64 << 20,
            claude_roots: Vec::new(),
            codex_homes: Vec::new(),
        }
    }
}

fn safe(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c == '\t' || c == '\n' || c == '\r' {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// Folders for the script's `'…'` lists: no quotes, no shell, no newlines.
fn folders(v: &[String]) -> String {
    v.iter()
        .map(|d| d.trim())
        .filter(|d| {
            !d.is_empty()
                && !d
                    .chars()
                    .any(|c| matches!(c, '\'' | '\\' | '$' | '`' | ',' | '"') || c.is_control())
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The script for one machine: budgets, agents and last run's offsets filled in.
pub fn script(o: &Opts, known: &[Known]) -> String {
    let mut k = String::new();
    for f in known {
        // a path can't hold a newline here, and the heredoc ends at its marker
        if f.path.contains('\n') || f.path.contains('\t') || f.path.is_empty() {
            continue;
        }
        k.push_str(&format!(
            "K\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            f.path,
            f.dev,
            f.ino,
            f.size,
            f.mtime,
            f.off,
            if f.ctx.is_empty() {
                "-".to_string()
            } else {
                safe(&f.ctx)
            }
        ));
    }
    let agents: Vec<&str> = o
        .agents
        .iter()
        .map(String::as_str)
        .filter(|a| READERS.contains(a))
        .collect();
    SCRIPT
        .replace("@READ_AWK@", READ_AWK)
        .replace("@JOIN_AWK@", JOIN_AWK)
        .replace("@STAT_AWK@", STAT_AWK)
        .replace("@TMAX@", &o.max_seconds.max(1).to_string())
        .replace("@BMAX@", &o.max_bytes.max(1).to_string())
        .replace("@DAYS@", &o.days.to_string())
        .replace("@AGENTS@", &agents.join(" "))
        .replace("@CROOTS@", &folders(&o.claude_roots))
        .replace("@XHOMES@", &folders(&o.codex_homes))
        .replace("@KNOWN@\n", &k)
}

/// One file's part of a run.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FileRun {
    pub agent: String,
    pub reader: String,
    pub path: String,
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime: u64,
    pub start: u64,
    /// set by the `f` trailer: the file was read to here (None: cut off)
    pub next: Option<u64>,
    pub ctx: String,
    pub lines: u64,
    pub records: Vec<parse::Record>,
    pub limits: Vec<parse::Limit>,
    /// the accounts its records named (Codex's own id, `p:<provider>`): id → as the log said it
    pub accts: std::collections::BTreeMap<String, String>,
    /// what the agent did, for the timeline (prompts, tool calls, …)
    pub steps: Vec<super::steps::Step>,
}

/// Per agent: what the listing found.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentSeen {
    pub agent: String,
    /// log folders found (empty: none)
    pub roots: Vec<String>,
    pub listed: u64,
    pub unchanged: u64,
    pub to_read: u64,
    pub zst_skipped: u64,
}

/// Everything one run printed.
#[derive(Clone, Debug, Default)]
pub struct Output {
    /// what went wrong in a run that still printed something (`read_problem`)
    pub problem: Option<String>,
    pub hostname: String,
    pub home: String,
    pub now: u64,
    pub has_zstd: bool,
    pub has_sqlite: bool,
    pub agents: Vec<AgentSeen>,
    pub files: Vec<FileRun>,
    pub gone: Vec<String>,
    pub moved: Vec<(String, String)>,
    pub reset: Vec<String>,
    /// (agent, path, why)
    pub skipped: Vec<(String, String, String)>,
    pub listing_done: bool,
    /// (why, files left, bytes left)
    pub budget: Option<(String, u64, u64)>,
    pub ended: bool,
    pub bytes_read: u64,
    /// (agent, limit): plan windows from an agent's own settings (`P`)
    pub plans: Vec<(String, parse::Limit)>,
    /// agents not installed there (`Q`): a plan read from their settings before goes
    pub no_plan: Vec<String>,
    /// whose each log folder is (`N`): Claude's config folders, Gemini's sign-in
    pub accounts: Vec<RootAccount>,
}

/// A log folder's account, as the agent's own settings say it (`N`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RootAccount {
    pub agent: String,
    /// the folder its logs are in (the `A` line's realpath)
    pub root: String,
    /// the agent's id for the account ("" none: an API key, a cloud provider)
    pub id: String,
    pub email: String,
    pub org: String,
    pub org_type: String,
    pub billing: String,
    pub tier: String,
}

fn u(s: &str) -> u64 {
    s.trim().parse().unwrap_or(0)
}

/// Parse a run's output. `host` labels the records.
pub fn parse_output(text: &str, host: &str) -> Output {
    let mut o = Output::default();
    let mut cur: Option<FileRun> = None;
    let mut agent_of: HashMap<String, String> = HashMap::new();
    let finish = |o: &mut Output, f: Option<FileRun>| {
        if let Some(mut f) = f {
            // a step with no time of its own (a subagent's link, from its meta file): the file's first
            let first = f
                .steps
                .iter()
                .map(|s| s.ms)
                .filter(|m| *m > 0)
                .chain(f.records.iter().map(|r| r.ts * 1000))
                .min();
            for s in f.steps.iter_mut().filter(|s| s.ms == 0) {
                s.ms = first.unwrap_or(o.now * 1000);
            }
            o.files.push(f);
        }
    };
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        match f[0] {
            "H" if f.len() >= 7 => {
                o.hostname = f[1].to_string();
                o.home = f[2].to_string();
                o.now = u(f[3]);
                o.has_zstd = f[5] == "zstd";
                o.has_sqlite = f[6] == "sqlite3";
            }
            "A" if f.len() >= 4 => {
                let a = match o.agents.iter_mut().position(|x| x.agent == f[1]) {
                    Some(i) => &mut o.agents[i],
                    None => {
                        o.agents.push(AgentSeen {
                            agent: f[1].to_string(),
                            ..Default::default()
                        });
                        o.agents.last_mut().unwrap()
                    }
                };
                if f[3] != "-" && !a.roots.iter().any(|r| r == f[3]) {
                    a.roots.push(f[3].to_string());
                }
            }
            "C" if f.len() >= 6 => {
                if let Some(a) = o.agents.iter_mut().find(|x| x.agent == f[1]) {
                    a.listed = u(f[2]);
                    a.unchanged = u(f[3]);
                    a.to_read = u(f[4]);
                    a.zst_skipped = f.get(5).map(|s| u(s)).unwrap_or(0);
                }
            }
            "F" if f.len() >= 9 => {
                let prev = cur.take();
                finish(&mut o, prev);
                agent_of.insert(f[3].to_string(), f[1].to_string());
                cur = Some(FileRun {
                    agent: f[1].to_string(),
                    reader: f[2].to_string(),
                    path: f[3].to_string(),
                    dev: u(f[4]),
                    ino: u(f[5]),
                    size: u(f[6]),
                    mtime: u(f[7]),
                    start: u(f[8]),
                    ..Default::default()
                });
            }
            "u" | "t" => {
                if let Some(c) = cur.as_mut() {
                    if let Some(r) = parse::from_wire(line, host, &c.agent) {
                        if !r.acct.is_empty() {
                            if let Some(raw) = f.get(16) {
                                c.accts
                                    .entry(r.acct.clone())
                                    .or_insert_with(|| raw.trim().to_string());
                            }
                        }
                        c.records.push(r);
                    }
                }
            }
            "p" | "ks" | "ke" | "c" | "e" | "sl" => {
                if let Some(c) = cur.as_mut() {
                    if let Some(s) = super::steps::from_wire(line, host, &c.agent) {
                        c.steps.push(s);
                    }
                }
            }
            "l" => {
                if let (Some(c), Some(l)) = (cur.as_mut(), parse::limit_from_wire(line)) {
                    c.limits.push(l);
                }
            }
            "Q" if f.len() >= 2 => o.no_plan.push(f[1].to_string()),
            "N" if f.len() >= 3 => {
                let g = |i: usize| f.get(i).map(|s| s.trim().to_string()).unwrap_or_default();
                o.accounts.push(RootAccount {
                    agent: g(1),
                    root: g(2),
                    id: g(3),
                    email: g(4),
                    org: g(5),
                    org_type: g(6),
                    billing: g(7),
                    tier: g(8),
                });
            }
            "P" if f.len() >= 5 => {
                // no windows: the account and plan alone (no reading there yet), at time 0 so any reading wins
                let read = parse::limit_from_wire(&format!("l\t{}", f[2..].join("\t")));
                let bare = || parse::Limit {
                    id: f[3].to_string(),
                    plan: f[4].to_string(),
                    ..Default::default()
                };
                if let Some(mut l) =
                    read.or_else(|| f.get(13).filter(|a| !a.trim().is_empty()).map(|_| bare()))
                {
                    if f[1] == "claude" {
                        l.plan = parse::claude_plan(&l.plan);
                    }
                    l.source = "cache".into();
                    // whose: one account on two machines is one limit (only its hash is kept)
                    if let Some(a) = f.get(13).map(|a| a.trim()).filter(|a| !a.is_empty()) {
                        l.account = parse::hash(&[f[1], a]);
                    }
                    o.plans.push((f[1].to_string(), l));
                }
            }
            "f" if f.len() >= 6 => {
                if let Some(c) = cur.as_mut() {
                    if c.path == f[1] {
                        c.next = Some(u(f[2]));
                        c.ctx = if f[3] == "-" {
                            String::new()
                        } else {
                            f[3].to_string()
                        };
                        c.lines = u(f[4]);
                    }
                }
                let prev = cur.take();
                finish(&mut o, prev);
            }
            "x" if f.len() >= 4 => {
                // a reader's own complaint belongs to its file
                o.skipped
                    .push((f[1].to_string(), f[2].to_string(), f[3..].join(" ")));
            }
            "R" if f.len() >= 2 => o.reset.push(f[1].to_string()),
            "M" if f.len() >= 3 => o.moved.push((f[1].to_string(), f[2].to_string())),
            "V" if f.len() >= 2 => o.gone.push(f[1].to_string()),
            "W" => o.listing_done = true,
            "B" if f.len() >= 4 => o.budget = Some((f[1].to_string(), u(f[2]), u(f[3]))),
            "E" if f.len() >= 4 => {
                o.ended = true;
                o.bytes_read = u(f[3]);
            }
            _ => {}
        }
    }
    let last = cur.take();
    finish(&mut o, last);
    o
}

#[cfg(test)]
pub(crate) mod tests {

    #[test]
    fn the_awk_programs_hold_no_single_quote() {
        // they go into the script inside '…': one apostrophe (a comment's
        // "call's") ends the quoting and breaks every reader
        for (name, awk) in [
            ("READ_AWK", READ_AWK),
            ("JOIN_AWK", JOIN_AWK),
            ("STAT_AWK", STAT_AWK),
        ] {
            let bad: Vec<(usize, &str)> = awk
                .lines()
                .enumerate()
                .filter(|(_, l)| l.contains('\''))
                .map(|(n, l)| (n + 1, l))
                .collect();
            assert!(bad.is_empty(), "{name} has a single quote: {bad:?}");
        }
    }

    #[test]
    fn plans_and_their_absence() {
        let out = parse_output(
            "P\tclaude\t1792737920957\tclaude\tdefault_claude_max_20x\t13\t300\t2026-10-23T10:59:59Z\t\t44\t10080\t\t\tacct-1\nQ\tcodex\n",
            "h",
        );
        let (agent, l) = &out.plans[0];
        assert_eq!(
            (agent.as_str(), l.plan.as_str(), l.source.as_str()),
            ("claude", "Max 20x", "cache")
        );
        assert_eq!(l.windows.len(), 2);
        assert_eq!(l.account, parse::hash(&["claude", "acct-1"]));
        assert_eq!(
            out.no_plan,
            ["codex"],
            "not installed there: its old plan goes"
        );
    }

    use super::*;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    pub(crate) fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/usage")
    }

    /// Run one reader over a file the way the script does; its output lines.
    pub(crate) fn read(
        kind: &str,
        file: &Path,
        off: u64,
        lim: i64,
        ctx: &str,
        home: &str,
    ) -> Vec<String> {
        let data = std::fs::read(file).unwrap();
        let size = data.len();
        let mut c = Command::new("awk")
            .args(["-v", &format!("K={kind}"), "-v", &format!("OFF={off}")])
            .args(["-v", &format!("LIM={lim}"), READ_AWK])
            .env("LC_ALL", "C")
            .env("HOME", home)
            .env("FP", file)
            .env("SZ", size.to_string())
            .env("CTX", if ctx.is_empty() { "-" } else { ctx })
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("awk");
        let tail = data[off as usize..].to_vec();
        let mut si = c.stdin.take().unwrap();
        std::thread::spawn(move || {
            let _ = si.write_all(&tail);
        });
        let o = c.wait_with_output().unwrap();
        String::from_utf8(o.stdout)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn recs(lines: &[String], agent: &str) -> Vec<parse::Record> {
        lines
            .iter()
            .filter_map(|l| parse::from_wire(l, "h", agent))
            .collect()
    }

    fn steps_of(lines: &[String], agent: &str) -> Vec<crate::usage::steps::Step> {
        lines
            .iter()
            .filter_map(|l| crate::usage::steps::from_wire(l, "h", agent))
            .collect()
    }

    /// The steps of a reader's lines as `(kind, name, class)`, no text anywhere.
    fn shape(lines: &[String], agent: &str) -> Vec<(crate::usage::steps::Kind, String, String)> {
        let all = lines.join("\n");
        assert!(!all.contains("SECRET"), "conversation text leaked:\n{all}");
        steps_of(lines, agent)
            .into_iter()
            .map(|s| (s.kind, s.name, s.class))
            .collect()
    }

    #[test]
    fn claude_steps_prompts_tools_compactions_and_a_subagent() {
        use crate::usage::steps::Kind::*;
        let dir = fixtures().join("claude/projects/-home-u-shop");
        let out = read(
            "claude",
            &dir.join("7a7a0000-0000-4000-8000-000000000001.jsonl"),
            0,
            -1,
            "",
            "/home/u",
        );
        let st = steps_of(&out, "claude");
        assert_eq!(
            shape(&out, "claude"),
            vec![
                (Prompt, "u".into(), String::new()),
                (ToolStart, "Bash".into(), "tool".into()),
                (ToolEnd, "err".into(), String::new()),
                (Compact, String::new(), String::new()),
                (Prompt, "a".into(), String::new()),
            ],
            "{out:#?}"
        );
        // the call's start and end share a key; times to the ms; the meta prompt isn't one
        assert_eq!(st[1].key, st[2].key);
        assert_eq!(st[2].ms - st[1].ms, 4_500);
        assert_eq!((st[3].n1, st[3].n2), (150_000, 30_000));
        // the request still counts as usage
        assert_eq!(recs(&out, "claude").len(), 1);
        // a subagent: its lane (from its file), what started it (its meta file), its own prompt
        let sub = dir.join("3f2a9c1e-0b7d-4c55-9a61-2e8f0c4d7a10/subagents/agent-a1b2c3.jsonl");
        let out = read("claude", &sub, 0, -1, "", "/home/u");
        let st = steps_of(&out, "claude");
        let lane = st.iter().find(|s| s.kind == Lane).expect("its lane");
        assert_eq!(
            (lane.lane.as_str(), lane.name.as_str()),
            ("a1b2c3", "Explore")
        );
        assert_eq!(lane.session, "3f2a9c1e-0b7d-4c55-9a61-2e8f0c4d7a10");
        assert_eq!(lane.class, parse::hash(&["claude", "toolu_T0"]));
        assert!(st
            .iter()
            .any(|s| s.kind == Prompt && s.name == "a" && s.lane == "a1b2c3"));
        assert!(recs(&out, "claude").iter().all(|r| r.lane == "a1b2c3"));
        assert!(!out.join("").contains("SECRET"));
        // read again from the middle: no second lane line
        let later = read("claude", &sub, 10, -1, "", "/home/u");
        assert!(!steps_of(&later, "claude").iter().any(|s| s.kind == Lane));
    }

    #[test]
    fn codex_steps() {
        use crate::usage::steps::Kind::*;
        let f = fixtures().join("codex/sessions/2026/10/22/rollout-2026-10-22T09-00-00-019a0001-aaaa-7bbb-8ccc-000000000001.jsonl");
        let out = read("codex", &f, 0, -1, "", "/home/u");
        let sh = shape(&out, "codex");
        assert!(sh.contains(&(Prompt, "u".into(), String::new())), "{sh:?}");
        assert!(
            sh.contains(&(ToolStart, "shell".into(), "tool".into())),
            "{sh:?}"
        );
        assert!(
            sh.contains(&(ToolStart, "web_search".into(), "web".into())),
            "{sh:?}"
        );
        assert!(
            sh.contains(&(Error, "interrupted".into(), String::new())),
            "{sh:?}"
        );
        assert!(sh.iter().any(|x| x.0 == Compact), "{sh:?}");
        let st = steps_of(&out, "codex");
        let start = st
            .iter()
            .find(|s| s.kind == ToolStart && s.name == "shell")
            .unwrap();
        // two ends for one call: its output, then the item with its exit code and time (the last wins)
        let ends: Vec<_> = st
            .iter()
            .filter(|s| s.kind == ToolEnd && s.key == start.key)
            .collect();
        assert_eq!(ends.len(), 2);
        assert_eq!((ends[1].name.as_str(), ends[1].n1), ("err", 2_000));
    }

    #[test]
    fn codex_0164_helper_threads_are_lanes_of_their_parent() {
        use crate::usage::steps::Kind::*;
        let dir = std::env::temp_dir().join(format!("ch-codex-guard-{}", std::process::id()));
        let f = dir.join("sessions/2026/10/26/rollout-2026-10-26T15-04-34-019a0003-cab0-7012-87d0-000000000003.jsonl");
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(
            &f,
            concat!(
                r#"{"timestamp":"2026-10-26T09:34:34.672Z","type":"session_meta","payload":{"session_id":"019a0003-ca6e-73a2-a75b-000000000001","id":"019a0003-cab0-7012-87d0-000000000003","parent_thread_id":"019a0003-ca6e-73a2-a75b-000000000001","cwd":"/home/u/px","originator":"codex-tui","cli_version":"0.164.0","source":{"subagent":{"other":"guardian"}},"thread_source":"guardian_review","base_instructions":"SECRET"}}"#,
                "\n",
                r#"{"timestamp":"2026-10-26T09:34:35.000Z","type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}"#,
                "\n"
            ),
        )
        .unwrap();
        let out = read("codex", &f, 0, -1, "", "/home/u");
        let lane = steps_of(&out, "codex").into_iter().find(|s| s.kind == Lane);
        let lane = lane.unwrap_or_else(|| panic!("no lane in {out:?}"));
        assert_eq!(lane.session, "019a0003-ca6e-73a2-a75b-000000000001");
        assert_eq!(lane.lane, "019a0003-cab0-7012-87d0-000000000003");
        assert_eq!(lane.name, "guardian");
        assert!(!out.join("\n").contains("SECRET"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pi_and_gemini_steps() {
        use crate::usage::steps::Kind::*;
        let f = std::fs::read_dir(fixtures().join("pi/sessions/--home-u-notes--"))
            .unwrap()
            .flatten()
            .next()
            .unwrap()
            .path();
        let out = read("pi", &f, 0, -1, "", "/home/u");
        let sh = shape(&out, "pi");
        assert_eq!(sh.iter().filter(|x| x.0 == ToolStart).count(), 2, "{sh:?}");
        assert!(sh.contains(&(ToolEnd, "err".into(), String::new())));
        assert!(sh.contains(&(Prompt, "u".into(), String::new())));
        assert!(sh.iter().any(|x| x.0 == Compact));
        assert_eq!(
            recs(&out, "pi").len(),
            2,
            "a tool call with no tokens adds no request"
        );
        let g = std::fs::read_dir(fixtures().join("gemini/tmp"))
            .unwrap()
            .flatten()
            .next()
            .unwrap()
            .path()
            .join("chats/session-2026-10-22T08-30-b2c3d4e5.jsonl");
        let out = read("geminil", &g, 0, -1, "", "/home/u");
        let sh = shape(&out, "gemini");
        assert!(
            sh.contains(&(ToolStart, "run_shell_command".into(), "tool".into())),
            "{sh:?}"
        );
        assert!(
            sh.contains(&(ToolStart, "google_web_search".into(), "web".into())),
            "{sh:?}"
        );
        assert!(
            sh.contains(&(ToolEnd, "err".into(), String::new())),
            "{sh:?}"
        );
        assert!(sh.contains(&(Prompt, "u".into(), String::new())), "{sh:?}");
    }

    fn trailer(lines: &[String]) -> Vec<String> {
        lines
            .iter()
            .find(|l| l.starts_with("f\t"))
            .expect("a trailer")
            .split('\t')
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn no_single_quotes_in_the_embedded_awk() {
        for (name, p) in [("read", READ_AWK), ("join", JOIN_AWK), ("stat", STAT_AWK)] {
            assert!(!p.contains('\''), "{name} awk has a single quote");
        }
        let s = script(&Opts::default(), &[]);
        assert!(!s.contains("@READ_AWK@") && !s.contains("@KNOWN@") && !s.contains("@TMAX@"));
    }

    #[test]
    fn script_is_posix_sh() {
        let k = vec![Known {
            path: "/home/u/.claude/projects/p/a b.jsonl".into(),
            size: 10,
            off: 10,
            ..Default::default()
        }];
        let s = script(&Opts::default(), &k);
        for sh in ["sh", "dash", "bash"] {
            let Ok(mut c) = Command::new(sh)
                .arg("-n")
                .stdin(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
            else {
                continue;
            };
            c.stdin.take().unwrap().write_all(s.as_bytes()).unwrap();
            let o = c.wait_with_output().unwrap();
            assert!(
                o.status.success(),
                "{sh} -n: {}",
                String::from_utf8_lossy(&o.stderr)
            );
        }
    }

    /// Every awk program the script runs: the readers, and the short ones
    /// inside its shell functions (`awk '…'`).
    fn awk_programs() -> Vec<String> {
        let mut v = vec![
            READ_AWK.to_string(),
            JOIN_AWK.to_string(),
            STAT_AWK.to_string(),
        ];
        let mut rest = SCRIPT;
        while let Some(i) = rest.find("awk ") {
            let after = &rest[i + 4..];
            let eol = after.find('\n').unwrap_or(after.len());
            let Some(q) = after[..eol].find('\'') else {
                rest = after;
                continue;
            };
            let body = &after[q + 1..];
            let Some(j) = body.find('\'') else { break };
            v.push(body[..j].to_string());
            rest = &body[j + 1..];
        }
        v
    }

    /// The awks on this computer, as commands. `ALEPH_REQUIRE_AWKS` (a space
    /// list, set in CI) fails the test when one of them isn't there.
    fn awks() -> Vec<Vec<&'static str>> {
        let all: [&[&'static str]; 4] = [&["awk"], &["mawk"], &["gawk"], &["busybox", "awk"]];
        let works = |c: &[&str]| {
            Command::new(c[0])
                .args(&c[1..])
                .arg("BEGIN { exit 0 }")
                .stdin(Stdio::null())
                .output()
                .is_ok_and(|o| o.status.success())
        };
        let found: Vec<Vec<&str>> = all
            .iter()
            .filter(|c| works(c))
            .map(|c| c.to_vec())
            .collect();
        for want in std::env::var("ALEPH_REQUIRE_AWKS")
            .unwrap_or_default()
            .split_whitespace()
        {
            assert!(
                found.iter().any(|c| c[0] == want),
                "{want} is required here (ALEPH_REQUIRE_AWKS) but isn't installed"
            );
        }
        found
    }

    #[test]
    fn awk_programs_parse() {
        // every awk here must at least load (mawk / BusyBox reject what gawk forgives)
        let programs = awk_programs();
        assert!(programs.len() > 3, "the script's own awk programs");
        for awk in awks() {
            for p in &programs {
                let o = Command::new(awk[0])
                    .args(&awk[1..])
                    .args(["-v", "K=claude", "-v", "LIM=-1", p])
                    .stdin(Stdio::null())
                    .output()
                    .unwrap();
                assert!(
                    o.status.success(),
                    "{awk:?}: {}\nin the program starting: {}",
                    String::from_utf8_lossy(&o.stderr),
                    p.chars().take(120).collect::<String>()
                );
            }
        }
    }

    /// The names a program defines as functions, and each definition's
    /// parameters. A hand reading, enough for how these programs are written.
    fn awk_functions(p: &str) -> Vec<(String, Vec<String>)> {
        let mut out = Vec::new();
        let mut rest = p;
        while let Some(i) = rest.find("function ") {
            let after = &rest[i + 9..];
            let (Some(open), Some(close)) = (after.find('('), after.find(')')) else {
                break;
            };
            if open < close {
                let params = after[open + 1..close]
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                out.push((after[..open].trim().to_string(), params));
            }
            rest = &after[close.max(open) + 1..];
        }
        out
    }

    #[test]
    fn no_awk_parameter_is_named_like_a_function() {
        // this Mac's awk takes it; mawk, every Linux machine's, refuses the
        // whole program, and no log gets read there at all
        for p in awk_programs() {
            let funcs = awk_functions(&p);
            let names: Vec<&str> = funcs.iter().map(|(n, _)| n.as_str()).collect();
            for (f, params) in &funcs {
                for a in params {
                    assert!(
                        !names.contains(&a.as_str()),
                        "function {f}: its parameter {a} is also a function"
                    );
                }
            }
        }
    }

    #[test]
    fn the_function_reading_finds_parameters() {
        let f = awk_functions("function a(x, y,   z) { }\n  function b() { return 1 }");
        assert_eq!(f[0].0, "a");
        assert_eq!(f[0].1, vec!["x", "y", "z"]);
        assert_eq!((f[1].0.as_str(), f[1].1.len()), ("b", 0));
    }

    #[test]
    fn claude_reader() {
        let f = fixtures()
            .join("claude/projects/-home-u-shop/3f2a9c1e-0b7d-4c55-9a61-2e8f0c4d7a10.jsonl");
        let out = read("claude", &f, 0, -1, "", "/home/u");
        let r = recs(&out, "claude");
        let u: Vec<&parse::Record> = r.iter().filter(|r| !r.is_turn()).collect();
        // msg_A has three streamed lines (the last one counts), msg_B one, the
        // synthetic message and the user line with a Task result none
        assert_eq!(u.len(), 3, "{out:#?}");
        let a = u
            .iter()
            .find(|r| r.key == parse::hash(&["claude", "msg_A:req_A"]))
            .unwrap();
        assert_eq!((a.input, a.output, a.cache_read), (3, 120, 4000));
        assert_eq!((a.cache_write_5m, a.cache_write_1h), (200, 1000));
        assert_eq!(a.reasoning, 40);
        assert_eq!(a.model, "claude-opus-5-5");
        assert_eq!(a.effort, "xhigh");
        assert_eq!(a.project, "~/shop");
        assert_eq!(a.session, "3f2a9c1e-0b7d-4c55-9a61-2e8f0c4d7a10");
        let b = u
            .iter()
            .find(|r| r.key == parse::hash(&["claude", "msg_B:req_B"]))
            .unwrap();
        // no per-TTL split: everything is a 5-minute write; fast mode; web search
        assert_eq!((b.cache_write_5m, b.cache_write_1h), (500, 0));
        assert!(b.fast() && b.web == 2);
        let old = u
            .iter()
            .find(|r| r.model == "claude-sonnet-4-20250514")
            .unwrap();
        assert_eq!(old.cost, Some(0.0042), "older Claude Code wrote costUSD");
        let t: Vec<&parse::Record> = r.iter().filter(|r| r.is_turn()).collect();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].turn_ms, 64_000);
        // nothing from the conversation leaks into the output
        let all = out.join("\n");
        assert!(!all.contains("SECRET"), "{all}");
        let tr = trailer(&out);
        assert_eq!(tr[2], std::fs::metadata(&f).unwrap().len().to_string());
    }

    #[test]
    fn claude_subagent_lines_are_sidechains() {
        let f = fixtures().join(
            "claude/projects/-home-u-shop/3f2a9c1e-0b7d-4c55-9a61-2e8f0c4d7a10/subagents/agent-a1b2c3.jsonl",
        );
        let r = recs(&read("claude", &f, 0, -1, "", "/home/u"), "claude");
        assert_eq!(r.len(), 1);
        assert!(r[0].sidechain());
        assert_eq!(r[0].model, "claude-haiku-4-5-20251001");
        assert_eq!(r[0].session, "3f2a9c1e-0b7d-4c55-9a61-2e8f0c4d7a10");
    }

    #[test]
    fn incomplete_last_line_waits() {
        let dir = std::env::temp_dir().join(format!("aleph-usage-partial-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = fixtures()
            .join("claude/projects/-home-u-shop/3f2a9c1e-0b7d-4c55-9a61-2e8f0c4d7a10.jsonl");
        let full = std::fs::read_to_string(src).unwrap();
        // cut the file in the middle of its last line: that line isn't consumed
        let cut = full.trim_end().rfind('\n').unwrap() + 1 + 20;
        let f = dir.join("s.jsonl");
        std::fs::write(&f, &full[..cut]).unwrap();
        let out = read("claude", &f, 0, cut as i64, "", "/home/u");
        let tr = trailer(&out);
        let next: usize = tr[2].parse().unwrap();
        assert_eq!(next, cut - 20, "stops before the partial line");
        // the rest of the file, from that offset, completes it
        std::fs::write(&f, &full).unwrap();
        let out2 = read(
            "claude",
            &f,
            next as u64,
            (full.len() - next) as i64,
            "",
            "/home/u",
        );
        assert_eq!(trailer(&out2)[2], full.len().to_string());
        let n1 = recs(&out, "claude").len();
        let n2 = recs(&out2, "claude").len();
        let all = recs(&read("claude", &f, 0, -1, "", "/home/u"), "claude").len();
        assert!(n1 + n2 >= all && n2 >= 1, "{n1} + {n2} vs {all}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_reader_prefers_per_response_records() {
        let f = fixtures().join("codex/sessions/2026/10/22/rollout-2026-10-22T09-00-00-019a0001-aaaa-7bbb-8ccc-000000000001.jsonl");
        let out = read("codex", &f, 0, -1, "", "/home/u");
        let r = recs(&out, "codex");
        let u: Vec<&parse::Record> = r.iter().filter(|r| !r.is_turn()).collect();
        // two token_usage_record lines (one of them a compaction request); the
        // token_count events of the same responses don't count again
        assert_eq!(u.len(), 2, "{out:#?}");
        let first = u[0];
        assert_eq!(
            (first.input, first.cache_read, first.output, first.reasoning),
            (1000, 9000, 300, 120)
        );
        assert_eq!(first.model, "gpt-5.1-codex");
        assert_eq!(first.effort, "high");
        assert_eq!(first.project, "~/api");
        assert_eq!(first.session, "019a0001-aaaa-7bbb-8ccc-000000000001");
        let t: Vec<&parse::Record> = r.iter().filter(|r| r.is_turn()).collect();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].turn_ms, 95_000);
        let l: Vec<parse::Limit> = out
            .iter()
            .filter_map(|l| parse::limit_from_wire(l))
            .collect();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].windows[0].used_percent, 12.5);
        assert_eq!(l[0].plan, "pro");
        // the state for next time: session, model, effort, folder, new format
        let tr = trailer(&out);
        let ctx: Vec<&str> = tr[3].split('\u{1f}').collect();
        assert_eq!(
            ctx[..4],
            [
                "019a0001-aaaa-7bbb-8ccc-000000000001",
                "gpt-5.1-codex",
                "high",
                "/home/u/api"
            ]
        );
        assert_eq!(ctx[4], "1");
    }

    #[test]
    fn codex_reader_old_logs_and_carried_state() {
        let f = fixtures().join("codex/sessions/2026/10/21/rollout-2026-10-21T18-30-00-019a0002-aaaa-7bbb-8ccc-000000000002.jsonl");
        let out = read("codex", &f, 0, -1, "", "/home/u");
        let u = recs(&out, "codex");
        // three token_count events, one a repeat of the running total
        assert_eq!(u.len(), 2, "{out:#?}");
        assert_eq!(u[1].input, 2000 - 1500);
        assert_eq!(u[1].effort, "medium");
        // reading on from the middle keeps the model and drops the repeat
        let text = std::fs::read_to_string(&f).unwrap();
        let mid = text.find("\"total_tokens\":4200").unwrap();
        let off = text[..mid].rfind('\n').unwrap() + 1;
        let first = read("codex", &f, 0, off as i64, "", "/home/u");
        let ctx = trailer(&first)[3].clone();
        let rest = read(
            "codex",
            &f,
            off as u64,
            (text.len() - off) as i64,
            &ctx,
            "/home/u",
        );
        let r2 = recs(&rest, "codex");
        assert_eq!(r2.len(), 1, "{rest:#?}");
        assert_eq!(r2[0].model, "gpt-5-codex");
        assert_eq!(r2[0].session, "019a0002-aaaa-7bbb-8ccc-000000000002");
    }

    #[test]
    fn pi_reader() {
        let f = fixtures().join("pi/sessions/--home-u-notes--/2026-10-22T08-00-00-000Z_019a0003-aaaa-7bbb-8ccc-000000000003.jsonl");
        let out = read("pi", &f, 0, -1, "", "/home/u");
        let r = recs(&out, "pi");
        assert_eq!(
            r.len(),
            2,
            "aborted message with no usage is skipped: {out:#?}"
        );
        assert_eq!(r[0].model, "claude-sonnet-4-5");
        assert_eq!(r[0].effort, "medium");
        assert_eq!(r[0].project, "~/notes");
        assert_eq!(r[0].cost, Some(0.0213));
        assert_eq!(r[1].model, "gpt-5.1");
        assert_eq!(r[1].effort, "high");
        assert_eq!(
            (r[1].input, r[1].cache_read, r[1].cache_write_5m),
            (800, 2000, 0)
        );
    }

    #[test]
    fn pi_session_from_the_file_name() {
        // OMP writes Pi-style sessions; without a typed header line the id
        // comes from <timestamp>_<session id>.jsonl
        let name = "2026-10-22T08-00-00-000Z_019a0003-aaaa-7bbb-8ccc-000000000003.jsonl";
        let text =
            std::fs::read_to_string(fixtures().join("pi/sessions/--home-u-notes--").join(name))
                .unwrap();
        let body: String = text.lines().skip(1).map(|l| format!("{l}\n")).collect();
        let dir = std::env::temp_dir().join(format!("aleph-usage-omp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join(name);
        std::fs::write(&f, body).unwrap();
        let r = recs(&read("pi", &f, 0, -1, "", "/home/u"), "omp");
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].session, "019a0003-aaaa-7bbb-8ccc-000000000003");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gemini_reader() {
        let f = fixtures().join("gemini/tmp/7d1e4c0a9b2f3e6d8c5a4b3f2e1d0c9b8a7f6e5d4c3b2a1f0e9d8c7b6a5f4e3d/chats/session-2026-10-22T07-15-a1b2c3d4.json");
        let out = read("gemini", &f, 0, -1, "", "/home/u");
        let r = recs(&out, "gemini");
        assert_eq!(r.len(), 2, "{out:#?}");
        // input includes cached; thoughts are output; tool prompts are input
        assert_eq!(
            (r[0].input, r[0].cache_read, r[0].output, r[0].reasoning),
            (1200 - 200 + 30, 200, 450 + 50, 50)
        );
        assert_eq!(r[0].model, "gemini-2.5-pro");
        assert_eq!(r[0].session, "a1b2c3d4-0000-4000-8000-000000000004");
        assert_eq!(r[0].project, "", "a hashed project folder says nothing");
        let tr = trailer(&out);
        assert_eq!(tr[2], std::fs::metadata(&f).unwrap().len().to_string());
        // the newest message it gave is the state: a rewritten chat read again
        // gives only that one and what came after (the store keeps one copy)
        assert_eq!(tr[3], "2026-10-22T07:16:30.000Z");
        let again = recs(&read("gemini", &f, 0, -1, &tr[3], "/home/u"), "gemini");
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].model, "gemini-2.5-flash");
        let before = recs(
            &read("gemini", &f, 0, -1, "2026-10-22T07:00:00.000Z", "/home/u"),
            "gemini",
        );
        assert_eq!(before.len(), 2);
    }

    #[test]
    fn gemini_jsonl_reader() {
        let f = fixtures().join("gemini/tmp/7d1e4c0a9b2f3e6d8c5a4b3f2e1d0c9b8a7f6e5d4c3b2a1f0e9d8c7b6a5f4e3d/chats/session-2026-10-22T08-30-b2c3d4e5.jsonl");
        let out = read("geminil", &f, 0, -1, "", "/home/u");
        let r = recs(&out, "gemini");
        // the message line written before its tokens doesn't count; the one after does
        assert_eq!(r.len(), 1, "{out:#?}");
        assert_eq!(
            (r[0].input, r[0].cache_read, r[0].output, r[0].reasoning),
            (3000 - 1000, 1000, 600 + 120, 120)
        );
        let sid = "b2c3d4e5-0000-4000-8000-000000000005";
        assert_eq!(r[0].session, sid);
        assert_eq!(r[0].key, parse::hash(&["gemini", &format!("{sid}:n-0002")]));
        assert_eq!(r[0].model, "gemini-2.5-pro");
        // the session id travels to the next read (the metadata line is first)
        let tr = trailer(&out);
        assert!(tr[3].starts_with(sid), "{tr:?}");
        assert!(!out.join("\n").contains("SECRET"));
        // read on from the middle: the session is still known
        let text = std::fs::read_to_string(&f).unwrap();
        let off = text
            .find("{\"$set\":{\"lastUpdated\":\"2026-10-22T08:30:20")
            .unwrap();
        let rest = read(
            "geminil",
            &f,
            off as u64,
            (text.len() - off) as i64,
            &tr[3],
            "/home/u",
        );
        let r2 = recs(&rest, "gemini");
        assert_eq!((r2.len(), r2[0].session.as_str()), (1, sid));
    }

    #[test]
    fn opencode_message_files() {
        let f = fixtures().join("opencode/storage/message/ses_5a1b/msg_7c2d.json");
        let r = recs(&read("ocjson", &f, 0, -1, "", "/home/u"), "opencode");
        assert_eq!(r.len(), 1);
        assert_eq!(
            (r[0].input, r[0].output, r[0].reasoning, r[0].cache_read),
            (6471, 149 + 56, 56, 128)
        );
        assert_eq!(r[0].model, "claude-sonnet-4-5");
        assert_eq!(r[0].cost, Some(0.0213));
        assert_eq!(r[0].project, "~/site");
    }

    #[test]
    fn qwen_jsonl_reader() {
        let f = fixtures()
            .join("qwen/tmp/-home-u-tool/chats/0c7e2b9a-1111-4222-8333-444455556666.jsonl");
        let r = recs(&read("qwen", &f, 0, -1, "", "/home/u"), "qwen");
        assert_eq!(r.len(), 1);
        assert_eq!(
            (r[0].input, r[0].cache_read, r[0].output),
            (900 - 100, 100, 300)
        );
        assert_eq!(r[0].model, "qwen3-coder-plus");
    }

    /// `USAGE_SCRIPT=file cargo test -- --ignored usage_write_script`: the
    /// extractor with no offsets, for `ssh host sh -s < file`.
    #[test]
    #[ignore]
    fn usage_write_script() {
        if let Ok(p) = std::env::var("USAGE_SCRIPT") {
            std::fs::write(p, script(&Opts::default(), &[])).unwrap();
        }
    }

    /// Run the join over known (K) and listed (L) lines.
    fn join(input: &str, zstd: bool, sqlite: bool) -> Vec<Vec<String>> {
        let mut c = Command::new("awk")
            .args(["-v", if zstd { "Z=zstd" } else { "Z=" }])
            .args(["-v", if sqlite { "Q=sqlite3" } else { "Q=" }, JOIN_AWK])
            .env("LC_ALL", "C")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        c.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        let o = c.wait_with_output().unwrap();
        String::from_utf8(o.stdout)
            .unwrap()
            .lines()
            .map(|l| l.split('\t').map(str::to_string).collect())
            .collect()
    }

    #[test]
    fn join_decides_what_to_read() {
        let k = "K\t/c/same.jsonl\t1\t10\t500\t100\t500\t-\n\
K\t/c/grew.jsonl\t1\t11\t500\t100\t500\tctx1\n\
K\t/c/cut.jsonl\t1\t12\t900\t100\t900\t-\n\
K\t/c/new-inode.jsonl\t1\t13\t500\t100\t500\t-\n\
K\t/c/old-name.jsonl\t1\t14\t500\t100\t500\tctx2\n\
K\t/c/gone.jsonl\t1\t15\t500\t100\t500\t-\n\
K\t/g/chat.json\t1\t16\t300\t100\t300\t2026-10-22T07:15:30.000Z\n\
K\t/o/opencode.db\t1\t17\t9000\t100\t1792660005000\t-\n";
        let l = "A\tclaude\tclaude\t/c\n\
L\tclaude\tclaude\t1\t10\t500\t200\t/c/same.jsonl\n\
L\tclaude\tclaude\t1\t11\t800\t200\t/c/grew.jsonl\n\
L\tclaude\tclaude\t1\t12\t300\t200\t/c/cut.jsonl\n\
L\tclaude\tclaude\t1\t99\t500\t200\t/c/new-inode.jsonl\n\
L\tclaude\tclaude\t1\t14\t700\t200\t/c/new-name.jsonl\n\
L\tgemini\tgemini\t1\t16\t300\t250\t/g/chat.json\n\
L\tcodex\tcodex\t1\t20\t40\t200\t/x/rollout-a.jsonl.zst\n\
L\topencode\tocdb\t1\t17\t9000\t200\t/o/opencode.db\n";
        let out = join(&format!("{k}{l}"), false, true);
        let work: Vec<&Vec<String>> = out.iter().filter(|f| f[0] == "1").collect();
        let pass: Vec<&Vec<String>> = out.iter().filter(|f| f[0] == "0").collect();
        let start = |p: &str| {
            work.iter()
                .find(|f| f[9] == p)
                .map(|f| (f[4].clone(), f[8].clone()))
        };
        assert_eq!(start("/c/same.jsonl"), None, "unchanged");
        assert_eq!(start("/c/grew.jsonl"), Some(("500".into(), "ctx1".into())));
        assert_eq!(
            start("/c/cut.jsonl"),
            Some(("0".into(), "-".into())),
            "truncated: from the start"
        );
        assert_eq!(
            start("/c/new-inode.jsonl"),
            Some(("0".into(), "-".into())),
            "replaced"
        );
        assert_eq!(
            start("/c/new-name.jsonl"),
            Some(("500".into(), "ctx2".into())),
            "moved: carries on"
        );
        assert_eq!(
            start("/g/chat.json"),
            Some(("0".into(), "2026-10-22T07:15:30.000Z".into())),
            "rewritten JSON: whole, with what it gave before"
        );
        assert_eq!(
            start("/o/opencode.db").map(|s| s.0),
            Some("1792660005000".into()),
            "a watermark"
        );
        assert!(
            start("/x/rollout-a.jsonl.zst").is_none(),
            "no zstd: skipped"
        );
        let has = |t: &[&str]| {
            pass.iter()
                .any(|f| f[1..].iter().map(String::as_str).eq(t.iter().copied()))
        };
        assert!(has(&["R", "/c/cut.jsonl"]) && has(&["R", "/c/new-inode.jsonl"]));
        assert!(has(&["M", "/c/old-name.jsonl", "/c/new-name.jsonl"]));
        assert!(has(&["V", "/c/gone.jsonl"]));
        assert!(!pass
            .iter()
            .any(|f| f[1] == "V" && f[2] == "/c/old-name.jsonl"));
        assert!(has(&["A", "claude", "claude", "/c"]));
        assert!(has(&["C", "codex", "1", "0", "0", "1"]), "{pass:?}");
        assert!(has(&["C", "claude", "5", "1", "4", "0"]), "{pass:?}");
        assert!(has(&["W"]));
        // without sqlite3 the database is skipped, and says why
        let out = join(&format!("{k}{l}"), true, false);
        assert!(out
            .iter()
            .any(|f| f.len() > 4 && f[1] == "x" && f[4] == "needs sqlite3"));
        assert!(out
            .iter()
            .any(|f| f[0] == "1" && f[9] == "/x/rollout-a.jsonl.zst"));
    }

    #[test]
    fn output_parses_into_files_and_commits() {
        let text = "H\tbox\t/home/u\t1792400000\tgnu\tzstd\t\n\
A\tclaude\tclaude\t/home/u/.claude/projects\n\
A\tcodex\tcodex\t-\n\
C\tclaude\t3\t1\t2\t0\n\
V\t/home/u/.claude/projects/p/old.jsonl\n\
M\t/a/old.jsonl\t/a/new.jsonl\n\
W\n\
F\tclaude\tclaude\t/home/u/.claude/projects/p/s.jsonl\t2049\t77\t900\t1792399000\t0\n\
u\t2026-10-22T09:00:00Z\ts\tm1:r1\tclaude-opus-5-5\txhigh\t~/p\t1\t2\t3\t4\t0\t0\t\t\t0\n\
f\t/home/u/.claude/projects/p/s.jsonl\t900\t-\t5\t1\n\
F\tclaude\tclaude\t/home/u/.claude/projects/p/t.jsonl\t2049\t78\t500\t1792399000\t100\n\
u\t2026-10-22T09:01:00Z\ts\tm2:r2\tclaude-opus-5-5\txhigh\t~/p\t1\t2\t3\t4\t0\t0\t\t\t0\n\
B\ttime\t4\t123456\n";
        let o = parse_output(text, "b31");
        assert_eq!(o.hostname, "box");
        assert!(o.has_zstd && !o.has_sqlite && o.listing_done && !o.ended);
        assert_eq!(o.agents.len(), 2);
        assert_eq!(o.agents[0].roots, vec!["/home/u/.claude/projects"]);
        assert_eq!((o.agents[0].listed, o.agents[0].to_read), (3, 2));
        assert!(o.agents[1].roots.is_empty());
        assert_eq!(o.gone, vec!["/home/u/.claude/projects/p/old.jsonl"]);
        assert_eq!(
            o.moved,
            vec![("/a/old.jsonl".to_string(), "/a/new.jsonl".to_string())]
        );
        assert_eq!(o.files.len(), 2);
        assert_eq!(o.files[0].next, Some(900));
        assert_eq!(o.files[0].records.len(), 1);
        assert_eq!(o.files[0].records[0].host, "b31");
        assert_eq!(o.files[1].next, None, "no trailer: not committed");
        assert_eq!(o.budget, Some(("time".to_string(), 4, 123_456)));
    }
}
