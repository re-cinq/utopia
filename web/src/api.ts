import type { SourceKind } from "./sourceKinds";
import { S } from "./i18n";
import { createParser } from "eventsource-parser";

export class ApiError extends Error {
  status: number;
  /** Stable error code from the server (without one, it is an untranslated contract guard and message is already the English sentence) */
  code?: string;
  constructor(status: number, message: string, code?: string) {
    super(message);
    this.status = status;
    this.code = code;
  }
}

/** Body returned when the server rejects a request */
type Refusal = { error?: string; code?: string; detail?: string };

/** The sentence shown to the person for a rejection.
 *
 *  With a code, look it up in i18n. Without one (or when this entry is not in the table yet), fall back to the server's English sentence,
 *  because the server always speaks English and the UI language lives on the client (docs/decisions/0004).
 *  **Plain requests and the chat stream share this one place.** The chat stream used to read only `error`, so
 *  `no_chat_model`, which has a translation, showed as English in the Chinese UI */
function refusalMessage(body: Refusal, fallback: string): string {
  let message = body.error || fallback;
  // code comes from the network, not a literal. This one cast buys full type checking of the err table itself
  const worded = body.code
    ? (S.err as Record<string, string | undefined>)[body.code]
    : undefined;
  if (worded) message = worded;
  if (body.detail) message = S.errDetail(message, body.detail);
  return message;
}

/** The `error` frame in the chat stream. It has the same envelope `{error, code}` as a rejected request and is read by the same function.
 *  If it is not valid JSON, it is the old format's single English sentence, shown as is */
function streamFailure(data: string): string {
  try {
    const body: unknown = JSON.parse(data);
    if (body && typeof body === "object") return refusalMessage(body as Refusal, data);
  } catch { /* Old format: plain text */ }
  return data;
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(path, {
    credentials: "include",
    headers:
      init?.body instanceof FormData
        ? {}
        : { "Content-Type": "application/json" },
    ...init,
  });
  if (!res.ok) {
    // The wording is fixed at this one choke point: the toast.error(e.message) calls in 22 files need no change
    let message = res.statusText;
    let code: string | undefined;
    try {
      const body = (await res.json()) as Refusal;
      code = body.code;
      message = refusalMessage(body, message);
    } catch {
      // Non-JSON response body, keep statusText
    }
    throw new ApiError(res.status, message, code);
  }
  return res.json() as Promise<T>;
}

export interface User {
  id: string;
  org_id: string;
  email: string;
  display_name: string;
  is_admin: boolean;
  created_at: string;
}

/** Personal access token (0014): a long-lived key for MCP clients that acts as the identity of the person who issued it.
 *  Effective permissions = the person's role ∩ scope. An empty `kb_ids` means every knowledge base this person can enter. */
export interface TokenView {
  id: string;
  name: string;
  /** The small visible part (`utp_pat_ab12`), enough to match the string in the config file but not enough to reconstruct it */
  token_prefix: string;
  scope: "read" | "write";
  kb_ids: string[] | null;
  expires_at: string | null;
  last_used_at: string | null;
  /** Revoking stamps the row instead of deleting it */
  revoked_at: string | null;
  created_at: string;
}

export interface Workspace {
  id: string;
  org_id: string;
  name: string;
  created_at: string;
}

/** Preset ontology packs that can be picked when creating a knowledge base (`GET /ontology-packs`). */
export type OntologyPack = {
  id: string;
  name: string;
  summary: string;
  classes: number;
  properties: number;
};

/** Scenario pack (0073): both the name and the description are given in two languages, and the UI picks the one in its own language */
export type ScenarioPack = {
  id: string;
  name: { zh: string; en: string };
  summary: { zh: string; en: string };
  classes: number;
  properties: number;
  questions: number;
};

export interface Kb {
  id: string;
  workspace_id: string;
  name: string;
  kind: string;
  description: string | null;
  visibility: "open" | "restricted";
  /** The deployment's public default space (the first knowledge base created): always open, cannot be deleted */
  is_default: boolean;
  /** Whether extraction may, when it meets a wording outside the ontology, add it to the ontology automatically and rewrite the facts that wait on it.
      Turning this off does not affect "noticing": the unmatched-term stats still accumulate and show, they just become proposals you act on with a click */
  auto_extend_ontology: boolean;
  /** Write inferred facts into the ledger (R1). **Off by default**: inference adds things to the graph,
   *  and a declaration may be wrong, so the graph should not change based on it unless the user has opted in */
  materialize_inferences: boolean;
  /** Automatically queue a round of type resolution once extraction finishes (only the refinements within a subtree that are safe to land automatically) */
  auto_type_resolution: boolean;
  /** Governance switch (0025, off by default): the agent takes repeated pairs that have waited on a person in first-in-first-out order, reads the
   *  person's precedents in the ledger first, then decides. Turning it on starts it, turning it off stops it */
  governance: boolean;
  /** When governance was switched on; the circuit breaker only counts withdrawals after this moment */
  governance_since: string | null;
  /** How often to re-infer (in minutes). Facts keep changing, and relying on manual clicks alone would leave the derivations perpetually stale */
  inference_interval_minutes: number;
  /** When the last inference run finished */
  last_inference_at: string | null;
  /** Which language the built-in ontology is seeded in, and which language new descriptions are written in. **Follows the corpus, not the UI**
      (The UI language lives on the client, see docs/decisions/0004) */
  ontology_lang: "en" | "zh";
  /** The caller's role in this knowledge base (returned only by the detail endpoint): the frontend uses it to gate destructive entry points */
  my_role?: "viewer" | "editor" | "admin" | "owner" | null;
}

/** Account-level "My knowledge bases" row: the KB, my role, join info and overview stats. */
export interface MyKb {
  kb: Kb;
  my_role: "viewer" | "editor" | "admin" | "owner" | null;
  joined_at: string | null;
  added_by_name: string | null;
  doc_count: number;
  member_count: number;
}

/** Audit event (audit display only). */
export interface AuditEvent {
  id: string;
  action: string;
  target_kind: string;
  target_id: string | null;
  detail: Record<string, unknown>;
  /** null = the engine automatically (adjudicator, consistency checks, inference); an id with no name = the account has been removed */
  actor_id: string | null;
  actor_name: string | null;
  created_at: string;
}

export interface KbMember {
  user_id: string;
  email: string;
  display_name: string;
  role: "viewer" | "editor" | "admin";
}

export interface Doc {
  id: string;
  kb_id: string;
  source_id: string | null;
  filename: string;
  mime: string;
  size_bytes: number;
  status: string;
  graph_status: string;
  /** Failure reason of the ingestion pipeline */
  error: string | null;
  /** Failure reason of the graph extraction pipeline (kept apart from error: each of the two pipelines stores its own) */
  graph_error: string | null;
  chunk_count: number;
  /** Document tags. **No UI uses them today**, kept on purpose; the reason is written on
   *  the `tags` column in `migrations/0002_ingest.sql` */
  tags: string[];
  missing_since: string | null;
  /** Tombstone (#268): deleted but kept, can be undone */
  deleted_at: string | null;
  /** Really deleted: the content is gone and cannot come back */
  purged_at: string | null;
  /** Which kind of model must read this file's text when that model is not configured (0040): ocr / transcribe; the document is failed at this point */
  reader_needed: "ocr" | "transcribe" | null;
  created_at: string;
  /** Document date (`#610`): the "date written on the file", parsed from the file name or the first line of the body,
   *  must be shown separately from `created_at` (the upload time). `null` means it was not recognized (`#610` decision 3) */
  doc_time: string | null;
  /** How `doc_time` was derived: `"content"` = file name or body, `"none"` = not recognized.
   *  The upload time is not here, `created_at` is that */
  doc_time_source: "content" | "none";
}

/** An aggregate of one kind of extraction that was dropped within a document: the facts were extracted, but could not land. */
export interface ExtractionDrop {
  document_id: string;
  /** Stable reason code; the frontend looks up the copy by it (attr_domain_mismatch / low_confidence / ...) */
  reason: string;
  /** The specific object under this reason (attribute key, predicate name, "salary@organization") */
  detail: string;
  count: number;
  example: string | null;
}

export interface SourceView {
  id: string;
  kind: SourceKind;
  name: string;
  config: {
    urls?: string[];
    feed_url?: string;
    content_mode?: "feed" | "full_new_items";
    endpoint?: string;
    /** github_issues / github_repo: owner/name */
    repo?: string;
    /** github_issues: a PR is also an issue in the GitHub model, not collected by default */
    include_pull_requests?: boolean;
    /** gitlab_issues: merge requests are not collected by default, same reason as GitHub PRs */
    include_merge_requests?: boolean;
    /** github_repo / gitlab_repo: when given, it replaces the default glob list entirely (markdown; hidden directories, node_modules, vendor) */
    include?: string[];
    exclude?: string[];
    /** jira_issues: the site address, e.g. https://issues.apache.org/jira */
    base_url?: string;
    /** jira_issues: the project key, e.g. KAFKA; gitlab_repo / gitlab_issues: the project path, e.g. group/subgroup/name */
    project?: string;
    /** google_drive: the selected folder (the name is only for display, sync matches on the id only) */
    folders?: { id: string; name: string }[];
    /** google_drive: the connected Google account; a value in `connected_at` means it is connected (tokens are never sent down) */
    account_email?: string | null;
    connected_at?: string;
    /** google_drive: the Utopia user who connected; only this user may browse and pick folders */
    connected_by?: string;
    /** google_drive: the name of the person who connected, used in the confirmation before disconnecting */
    connected_by_name?: string;
    /** false = documents under this source are only retrieved, not extracted (schema documents, 0035 decision 7); defaults to extracted */
    extract?: boolean;
  } | null;
  icon: string | null;
  sync_interval_minutes: number | null;
  sync_cron: string | null;
  /** null = documents do not decay; a setting from after opening (RC-003). Hand-written objects may lack fields, and missing ones take the default */
  decay: Partial<DecaySettings> | null;
  last_sync_at: string | null;
  last_sync_status: "never" | "queued" | "running" | "ok" | "failed";
  last_sync_error: string | null;
  last_sync_added: number;
  /** How far the ticket source has been read (ADR 0072): the update time that was consumed, and at that moment the whole batch had been read.
   *  null = no cursor (first sync round, or this kind of source does not use a cursor). It says "how far was read",
   *  not the "when this round ran" of `last_sync_at` */
  sync_checkpoint: string | null;
  doc_count: number;
  missing_count: number;
  /** The full-text backfill part; **null for sources that are not RSS full-text sources**, so there is no need to infer again from kind and
   *  content_mode whether it applies (0026 / #417) */
  rss_full_content: RssFullContentSummary | null;
}

/** The source's decay setting (RC-003): once over max_items, evict the lowest-scoring documents that are at least min_age_days old. */
export interface DecaySettings {
  max_items: number;
  half_life_days: number;
  stale_after_days: number;
  min_age_days: number;
}

/** Full-text backfill progress for the current generation of an RSS source. The five counts are read together, so they are given together. */
export interface RssFullContentSummary {
  state: "pending" | "active" | "disabled";
  pending: number;
  /** queued and hydrating are merged into one bucket */
  queued: number;
  retrying: number;
  complete: number;
  /** terminal, deleted and superseded are merged into one bucket */
  terminal: number;
}

export interface SearchResult {
  id: string;
  document_id: string;
  seq: number;
  text: string;
  filename: string;
}

/** How far this knowledge base has got (#313). Credentials do not leave the KB: the model item only says whether it is configured */
export interface Readiness {
  has_chat_model: boolean;
  documents: number;
  processing: number;
  failed: number;
  entities: number;
}

export interface LlmSettingsView {
  chat_base_url?: string | null;
  chat_model?: string | null;
  /** Reasoning effort: none | minimal | low | medium | high; empty = endpoint default */
  chat_reasoning_effort?: string | null;
  has_chat_key?: boolean;
  embed_base_url?: string | null;
  embed_model?: string | null;
  embed_dim?: number | null;
  has_embed_key?: boolean;
  /** mineru | ark (0065) */
  ocr_provider?: string;
  ocr_base_url?: string | null;
  ocr_backend?: string | null;
  ocr_model?: string | null;
  has_ocr_key?: boolean;
  transcribe_base_url?: string | null;
  transcribe_model?: string | null;
  has_transcribe_key?: boolean;
}

export interface Member {
  user_id: string;
  email: string;
  display_name: string;
  role: string;
  is_admin: boolean;
}

export interface OrgUser {
  id: string;
  email: string;
  display_name: string;
  is_admin: boolean;
}

/** The account that an identity provider subject is bound to (0056) */
export interface OidcIdentity {
  user_id: string;
  subject: string;
  email: string;
}

/** Data source for question-answering (no credentials sent down, only a host:port/db summary). */
export interface DataSourceView {
  id: string;
  name: string;
  engine: string;
  summary: string;
  created_at: string;
  last_test_at: string | null;
  last_test_ok: boolean | null;
}

export interface GraphNode {
  id: string;
  name: string;
  // null when the type could not be determined (0009)
  type_key: string | null;
  type_label: string | null;
  color: string;
  shape: "circle" | "square";
  degree: number;
  disambiguator: string | null;
}

export interface SyncRun {
  id: string;
  started_at: string;
  finished_at: string | null;
  status: "running" | "ok" | "failed";
  created_docs: number;
  updated_docs: number;
  error: string | null;
}

/** Extraction output of a chunk (the right column of the document viewer). */
export interface ChunkFact {
  chunk_id: string;
  fact_id: string;
  /** A live open-layer statement with a usable phrase, eligible for a regression case. */
  is_open_statement: boolean;
  subject_id: string;
  subject: string;
  /** When the ontology did not recognize this relation, this is the wording from the source text; null if neither can be produced */
  predicate: string | null;
  /** true = the name comes from the source text, not from a relation the ontology recognized */
  inferred: boolean;
  object_id: string | null;
  object: string | null;
  valid_from: string | null;
  valid_to: string | null;
  confidence: number;
}

/** An inferred fact, together with its proof (the "inferred" tier of the entity panel).
 *
 * `premises` is the reason this tier exists: without the premises, a derived edge looks no different from an ordinary edge
 * in the UI, and that is exactly what "inference contaminating knowledge" looks like. */
/** One condition: this attribute, compared this way, against this value */
export interface RuleCondition {
  /** Group number: AND within a group, OR between groups (decision record 0029). Absent = group 0 */
  group?: number;
  predicate_id: string;
  /** gt | gte | lt | lte | between | in | not_in | present */
  op: string;
  /** number / [lo,hi] / string array; present takes no value */
  operand?: unknown;
  predicate_label?: string;
  /** x = rule subject (default); y = the entity reached by the one declared join */
  side?: "x" | "y";
}

export interface RuleInput {
  name: string;
  description?: string;
  subject_type_id: string;
  /** typing = class; attribute = value; relation = an edge to the joined Y */
  conclusion: "typing" | "attribute" | "relation";
  conclude_type_id?: string;
  conclude_predicate_id?: string;
  join_predicate_id?: string;
  conclude_value?: unknown;
  conditions: RuleCondition[];
}

/** An entity flagged by one rule, together with the readings that made it hold */
export interface RuleMatch {
  derived_id: string;
  entity_id: string;
  entity: string;
  concluded: string | null;
  valid_from: string | null;
  valid_to: string | null;
  object_id?: string | null;
  object_entity?: string | null;
  relation_predicate?: string | null;
  /** A readable form such as "total hydrocarbons = 12.3", in premise order */
  premises: string[];
}

export interface BusinessRule extends Omit<RuleInput, "conclusion" | "join_predicate_id"> {
  conclusion: "typing" | "attribute" | "computed" | "relation";
  /** Raw server tree; unsupported nodes must remain read-only. */
  conclude_expr?: unknown;
  id: string;
  enabled: boolean;
  subject_label: string;
  conclude_type_label: string | null;
  conclude_predicate_label: string | null;
  join_predicate_id?: string | null;
  join_predicate_label?: string | null;
  /** The number of conclusions currently holding because of it */
  derived_count: number;
  /** On the last run, the number of entities whose reading combinations were not fully expanded. **Greater than zero means some inferences were missed** */
  capped: number;
  /** Which version the current definition is. Changing the criterion or the conclusion adds one; renaming does not count. Old fixtures lack it, and the UI reads them as version 1 */
  version?: number;
}

/** One version in the history of a rule definition: what it said, from when to when, and how many instances hold under it right now */
export interface RuleVersion {
  id: string;
  seq: number;
  definition: {
    subject_type_id: string;
    conclusion: BusinessRule["conclusion"];
    conclude_type_id: string | null;
    conclude_predicate_id: string | null;
    conclude_value: unknown;
    conclude_expr: unknown;
    join_predicate_id: string | null;
    conditions: { group: number; seq: number; side: string; predicate_id: string; op: string; operand: unknown }[];
  };
  recorded_at: string;
  superseded_at: string | null;
  derived_count: number;
  /** What the classes and predicates named in the definition are called now; renamed or deleted ones cannot be found, and the UI shows the id */
  labels: Record<string, string>;
}

export interface DerivedFact {
  id: string;
  subject_id: string;
  subject: string;
  /** Literal-valued conclusions (classifications and attributes inferred by rules) have no entity object */
  object_id: string | null;
  object: string;
  predicate: string;
  /** Which rule inferred it */
  /** Which rule inferred it. The last two kinds are cross-predicate rules added by 0017 */
  rule: "transitive" | "symmetric" | "inverse" | "sub_property" | "business";
  /** The name of the business rule. Null for axiom inferences, since axioms have no name */
  rule_name?: string | null;
  /** Which version of the business rule definition inferred it. Null for axiom inferences */
  rule_version?: number | null;
  valid_from: string | null;
  valid_to: string | null;
  confidence: number;
  derived_at: string;
  /** Direct premises, in derivation order */
  premises: string[];
}

/** A derivation that was **not landed** (0017 §3): it was inferred, but hit an assertion and was blocked from the graph.
 *  It has no id of its own; it is pointed at by the id of the violation it hit. The panel, ghost edges and the Review card all match on it */
export interface BlockedDerivation {
  violation_id: string;
  subject_id: string;
  subject: string;
  object_id: string;
  object: string;
  predicate: string;
  rule: string;
  via_label: string;
  valid_from: string | null;
  valid_to: string | null;
  against_fact: string;
  against_text: string;
  premises: string[];
}

/** One step of a proof: a premise, together with its evidence (0002 R2).
 *
 * The step's own premises sit one level further down in `premises`, so a proof is a tree,
 * with the same depth limit as inference. For an assertion step, `premises` is empty: its leaves are
 * the source sentences in `evidence`, and there is no need to ask further down
 */
export interface ProofStep {
  seq: number;
  fact_id: string;
  subject_id: string;
  subject: string;
  predicate_id: string | null;
  predicate: string | null;
  object_id: string | null;
  object: string | null;
  valid_from: string | null;
  valid_to: string | null;
  confidence: number;
  /** This premise was later withdrawn; the derivation is then invalid, but the proof must still show what it relied on at the time */
  retracted: boolean;
  evidence: Evidence[];
  /** The step's own premises (0030): sub-proofs expanded by seq. The leaves have empty premises */
  premises: ProofStep[];
}

export interface Proof {
  derived: DerivedFact;
  steps: ProofStep[];
}

/** Tiers of the review page. **These are the same literal values as the server's queue parameter** — a typo gets an explicit unknown_queue error
 *  rather than a silently empty list. */
export type ReviewQueue =
  | "pending"
  | "duplicates"
  | "conflicts"
  | "unconfirmed"
  | "lowconf"
  | "mappings"
  | "violations"
  | "defects"
  // Signatures and category words the aligner's two votes disagree on (#725, 0044 decision 3)
  | "alignment"
  // Actions the erratum agent was blocked by the gate and that await an answer from a person (0044 decision 7)
  | "errata"
  | "merges"
  // Every entry of the agent (0025): suggestions, automatic adjudications and the person's answers
  | "agent";

/** Filter duplicates by the relation of their two types: any = all; same = both sides have types and they are equal; conflict = both have types and they differ */
export type ReviewTypeFilter = "any" | "same" | "conflict";

/** The real count for each tier. The badge in the left column reads this, not the list length */
export interface ReviewCounts {
  /** Facts extracted from memory, waiting for a person's approval (0015) */
  pending: number;
  duplicates: number;
  /** Among duplicates, how many have the same type on both sides versus a type conflict (#428); a side with no type counts in neither tier */
  duplicates_same_type: number;
  duplicates_type_conflict: number;
  conflicts: number;
  unconfirmed: number;
  lowconf: number;
  mappings: number;
  violations: number;
  defects: number;
  /** Signatures and category words the aligner cannot decide (#725) */
  alignment: number;
  /** Actions left to a person by the erratum agent (0044 decision 7) */
  errata: number;
  merges: number;
  /** Suggestions the agent wrote, waiting for an answer (0025) */
  agent: number;
  /** All of the agent's records (for paging through the Agent queue) */
  agent_rows: number;
  /** The govern task of this knowledge base is running right now */
  agent_running: boolean;
  /** Pairs the agent has not yet got to */
  agent_queue: number;
}

/** One suggestion from type resolution: an entity to refine, the profile sent to retrieval, and the candidate classes.
 *
 *  `profile` is returned to the caller on purpose: when retrieval finds nothing, the first thing to check is
 *  "what did we search with", not guess whether the profile or the class description is wrong. */
export interface TypeSuggestion {
  entity_id: string;
  name: string;
  /** The class currently attached, which may be none (0009) */
  coarse: string | null;
  coarse_description: string | null;
  proposed_type: string | null;
  specific_type: string | null;
  fact_count: number;
  profile: string;
  candidates: {
    id: string;
    key: string;
    label: string;
    description: string;
    distance: number;
  }[];
}

/** The result of one round. **Reported in three separate tiers**: what was changed automatically, what is left to a person, and what the adjudicator said was "none of these".
 *  The last tier carries its reason: this step rests on "none of the choices is a decent answer",
 *  and without the reason the biggest tier would be opaque. */
export interface ResolutionOutcome {
  batch: string | null;
  retyped: number;
  for_review: {
    entity_id: string;
    name: string;
    coarse: string | null;
    from_type_id: string | null;
    to_type_id: string;
    choice: string;
    confidence: number;
    reason: string | null;
    /** The chosen class is not in the subtree of the coarse class: the axis of classification was switched, not moved one step down */
    crosses_axis: boolean;
  }[];
  left_alone: {
    name: string;
    coarse: string | null;
    specific_type: string | null;
    reason: string | null;
    top_candidate: string | null;
  }[];
}
export interface ReviewSide {
  id: string;
  name: string;
  // null when the type could not be determined (0009)
  type_label: string | null;
  color: string;
  disambiguator: string | null;
  degree: number;
  top_facts: string[];
}

/** Open suggestions the agent left on a pair (0025); the verdict on the card is an answer to it */
export interface ReviewProposal {
  id: string;
  action: "merge" | "keep" | "unsure";
  confidence: number;
  reason: string | null;
}

export interface ReviewItem {
  id: string;
  score: number;
  reason: string | null;
  stage: "adjudicating" | "human";
  created_at: string;
  left: ReviewSide;
  right: ReviewSide;
  proposal: ReviewProposal | null;
}

/** One entry of the agent (0025): which pair it looked at, what it wanted to do, on what grounds, and how the person answered */
export interface AgentDecision {
  id: string;
  run_id: string;
  target_kind: "review";
  target_id: string;
  action: "merge" | "keep" | "unsure";
  confidence: number;
  reason: string | null;
  /** The precedents it was shown: one entry each for same-pair / same-name / withdrawn; type-pair habits are one summary entry */
  precedents: AgentPrecedent[];
  status: "proposed" | "applied" | "accepted" | "overridden" | "reverted" | "superseded";
  merge_id: string | null;
  /** The one question deferred to a person; only rows marked unsure have it */
  question: string | null;
  /** What the second tier looked at: one query per entry */
  trace: { tool: string; args: Record<string, string>; note: string }[];
  /** The model calls spent by the second tier */
  calls: number;
  created_at: string;
  decided_at: string | null;
  decided_by_name: string | null;
  left: string | null;
  right: string | null;
}

export type AgentPrecedent =
  | {
      family: "same_pair" | "same_name" | "revert";
      event_id: string;
      action: string;
      left: string;
      right: string;
      at: string;
      /** The sentence written when a person decided (0026); decisions before 0026 have none */
      why?: string | null;
    }
  | { family: "type_pair"; merged: number; kept: number; reverted: number };

/** One convention of data mapping: business concept → data asset definition (see docs/decisions/0011).
 *
 * The fields are columns, not keys in JSON: previously this was a `mapped_to` fact,
 * and all of these were stuffed into `object_value`. */
export interface ConceptMapping {
  id: string;
  concept_id: string;
  concept_name: string;
  /** Mounted data sources. The same concept may have different definitions on different sources */
  source: string;
  table_name: string | null;
  expr: string | null;
  sql: string | null;
  unit: string | null;
  summary: string | null;
  /** Derived metric: computed, not a column in the table */
  derived: boolean;
  status: "proposed" | "confirmed" | "rejected";
}
/** The shape of a convention before a change. **A full snapshot rather than a diff** (0006):
 *  when reading, the question is "what was it at that time", which a diff can only answer by replaying from the start */
export interface MappingRevision {
  id: string;
  before: Record<string, unknown>;
  /** The person who changed it; users are soft-deleted, so attribution is not lost when a person leaves */
  changed_by_name: string | null;
  changed_at: string;
}
/** One axiom violation (0002 R0). The criterion comes from the axioms the ontology declares itself; if none are declared, nothing is reported */
/** Only for derived_contradiction (0017): the triple that was inferred. It was never persisted,
 *  so it can only be written out here. Other kinds are `{}` */
export interface ViolationDetail {
  axiom?: "functional" | "inverse_functional" | "asymmetry" | "self_loop";
  rule?: "transitive" | "symmetric" | "inverse" | "sub_property";
  via_label?: string;
  subject?: string;
  predicate?: string;
  object?: string;
  valid_from?: string | null;
  valid_to?: string | null;
  premises?: string[];
}
export type ViolationResolution =
  | "fact_retracted"
  | "fact_closed"
  | "axiom_relaxed"
  | "accepted";
export interface AxiomViolation {
  id: string;
  kind:
    | "self_loop"
    | "asymmetry"
    | "cycle"
    | "functional"
    | "inverse_functional"
    | "signature"
    | "derived_contradiction";
  /** Which relation the criterion came from. When judging "the axiom is written wrong", this is where to go into the ontology to change it */
  predicate: string | null;
  left_fact: string;
  left_text: string;
  /** The reflexive kind is the same as left: one fact contradicts itself */
  right_fact: string;
  right_text: string;
  /** The length of `path`: facts on the cycle, facts in a mutually exclusive group, and premises of a derivation; self-loops and signatures are 0 */
  path_len: number;
  detected_at: string;
  detail: ViolationDetail;
  /** A review lead (0017 §2), given one at a time: an old assertion has no end date, there is a same-name entity,
   *  or the extraction confidence is low. Empty if none */
  hint: "stale" | "duplicate" | "unsure" | null;
  /** Every fact on the cycle (in order), or every fact in the mutually exclusive group (by id); self-loops and signatures are empty.
   *  To retract a fact you must name which one to retract (#202) */
  path: { id: string; text: string }[];
}
/** The ontology's own self-contradiction. **Not the same thing as AxiomViolation**: that one says
 *  "the facts conflict with the definition", this one says "the definition itself does not hold up", which is more fundamental */
/** One entry in the alignment queue (#725): a phrase signature or a category word, which the aligner's two votes disagreed on and a person must decide */
export type AlignmentItem =
  | {
      kind: "phrase";
      id: string;
      phrase: string;
      subject_class: string | null;
      object_class: string | null;
      object_is_value: boolean;
      statement_count: number;
      examples: string[];
      /** The two votes; when there are too many candidates to ask the model, both votes are empty and `reason` explains (0053) */
      votes: {
        first?: AlignmentVote | null;
        second?: AlignmentVote | null;
        reason?: string;
        candidates?: number;
      } | null;
      decided_at: string;
      /** A signature a person has bound but has not yet said which end a single date marks (#966): the attribute and direction bound now.
       *  Both are null for entries whose two votes disagree */
      bound_to: string | null;
      direction: "forward" | "reverse" | null;
    }
  | {
      kind: "kind_word";
      kind_word: string;
      words: string[];
      examples: string[];
      phrases: string[];
      entity_count: number;
      votes: { first?: string | null; second?: string | null } | null;
      decided_at: string;
    }
  | {
      /** A rule of implication proposed by the aligner (0044 decision 3, slice five) */
      kind: "rule";
      id: string;
      trigger: "phrase" | "kind_word";
      phrase: string;
      subject_class: string | null;
      object_class: string | null;
      object_is_value: boolean;
      property: string;
      property_label: string;
      reading: string | null;
      statement_count: number;
      examples: string[];
      votes: { agent?: { property: string; reading: string | null } | null } | null;
      decided_at: string;
    };
/** One entry the erratum agent was blocked by the gate (0044 decision 7): what it wanted to retract, change or add, on which quoted sentence, and why it was left to a person */
export interface ErrataItem {
  id: string;
  document_id: string;
  document: string;
  action: "retract" | "revise" | "add";
  /** The reason for the structural report; empty = seen in the sample */
  flag: "domain" | "range" | "name_absent" | "no_date" | null;
  fact_id: string | null;
  /** The fact the action points at: a retraction targets the one it looked at, a change targets the one it changes to, an addition is the added one */
  proposed: { subject: string; property: string; object: string } | null;
  reason: string;
  quote: string | null;
  /** The gate's reason: `derived 2` / `answered 1` / `contradiction CEO of` (the 0027 format) */
  detail: string | null;
  created_at: string;
}
export interface AlignmentVote {
  property: string;
  direction: "forward" | "reverse";
  /** Under a state attribute, which date a statement carrying only one date marks (start or end); events and constants have none (#966) */
  marks?: AlignmentMarks | null;
}
/** Whether a single date on a state marks the start, the end, or neither (`phrase_bindings.marks`, #966) */
export type AlignmentMarks = "start" | "end" | "none";

export interface OntologyDefect {
  id: string;
  kind:
    | "symmetric_and_asymmetric"
    | "transitive_and_functional"
    | "subclass_cycle"
    | "disjoint_with_ancestor"
    | "inherits_disjoint"
    // The three kinds added by 0017: all on predicates; the first two concern the inverse, the third is a sub-property forming a cycle
    | "inverse_of_itself"
    | "inverse_not_mutual"
    | "sub_property_cycle"
    // 0017: two rules together produce mutually exclusive derivations; reported once per rule pair in aggregate
    | "rules_disagree";
  subject_label: string | null;
  other_label: string | null;
  path_labels: string[];
  detected_at: string;
  detail: DefectDetail;
}
/** rules_disagree only: which two rules, which axiom they collide on, how many pairs, and how many examples */
export interface DefectDetail {
  count?: number;
  rules?: {
    rule_a: string;
    via_a: string;
    rule_b: string;
    via_b: string;
    axiom: string;
    count: number;
    examples: [string, string][];
  }[];
}
export interface FactReviewItem {
  id: string;
  subject_name: string;
  predicate_label: string | null;
  object_name: string | null;
  valid_from: string | null;
  valid_to: string | null;
  confidence: number;
  evidence_count: number;
  quote: string | null;
}

/** One sentence of memory extracted as a fact, waiting for a person's approval (0015). `quote` is the full text of that sentence
 *  The confirm screen puts the source sentence above the triple, so the person judges against the sentence, not the triple in a vacuum. */
export interface PendingFactItem {
  id: string;
  subject_id: string;
  subject_name: string;
  predicate_id: string | null;
  /** The relation name in the ontology; when empty, `proposed_predicate` is shown (italic, marking it as the original wording) */
  predicate_label: string | null;
  proposed_predicate: string | null;
  /** Open statement (0044): the document's own relation phrase; when present there is no predicate, which is not a defect */
  phrase: string | null;
  /** Qualifier recorded by the document's role word: a value or an entity */
  qualifiers: { role: string; value?: unknown; entity_id?: string; entity_name?: string }[] | null;
  /** A time word copied verbatim, not a computed date */
  time_words: { text: string; char_start: number }[] | null;
  object_id: string | null;
  object_name: string | null;
  object_value: { value?: unknown; unit?: string; summary?: string } | null;
  valid_from: string | null;
  valid_to: string | null;
  confidence: number;
  chunk_id: string;
  quote: string;
  proposed_by: string | null;
  proposed_by_name: string | null;
  /** When recorded via MCP, the token name of that agent; empty for chats in the web UI */
  proposed_token_name: string | null;
  created_at: string;
}

/** A temporal conflict (the ones auto-closing is unsure about): the old fact versus the new fact. */
export interface ConflictItem {
  id: string;
  reason: "no_time" | "simultaneous" | "low_confidence";
  created_at: string;
  predicate_label: string;
  old_fact_id: string;
  old_subject: string;
  old_object: string | null;
  old_valid_from: string | null;
  old_valid_from_precision: string | null;
  new_fact_id: string;
  new_subject: string;
  new_object: string | null;
  new_valid_from: string | null;
  new_valid_from_precision: string | null;
  new_confidence: number;
}

/** A decision-ledger event (the review-domain slice of audit_events): detail is a self-contained snapshot taken at decision time. */
export interface ReviewHistoryEvent {
  id: string;
  action: string;
  target_kind: string;
  target_id: string | null;
  detail: Record<string, unknown>;
  /** null = the system (AI adjudicator) */
  actor_name: string | null;
  created_at: string;
}

/** For one queue tier: how many items are waiting, and since when the oldest has waited (null for an empty queue) */
export interface QueueWait {
  count: number;
  oldest_at: string | null;
}

/** Decisions within a time window */
export interface DecidedWindow {
  total: number;
  /** Among these, those with no actor in the ledger: done by the AI adjudicator itself */
  automatic: number;
  by_action: { action: string; count: number }[];
  /** A row with actor_id null is the adjudicator; label is the identity snapshot in the ledger */
  by_actor: { actor_id: string | null; label: string | null; count: number }[];
}

/** Rows the agent wrote within a time window, by their current state count */
export interface AgentWindow {
  applied: number;
  proposed: number;
  accepted: number;
  overridden: number;
  reverted: number;
}

/** Review desk overview (#377): what is waiting to be handled, what has been handled, and the state of the KB. Same counting rules as the left-column counts */
export interface ReviewSummary {
  /** What the agent has done in this knowledge base (0025) */
  agent: { running: boolean; queue: number; open: number; last_7d: AgentWindow; last_30d: AgentWindow };
  waiting: Record<
    | "pending"
    | "duplicates"
    | "conflicts"
    | "unconfirmed"
    | "lowconf"
    | "violations"
    | "defects"
    | "alignment"
    | "errata",
    QueueWait
  >;
  decided: {
    last_7d: DecidedWindow;
    last_30d: DecidedWindow;
    /** The last 14 days, one entry per day, zeros included */
    daily: { day: string; count: number }[];
  };
  health: {
    facts: number;
    low_confidence: number;
    unconfirmed: number;
    contested: number;
  };
}

export interface MergeLog {
  id: string;
  source_name: string;
  target_name: string;
  merged_by_name: string | null;
  reason: string | null;
  created_at: string;
  reverted_at: string | null;
}

/** Attributes on an edge (0037): `{ key: "amount", value: { value: 4e9, unit: "$" } }` */
export interface FactQualifier {
  qualifier_type_id: string;
  key: string;
  label: string;
  value: { value?: unknown; unit?: string } | null;
  entity_id: string | null;
  entity_name: string | null;
}

export interface GraphEdge {
  id: string;
  source: string;
  target: string;
  /** When the ontology did not recognize this relation, this is the wording from the source text; null if neither can be produced (old data from before 0052) */
  predicate: string | null;
  label: string | null;
  /** Which open statement this edge was computed from, that statement's source wording (0044 decision 1):
   *  the canvas draws one edge per statement; the typed row is drawn when it exists, and the original wording is kept here without loss */
  said_as: string | null;
  /** true = the name of this edge comes from the source text, not from a relation the ontology recognized */
  inferred: boolean;
  /** true = this edge is **inferred**, not asserted by anyone (R1).
   *  Not the same as `inferred`: that one says "the name comes from the source text", this one says "nobody said it" */
  derived: boolean;
  /** The rule that inferred it; null for asserted edges.
   *  The UI uses it to recognize `inverse`: that kind of edge and its source edge are two ways of saying the same thing,
   *  and drawing both would only draw the redundancy twice (see `layOutParallelEdges` in Graph) */
  rule: string | null;
  /** The fact ids of the premises used to infer it (in proof order); empty for asserted edges.
   *  Edge pairing relies on it to pick the right source: matching on node pairs alone would hang the wording on the wrong edge */
  premises: string[];
  valid_from: string | null;
  valid_to: string | null;
  /** The **read-out** interval (0022): a fact with no start is taken from the earliest evidence, and an end of unknown date runs up to
   *  the document that said it ended. The slider filters on these two; the two above are what the source text said, shown for display only.
   *  The frontend does not interpret NULL itself: the rule lives in one place only, world_axis on the server */
  holds_from: string | null;
  holds_to: string | null;
  confidence: number;
  /** Disputed (0017 §3): an open axiom violation or temporal conflict points at it. The whole edge is drawn in warning colour */
  contested: boolean;
  /** Ghost edge (0017 §3): a derivation that did not land. `id` is the id of that `derived_contradiction` violation;
   *  `derived` is also true and follows the derivation toggle. Clicking it opens the subject's panel */
  blocked: boolean;
  /** Attributes on the edge (0037) */
  qualifiers: FactQualifier[];
  /** Document role words (0044): not ontology attributes, kept in a separate place */
  statement_qualifiers: StatementQualifier[];
}

/** One name of an entity (0041). `canonical` is the one in the panel title; a former name has an end on the world axis */
export interface NameView {
  fact_id: string;
  name: string;
  canonical: boolean;
  recorded_at: string;
  valid_from: string | null;
  valid_from_precision: string | null;
  valid_to: string | null;
  valid_to_precision: string | null;
  document_ids: string[];
  evidence_count: number;
}

/** Qualifiers recorded by the document's role words on an open statement (0044): a value or an entity */
export interface StatementQualifier {
  role: string;
  value: unknown | null;
  entity_id: string | null;
  entity_name: string | null;
  /** The blocks that state this value (0070): two living blocks that word it differently each get their own entry */
  chunk_ids: string[];
}

export interface EntityFact {
  id: string;
  direction: "out" | "in";
  /** Same as GraphEdge: a relation outside the ontology falls back to the source wording; null when neither exists */
  predicate_key: string | null;
  predicate_label: string | null;
  /** Which open statement this edge was computed from, that statement's source wording (0044 decision 1):
   *  the canvas draws one edge per statement; the typed row is drawn when it exists, and the original wording is kept here without loss */
  said_as: string | null;
  /** true = the name comes from the source text, not from a relation the ontology recognized */
  inferred: boolean;
  /** The temporal category of the relation. Without a predicate there is nothing to speak of, so it is null */
  temporal: string | null;
  other_id: string | null;
  other_name: string | null;
  /** Literal-valued object (attribute facts / question-answering mapping): {"value":…} or {"summary":…} */
  object_value: Record<string, unknown> | null;
  /** Attributes on the edge (0037) */
  qualifiers: FactQualifier[];
  /** Document role words (0044): not ontology attributes, kept in a separate place */
  statement_qualifiers: StatementQualifier[];
  valid_from: string | null;
  valid_to: string | null;
  /** The read-out interval (0022), same meaning as in GraphEdge: "holds right now" is judged by it */
  holds_from: string | null;
  holds_to: string | null;
  /** Records which date in the document the evidence comes from, quoted as the document words it (0064): "submission date 4 September 2026". For a fact with no start or end,
      `holds_from` is the date of the evidence: it means "as of", not a start */
  attested_by?: string | null;
  valid_from_precision: string | null;
  /** year | month | day, plus unknown = the source text says it ended but not on which day */
  valid_to_precision: string | null;
  confidence: number;
  evidence_count: number;
  /** All evidence stays in an older version of the source document (not confirmed by the current content; does not mean the fact is invalid) */
  stale: boolean;
  /** Correction row: the interval was closed by engine reconciliation or a human ruling, not from the extracted source text */
  corrected: boolean;
  /** Disputed (0017 §3): which kind, the id of that item in Review, and the sentence inferred when a derivation hits an assertion.
   *  The row is not dimmed: the assertion is still alive */
  contested: {
    kind: string;
    ref_id: string;
    derived?: string | null;
  } | null;
  /** The latest document time among the evidence set (the "last confirmed time" of an open fact) */
  last_evidence_time: string | null;
}

/** A cognitive change to an entity (an event on the recording timeline, orthogonal to the validity timeline of EntityFact).
 *
 * Not every event comes from a fact: retyped / retype_reverted come from the retyping ledger,
 * and have no predicate, no counterpart and no direction. */
export interface EntityHistoryEvent {
  /** Only fact events have this; for retyping events it is null */
  fact_id: string | null;
  /** Time of recording: a write = recorded_at, an invalidation = invalidated_at, a retyping = the moment of retyping */
  at: string;
  kind:
    | "asserted"
    | "corrected"
    | "rejected"
    | "merged"
    /** Another entity was merged into it: its facts moved under it */
    | "merged_in"
    /** It was merged into another entity: this id no longer exists on its own from now on */
    | "merged_away"
    | "merge_reverted"
    | "retyped"
    | "retype_reverted";
  direction: "out" | "in" | null;
  predicate_label: string | null;
  other_name: string | null;
  object_value: Record<string, unknown> | null;
  valid_from: string | null;
  valid_to: string | null;
  valid_from_precision: string | null;
  /** year | month | day, plus unknown = the source text says it ended but not on which day */
  valid_to_precision: string | null;
  confidence: number | null;
  /** null = the engine automatically (extraction write / temporal reconciliation closing / high-confidence automatic retyping) */
  actor_name: string | null;
  action: string | null;
  document_id: string | null;
  filename: string | null;
  quote: string | null;
  /** The two ends of a retyping event. A null start = retyped from "unclassified" (the most common kind since 0009) */
  from_type_label: string | null;
  to_type_label: string | null;
}

/** Stored comparisons against the regular phrase-alignment decisions. */
export interface OntologyRegressionCase {
  id: string;
  kb_id: string;
  statement_id: string;
  expected_binding: "bound" | "none";
  expected_property_id: string | null;
  expected_direction: "forward" | "reverse" | null;
  created_by: string | null;
  created_at: string;
  origin: "adoption" | "person";
  last_checked_at: string | null;
  last_result: {
    passed: boolean;
    human_bound: boolean;
    conflict?: "human_binding" | null;
    actual_property_id: string | null;
    actual_direction: "forward" | "reverse" | null;
    status: "bound" | "none" | "undecided";
    decided_at: string | null;
  } | null;
  subject_id: string;
  subject_label: string;
  phrase: string;
  object_id: string | null;
  object_label: string | null;
  object_value: Record<string, unknown> | null;
  expected_property_label: string | null;
  expected_property_key: string | null;
  actual_property_label: string | null;
  actual_property_key: string | null;
  created_by_label: string | null;
}

export interface Evidence {
  /** The predicate wording the model actually used in this chunk. Predicates outside the ontology do not land on the relation; this is what the UI shows */
  proposed_predicate: string | null;
  quote: string | null;
  chunk_id: string;
  document_id: string;
  filename: string;
  seq: number;
  /** Which version of the document the evidence came from */
  doc_version: number;
  /** The document has a newer version (the evidence stays in the old one; does not mean the fact is invalid) */
  stale: boolean;
  /** The document for this evidence has been deleted; the fact is still alive because it has another source (#268) */
  document_deleted: boolean;
  /** Where this block of text came from (0040); the proof-chain path does not carry it, and its absence means the source text */
  origin?: "stated" | "ocr" | "transcribed" | "described";
  origin_model?: string | null;
  anchor?: Record<string, unknown> | null;
}

export interface ChunkFull {
  id: string;
  seq: number;
  text: string;
  /** Where this block of text came from (0040) */
  origin: "stated" | "ocr" | "transcribed" | "described";
  origin_model: string | null;
  /** ocr: {page, bbox?}; transcribed: {start_ms, end_ms, speaker} */
  anchor: Record<string, unknown> | null;
}

export interface EntityTypeView {
  id: string;
  key: string;
  label: string;
  color: string;
  shape: "circle" | "square";
  builtin: boolean;
  /** All parent classes (subClassOf can have several) */
  parents: string[];
  /** Classes disjoint with it: **declares "cannot be both at once"** */
  disjoint: string[];
  /** Which branch it hangs under when the left column draws the tree. It has no semantics, it only affects display */
  primary_parent: string | null;
  description: string;
  /** How the type asks to be shown (RC-008): the `utopia:*` literals of its ontology file */
  display_hints: Record<string, string>;
  usage: number;
}

export interface RelationTypeView {
  id: string;
  key: string;
  label: string;
  temporal: string;
  functional: boolean;
  inverse_functional: boolean;
  /** The other four OWL axioms. **All of the reasoner's criteria are here** */
  is_transitive: boolean;
  is_symmetric: boolean;
  is_asymmetric: boolean;
  is_irreflexive: boolean;
  /** The two entries pointing at another relation: `p⁻¹ = q` and `p ⊑ q`. They are ids, not booleans,
   *  so the UI shows them as dropdowns */
  inverse_of: string | null;
  sub_property_of: string | null;
  builtin: boolean;
  description: string;
  /** relation (object is an entity) | attribute (object is a literal value) */
  kind: "relation" | "attribute";
  /** Classes that may be the subject. An attribute needs at least one; for a relation, empty = unrestricted */
  domains: string[];
  /** Classes that may be the object. Only meaningful for relations; an attribute's range is its datatype */
  ranges: string[];
  /** Which attributes an edge of this relation can carry (0037): ids of attribute definitions */
  qualifiers: string[];
  datatype: "text" | "number" | "date" | "bool" | null;
  unit: string | null;
  /** How the property asks to be shown (RC-008): the `utopia:*` literals of its ontology file */
  display_hints: Record<string, string>;
  usage: number;
}

/** One capability question (0061 decision 1): a question the ontology should be able to answer, which the agent checks when it proposes the ontology */
export interface CompetencyQuestion {
  id: string;
  kb_id: string;
  question: string;
  expected_answer: string | null;
  needs: unknown | null;
  origin: "person" | "agent";
  status: "accepted" | "proposed" | "rejected" | "retired";
  created_at: string;
  updated_at: string;
  last_checked_at: string | null;
  last_result: unknown | null;
}

/** The two numbers of the ontology (0061 decision 5) */
export interface QuestionReport {
  questions: {
    accepted: number;
    proposed: number;
    checked: number;
    answered: number;
    /** Answered, and facts were fetched from the graph on the way */
    answered_with_graph: number;
    /** Answered using only the graph: the ontology itself answered it */
    answered_graph_only: number;
  };
  proposals: {
    open: number;
    adopted: number;
    adopted_edited: number;
    rejected: number;
    decided: number;
    changed: number;
    changed_share: number | null;
  };
  /** Mentioned in the document but the ontology cannot hold yet: category words with no class, phrase shapes not bound to an attribute (these two are what the agent reads) */
  uncovered: { kind_words: number; phrases: number };
}


/** One end of a predicate has more than one open value (#341).
 *
 *  No one declared uniqueness in a library that grew out of the ontology, so a successor does not close out its predecessor: two `leads`
 *  both stay open, and "who is in charge in June" answers both. The engine does not infer this axiom automatically (it drives rewriting of the ledger),
 *  so the only option is to lay the evidence out and ask a person. */
export interface UniquenessCandidate {
  predicate_id: string;
  key: string;
  label: string;
  kind: string;
  /** subject = subject side (functional); object = object side (inverse functional) */
  side: "subject" | "object";
  axiom: "functional" | "inverse_functional";
  /** Already declared, just not yet reconciled (imported, or rows that existed before the declaration) */
  declared: boolean;
  holders: number;
  open_facts: number;
  /** How many reconciliations would close, and how many of the uncertain ones must go to human review */
  would_close: number;
  would_review: number;
  examples: {
    holder: string;
    values: {
      fact_id: string;
      name: string | null;
      valid_from: string | null;
      confidence: number;
    }[];
  }[];
}

/** `description` and `reason` are not the same thing: description goes verbatim into the extraction prompt and is the only basis the model uses to judge
    "what counts as this class"; reason is only for people, "why it should be added". Feeding the wrong class description makes it the next dumping ground,
    as measured, that is how technology came about. */
/** Extra columns an agent proposal carries (0061): who proposed it, which questions it serves, which shapes it would bind.
    A Suggest entry without these still renders as before */
export interface AgentProposalFields {
  proposed_by?: "suggest" | "agent";
  /** Id of the capability question served */
  serves?: string[];
  /** Category words that will fall under this class */
  kind_words?: string[];
  /** A few sentences from the source text, for a person to check the definition against */
  examples?: string[];
  /** The closest existing element in the ontology to this proposal (by vector): check before adopting whether it already exists */
  closest?: { key: string; label: string; distance: number }[];
  domains?: string[];
  ranges?: string[];
  parents?: string[];
  signatures?: {
    phrases?: {
      phrase: string;
      subject: string | null;
      object: string | null;
      value: boolean;
      /** Shape of map_to: direction for reading the attribute, forward | reverse */
      direction?: string;
    }[];
    kind_words?: string[];
  };
}

export interface OntologyProposals {
  entity_types: (AgentProposalFields & {
    key: string;
    label: string;
    description?: string;
    reason?: string;
  })[];
  relation_types: (AgentProposalFields & {
    key: string;
    label: string;
    temporal?: string;
    functional?: boolean;
    description?: string;
    reason?: string;
    /** Which surface wordings this relation has merged. Only with these can waiting facts be rewritten to it */
    forms?: string[];
  })[];
  /**
   * Wordings whose object is a literal value ("date of establishment = 2015").
   *
   * Kept separate from relation_types because they need different things: an attribute has a datatype,
   * and building it as a relation would turn the value into a fake entity. domain is not here:
   * the server takes it from the subject type of the fact, and a wrong guess drops the whole thing
   */
  attribute_types?: (AgentProposalFields & {
    key: string;
    label: string;
    datatype?: string;
    unit?: string;
    description?: string;
    reason?: string;
    forms?: string[];
  })[];
  /**
   * The ontology **already has** this meaning, the wording only needs to be attached to it.
   *
   * The difference from relation_types is that nothing is built: a second key grows for the same meaning,
   * and these facts stay split in two places forever, with nobody recognizing that they are the same thing.
   */
  map_to?: (AgentProposalFields & {
    key: string;
    /** Set by the server: whether the target lands on a relation or an attribute. The two rewrite paths differ,
        and the model can only answer one key, so it cannot tell which tier it is in. The agent's proposal also carries class */
    kind?: string;
    /** From the agent's proposal: the label of the target; people cannot read keys */
    label?: string;
    forms?: string[];
    reason?: string;
  })[];
}


/** Where a class/attribute went in this import. key_taken = the key is held by another IRI; reported, not changed */
export interface PlannedItem {
  iri: string;
  key: string;
  label: string;
  has_description: boolean;
  disposition: "create" | "update" | "key_taken" | "aligned" | "superseded";
  functional?: boolean;
  conflict_with?: string | null;
}

/** Preview and commit return the same plan: what happens after clicking confirm is exactly what was just previewed */
export interface ImportPlan {
  format: string;
  triples: number;
  classes: PlannedItem[];
  relations: PlannedItem[];
  attributes: PlannedItem[];
  /** Axioms that appeared but are not consumed today → counts. Not skipped; not yet projected */
  unprojected: [string, number][];
  classes_without_description: number;
  functional_relations: number;
}

/** The ledger recorded for an import. **Keys correspond one-to-one with what `owl_import.rs` writes into `summary`**;
 *  all are optional: older records may lack fields, and the UI treats them as missing rather than showing 0 */
export interface OntologyImportSummary {
  classes_created?: number;
  classes_updated?: number;
  classes_key_taken?: number;
  classes_without_description?: number;
  relations_seen?: number;
  relations_created?: number;
  relations_superseded?: number;
  relations_updated?: number;
  functional_relations?: number;
  /** How many inverse / parent property links were made. Silently skipped when the target IRI is not in this knowledge base */
  inverse_linked?: number;
  sub_property_linked?: number;
  attributes_seen?: number;
  attributes_created?: number;
  /** **Counts grouped by reason**, not a single number: `{no_domain: 3, unknown_domain: 1}`.
   *  There is more than one reason to skip a property, and the reason is what the person has to act on next */
  attributes_skipped?: Record<string, number>;
  /** IRIs that were not projected, with their occurrence counts (the server records at most 30) */
  unprojected?: [string, number][];
  triples?: number;
}

export interface OntologyImportView {
  id: string;
  filename: string;
  format: string;
  byte_size: number;
  summary: OntologyImportSummary;
  imported_by_name: string | null;
  imported_at: string;
}

export interface Source {
  n: number;
  /** Default = document chunk reference; charter = built-in handbook (jumps to /docs/{slug}#{anchor}) */
  kind?: "charter";
  chunk_id?: string;
  document_id?: string;
  slug?: string;
  anchor?: string;
  heading?: string;
  filename: string;
  excerpt: string;
  /** The few sentences in the block that are cited (quotes for graph facts, a follow-up to #968): the UI marks them in the source text */
  quotes?: string[];
}

/** The action trail of an agentic chat (one entry per tool call step). */
export interface ChatStep {
  kind:
    | "search"
    | "document"
    | "docs"
    | "entity"
    | "facts"
    | "neighbors"
    | "timeline"
    | "path"
    | "changes"
    | "query"
    | "tool";
  /** Data: queries, entity names, source names, tool names. Shown as is */
  label: string;
  /** English written by the server. **Steps with a `status` do not read it**; the UI speaks from the fields below (#942);
   *  messages stored before that only have it. The detail of question-answering and remember is data (the purpose, the sentence recorded) */
  detail: string;
  status?: "ok" | "failed" | "not_found" | "invalid";
  /** How many were listed; `total` is how many there are in all (when fewer are listed than exist) */
  count?: number;
  total?: number;
  /** Stopped at the listing limit; there may be more after, and the total is unknown */
  more?: boolean;
  /** World time: asks for facts that held at that time. RFC 3339 */
  valid_at?: string;
  /** Recording time: records made at that time (`as_of`) or before it (`before`). RFC 3339 */
  as_of?: string;
  before?: string;
  /** Window for changes, YYYY-MM-DD; without until it runs up to now */
  since?: string;
  until?: string;
  /** paths_between: how many hops the shortest one takes */
  hops?: number;
  /** Which argument is wrong; `missing` is separate from given-but-wrong */
  param?: string;
  missing?: boolean;
  /** The SQL that question-answering ran (#936) */
  sql?: string;
  /** The `remember` step carries it: the chunk the memory was saved as. The confirmation card in the conversation fetches
   *  pending items by it (0015); replay also redraws from it */
  chunk_id?: string;
  /** How long the body already was when this step happened (UTF-16 code units, the same unit as `string.length`).
   *  The trail is threaded back into the body by it, rather than all piled at the top.
   *  **Messages from before this migration do not have it**: when absent, the whole trail goes back to the top, the old way */
  at?: number;
}

/** Session row (the Chat left-column list). */
export interface ConversationRow {
  id: string;
  title: string;
  created_at: string;
  updated_at: string;
  message_count: number;
}

/** Session message (includes the stored action trail and citations, used for history replay). */
export interface ConversationMessage {
  id: string;
  role: "user" | "assistant";
  content: string;
  steps: ChatStep[];
  sources: Source[];
  stopped: boolean;
  created_at: string;
}

/** One group in the alert centre (0005): **consecutive failures of the same kind**.
 *
 * Storage still holds one failure per row; the folding is done when the server reads, so paging counts groups,
 * and a run of consecutive failures is not cut by a page boundary. */
export type AlertGroup = {
  kb_id: string | null;
  /** System-level alerts have no knowledge base name */
  kb_name: string | null;
  /** `source.sync_failed` / `llm.unreachable` — the wording is looked up in i18n by this */
  kind: string;
  severity: "info" | "warning" | "error";
  /** How many times in this group */
  count: number;
  /** Of those, how many I have not read */
  unread: number;
  latest_at: string;
  /** Together with latest_at, delimits this group; sent back as is when marking it read */
  earliest_at: string;
  /** Detail, at most a few entries, newest first */
  lines: {
    name?: string;
    error?: string;
    job?: string;
    warnings?: {
      kind: string;
      detail: Record<string, unknown>;
    }[];
  }[];
};

export const api = {
  health: () =>
    request<{ status: string; name: string; version: string }>(
      "/api/v1/health",
    ),
  me: () => request<User>("/api/v1/auth/me"),
  alerts: (o: { q?: string; limit?: number; offset?: number }) => {
    const p = new URLSearchParams();
    if (o.q?.trim()) p.set("q", o.q.trim());
    if (o.limit != null) p.set("limit", String(o.limit));
    if (o.offset) p.set("offset", String(o.offset));
    return request<{ items: AlertGroup[]; total: number }>(
      `/api/v1/alerts?${p}`,
    );
  },
  alertsUnread: () => request<{ unread: number }>("/api/v1/alerts/unread"),
  /** Returns a failed task to the queue (#216). One per knowledge base, one global (admin); the scope can be narrowed by kind and failure time */
  failedJobs: (kbId: string) =>
    request<{ failed: number }>(`/api/v1/kbs/${kbId}/jobs/failed`),
  requeueJobs: (
    kbId: string | null,
    body: { kind?: string; failed_since?: string } = {},
  ) =>
    request<{ requeued: number }>(
      kbId ? `/api/v1/kbs/${kbId}/jobs/requeue` : "/api/v1/jobs/requeue",
      { method: "POST", body: JSON.stringify(body) },
    ),
  alertReadGroup: (g: {
    kb_id: string | null;
    kind: string;
    from: string;
    to: string;
  }) =>
    request<{ marked: number }>("/api/v1/alerts/read-group", {
      method: "POST",
      body: JSON.stringify(g),
    }),
  alertsReadAll: () =>
    request<{ ok: boolean }>("/api/v1/alerts/read-all", { method: "POST" }),
  login: (email: string, password: string) =>
    request<{ user: User }>("/api/v1/auth/login", {
      method: "POST",
      body: JSON.stringify({ email, password }),
    }),
  register: (email: string, password: string, displayName: string) =>
    request<{ user: User; workspace: Workspace }>("/api/v1/auth/register", {
      method: "POST",
      body: JSON.stringify({ email, password, display_name: displayName }),
    }),
  logout: () =>
    request<{ ok: boolean }>("/api/v1/auth/logout", { method: "POST" }),
  updateMe: (displayName: string) =>
    request<User>("/api/v1/auth/me", {
      method: "PATCH",
      body: JSON.stringify({ display_name: displayName }),
    }),
  changePassword: (currentPassword: string, newPassword: string) =>
    request<{ ok: boolean }>("/api/v1/auth/password", {
      method: "POST",
      body: JSON.stringify({
        current_password: currentPassword,
        new_password: newPassword,
      }),
    }),
  /** Personal tokens I have issued (0014). Revoked ones are included too: the fact of revoking must stay visible */
  tokens: () => request<{ tokens: TokenView[] }>("/api/v1/me/tokens"),
  /** Issue one. **The plaintext is only in this one response**; the list can never give it back */
  issueToken: (body: {
    name: string;
    scope: "read" | "write";
    /** Default = every knowledge base this person can enter */
    kb_ids?: string[] | null;
    /** 0 = never expires */
    expires_in_days: number;
  }) =>
    request<{ token: string; info: TokenView }>("/api/v1/me/tokens", {
      method: "POST",
      body: JSON.stringify(body),
    }),
  revokeToken: (tokenId: string) =>
    request<{ ok: boolean }>(`/api/v1/me/tokens/${tokenId}`, { method: "DELETE" }),
  workspaces: () => request<Workspace[]>("/api/v1/workspaces"),
  /** This workspace, and **what role I hold in it**. Creating a KB needs Admin+ (see the create in api/kbs.rs),
   *  and the `GET /workspaces` list does not carry the role */
  workspaceRole: (workspaceId: string) =>
    request<{ workspace: Workspace; role: string }>(
      `/api/v1/workspaces/${workspaceId}`,
    ),

  kbs: (workspaceId: string) =>
    request<Kb[]>(`/api/v1/workspaces/${workspaceId}/kbs`),
  /** The audit ledger. **Paged + filtered**: the ledger is compliance material, and seeing only the latest 100 entries means history cannot be queried.
   *  `action` is a prefix: `entity.` picks up the entity.retyped / entity.renamed family. */
  kbAudit: (
    kbId: string,
    opts: {
      action?: string;
      actor?: string;
      since?: string;
      until?: string;
      limit: number;
      offset: number;
    },
  ) => {
    const p = new URLSearchParams({
      limit: String(opts.limit),
      offset: String(opts.offset),
    });
    if (opts.action) p.set("action", opts.action);
    if (opts.actor) p.set("actor", opts.actor);
    if (opts.since) p.set("since", opts.since);
    if (opts.until) p.set("until", opts.until);
    return request<{
      events: AuditEvent[];
      total: number;
      /** The actions that actually happened in this knowledge base; the filter dropdown is populated from it */
      actions: string[];
    }>(`/api/v1/kbs/${kbId}/audit?${p}`);
  },
  myKbs: (workspaceId: string) =>
    request<{ kbs: MyKb[] }>(`/api/v1/workspaces/${workspaceId}/my-kbs`),
  createKb: (
    workspaceId: string,
    body: {
      name: string;
      description?: string | null;
      visibility?: string;
      /** Preset ontology pack ids; order matters: the first one claims seed classes with the same name */
      ontology_packs?: string[];
      /** Scenario pack id (0073): classes, attributes and capability questions preset for a scenario */
      scenario_packs?: string[];
    },
  ) =>
    request<Kb>(`/api/v1/workspaces/${workspaceId}/kbs`, {
      method: "POST",
      body: JSON.stringify(body),
    }),

  ontologyPacks: () =>
    request<{ packs: OntologyPack[] }>("/api/v1/ontology-packs"),
  scenarioPacks: () =>
    request<{ packs: ScenarioPack[] }>("/api/v1/scenario-packs"),

  kbDetail: (kbId: string) => request<Kb>(`/api/v1/kbs/${kbId}`),
  updateKb: (kbId: string, body: Record<string, unknown>) =>
    request<Kb>(`/api/v1/kbs/${kbId}`, {
      method: "PATCH",
      body: JSON.stringify(body),
    }),
  deleteKb: (kbId: string) =>
    request<{ ok: boolean }>(`/api/v1/kbs/${kbId}`, { method: "DELETE" }),
  kbMembers: (kbId: string) =>
    request<{ members: KbMember[] }>(`/api/v1/kbs/${kbId}/members`),
  setKbMember: (kbId: string, userId: string, role: string) =>
    request<{ ok: boolean }>(`/api/v1/kbs/${kbId}/members/${userId}`, {
      method: "PUT",
      body: JSON.stringify({ role }),
    }),
  removeKbMember: (kbId: string, userId: string) =>
    request<{ ok: boolean }>(`/api/v1/kbs/${kbId}/members/${userId}`, {
      method: "DELETE",
    }),

  adminDeployment: () =>
    request<{
      open_registration: boolean;
      /** Outer safety net against tasks piling up without limit; the real throttle is the per-model limit */
      worker_concurrency: number;
      default_model_concurrency: number;
      /** Default ontology language for a new knowledge base. **Not the UI language**, which lives on the client */
      default_ontology_lang: "en" | "zh";
      model_limits: {
        base_url: string;
        model: string;
        max_concurrent: number;
      }[];
      models_in_use: { base_url: string; model: string; kind: string }[];
    }>("/api/v1/admin/deployment"),
  saveAdminDeployment: (
    openRegistration: boolean,
    workerConcurrency?: number,
    defaultModelConcurrency?: number,
    modelLimit?: {
      base_url: string;
      model: string;
      max_concurrent: number | null;
    },
    defaultOntologyLang?: "en" | "zh",
  ) =>
    request<{ ok: boolean }>("/api/v1/admin/deployment", {
      method: "PUT",
      body: JSON.stringify({
        open_registration: openRegistration,
        ...(workerConcurrency !== undefined
          ? { worker_concurrency: workerConcurrency }
          : {}),
        ...(defaultModelConcurrency !== undefined
          ? { default_model_concurrency: defaultModelConcurrency }
          : {}),
        ...(modelLimit ? { model_limit: modelLimit } : {}),
        ...(defaultOntologyLang
          ? { default_ontology_lang: defaultOntologyLang }
          : {}),
      }),
    }),
  adminCreateUser: (body: {
    email: string;
    display_name: string;
    password: string;
    role: string;
  }) =>
    request<{ user: User }>("/api/v1/admin/users", {
      method: "POST",
      body: JSON.stringify(body),
    }),

  /** Deactivated accounts. **Without this, restoring is out of reach**: the person vanished from every list,
   *  and the restore endpoint needs exactly their id */
  deactivatedUsers: () => request<OrgUser[]>("/api/v1/users/deactivated"),
  /** Restore a deactivated account */
  adminReactivateUser: (userId: string) =>
    request<{ ok: boolean }>(`/api/v1/admin/users/${userId}`, {
      method: "POST",
    }),
  /** Deactivate an account (soft delete). Attribution is still queryable: the audit, the merge log and the retyping ledger all rely on it */
  adminDeactivateUser: (userId: string) =>
    request<{ ok: boolean }>(`/api/v1/admin/users/${userId}`, {
      method: "DELETE",
    }),
  /** Grant or revoke system administrator (#1233) */
  adminSetSystemAdmin: (userId: string, isAdmin: boolean) =>
    request<{ ok: boolean }>(`/api/v1/admin/users/${userId}`, {
      method: "PUT",
      body: JSON.stringify({ is_admin: isAdmin }),
    }),
  adminDataSources: () =>
    request<{ data_sources: DataSourceView[] }>("/api/v1/admin/data-sources"),
  adminCreateDataSource: (body: { name: string; conn_string: string }) =>
    request<{ id: string }>("/api/v1/admin/data-sources", {
      method: "POST",
      body: JSON.stringify(body),
    }),
  adminDeleteDataSource: (id: string) =>
    request<{ ok: boolean }>(`/api/v1/admin/data-sources/${id}`, {
      method: "DELETE",
    }),
  /** Try it once before saving: **nothing is stored**. The response is ok; when it cannot connect there is still a reason:
   *  wrong password, mistyped database name and unreachable port are three different things */
  adminTestConnString: (conn_string: string) =>
    request<{ ok: boolean; engine?: string; error?: string }>(
      "/api/v1/admin/data-sources/test",
      { method: "POST", body: JSON.stringify({ conn_string }) },
    ),
  adminTestDataSource: (id: string) =>
    request<{ ok: boolean }>(`/api/v1/admin/data-sources/${id}/test`, {
      method: "POST",
    }),
  /** Which workspaces this source is authorized for (0014). Authorization and mounting are two layers:
   *  a system administrator writes authorization, and a KB administrator picks from the authorized set to mount */
  dataSourceGrants: (id: string) =>
    request<{ workspaces: { id: string; name: string }[] }>(
      `/api/v1/admin/data-sources/${id}/grants`,
    ),
  grantDataSource: (id: string, workspaceId: string) =>
    request<{ ok: boolean }>(
      `/api/v1/admin/data-sources/${id}/grants/${workspaceId}`,
      { method: "PUT" },
    ),
  /** Revoke authorization. **Also unmounts whatever is already mounted in that workspace**, and returns how many were unmounted */
  revokeDataSource: (id: string, workspaceId: string) =>
    request<{ ok: boolean; unmounted: number }>(
      `/api/v1/admin/data-sources/${id}/grants/${workspaceId}`,
      { method: "DELETE" },
    ),

  /** One page of definitions. **Viewers can see it**: the answer to a question-answer is decided directly by the definition,
   *  and seeing the answer without the definition is like asking people to trust an algorithm they cannot inspect */
  mappings: (
    kbId: string,
    opts: {
      status?: "proposed" | "confirmed" | "rejected";
      q?: string;
      limit?: number;
      offset?: number;
    } = {},
  ) => {
    const p = new URLSearchParams();
    if (opts.status) p.set("status", opts.status);
    if (opts.q) p.set("q", opts.q);
    if (opts.limit != null) p.set("limit", String(opts.limit));
    if (opts.offset != null) p.set("offset", String(opts.offset));
    const qs = p.toString();
    return request<{
      items: ConceptMapping[];
      total: number;
      counts: { proposed: number; confirmed: number; rejected: number };
      // The ledger of the most recent exploration round (#503). The list alone cannot say "how much was missed":
      // a wide table of 80 columns and a small library fully covered look the same. */
      last_run?: {
        id: string;
        started_at: string;
        finished_at: string | null;
        sources: string[];
        tables_scanned: number;
        columns_scanned: number;
        schema_truncated: boolean;
        cap: number;
        returned: number;
        accepted: number;
        dropped: Record<string, { n: number; example: string }>;
        tables_covered: string[];
        error: string | null;
      };
    }>(`/api/v1/kbs/${kbId}/mappings${qs ? `?${qs}` : ""}`);
  },
  /** Change one definition. The version from before the change is automatically put into revisions */
  reviseMapping: (
    kbId: string,
    mappingId: string,
    body: {
      table_name?: string | null;
      expr?: string | null;
      sql?: string | null;
      unit?: string | null;
      summary?: string | null;
      derived: boolean;
    },
  ) =>
    request<{ ok: boolean }>(`/api/v1/kbs/${kbId}/mappings/${mappingId}`, {
      method: "PATCH",
      body: JSON.stringify(body),
    }),
  mappingRevisions: (kbId: string, mappingId: string) =>
    request<{ revisions: MappingRevision[] }>(
      `/api/v1/kbs/${kbId}/mappings/${mappingId}/revisions`,
    ),
  kbDataSources: (kbId: string) =>
    request<{ data_sources: DataSourceView[] }>(
      `/api/v1/kbs/${kbId}/data-sources`,
    ),
  kbDataSourcesAvailable: (kbId: string) =>
    request<{ data_sources: DataSourceView[] }>(
      `/api/v1/kbs/${kbId}/data-sources/available`,
    ),
  /** Mounting. **It counts as mounted even when `schema_error` is non-empty**: the source really is mounted, only its
   *  database schema was not ingested, so question-answering cannot see which tables exist. The same thing goes into the alert centre */
  mountDataSource: (kbId: string, dsId: string) =>
    request<{
      ok: boolean;
      schema_tables: number;
      schema_error?: string | null;
    }>(`/api/v1/kbs/${kbId}/data-sources/${dsId}`, {
      method: "PUT",
    }),
  unmountDataSource: (kbId: string, dsId: string) =>
    request<{ ok: boolean }>(`/api/v1/kbs/${kbId}/data-sources/${dsId}`, {
      method: "DELETE",
    }),
  exploreMappings: (kbId: string) =>
    request<{ ok: boolean }>(`/api/v1/kbs/${kbId}/data-sources/explore`, {
      method: "POST",
    }),
  syncDataSourceSchema: (kbId: string, dsId: string) =>
    request<{ ok: boolean; schema_tables: number }>(
      `/api/v1/kbs/${kbId}/data-sources/${dsId}/sync-schema`,
      { method: "POST" },
    ),

  /** One page of the library. **Server-side filtering and paging**: previously the whole library was fetched at once and the frontend sliced it,
   *  and client-side filtering could only filter what had already been fetched */
  documents: (
    kbId: string,
    opts: {
      source?: string;
      q?: string;
      graph?: string;
      /** "deleted" = the "Deleted" view: lists only tombstones (#268) */
      state?: "deleted";
      limit: number;
      offset: number;
    },
  ) => {
    const p = new URLSearchParams({
      limit: String(opts.limit),
      offset: String(opts.offset),
    });
    if (opts.source) p.set("source", opts.source);
    if (opts.q) p.set("q", opts.q);
    if (opts.graph) p.set("graph", opts.graph);
    if (opts.state) p.set("state", opts.state);
    return request<{
      docs: Doc[];
      total: number;
      /** The four below are **computed by source scope only**, unaffected by the name/status filters:
       *  they are the scope of the batch buttons */
      ready: number;
      /** `graph_status = 'done'`: the numerator of the extraction progress bar */
      done: number;
      extracting: number;
      failed: number;
      /** Tombstone count for the whole knowledge base (deleted, not yet purged), does not change with scope */
      deleted: number;
    }>(`/api/v1/kbs/${kbId}/documents?${p}`);
  },
  /** One-click retry of all failed-extraction documents in this scope */
  retryFailedDocs: (kbId: string, source?: string) =>
    request<{ queued: number; found: number }>(
      `/api/v1/kbs/${kbId}/documents/retry-failed${source ? `?source=${source}` : ""}`,
      { method: "POST" },
    ),
  /** Fetch the whole knowledge base in one go: once rows are aggregated by (document × reason × object) it is small, avoiding a request per row */
  extractionDrops: (kbId: string) =>
    request<{ drops: ExtractionDrop[] }>(
      `/api/v1/kbs/${kbId}/extraction-drops`,
    ),
  upload: (kbId: string, files: File[], sourceId?: string) => {
    const form = new FormData();
    for (const f of files) form.append("files", f, f.name);
    const qs = sourceId ? `?source=${sourceId}` : "";
    return request<{ created: Doc[]; skipped: unknown[] }>(
      `/api/v1/kbs/${kbId}/documents${qs}`,
      { method: "POST", body: form },
    );
  },
  deleteDocument: (id: string) =>
    request<{ ok: boolean; deletion_id: string; invalidated_facts: number }>(
      `/api/v1/documents/${id}`,
      { method: "DELETE" },
    ),
  /** Undo a deletion (#268): documents, chunks and the facts that were invalidated along with them come back the same way */
  restoreDocument: (id: string) =>
    request<{ ok: boolean }>(`/api/v1/documents/${id}/restore`, { method: "POST" }),
  /** Hard delete (#268, second half): only for items already deleted; knowledge base admin; cannot be undone */
  purgeDocument: (id: string) =>
    request<{ ok: boolean; chunks: number; blobs: number }>(
      `/api/v1/documents/${id}/purge`,
      { method: "POST" },
    ),

  search: (kbId: string, q: string, offset: number, topK: number, signal?: AbortSignal) =>
    request<{ results: SearchResult[]; has_more: boolean }>(`/api/v1/kbs/${kbId}/search`, {
      method: "POST",
      body: JSON.stringify({ q, offset, top_k: topK }),
      signal,
    }),

  settings: (workspaceId: string) =>
    request<LlmSettingsView>(`/api/v1/workspaces/${workspaceId}/settings`),
  saveSettings: (workspaceId: string, body: Record<string, unknown>) =>
    request<{ ok: boolean }>(`/api/v1/workspaces/${workspaceId}/settings`, {
      method: "PUT",
      body: JSON.stringify(body),
    }),
  graphOverview: (kbId: string, limit?: number) =>
    request<{
      nodes: GraphNode[];
      edges: GraphEdge[];
      /** How many there are in the knowledge base in total. **Not the same as nodes.length**: the canvas draws only
       *  the highest-degree batch, and showing the cap as the size was the most misleading thing this endpoint used to do */
      total_nodes?: number;
      total_edges?: number;
    }>(`/api/v1/kbs/${kbId}/graph/overview${limit ? `?limit=${limit}` : ""}`),
  /** The neighbourhood view has **no total**: it is only a small slice to begin with, so saying "325 in total" means nothing.
   *  The two fields are declared optional so that callers and the overview can share one type */
  graphNeighborhood: (kbId: string, entityId: string) =>
    request<{
      nodes: GraphNode[];
      edges: GraphEdge[];
      total_nodes?: number;
      total_edges?: number;
    }>(`/api/v1/kbs/${kbId}/graph/neighborhood?entity=${entityId}&hops=2`),
  /** Find entities by name. **Also returns the total**: "split rather than merge" creates many with the same name,
   *  and the one you are looking for may not be among the fixed ten */
  searchEntities: (kbId: string, q: string, limit = 10) =>
    request<{ entities: GraphNode[]; total: number }>(
      `/api/v1/kbs/${kbId}/entities?q=${encodeURIComponent(q)}&limit=${limit}`,
    ),
  entityDetail: (kbId: string, entityId: string) =>
    request<{
      entity: GraphNode;
      facts: EntityFact[];
      /** This entity's names (0041): the real name, short names and former names, each with source and validity period.
       *  Name facts are not in facts: a name is not "a fact about it" */
      names: NameView[];
      /** Things inferred are kept under **their own key**, not mixed into facts: in one list
       *  users could not tell "written in the document" from "the engine inferred it" */
      derived: DerivedFact[];
      /** Derivations that did not land (0017 §3): not even in derived_facts, so they also get a key of their own */
      blocked: BlockedDerivation[];
      /** Other entities with the same name. **Given as soon as the panel opens**: the merge entry must sit where same-name entities are visible,
       *  not be hidden behind "rename once" */
      same_name: GraphNode[];
    }>(`/api/v1/kbs/${kbId}/entities/${entityId}`),
  /** History of cognitive changes (recording timeline): paged by the server */
  /** Manually correct an entity's type or name. A same name is not blocked: the returned same_name lets the UI prompt whether to merge. */
  updateEntity: (
    kbId: string,
    entityId: string,
    body: { type_id?: string; canonical_name?: string },
  ) =>
    request<{ entity: GraphNode; same_name: GraphNode[] }>(
      `/api/v1/kbs/${kbId}/entities/${entityId}`,
      { method: "PATCH", body: JSON.stringify(body) },
    ),

  /** Manually correct the validity interval of a fact (302). **Whole replacement**: all four values are submitted together,
   *  the server voids the old row and inserts a correction row: it is not an in-place edit, so the change shows up in History. */
  updateFactTime: (
    kbId: string,
    factId: string,
    body: {
      valid_from: string | null;
      valid_from_precision: string | null;
      valid_to: string | null;
      valid_to_precision: string | null;
      note?: string;
    },
  ) =>
    request<{
      ok: boolean;
      unchanged?: boolean;
      fact_id?: string;
      closed?: number;
      conflicts?: number;
    }>(`/api/v1/kbs/${kbId}/facts/${factId}`, {
      method: "PATCH",
      body: JSON.stringify(body),
    }),

  /** How far this knowledge base has got (#313): shared by the empty states of four pages. Returns only booleans and counts */
  readiness: (kbId: string) =>
    request<Readiness>(`/api/v1/kbs/${kbId}/readiness`),

  /* ---- Business rules (0021 / #277): criteria written by a person; the engine runs them on the materialization schedule ---- */
  rules: (kbId: string) =>
    request<{ rules: BusinessRule[] }>(`/api/v1/kbs/${kbId}/rules`),
  createRule: (kbId: string, body: RuleInput) =>
    request<{ id: string }>(`/api/v1/kbs/${kbId}/rules`, {
      method: "POST",
      body: JSON.stringify(body),
    }),
  updateRule: (
    kbId: string,
    ruleId: string,
    body: {
      name?: string;
      description?: string;
      enabled?: boolean;
      conditions?: RuleCondition[];
      /** Replace the conclusion set as a whole: the three fields define each other, so changing only one would leave a half-state */
      conclusion?: "typing" | "attribute" | "relation";
      conclude_type_id?: string;
      conclude_predicate_id?: string;
      join_predicate_id?: string;
      conclude_value?: unknown;
    },
  ) =>
    request<{ ok: boolean }>(`/api/v1/kbs/${kbId}/rules/${ruleId}`, {
      method: "PATCH",
      body: JSON.stringify(body),
    }),
  /** Who one rule is currently flagging. The number on the rule card opens this list */
  ruleMatches: (kbId: string, ruleId: string, page = 0, per = 20) =>
    request<{ matches: RuleMatch[]; total: number }>(
      `/api/v1/kbs/${kbId}/rules/${ruleId}/matches?page=${page}&per=${per}`,
    ),
  /** The definition history of one rule: how many times it changed and what each version said */
  ruleVersions: (kbId: string, ruleId: string) =>
    request<{ versions: RuleVersion[] }>(`/api/v1/kbs/${kbId}/rules/${ruleId}/versions`),
  deleteRule: (kbId: string, ruleId: string) =>
    request<{ ok: boolean }>(`/api/v1/kbs/${kbId}/rules/${ruleId}`, {
      method: "DELETE",
    }),
  /** Run once right away. Materialization runs hourly by default, but the person writing the rule wants to see what it inferred immediately */
  runRules: (kbId: string) =>
    request<{
      rules: number;
      hits: number;
      capped: number;
      inserted: number;
      invalidated: number;
      /** How many rounds the chain ran: 1 means no chain; 2 means one rule read another rule's conclusion (0030) */
      rounds: number;
      /** Still producing when the cap is reached: the chain is longer than MAX_DEPTH, and the last links are not connected */
      rounds_capped: boolean;
    }>(`/api/v1/kbs/${kbId}/rules/run`, { method: "POST" }),

  entityHistory: (kbId: string, entityId: string, page: number, per = 30) =>
    request<{ events: EntityHistoryEvent[]; total: number }>(
      `/api/v1/kbs/${kbId}/entities/${entityId}/history?page=${page}&per=${per}`,
    ),
  factEvidence: (kbId: string, factId: string) =>
    request<{ evidence: Evidence[] }>(
      `/api/v1/kbs/${kbId}/facts/${factId}/evidence`,
    ),
  /** The proof of a derived fact (0002 R2): premises in derivation order, each with evidence, all the way to the source sentence.
   *  Returns null when the derivation is no longer valid; that is not an error */
  derivedProof: (kbId: string, derivedId: string) =>
    request<{ proof: Proof | null }>(
      `/api/v1/kbs/${kbId}/derived/${derivedId}/proof`,
    ),
  /** The proof chain of a derivation that did not land (0017 §3): the premises are in the violation's path */
  blockedProof: (kbId: string, violationId: string) =>
    request<{ steps: ProofStep[] | null }>(
      `/api/v1/kbs/${kbId}/violations/${violationId}/proof`,
    ),
  documentDetail: (id: string) =>
    request<{ document: Doc; chunks: ChunkFull[] }>(`/api/v1/documents/${id}`),
  extractDocument: (id: string) =>
    request<{ job_id: number }>(`/api/v1/documents/${id}/extract`, {
      method: "POST",
    }),
  reprocessDocument: (id: string) =>
    request<{ job_id: number }>(`/api/v1/documents/${id}/reprocess`, {
      method: "POST",
    }),
  /** Full re-extraction for the source (incremental semantics: existing decisions are kept) */
  reExtractSource: (kbId: string, sourceId: string) =>
    request<{ queued: number }>(
      `/api/v1/kbs/${kbId}/sources/${sourceId}/re-extract`,
      {
        method: "POST",
      },
    ),
  /** Rebuild the graph (liquidation semantics: clear the graph layer, then re-extract in full; KB admin) */
  rebuildGraph: (kbId: string) =>
    request<{
      entities_removed: number;
      facts_removed: number;
      queued: number;
    }>(`/api/v1/kbs/${kbId}/graph/rebuild`, { method: "POST" }),

  ontology: (kbId: string) =>
    request<{
      entity_types: EntityTypeView[];
      relation_types: RelationTypeView[];
    }>(`/api/v1/kbs/${kbId}/ontology`),
  ontologyRegressions: (kbId: string) =>
    request<{ cases: OntologyRegressionCase[] }>(
      `/api/v1/kbs/${kbId}/ontology/regressions`,
    ),
  addOntologyRegression: (
    kbId: string,
    body: { statement_id: string } & ({
      expected_binding?: "bound";
      relation_type_id: string;
      direction: "forward" | "reverse";
    } | {
      expected_binding: "none";
      relation_type_id?: null;
      direction?: null;
    }),
  ) =>
    request<{ id: string }>(`/api/v1/kbs/${kbId}/ontology/regressions`, {
      method: "POST",
      body: JSON.stringify(body),
    }),
  createEntityType: (kbId: string, body: Record<string, unknown>) =>
    request<{ id: string }>(`/api/v1/kbs/${kbId}/ontology/entity-types`, {
      method: "POST",
      body: JSON.stringify(body),
    }),
  updateEntityType: (kbId: string, id: string, body: Record<string, unknown>) =>
    request<{ ok: boolean }>(
      `/api/v1/kbs/${kbId}/ontology/entity-types/${id}`,
      {
        method: "PATCH",
        body: JSON.stringify(body),
      },
    ),
  deleteEntityType: (kbId: string, id: string) =>
    request<{ ok: boolean }>(
      `/api/v1/kbs/${kbId}/ontology/entity-types/${id}`,
      {
        method: "DELETE",
      },
    ),
  typeEntities: (kbId: string, typeId: string, page: number, per = 12) =>
    request<{
      entities: { id: string; name: string; fact_count: number }[];
      total: number;
    }>(
      `/api/v1/kbs/${kbId}/ontology/entity-types/${typeId}/entities?page=${page}&per=${per}`,
    ),
  createRelationType: (kbId: string, body: Record<string, unknown>) =>
    request<{ id: string }>(`/api/v1/kbs/${kbId}/ontology/relation-types`, {
      method: "POST",
      body: JSON.stringify(body),
    }),
  /** A predicate with more than one open value at one end (#341): when no one declared uniqueness in the ontology, a successor does not close out its predecessor */
  uniquenessCandidates: (kbId: string) =>
    request<{ candidates: UniquenessCandidate[] }>(
      `/api/v1/kbs/${kbId}/ontology/uniqueness`,
    ),

  /** After a declaration is made, run the open rows already on the ledger through it once. The declaration itself goes through updateRelationType */
  reconcileRelationType: (kbId: string, id: string) =>
    request<{ corrected: number; conflicts: number }>(
      `/api/v1/kbs/${kbId}/ontology/relation-types/${id}/reconcile`,
      { method: "POST" },
    ),

  updateRelationType: (
    kbId: string,
    id: string,
    body: Record<string, unknown>,
  ) =>
    request<{ ok: boolean }>(
      `/api/v1/kbs/${kbId}/ontology/relation-types/${id}`,
      {
        method: "PATCH",
        body: JSON.stringify(body),
      },
    ),
  deleteRelationType: (kbId: string, id: string) =>
    request<{ ok: boolean }>(
      `/api/v1/kbs/${kbId}/ontology/relation-types/${id}`,
      {
        method: "DELETE",
      },
    ),
  /** Proposals last computed that nobody has yet responded to (0049). Page refresh relies on it, so the model need not run again */
  storedProposals: (kbId: string) =>
    request<OntologyProposals>(`/api/v1/kbs/${kbId}/ontology/proposals`),
  /** A proposal was rejected by someone. Changing the status does not delete the row: the rejection leaves a trace, and the next Suggest round does not bring it back as pending.
   *  Only rejection is recorded here: adopting goes through adoptProposal, with the element and the adopted status committed together (0061 decision 3) */
  decideProposal: (
    kbId: string,
    section: string,
    key: string,
    status: "rejected",
    /** The reason for rejection (0061): the next agent round can read it */
    reason?: string,
  ) =>
    request<{ ok: boolean }>(`/api/v1/kbs/${kbId}/ontology/proposals`, {
      method: "POST",
      body: JSON.stringify({ section, key, status, reason }),
    }),
  /** Have the ontology agent take a look (0061 decision 2): queue a task, and the result lands in storedProposals */
  proposeOntology: (kbId: string) =>
    request<{ queued: boolean }>(`/api/v1/kbs/${kbId}/ontology/propose`, {
      method: "POST",
      body: "{}",
    }),
  /** Adopt one of the agent's proposals (0061 decision 3): the element, the regression case, the adopted status and the follow-up task are committed in one go */
  adoptProposal: (
    kbId: string,
    section: string,
    key: string,
    edits?: {
      label?: string;
      description?: string;
      datatype?: string;
      domains?: string[];
      ranges?: string[];
      parents?: string[];
    },
  ) =>
    request<{
      id: string;
      /** Shapes or category words that were left unwritten when reusing, each with what the person decided: the person decided differently from the proposal after the agent read it */
      skipped: ({ phrase: string } | { kind_word: string })[];
    }>(`/api/v1/kbs/${kbId}/ontology/proposals/adopt`, {
      method: "POST",
      body: JSON.stringify({ section, key, ...(edits ?? {}) }),
    }),
  /** Capability question (0061 decision 1) */
  listQuestions: (kbId: string) =>
    request<CompetencyQuestion[]>(`/api/v1/kbs/${kbId}/questions`),
  createQuestion: (
    kbId: string,
    body: { question: string; expected_answer?: string },
  ) =>
    request<{ id: string }>(`/api/v1/kbs/${kbId}/questions`, {
      method: "POST",
      body: JSON.stringify(body),
    }),
  updateQuestion: (
    kbId: string,
    qid: string,
    body: {
      question?: string;
      expected_answer?: string;
      status?: "accepted" | "rejected" | "retired";
    },
  ) =>
    request<{ ok: boolean }>(`/api/v1/kbs/${kbId}/questions/${qid}`, {
      method: "PATCH",
      body: JSON.stringify(body),
    }),
  deleteQuestion: (kbId: string, qid: string) =>
    request<{ ok: boolean }>(`/api/v1/kbs/${kbId}/questions/${qid}`, {
      method: "DELETE",
    }),
  /** Have the agent ask the knowledge base questions (0061 decision 1): queues a task, and the result is a proposed question */
  proposeQuestions: (kbId: string) =>
    request<{ queued: boolean }>(`/api/v1/kbs/${kbId}/questions/propose`, {
      method: "POST",
      body: "{}",
    }),
  /** The two numbers (0061 decision 5) */
  questionReport: (kbId: string) =>
    request<QuestionReport>(`/api/v1/kbs/${kbId}/questions/report`),

  /** Uploading an ontology file only computes a plan; not a single byte is written to the knowledge base */
  previewOntologyImport: (kbId: string, file: File) => {
    const form = new FormData();
    form.append("file", file, file.name);
    return request<{ filename: string; plan: ImportPlan }>(
      `/api/v1/kbs/${kbId}/ontology/imports/preview`,
      { method: "POST", body: form },
    );
  },
  /** Execute the plan that was just previewed: the server recomputes it, and both paths share the same code */
  applyOntologyImport: (kbId: string, file: File) => {
    const form = new FormData();
    form.append("file", file, file.name);
    return request<{ import_id: string; plan: ImportPlan }>(
      `/api/v1/kbs/${kbId}/ontology/imports`,
      { method: "POST", body: form },
    );
  },
  ontologyImports: (kbId: string) =>
    request<{ imports: OntologyImportView[] }>(
      `/api/v1/kbs/${kbId}/ontology/imports`,
    ),

  sources: (kbId: string) =>
    request<{ sources: SourceView[] }>(`/api/v1/kbs/${kbId}/sources`),
  createSource: (kbId: string, body: Record<string, unknown>) =>
    request<{ source: SourceView; ingest_token?: string | null }>(
      `/api/v1/kbs/${kbId}/sources`,
      {
        method: "POST",
        body: JSON.stringify(body),
      },
    ),
  sourceToken: (kbId: string, sourceId: string) =>
    request<{ ingest_token: string | null }>(
      `/api/v1/kbs/${kbId}/sources/${sourceId}/token`,
    ),
  rotateSourceToken: (kbId: string, sourceId: string) =>
    request<{ ingest_token: string }>(
      `/api/v1/kbs/${kbId}/sources/${sourceId}/rotate-token`,
      { method: "POST" },
    ),
  updateSource: (
    kbId: string,
    sourceId: string,
    body: Record<string, unknown>,
  ) =>
    request<{ source: SourceView }>(`/api/v1/kbs/${kbId}/sources/${sourceId}`, {
      method: "PATCH",
      body: JSON.stringify(body),
    }),
  deleteSource: (kbId: string, sourceId: string) =>
    request<{ ok: boolean }>(`/api/v1/kbs/${kbId}/sources/${sourceId}`, {
      method: "DELETE",
    }),
  cleanupMissing: (kbId: string, sourceId: string) =>
    request<{ deleted: number }>(
      `/api/v1/kbs/${kbId}/sources/${sourceId}/missing/cleanup`,
      {
        method: "POST",
      },
    ),
  googleDriveStatus: () =>
    request<{ enabled: boolean }>(`/api/v1/google-drive/status`),
  googleDriveConnect: (kbId: string, sourceId: string) =>
    request<{ url: string }>(
      `/api/v1/kbs/${kbId}/sources/${sourceId}/google-drive/connect`,
      { method: "POST" },
    ),
  googleDriveDisconnect: (kbId: string, sourceId: string) =>
    request<{ ok: boolean }>(
      `/api/v1/kbs/${kbId}/sources/${sourceId}/google-drive/connection`,
      { method: "DELETE" },
    ),
  googleDriveFolders: (kbId: string, sourceId: string, parent?: string) =>
    request<{ folders: { id: string; name: string }[] }>(
      `/api/v1/kbs/${kbId}/sources/${sourceId}/google-drive/folders` +
        (parent ? `?parent=${encodeURIComponent(parent)}` : ""),
    ),
  googleDriveSelect: (
    kbId: string,
    sourceId: string,
    folders: { id: string; name: string }[],
  ) =>
    request<{ source: SourceView }>(
      `/api/v1/kbs/${kbId}/sources/${sourceId}/google-drive/folders`,
      { method: "PUT", body: JSON.stringify({ folders }) },
    ),
  syncSource: (kbId: string, sourceId: string) =>
    request<{ queued: boolean }>(
      `/api/v1/kbs/${kbId}/sources/${sourceId}/sync`,
      {
        method: "POST",
      },
    ),
  sourceRuns: (kbId: string, sourceId: string) =>
    request<{ runs: SyncRun[] }>(
      `/api/v1/kbs/${kbId}/sources/${sourceId}/runs`,
    ),
  documentExtractions: (docId: string) =>
    request<{ facts: ChunkFact[] }>(`/api/v1/documents/${docId}/extractions`),

  /** **Real counts** of each tier of the review queue. Fetched separately from the list: the list has a page limit, counting does not.
   *  The badge used to read the array length, and the endpoint always returns only 100, so 164 showed as 100. */
  review: (
    kbId: string,
    queue: ReviewQueue,
    limit: number,
    offset: number,
    /** Only meaningful for duplicates: filter by the relation of the two sides' types (#428) */
    types: ReviewTypeFilter = "any",
  ) =>
    request<{
      counts: ReviewCounts;
      queue: ReviewQueue;
      /** Only one page of the current tier. Types differ by tier, so the call site narrows by queue */
      items: unknown[];
    }>(
      `/api/v1/kbs/${kbId}/review?queue=${queue}&limit=${limit}&offset=${offset}&types=${types}`,
    ),
  /** One action for a batch of duplicates (#428): each is decided and recorded in the ledger individually, and the reply says item by item whether each succeeded */
  /** `rationale`: the sentence written when a person ruled (0026): what made you decide this. Optional; if written, it goes into
   *  the ledger together with the decision, so the precedent the next adjudicator and agent read is more than the result */
  reviewBatch: (kbId: string, ids: string[], action: "merge" | "keep", rationale?: string) =>
    request<{
      decided: number;
      outcomes: { id: string; error: string | null }[];
    }>(`/api/v1/kbs/${kbId}/review/batch`, {
      method: "POST",
      body: JSON.stringify({ ids, action, rationale: rationale || null }),
    }),
  /** Closing dates carry precision (year | month | day): the digits written set the precision, and the server stores them as given */
  closeFact: (kbId: string, factId: string, validTo: string, precision: string) =>
    request<{ ok: boolean }>(`/api/v1/kbs/${kbId}/facts/${factId}/close`, {
      method: "POST",
      body: JSON.stringify({ valid_to: validTo, valid_to_precision: precision }),
    }),
  resolveConflict: (
    kbId: string,
    conflictId: string,
    body: {
      action: "close" | "retime_new" | "keep" | "reject_new";
      close_at?: string;
      close_at_precision?: string;
      new_from?: string;
      new_from_precision?: string;
    },
  ) =>
    request<{ ok: boolean }>(`/api/v1/kbs/${kbId}/conflicts/${conflictId}`, {
      method: "POST",
      body: JSON.stringify(body),
    }),
  /** All pending items extracted from one sentence of memory (0015). An empty array = this sentence has nothing left waiting on a person */
  pendingForChunk: (kbId: string, chunkId: string) =>
    request<{ items: PendingFactItem[] }>(
      `/api/v1/kbs/${kbId}/review/pending?chunk_id=${chunkId}`,
    ),
  decidePending: (kbId: string, pendingId: string, action: "confirm" | "reject") =>
    request<{ ok: boolean; fact_id?: string; created?: boolean; conflicts?: number }>(
      `/api/v1/kbs/${kbId}/review/pending/${pendingId}`,
      { method: "POST", body: JSON.stringify({ action }) },
    ),
  decideReview: (kbId: string, reviewId: string, action: "merge" | "keep", rationale?: string) =>
    request<{ ok: boolean }>(`/api/v1/kbs/${kbId}/review/${reviewId}`, {
      method: "POST",
      body: JSON.stringify({ action, rationale: rationale || null }),
    }),
  /** Take a position on one data-mapping convention (0011). Changing the status does not delete the row: rejection leaves a trace, and the next exploration round will not suggest it again */
  decideMapping: (
    kbId: string,
    mappingId: string,
    status: "confirmed" | "rejected",
  ) =>
    request<{ ok: boolean }>(
      `/api/v1/kbs/${kbId}/review/mappings/${mappingId}`,
      {
        method: "POST",
        body: JSON.stringify({ status }),
      },
    ),
  /** Run a consistency check. Returns synchronously: it is pure computation, with no model call and no network */
  runConsistencyCheck: (kbId: string) =>
    request<{
      edges: number;
      /** **Zero and zero are not the same**: with no axioms the conclusion is "no criteria", not "no contradiction found" */
      predicates_with_axioms: number;
      found: number;
      inserted: number;
      cleared: number;
      /** How many predicates the cycle search did not finish (#642). When not zero, the cycles on those predicates report only part of the picture */
      cycles_capped: number;
      classes: number;
      /** The ontology's own contradictions are returned **separately**, not added to found: the two counts are not the same kind of thing */
      defects_found: number;
      defects_new: number;
    }>(`/api/v1/kbs/${kbId}/consistency/check`, { method: "POST" }),
  /** A person sets a phrase signature: an attribute and direction, or none (the statement stays in the open graph). The judgment and its recompute task
   *  are submitted together; the reply is 202 with a job id (0051). The typed graph recomputes in the background; when the `review` / `graph`
   *  events arrive, it is done. The job id can also be queried at `/kbs/{id}/jobs/{job_id}` */
  decideAlignmentPhrase: (
    kbId: string,
    bindingId: string,
    property: string | null,
    direction: "forward" | "reverse",
    /** Given only under a state attribute; null = no statement (a statement carrying only one date stays in the open graph) */
    marks: AlignmentMarks | null,
  ) =>
    request<{ ok: boolean; job_id: number; status: "accepted" }>(
      `/api/v1/kbs/${kbId}/review/alignment/phrases/${bindingId}`,
      { method: "POST", body: JSON.stringify({ property, direction, marks }) },
    ),
  /** A person approves or rejects an implication rule: replies 202 with a job id, and the implied facts are computed in the background (0044 decision 3, slice five) */
  decideAlignmentRule: (kbId: string, ruleId: string, approve: boolean) =>
    request<{ ok: boolean; job_id: number; status: "accepted" }>(
      `/api/v1/kbs/${kbId}/review/alignment/rules/${ruleId}`,
      { method: "POST", body: JSON.stringify({ approve }) },
    ),
  /** A person answers an entry left by the erratum agent (0044 decision 7): approve to execute, reject to only record */
  decideErrata: (kbId: string, actionId: string, approve: boolean) =>
    request<{ ok: boolean }>(`/api/v1/kbs/${kbId}/review/errata/${actionId}`, {
      method: "POST",
      body: JSON.stringify({ approve }),
    }),
  /** A person sets a category word: a class, or none. Entities under it that change class have their phrase signature re-judged */
  decideAlignmentKindWord: (kbId: string, kindWord: string, cls: string | null) =>
    request<{ ok: boolean }>(
      `/api/v1/kbs/${kbId}/review/alignment/kind-words/${encodeURIComponent(kindWord)}`,
      { method: "POST", body: JSON.stringify({ class: cls }) },
    ),
  /** Take a position on one ontology defect. **Two ways out**: it never looked at the data, so there is no "the data is wrong" option */
  decideDefect: (
    kbId: string,
    defectId: string,
    resolution: "fixed" | "accepted",
  ) =>
    request<{ ok: boolean }>(`/api/v1/kbs/${kbId}/review/defects/${defectId}`, {
      method: "POST",
      body: JSON.stringify({ resolution }),
    }),
  /** Run inference once (R1). When the switch is off, the backend replies inference_off */
  /** Type resolution: **computes only, writes nothing**. The receipt carries the profile sent to retrieval: when retrieval finds nothing,
   *  the first thing to check is "what did we search with" */
  /** Manual merge: merge source into target. **The direction matters**: source disappears,
   *  its facts move onto target; a merge can be rolled back as a whole (entity_merges keeps the snapshot) */
  mergeEntities: (kbId: string, source: string, target: string, rationale?: string) =>
    request<{ ok: boolean }>(`/api/v1/kbs/${kbId}/entities/merge`, {
      method: "POST",
      body: JSON.stringify({ source, target, rationale: rationale || null }),
    }),
  typeResolutionPreview: (kbId: string) =>
    request<{ items: TypeSuggestion[] }>(
      `/api/v1/kbs/${kbId}/ontology/type-resolution/preview`,
      { method: "POST" },
    ),
  /** Run and commit. Returns the three tiers separately: automatically changed, left to a person, and "none of these" */
  typeResolutionApply: (kbId: string) =>
    request<ResolutionOutcome>(`/api/v1/kbs/${kbId}/ontology/type-resolution`, {
      method: "POST",
    }),
  /** Approve a "coarse class → fine class" pair, and change the entities carried along.
   *  **The class pair is approved, the entities are changed**: approve once, and the same pair no longer goes to manual review */
  approveRefinement: (
    kbId: string,
    body: { from_type_id: string; to_type_id: string; entity_ids: string[] },
  ) =>
    request<{ retyped: number }>(
      `/api/v1/kbs/${kbId}/ontology/type-resolution/approve`,
      { method: "POST", body: JSON.stringify(body) },
    ),
  /** Undo a whole batch: put those entities back in their original classes */
  typeResolutionUndo: (kbId: string, batchId: string) =>
    request<{ reverted: number }>(
      `/api/v1/kbs/${kbId}/ontology/type-resolution/${batchId}`,
      { method: "DELETE" },
    ),
  runInference: (kbId: string) =>
    request<{
      /** Number of compiled rules. When zero, it means "no rules", not "nothing can be inferred" */
      rules: number;
      edges: number;
      derived: number;
      inserted: number;
      /** Premise gone, so it is invalidated along with it */
      invalidated: number;
      /** Number of predicates that hit the single-predicate cap and were not fully inferred */
      capped: number;
    }>(`/api/v1/kbs/${kbId}/inference/run`, { method: "POST" }),
  /** Take a position on one axiom violation. Three ways out; the third is unique to this tier: the definition itself may be wrong */
  decideViolation: (
    kbId: string,
    violationId: string,
    resolution: ViolationResolution,
    opts: { closeAt?: string; closeAtPrecision?: string; factId?: string } = {},
  ) =>
    request<{ ok: boolean }>(
      `/api/v1/kbs/${kbId}/review/violations/${violationId}`,
      {
        method: "POST",
        body: JSON.stringify({
          resolution,
          close_at: opts.closeAt ?? null,
          close_at_precision: opts.closeAtPrecision ?? null,
          fact_id: opts.factId ?? null,
        }),
      },
    ),
  confirmFact: (kbId: string, factId: string) =>
    request<{ ok: boolean }>(`/api/v1/kbs/${kbId}/facts/${factId}/confirm`, {
      method: "POST",
    }),
  rejectFact: (kbId: string, factId: string) =>
    request<{ ok: boolean }>(`/api/v1/kbs/${kbId}/facts/${factId}/reject`, {
      method: "POST",
    }),
  revertMerge: (kbId: string, mergeId: string) =>
    request<{ ok: boolean }>(`/api/v1/kbs/${kbId}/merges/${mergeId}/revert`, {
      method: "POST",
    }),
  reviewSummary: (kbId: string) =>
    request<ReviewSummary>(`/api/v1/kbs/${kbId}/review/summary`),
  /** Answer one agent entry (0025): merge / keep answers a suggestion, revert withdraws one automatic merge,
   *  and merge can also overturn an automatic split. It goes through the person's ruling path and becomes a new precedent */
  agentAnswer: (
    kbId: string,
    decisionId: string,
    action: "merge" | "keep" | "revert",
    rationale?: string,
  ) =>
    request<{ ok: boolean }>(`/api/v1/kbs/${kbId}/review/agent/${decisionId}`, {
      method: "POST",
      body: JSON.stringify({ action, rationale: rationale || null }),
    }),
  reviewHistory: (kbId: string, page: number, per = 20) =>
    request<{ events: ReviewHistoryEvent[]; total: number }>(
      `/api/v1/kbs/${kbId}/review/history?page=${page}&per=${per}`,
    ),

  members: (workspaceId: string) =>
    request<Member[]>(`/api/v1/workspaces/${workspaceId}/members`),
  orgUsers: () => request<OrgUser[]>("/api/v1/users"),
  setMemberRole: (workspaceId: string, userId: string, role: string) =>
    request<{ ok: boolean }>(
      `/api/v1/workspaces/${workspaceId}/members/${userId}`,
      {
        method: "PUT",
        body: JSON.stringify({ role }),
      },
    ),
  removeMember: (workspaceId: string, userId: string) =>
    request<{ ok: boolean }>(
      `/api/v1/workspaces/${workspaceId}/members/${userId}`,
      {
        method: "DELETE",
      },
    ),

  /** Test only the `card` one, and only that item is returned (#1424) */
  testSettings: (workspaceId: string, card: "chat" | "embed" | "ocr" | "transcribe") =>
    request<{
      /** `ok` only when all three request shapes the jobs send pass; `checks` has each one (#1354) */
      chat?: {
        ok: boolean;
        reply?: string;
        error?: string;
        checks?: Record<"plain" | "streaming" | "tools", { ok: boolean; timed_out?: boolean; error?: string }>;
      };
      embed?: { ok: boolean; dim?: number; error?: string };
      ocr?: { ok: boolean; version?: string | null; error?: string };
      transcribe?: { ok: boolean; error?: string };
    }>(`/api/v1/workspaces/${workspaceId}/settings/test?card=${card}`, { method: "POST" }),
  /** The scanned-document reading service and the transcription model are each saved separately: saving them does not touch the chat and embedding columns.
   *  `requeued`: files that were waiting for the missing one; this save put this many back into the processing queue */
  saveOcrSettings: (
    workspaceId: string,
    body: { provider: string; base_url: string; api_key: string; backend: string; model: string },
  ) =>
    request<{ ok: boolean; requeued: number }>(
      `/api/v1/workspaces/${workspaceId}/settings/ocr`,
      { method: "PUT", body: JSON.stringify(body) },
    ),
  saveTranscribeSettings: (
    workspaceId: string,
    body: { base_url: string; api_key: string; model: string },
  ) =>
    request<{ ok: boolean; requeued: number }>(
      `/api/v1/workspaces/${workspaceId}/settings/transcribe`,
      { method: "PUT", body: JSON.stringify(body) },
    ),

  /** Whether single sign-on is configured (0056). If any one of the four environment variables is missing, this is false,
   *  and the login page uses it to decide whether to show the button */
  oidcStatus: () =>
    request<{ enabled: boolean; password_login: boolean }>("/api/v1/auth/oidc/status"),
  /** Which identity provider subject is bound to which account in this deployment, and who bound it (read-only + unbind) */
  oidcIdentities: () =>
    request<{
      issuer: string;
      client_id: string;
      redirect_uri: string;
      identities: OidcIdentity[];
    }>("/api/v1/admin/oidc/identities"),
  /** My own binding. Binding itself goes through `/api/v1/auth/oidc/start?link=1`, which the person completes at the identity provider */
  oidcMe: () =>
    request<{ enabled: boolean; linked: boolean; subject?: string | null }>(
      "/api/v1/auth/oidc/me",
    ),
  oidcUnlinkMe: () =>
    request<{ ok: boolean }>("/api/v1/auth/oidc/me", { method: "DELETE" }),
  /** Admin unbind. There is no endpoint to bind on someone else's behalf: that would be a way to log in as another person */
  oidcUnlink: (userId: string) =>
    request<{ ok: boolean }>(`/api/v1/admin/oidc/identities/${userId}`, {
      method: "DELETE",
    }),
};

/** RAG chat: streamed over SSE. Returns an abort function. */
export const conversationsApi = {
  /** **Searchable and pageable**: titles repeat (asking the same question twice makes a duplicate), and sessions beyond the fixed hundred
   *  do not exist in the session UI at all. Search covers two places, the title and the message body: what people remember is often
   *  the sentence they asked, not the title */
  list: (kbId: string, q = "", limit = 30, offset = 0) => {
    const p = new URLSearchParams({
      limit: String(limit),
      offset: String(offset),
    });
    if (q.trim()) p.set("q", q.trim());
    return request<{ conversations: ConversationRow[]; total: number }>(
      `/api/v1/kbs/${kbId}/conversations?${p}`,
    );
  },
  /** Rename. The title was originally taken automatically from the first sentence, and it is normal for a conversation to drift off course */
  rename: (kbId: string, id: string, title: string) =>
    request<{ ok: boolean }>(`/api/v1/kbs/${kbId}/conversations/${id}`, {
      method: "PATCH",
      body: JSON.stringify({ title }),
    }),
  detail: (kbId: string, id: string) =>
    request<{ messages: ConversationMessage[] }>(
      `/api/v1/kbs/${kbId}/conversations/${id}`,
    ),
  stop: (kbId: string, id: string, generationId: string) =>
    request<{ ok: boolean }>(`/api/v1/kbs/${kbId}/chat/${id}/stop`, {
      method: "POST",
      body: JSON.stringify({ generation_id: generationId }),
    }),
  remove: (kbId: string, id: string) =>
    request<{ ok: boolean }>(`/api/v1/kbs/${kbId}/conversations/${id}`, {
      method: "DELETE",
    }),
};

export interface ChatHandlers {
  /** `messageId`: the id this question was saved under. If the answer failed halfway, it is used to answer again (#936); old servers do not send it */
  onConversation: (id: string, messageId?: string, generationId?: string) => void;
  onSources: (s: Source[]) => void;
  onStep: (s: ChatStep) => void;
  onDelta: (text: string) => void;
  onDone: (stopped: boolean) => void;
  onError: (message: string, error?: ApiError) => void;
  /** Attach to an answer that is already running: this is its state right now. **Overwrites, does not append** */
  onSnapshot?: (s: { generation_id: string; content: string; steps: ChatStep[]; sources: Source[] }) => void;
  /** No generation is running for this session: the most common outcome, and not an error */
  onIdle?: () => void;
}

/** Attach to an answer being generated (after a page refresh, this path is used).
 *
 *  It reads the same event stream as `streamChat`, with the snapshot at the start added. */
export function reattachChat(
  kbId: string,
  conversationId: string,
  handlers: ChatHandlers,
): () => void {
  return consumeChatStream(
    (signal) =>
      fetch(`/api/v1/kbs/${kbId}/conversations/${conversationId}/stream`, {
        credentials: "include",
        signal,
      }),
    handlers,
    true,
  );
}

export function streamChat(
  kbId: string,
  /** `retry_message_id`: answer again the last question of this conversation that has no answer (#936), without saving it again */
  body: { conversation_id?: string; message: string; retry_message_id?: string },
  handlers: ChatHandlers,
): () => void {
  return consumeChatStream(
    (signal) =>
      fetch(`/api/v1/kbs/${kbId}/chat`, {
        method: "POST",
        credentials: "include",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
        signal,
      }),
    handlers,
  );
}

/** There is only one way to read SSE: the ways of starting the request differ, but what happens after it is received is entirely the same. */
function consumeChatStream(
  open: (signal: AbortSignal) => Promise<Response>,
  handlers: ChatHandlers,
  allowIdle = false,
): () => void {
  const controller = new AbortController();
  let reader: ReadableStreamDefaultReader<Uint8Array> | undefined;
  let terminal = false;
  const fail = (message: string, error?: ApiError) => {
    if (terminal || controller.signal.aborted) return;
    terminal = true;
    handlers.onError(message, error);
  };
  (async () => {
    try {
      const res = await open(controller.signal);
      if (controller.signal.aborted) {
        await res.body?.cancel();
        return;
      }
      if (!res.ok || !res.body) {
        let message = res.statusText;
        let code: string | undefined;
        try {
          const body = (await res.json()) as Refusal;
          code = body.code;
          message = refusalMessage(body, message);
        } catch { /* keep the HTTP status */ }
        fail(message, new ApiError(res.status, message, code));
        return;
      }
      reader = res.body.getReader();
      const decoder = new TextDecoder();
      let trailingCr = false;
      const parser = createParser({ onEvent: ({ event, data: value }) => {
        if (terminal || controller.signal.aborted) return;
        if (event === "done") {
          const stopped = JSON.parse(value).stopped === true;
          terminal = true;
          handlers.onDone(stopped);
        }
        else if (event === "error") fail(streamFailure(value));
        else if (event === "idle") {
          if (allowIdle) { terminal = true; handlers.onIdle?.(); }
          else fail(S.ask.streamInterrupted);
        } else if (event === "conversation") {
          const frame = JSON.parse(value);
          handlers.onConversation(frame.id, frame.message_id, frame.generation_id);
        }
        else if (event === "sources") handlers.onSources(JSON.parse(value));
        else if (event === "step") handlers.onStep(JSON.parse(value));
        else if (event === "delta") handlers.onDelta(JSON.parse(value).text);
        else if (event === "snapshot") handlers.onSnapshot?.(JSON.parse(value));
      } });
      while (!terminal && !controller.signal.aborted) {
        const { done, value } = await reader.read();
        if (done) {
          // v3 holds a final CR until the next character confirms its line ending.
          if (trailingCr) parser.feed("\n");
          break;
        }
        if (controller.signal.aborted) break;
        const chunk = decoder.decode(value, { stream: true });
        if (chunk) trailingCr = chunk.endsWith("\r");
        parser.feed(chunk);
      }
      // EOF never dispatches an incomplete frame and is not an application done.
      fail(S.ask.streamInterrupted);
    } catch (e) {
      fail(e instanceof SyntaxError ? S.ask.streamInterrupted : String(e));
    } finally {
      if (reader) {
        try { await reader.cancel(); } catch { /* terminal/abort already decided */ }
        reader.releaseLock();
      }
    }
  })();
  return () => {
    controller.abort();
    void reader?.cancel().catch(() => {});
  };
}
