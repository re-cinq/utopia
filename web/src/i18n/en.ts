// English language pack. This one is the **authority on structure**: `Strings = typeof en`, and the other packs are shaped by it,
// a missing entry fails to compile (see docs/decisions/0004).
//
// When adding new copy, add it here first, then fill in the other packs. Doing it the other way round gives a type error, which is the intent.
export const en = {
  hinted: {
    openChain: "Open chain",
    children: (n: number) => `Contains (${n})`,
    loadChildren: "Show what it contains",
    noNested: "Nothing nested",
    within: "Within",
    openLink: "Open link",
    ranges: "Ranges",
  },
  chain: {
    title: "Chain",
    from: (name: string) => `From ${name}`,
    openInGraph: "Open in graph",
    emptyNone: (label: string) => `Nothing along ${label}`,
    emptyUnloaded: "Loading…",
    noStart: "Pick an entity in the graph to follow its chain.",
    noHints: "This entity's type declares no chain.",
  },
  expressionDraft: {
    title: "Expression draft exploration",
    unsaved: "Unsaved draft only. Nothing here is saved to the knowledge base. Unit compatibility is not checked.",
    undeclared: "Undeclared",
    attribute: "Attribute",
    constant: "Number",
    add: "Add (+)",
    sub: "Subtract (−)",
    mul: "Multiply (×)",
    div: "Divide (÷)",
    expression: "Expression",
    left: "Left operand",
    right: "Right operand",
    kind: "Node type",
    depthLimit: "Depth limit reached: choose an attribute or a number.",
    choose: "Search and choose…",
    missing: "Attribute no longer available",
    loading: "Loading attributes and rules…",
    loadError: "Could not load this knowledge base. Check your access and retry.",
    retry: "Retry",
    empty: "This knowledge base has no attributes yet.",
    conclusion: "Conclusion",
    condition: "Condition",
    existing: "Explore an existing expression",
    unsupported: "This expression has an unsupported shape. It has not been converted. Use the existing rule editor for metadata changes.",
    preview: "Draft preview — not saved",
    incomplete: "Complete every operand with an available attribute or a finite number to preview.",
    reset: "Start a new draft",
    count: (n: number) => `${n} attributes loaded from this knowledge base`,
  },
  app: {
    name: "Utopia",
    // Adapted from the last sentence of the whole of *Utopia* (Burnet 1684 translation):
    // "there are many things in the commonwealth of Utopia that I rather wish,
    //  than hope, to see followed in our governments."
    // Change I to We, drop the interjection comma, and leave the object of 'to see' blank.
    tagline: "We rather wish than hope to see.",
    taglineSource: "— Thomas More, 1516",
    siteUrl: "https://utopia.bi",
    docsUrl: "https://utopia.bi/docs",
  },
  /** Wording for server validation errors. key = the code the server sends; a missing entry falls back to the English original, so nothing breaks.
      Contract guards (only reachable by calling the wrong API) are deliberately not here: their readers are developers */
  err: {
    bad_case_expectation: "Choose a property and direction, or expect no binding.",

    bad_email: "That doesn't look like an email address.",
    password_too_short: "Password must be at least 8 characters.",
    bad_display_name: "Display name must be 1-64 characters.",
    wrong_password: "Current password is incorrect.",
    registration_closed:
      "Sign-up is closed on this deployment — ask an administrator for an account.",
    password_login_disabled: "Password sign-in is turned off on this deployment. Continue with SSO.",
    no_chat_model:
      "No chat model configured yet. Set one under Administration → Models.",
    // Turn in the chat stream that failed to answer: the code the `error` frame carries (the server's English sentence is only for logs)
    answer_empty: "The model returned an empty answer. Ask again.",
    answer_tool_text: "The model wrote a tool call as text instead of answering. Ask again.",
    answer_tool_call: "The model kept calling tools after its lookups ran out. Ask again.",
    answer_too_long: "The answer went past the size limit and was not kept.",
    answer_failed: "The answer could not be completed. Ask again.",
    answer_not_saved: "The answer could not be saved. Try again later.",
    search_failed: "The documents could not be searched.",
    mapping_search_failed: "Relevant data mappings could not be retrieved.",
    stream_ended:
      "The answer stream ended unexpectedly. Reopen the conversation to check its status.",
    stream_lagged: "This page fell behind the answer stream. Reopen the conversation.",
    model_out_of_credit:
      "The model account cannot pay for this request. Top it up, or pick another endpoint under Administration → Models.",
    model_rate_limited: "The model endpoint is rate limiting. Wait a moment and ask again.",
    model_unavailable: "The model endpoint is unavailable right now. Try again shortly.",
    model_unreachable:
      "The model endpoint gave no usable answer. Check its address under Administration → Models.",
    context_too_long: "This conversation is too long for the model. Start a new conversation.",
    model_rejected:
      "The model endpoint refused the request. Check the model settings under Administration → Models.",
    // Regenerate (#936) was rejected. Asking a new question while generating (#961) is also a 409; both share one code
    answer_running: "An answer is still being written in this conversation. Wait for it to finish.",
    retry_answered: "That question already has an answer, or the conversation went on after it.",
    retry_not_question: "Only a question can be answered again.",
    retry_needs_conversation: "A retry needs the conversation its question is in.",
    bad_upload: "That upload could not be read.",
    upload_read_failed: "The file could not be read to the end.",
    no_files: "No file was attached.",
    upload_needs_folder: "Files can only be uploaded into a folder source.",
    empty_file: "That file is empty.",
    file_too_large: "That file is too large — the limit is 8 MB.",
    bad_ontology_file: "That is not a readable OWL or RDFS file.",
    bad_name: "Name must be 1-64 characters.",
    default_kb_open:
      "The default knowledge base stays open to everyone — create a separate one for private work.",
    default_kb_undeletable: "The default knowledge base cannot be deleted.",
    last_owner_demote: "This is the last owner — promote someone else first.",
    last_owner_remove: "This is the last owner — hand ownership over first.",
    key_required: "A key is required.",
    forms_required: "Pick at least one phrase this relation covers.",
    bad_key:
      "Keys are lowercase, letters digits and underscores only, up to 40 characters.",
    self_parent: "A class cannot be its own parent.",
    parent_cycle:
      "That class is already below this one — the hierarchy would loop.",
    bad_lang: "Pick a supported language.",
    attr_needs_class: "An attribute has to belong to a class.",
    builtin_name_attribute: "The name attribute is built in and cannot be edited.",
    attr_has_no_link:
      "An attribute has no inverse and no super-property — its value is a literal, not something to point back from.",
    link_target_is_attr:
      "Pick a relation, not an attribute — an attribute's value is a literal.",
    sub_property_self: "A relation cannot be its own super-property.",
    unknown_relation: "That relation is not in this knowledge base.",
    entity_name_required: "Name cannot be empty.",
    entity_name_too_long: "Name is too long — 100 characters at most.",
    unknown_entity_type: "That class is not in this ontology.",
    nothing_to_update: "Nothing to change.",
    self_merge: "An entity cannot be merged into itself.",
    new_from_required: "Enter the new start date for this fact.",
    close_at_required:
      "Pick the date this fact ended — the new one does not say when it started.",
    empty_query: "Type something to search for.",
    no_data_sources: "No databases are mounted on this knowledge base.",
    // Authorization is per workspace (0014): the source is not authorized for the workspace the library belongs to
    source_not_granted:
      "This data source is not granted to this workspace. Ask a deployment admin to grant it in Administration → Data sources.",
    memory_source_permanent:
      "The Memory source is part of the knowledge base and stays.",
    google_drive_not_configured:
      "Google Drive is not set up on this server. An administrator sets UTOPIA_GOOGLE_DRIVE_CLIENT_ID, _CLIENT_SECRET and _REDIRECT_URI.",
    google_drive_connected_by_someone_else:
      "This source reads someone else's Google Drive. Disconnect it first, then connect your own.",
    google_drive_not_connector:
      "Only the person who connected this Google Drive can choose its folders.",
    google_drive_not_connected: "Connect a Google account first.",
    google_drive_unavailable: "Google Drive could not be read.",
    google_drive_busy: "Too many Google Drive connections are in progress. Try again in a few minutes.",
    google_drive_too_many_folders: "Choose at most 50 folders. Choose a common parent instead.",
    google_drive_invalid_folder: "That is not a Google Drive folder id.",
    source_name_required: "Give this source a name.",
    bad_cron: "That cron expression could not be parsed.",
    bad_cron_fields:
      "A cron expression has five fields: minute hour day month weekday.",
    ds_name_required: "Give this data source a name.",
    only_postgres: "Only PostgreSQL is supported for now.",
    bad_conn_string: "A connection string starts with postgres://",
    // Refreshing the structure failed; the engine's own words go in detail (the sentence in brackets)
    schema_sync_failed:
      "The schema could not be read — the engine's own message is in parentheses. Check " +
      "the catalog, schema and account privileges in the connection string, then try Refresh schema again.",
    concurrency_range: "Pick a number between 1 and 256.",
    inference_off:
      "Materialized inference is off for this knowledge base. Turn it on in Settings.",
    bad_resolution: "That is not a valid decision.",
    interval_inverted:
      "The end is before the start. Give an end after the start.",
    empty_state_span:
      "A state that ends where it starts holds at no moment. Give an end after the start, or leave it open.",
    unknown_marks: "A single date marks the start, the end, or neither.",
    marks_needs_state:
      "Only a state property says what a single date marks. Events and timeless properties don't.",
    // Adopting a proposal (0061 decision 3): it collided with another write and was rolled back as a whole; what the reuse would write was already decided separately by a person
    alignment_busy: "Something else is updating the same items. Nothing was saved; try again shortly.",
    proposal_stale:
      "Nothing in this proposal is left to adopt: each of its phrases or kind words is decided differently by a person or no longer appears in any statement. Reject it to clear it.",
  },
  /** Extra machine detail (such as the cron parser's own words) is appended after the wording */
  errDetail: (msg: string, detail: string) => `${msg} (${detail})`,
  toast: {
    saved: "Saved",
    created: "Created",
    deleted: "Deleted",
    added: "Added to the ontology",
    // Every copy failure gets this one sentence: the page has no clipboard API, and the browser does not allow the old copy method either
    copyFailed: "Could not copy. Select the text and copy it by hand.",
  },
  account: {
    /* Wordmark in the account area: Persona, your mask of identity in this city */
    brand: "Utopia Persona",
    /* Short name for the web page title: `Utopia | Persona` */
    titleTag: "Persona",
    profile: "Profile",
    cancel: "Cancel",
    colLastUsed: "Last used",
    colCreated: "Created",
    pickBases: "Search bases…",
    administration: "Administration",
    adminChip: "Admin",
    backToApp: "← Back to app",
    profileTitle: "Profile",
    displayName: "Display name",
    email: "Email",
    save: "Save",
    passwordTitle: "Change password",
    currentPassword: "Current password",
    newPassword: "New password (min. 8 characters)",
    changePassword: "Update password",
    passwordChanged: "Password updated",
    sso: {
      title: "Single sign-on",
      hint:
        "Link your identity provider account to sign in with SSO. You'll be sent to the provider " +
        "to confirm it's you.",
      linkedAs: (subject: string) => `Linked as ${subject}`,
      notLinked: "Not linked.",
      link: "Link identity",
      unlink: "Unlink",
      unlinkTitle: "Unlink single sign-on?",
      unlinkHint:
        "You won't be able to sign in with SSO until you link again. Sessions already open stay signed in.",
      linked: "Identity linked. You can now sign in with SSO.",
      unlinked: "Identity unlinked.",
    },
    avatarHint: "Avatars are generated from your name for now.",
    language: "Language",
    theme: "Theme",
    themeNames: { dark: "Dark", light: "Light", system: "Follow system" } as Record<"dark" | "light" | "system", string>,
    kbsNav: "Knowledge bases",
    kbsTitle: "Knowledge bases",
    kbsFilter: "Filter by name…",
    kbOpen: "Open",
    kbRestricted: "Restricted",
    kbNameLabel: "Knowledge base",
    kbRoleLabel: "My role",
    kbDocsLabel: "Docs",
    kbMembersLabel: "Members",
    kbAccessLabel: "Access",
    addedBy: (name: string, date: string) => `Added by ${name} · ${date}`,
    joinedOn: (date: string) => `Joined ${date}`,
    openToEveryone: "Open to everyone",
    deploymentAdmin: "Deployment admin",
    openKb: "Open",
    kbSettingsBtn: "Settings",
    /* Personal token page (0014 / 0016 A2): keys for agents, acting as this person */
    tokensNav: "Agents & tokens",
    tokensTitle: "Personal access tokens",
    tokensHint: "A token lets an agent reach Utopia over MCP as you, with no more access than you have. Read-only by default, limited to the bases you pick, revocable here.",
    newToken: "New token",
    tokenName: "Name",
    tokenNamePlaceholder: "My laptop",
    tokenScope: "Scope",
    scopeRead: "Read",
    scopeWrite: "Write",
    scopeHint: "A ceiling, not a grant — a token never does more than you can.",
    tokenKbs: "Knowledge bases",
    kbsAllHint: "Nothing selected means every base you can open.",
    tokenExpires: "Expires",
    expiresDays: (n: number) => `${n} days`,
    expiresNever: "Never",
    issueToken: "Create token",
    issuedTitle: "Copy it now. It will not be shown again.",
    issuedHint:
      "Utopia keeps only a hash of it. If you lose it, revoke it here and create another.",
    copy: "Copy",
    copied: "Copied",
    mcpTitle: "MCP client configuration",
    mcpHint:
      "For Claude Code, Claude Desktop and other Streamable HTTP clients. This snippet connects to one knowledge base, even if the token allows all bases. Check the selected base before copying. Other clients spell the keys differently.",
    mcpBase: "Base",
    yourTokens: "Your tokens",
    noTokens: "No tokens yet.",
    allBases: "All bases",
    nBases: (n: number) => `${n} base${n === 1 ? "" : "s"}`,
    lastUsed: (d: string) => `last used ${d}`,
    neverUsed: "never used",
    expiresOn: (d: string) => `expires ${d}`,
    noExpiry: "no expiry",
    createdOn: (d: string) => `created ${d}`,
    revoke: "Revoke",
    revokeConfirm: "Revoke?",
    revokedOn: (d: string) => `revoked ${d}`,
    tokenDone: "Done",
    roleNames: {
      owner: "Owner",
      admin: "Admin",
      editor: "Editor",
      viewer: "Viewer",
    } as Record<string, string>,
  },
  docs: {
    /* Wordmark in the docs area: Charter, the city's founding charter, same font and size as the main wordmark */
    brand: "Utopia Charter",
    backTitle: "Back to Utopia",
    searchPlaceholder: "Search the docs…",
    noResults: "No matches.",
  },
  // Alert wording lives on the client and is looked up by kind; the server sends only kind and detail,
  // and does not produce display copy (docs/decisions/0004)
  alerts: {
    title: "Alerts",
    badgeLabel: "Alerts",
    empty: "Nothing needs attention",
    emptyHint:
      "Ingestion, sync and model failures show up here instead of only in the logs.",
    markAllRead: "Mark all read",
    close: "Close",
    // Say clearly what is searched: the title wording lives on the client and the server cannot search it,
    // so don't let people think typing 'sync failed' will return results
    searchPlaceholder: "Search sources, knowledge bases, errors",
    noMatch: "Nothing matches",
    andMore: (n: number) => `and ${n} more`,
    system: "System",
    partialRows: (sheet: string, read: number, total: number) =>
      `${sheet}: read ${read} of ${total} rows`,
    partialRecords: (read: number, total: number) =>
      `read ${read} of ${total} records`,
    // kind → one sentence saying what happened. The second sentence says what to do; that is what alerts have beyond logs.
    // **An alert is one failure**, so the title carries no count
    kinds: {
      "source.sync_failed": {
        title: "A source failed to sync",
        hint: "Nothing new came in from it. Check the source's settings.",
      },
      "data_source.schema_sync_failed": {
        title: "A data source is mounted, but its schema is not",
        hint: "Ask cannot see which tables exist, so it will guess column names. Check the connection, then use Refresh schema.",
      },
      "mapping.exploration_empty": {
        title: "Mapping exploration proposed nothing",
        hint: "The model read the mounted schema but found no metric or dimension to propose. Refresh the schema under Data mapping > Data sources, add column comments if you can, then run Explore again.",
      },
      "llm.unreachable": {
        title: "The model endpoint gave no usable answer",
        hint: "Extraction and embedding are stopped. Check the endpoint URL in Administration → Models.",
      },
      "llm.rate_limited": {
        title: "The model endpoint is rate limiting us",
        hint: "Documents were retried and still turned away, so some are missing facts. Lower model concurrency in Administration, or raise the quota on the account.",
      },
      "llm.out_of_credit": {
        title: "The model account cannot pay for requests",
        hint: "Extraction and embedding are stopped and will not resume on their own. Top up the account, or set an endpoint that can serve in Administration → Models.",
      },
      "document.needs_reader": {
        title: "A file needs a model to be read",
        hint: "Scans and images need a document-reading service, and recordings need a transcription model that labels speakers. Each line says what was missing. The file is kept and nothing was read from it yet; it is read as soon as the reader is saved under Administration → Models.",
      },
      "document.contents_truncated": {
        title: "A file was only partly read",
        hint: "A parser safety cap was reached. The line says which sheet or record range was left out; search and answers do not contain the omitted part.",
      },
      "governance.tripped": {
        title: "The agent stopped deciding on its own",
        hint: "Two of its merges were reverted within seven days, so the switch went off. Look at what it did under Review → Agent, then turn it back on in the base settings when you want it to resume.",
      },
    } as Record<string, { title: string; hint: string } | undefined>,
    // Unseen kinds must still display: the frontend may not be updated yet when a new alert source goes live
    unknownKind: (kind: string) => kind,
    /** Keep running after a fix (#216) */
    runAgain: "Run those again",
    requeued: (n: number) =>
      n === 1 ? "1 job back in the queue" : `${n} jobs back in the queue`,
  },

  kbScope: {
    deniedTitle: "You don't have access to this knowledge base",
    deniedBody:
      "The link points at a base you can't open. Ask whoever shared it to grant you access, or pick one of your own.",
    missingTitle: "This knowledge base is gone",
    missingBody:
      "It was deleted, or the link was mistyped. Your own bases are listed below.",
    myKbs: "My knowledge bases",
  },
  nav: {
    workspaceLabel: "Workspace",
    kbLabel: "Knowledge base",
    findKb: "Find a knowledge base…",
    noKbMatch: "No knowledge base matches",
    ask: "Chat",
    askHint: "Converse with your knowledge base — it can remember",
    search: "Search",
    searchHint: "Hybrid search",
    graph: "Graph",
    graphHint: "Entities & timelines",
    library: "Library",
    libraryHint: "Documents & ingestion",
    settings: "Settings",
    settingsHint: "Models & members",
    signOut: "Sign out",
    docs: "Docs",
    loading: "Loading…",
    serverUnreachable:
      "Punishment 500: Utopia has gone quiet — it isn't answering.",
    notFound: "Punishment 404: You are lost in Utopia.",
    returnHome: "Return home",
    reportIssue: "Report an issue",
    refresh: "Refresh",
  },
  /* Shared copy for empty states (#313).
     **State in one sentence; the action is on the button; do not explain the mechanism.** This used to be a whole paragraph:
     'The graph is empty. First go to Admin → Models to configure the chat model, then upload documents in the Library,
     entities and relations are extracted automatically.' That one sentence packed in the state, two actions and a mechanism,
     while someone staring at an empty page wants to know where to click next. The mechanism belongs in the docs, not the empty state */
  steps: {
    noModel: "No chat model yet.",
    /* Configuring models needs a workspace admin; everyone else can only ask someone, so the wording differs and no button is shown */
    noModelAsk: "No chat model yet. Ask an administrator.",
    configureModel: "Configure model",
    noDocs: "No documents yet.",
    upload: "Upload a document",
    processing: (n: number) => `Reading ${n} document${n === 1 ? "" : "s"}…`,
    viewProgress: "View progress",
    someFailed: (n: number) => `${n} document${n === 1 ? "" : "s"} failed.`,
    /* Documents are all in and extraction has finished too, yet the graph is still empty: not 'not started yet', it means nothing was extracted */
    nothingExtracted: "Nothing extracted yet.",
    openLibrary: "Open Library",
  },
  login: {
    signIn: "Sign in",
    signUp: "Sign up",
    displayName: "Display name",
    email: "Email",
    password: "Password (min. 8 characters)",
    submitting: "One moment…",
    createAccount: "Create account",
    networkError: "Network error, please try again",
    orDivider: "or",
    ssoButton: "Continue with SSO",
    ssoErrors: {
      unlinked:
        "This identity isn't linked to an account. Sign in with your password and link it from your account page.",
      unlinked_sso_only:
        "This identity can't sign in to this deployment. Ask an administrator for access.",
      cancelled: "Single sign-on was cancelled.",
      denied: "The identity provider's answer couldn't be verified.",
      session: "The sign-in started in a different browser session. Try again.",
      expired: "The sign-in took too long. Try again.",
      busy: "Too many sign-ins are in progress. Try again in a moment.",
      unavailable: "Single sign-on is unavailable right now.",
      invalid: "The sign-in request was incomplete. Try again.",
      taken: "That identity is already linked to another account.",
      already_linked: "Your account already has a linked identity. Unlink it first.",
    } as Record<string, string>,
    ssoErrorOther: "Single sign-on failed. Try again.",
    // Common consent phrasing: By continuing, you agree to the <Terms> and acknowledge the <Privacy>.
    agreePrefix: "By continuing, you agree to the ",
    agreeAnd: " and acknowledge the ",
    agreeSuffix: ".",
    githubUrl: "https://github.com/deeplethe/utopia",
  },
  legal: {
    privacyTitle: "Privacy policy",
    termsTitle: "Terms of use",
    backToSignIn: "← Back to sign in",
    privacy: {
      title: "Privacy policy",
      note: "Default text bundled with Utopia. The organization operating this deployment may replace it with its own policy.",
      sections: [
        {
          h: "A self-hosted platform",
          body: [
            "Utopia runs entirely on infrastructure chosen by the organization that deployed it (the operator). The Utopia project has no access to this deployment: the software sends no telemetry, no analytics and no crash reports to anyone.",
          ],
        },
        {
          h: "What this instance stores",
          body: ["Everything below lives on the operator's own servers:"],
          bullets: [
            "Account details — your email address, display name and a hash of your password.",
            "Content — uploaded documents, text extracted from them, search indexes and embeddings, and the knowledge graph (entities, relations and their sources) built from that content.",
            "Activity — your conversations with the assistant, review decisions and ingestion logs, kept so the features that need them can work.",
          ],
        },
        {
          h: "Where data can leave this server",
          body: [
            "If the operator configures an external model provider for chat or embeddings, excerpts of documents and your messages are sent to that provider to answer questions and index content. Which provider — or whether a fully local model is used — is a deployment setting. Nothing else is sent anywhere.",
          ],
        },
        {
          h: "Who can see what",
          body: [
            "Access follows knowledge-base roles: viewers see the knowledge bases they were granted, editors can change content, admins manage members and settings. Deployment administrators can create accounts and see the user list.",
          ],
        },
        {
          h: "Retention and deletion",
          body: [
            "Deleting a document removes it from the base and retires the facts that had no other source; facts with another source keep it as provenance. The content is kept so the deletion can be undone; a deleted document can be restored, and a re-upload of the same file restores it too. A knowledge-base admin can purge a deleted document, which removes its stored content for good. Deleting a knowledge base permanently removes its documents, graph and sources.",
          ],
        },
        {
          h: "Questions",
          body: [
            "This deployment is run by your organization. For questions about how your data is handled here, contact its administrator.",
          ],
        },
      ],
    },
    terms: {
      title: "Terms of use",
      note: "Default text bundled with Utopia. The organization operating this deployment may replace it with its own terms.",
      sections: [
        {
          h: "About these terms",
          body: [
            "This instance of Utopia is operated by the organization that deployed it, not by the Utopia project. Your use of it is governed by that organization's own policies; these default terms cover the basics until the operator replaces them.",
          ],
        },
        {
          h: "Your account",
          body: [
            "Keep your credentials to yourself. Administrators may create, suspend or remove accounts in line with the operator's policies.",
          ],
        },
        {
          h: "Acceptable use",
          body: [],
          bullets: [
            "Upload only content you are authorized to store and share within your organization.",
            "Respect access levels: do not attempt to view or change knowledge bases beyond the roles you were granted.",
            "Do not use the platform to store or spread unlawful content.",
          ],
        },
        {
          h: "AI-generated answers",
          body: [
            "Answers from the assistant are generated from your organization's documents by a language model, with citations. They can be wrong or incomplete — verify against the cited sources before relying on them.",
          ],
        },
        {
          h: "The software",
          body: [
            "Utopia is open-source software provided “as is”, without warranty of any kind. Responsibility for operating this deployment — including backups, availability and compliance — lies with the operator.",
          ],
        },
      ],
    },
  },
  library: {
    /** Deletion is a tombstone (#268): say how many facts were voided, and offer undo */
    deletedWithFacts: (n: number) =>
      n === 0
        ? "Document deleted"
        : n === 1
          ? "Document deleted · 1 fact retired with it"
          : `Document deleted · ${n} facts retired with it`,
    undo: "Undo",
    restored: "Document restored",
    /** The 'Deleted' view and real deletion (#268, second half) */
    deleted: "Deleted",
    colDeleted: "Deleted",
    restore: "Restore",
    purge: "Purge",
    purgeTitle: "Purge this document?",
    purgeHint: (name: string) =>
      `“${name}” is deleted, and its content is still stored so the deletion can be undone. ` +
      "Purging removes the stored file, its chunks and its evidence quotes for good. " +
      "The facts it retired stay retired, and the deletion stays on record. This cannot be undone.",
    purgeConfirm: "Purge",
    purged: "Content purged",
    deletedEmpty: "Nothing deleted. Deleted documents wait here until they are restored or purged.",
    title: "Library",
    upload: "Upload files",
    uploading: "Uploading…",
    uploadFailed: "Upload failed",
    dropHint: "Drag files here, or click “Upload files”",
    formats:
      "PDF · Word · Excel · PowerPoint · Markdown · HTML · TXT · CSV and more",
    emptyPull: "No documents yet — they arrive when this source syncs.",
    filterPlaceholder: "Filter by name",
    filterNoMatch: "No documents match your filter.",
    anyStatus: "Any extraction state",
    statusFailed: "Failed",
    statusDone: "Extracted",
    statusQueued: "Queued",
    statusExtracting: "Extracting",
    statusNone: "Not extracted",
    retryFailed: (n: number) => `Retry ${n} failed`,
    retryQueued: (n: number) => `${n} queued for extraction`,
    colFile: "File",
    colStatus: "Status",
    colGraph: "Graph",
    colChunks: "Chunks",
    colSize: "Size",
    colSource: "Source",
    delete: "Delete",
    extract: "Extract",
    reExtract: "Re-extract",
    reprocess: "Reprocess",
    // Extraction progress (aggregated in the current view, SSE drives the refresh)
    extractProgress: (done: number, total: number) =>
      `Extracting · ${done} / ${total}`,
    // Failure details: the chip is clickable, not only a tooltip any more
    errorTitle: "Failure details",
    errorParse: "Ingestion pipeline",
    errorGraph: "Graph extraction",
    copyError: "Copy",
    errorCopied: "Error copied",
    /* Extraction dropped: a fact was extracted but could not be landed. Previously this was completely silent: something was missing from the graph and nobody could say what */
    dropsChip: (n: number) => `${n} dropped`,
    dropsTitle: "Facts that did not land",
    dropsNote:
      "These were extracted from the document but blocked on the way in. " +
      "Each line says why, and how many.",
    dropsExample: "e.g.",
    dropReason: {
      object_missing: "Relation had no object",
      malformed_item: "The model's item did not fit the schema",
      truncated_reply: "The model's reply was cut off",
      quote_not_in_chunk: "Kept, but the quoted sentence is not in the text verbatim",
      time_not_in_quote: "A time mention's words are not in the text",
      unknown_ref: "The item points at nothing in the reply",
      chunk_unextracted: "A passage the endpoint could not answer for",
      phrase_is_value: "Kept, but the phrase is the value itself",
      phrase_is_subject: "Kept, but the phrase is the subject's own name",
      object_is_value: "Kept, with the object read as a value",
      attr_datatype: "Kept as an open statement, but the value cannot be read as the attribute's datatype",
    } as Record<string, string>,
    // Re-extracting a source: not dangerous, just slow and costly; a light confirmation, with the copy stating cost and what is kept
    reExtractSource: "Re-extract",
    reExtractTitle: "Re-extract this source?",
    reExtractHint: (n: number, name: string) =>
      `All ${n} ready document${n === 1 ? "" : "s"} in “${name}” go through the extraction model again. ` +
      `Existing merges, review decisions and confirmed facts are preserved.`,
    reExtractConfirm: "Re-extract",
    queuedDocs: (n: number) => `${n} document${n === 1 ? "" : "s"} queued`,
    // Rebuilding the whole library: destructive; confirmation by typing
    rebuild: "Rebuild graph",
    rebuildTitle: "Rebuild the knowledge graph?",
    rebuildHint: (docs: number, name: string) =>
      `Type “${name}” to confirm. Every entity, fact, merge and pending review in this knowledge base ` +
      `is permanently removed, then all ${docs} document${docs === 1 ? "" : "s"} are re-extracted from scratch. ` +
      `Documents, search indexes and the ontology are untouched; the decision ledger is kept.`,
    rebuildConfirm: "Rebuild permanently",
    rebuildDone: (e: number, f: number, q: number) =>
      `Cleared ${e} entities and ${f} facts · ${q} documents queued`,
    status: {
      pending: "Queued",
      parsing: "Parsing",
      indexing: "Indexing",
      embedding: "Embedding",
      ready: "Ready",
      failed: "Failed",
    },
    graphStatus: {
      none: "—",
      queued: "Queued",
      extracting: "Extracting",
      done: "Done",
      failed: "Failed",
      skipped: "Not extracted",
    },
    sources: "Sources",
    allDocs: "All documents",
    uploads: "Uploads",
    sourceType: "Type",
    sourceKinds: {
      folder: "Folder",
      url: "URLs",
      rss: "RSS feed",
      api: "API",
      statements: "Statements",
      custom: "Custom",
      github_issues: "GitHub issues",
      github_repo: "GitHub repository files",
      gitlab_repo: "GitLab project files",
      gitlab_issues: "GitLab issues",
      jira_issues: "Jira issues",
      s3: "S3 / MinIO",
      azure_blob: "Azure Blob",
      gcs: "Google Cloud Storage",
      webdav: "WebDAV",
      notion: "Notion",
      google_drive: "Google Drive",
    },
    sourceKindHints: {
      folder: "Holds the files you upload: add, drag in and remove them here.",
      url: "Fetches the listed web pages; changed pages update the same document.",
      rss: "Subscribes to a feed; each entry becomes a document dated by its publish time.",
      jira_issues:
        "Syncs a Jira project's issues. **Each ticket, together with its field-level change " +
        "history**, becomes one document — what changed from what to what, and when. " +
        "One call fetches everything; no per-ticket round trips.",
      github_issues:
        "Syncs a repository's issues. **Each ticket, together with its state history**, becomes " +
        "one document — when it was opened, closed, relabelled, reassigned, all dated. " +
        "Without a token GitHub allows only 60 requests an hour.",
      github_repo:
        "Reads markdown files off a repository's default branch. Each file becomes one " +
        "document dated by its last commit; unchanged files are not fetched again.",
      gitlab_repo:
        "Reads markdown files off a gitlab.com project's default branch, the same way as " +
        "GitHub repository files. A private project needs a token with read_api. " +
        "Self-managed GitLab has not been verified.",
      gitlab_issues:
        "Syncs a gitlab.com project's issues, and its merge requests when switched on. " +
        "**Each one, together with its state, label and milestone history**, becomes one " +
        "document, all dated. **A token with read_api is required**: GitLab does not return " +
        "notes or events without one, even for a public project. " +
        "Self-managed GitLab has not been verified.",
      s3:
        "Reads documents out of an S3 bucket, or anything speaking the same protocol " +
        "(MinIO, Ceph, R2). **Leave the endpoint empty for AWS**; fill it in for a " +
        "self-hosted one. Each object becomes a document dated by its last modification.",
      azure_blob:
        "Reads documents out of an Azure Blob container. Leave the endpoint empty for " +
        "Azure itself; fill it in for Azurite or a gateway.",
      gcs:
        "Reads documents out of a Google Cloud Storage bucket. The service account JSON " +
        "goes in whole — there is no file on the server to point at.",
      webdav:
        "Reads documents off a WebDAV share — Nextcloud, ownCloud, Synology, or anything " +
        "else speaking the protocol. Walks folders one level at a time; each file becomes " +
        "a document dated by its last modification.",
      notion:
        "Syncs the pages a Notion integration can see — share a page with the integration " +
        "and it appears here. Dated by when the page was last edited, which is the page's " +
        "own clock rather than ours.",
      google_drive:
        "Syncs the folders you choose from your Google Drive, shared drives included. " +
        "**You connect with your own Google account**, read-only; Docs, Sheets and Slides " +
        "arrive as Word, Excel and PowerPoint files. Dated by each file's last modification.",
      api: "External systems push JSON documents here, authenticated with this source's own token.",
      statements:
        "Your system pushes statements already in the extraction contract; no model reads them. Send events, not the state of a table.",
      custom:
        "Polls a URL you control on a schedule — your service returns JSON items and Utopia keeps them in sync.",
      memory:
        "Episodes remembered from Chat. Append-only: contradicted memories close their " +
        "validity range instead of being deleted — the timeline keeps the whole story.",
    },
    ingestGuide: "Read the ingest guide →",
    ingestGuideTitle: "Ingest interface guide",
    endpointField: "Endpoint URL",
    feedPlaceholder: "https://your-service/utopia-feed",
    endpointCopied: "Endpoint URL copied",
    copyEndpoint: "Click to copy the full URL",
    // api // Push status of api sources (queued/running never appear, only for type completeness)
    pushStatus: {
      never: "No pushes yet",
      queued: "—",
      running: "—",
      ok: "Push received",
      failed: "Push failed",
    },
    tokenTitle: "Source token",
    tokenWarning:
      "Anyone holding this token can push documents into this source. Rotate it if it leaks — the old token stops working immediately.",
    tokenUsage: "Send it with every push as:",
    tokenCopied: "Token copied",
    viewToken: "Token",
    rotateToken: "Rotate",
    noToken: "This source has no token yet — generate one to start pushing.",
    generateToken: "Generate token",
    close: "Close",
    authHeaderField: "Authorization header (optional, never shown again)",
    syncNow: "Sync now",
    syncStatus: {
      never: "Never synced",
      queued: "Queued",
      running: "Syncing…",
      ok: "Synced",
      failed: "Sync failed",
    },
    lastSyncAdded: (n: number) => `+${n} last sync`,
    sourceName: "Name",
    urlsField: "Page URLs (one per line)",
    feedUrl: "Feed URL",
    rssContentMode: "RSS content mode",
    rssModeFeed: "Feed content only",
    rssModeFull: "Full article content (new items)",
    rssModeFeedShort: "Feed only",
    rssModeFullShort: "Full articles",
    rssContentModeHint: "Full articles are fetched only for items that appear after you switch this on.",
    rssFullModeHint:
      "The first sync records what the feed holds now and imports nothing. Each item that appears after that is stored from the feed's own text when there is enough of it, otherwise from the linked article; an item with neither is listed as skipped.",
    rssFeedModeHint:
      "Stores what the feed itself carries — the entry text or its summary — and never opens the linked article.",
    // These five are read together, so pass one object: passing five positional arguments means a swap of two would go unnoticed
    rssHydrationCounts: (c: { pending: number; queued: number; retrying: number; complete: number; terminal: number }) =>
      `pending ${c.pending} · queued ${c.queued} · retrying ${c.retrying} · complete ${c.complete} · terminal ${c.terminal}`,
    repoField: "Repository (owner/name)",
    gitlabProjectField: "Project (group/name, subgroups allowed)",
    jiraUrlField: "Jira site URL",
    jiraProjectField: "Project key",
    s3BucketField: "Bucket",
    s3PrefixField: "Prefix (optional — without one the whole bucket is read)",
    s3EndpointField: "Endpoint (leave empty for AWS S3)",
    s3RegionField: "Region",
    s3KeyField: "Access key ID",
    s3SecretField: "Secret access key",
    azAccountField: "Storage account name",
    azKeyField: "Account key",
    gcsKeyField: "Service account JSON (paste the whole file)",
    davUrlField: "Server URL",
    davPathField: "Folder (optional — defaults to the root)",
    davUserField: "Username",
    davPassField: "Password",
    notionTokenField: "Internal integration token",
    notionQueryField: "Search term (optional — empty takes every shared page)",
    googleDrive: {
      createHint: "After creating the source, connect a Google account and choose the folders.",
      notConfigured:
        "Google Drive is not set up on this server. An administrator sets UTOPIA_GOOGLE_DRIVE_CLIENT_ID, _CLIENT_SECRET and _REDIRECT_URI.",
      account: "Google account",
      connect: "Connect Google Drive",
      reconnect: "Reconnect",
      disconnect: "Disconnect",
      waiting: "Finish in the Google window…",
      connectedAs: (email: string) => `Connected as ${email}`,
      connectedNoEmail: "Connected",
      notConnected: "Not connected. Syncing reads Drive with the permissions of whoever connects it.",
      folders: "Folders to sync",
      onlyConnector:
        "Only the person who connected this Google Drive can choose its folders, because they are folders from their own Drive.",
      foldersHint: "Subfolders are included. Files outside these folders are never read.",
      myDrive: "My Drive",
      sharedWithMe: "Shared with me",
      top: "Google Drive",
      empty: "No folders here.",
      open: "Open",
      noneSelected: "No folders chosen yet.",
      remove: "Remove",
      saveFolders: "Save folders and sync",
      saved: "Folders saved. A sync has started.",
      disconnectConfirm: (account: string, mine: boolean, connector: string) =>
        mine
          ? `Stop reading your Google Drive (${account})? Its access and the chosen folders are removed here, and connecting again starts from scratch.`
          : `Stop reading the Google Drive ${connector} connected (${account})? Only ${connector} chose its folders. Its access and those folders are removed here.`,
      unnamedAccount: "the connected account",
      someoneElse: "someone else",
      cancel: "Cancel",
      disconnected: "Google Drive disconnected.",
    },
    tokenField: "GitHub token (optional, never shown again)",
    gitlabTokenField: "GitLab token with read_api (optional, never shown again)",
    includePullRequests: "Treat pull requests as tickets too",
    gitlabIssuesTokenField: "GitLab token with read_api (needed for notes and events, never shown again)",
    includeMergeRequests: "Treat merge requests as tickets too",
    includeGlobsField: "Include globs (comma-separated, optional; replaces the default)",
    excludeGlobsField: "Exclude globs (comma-separated, optional; replaces the default)",
    interval: "Sync schedule",
    intervalManual: "Manual only",
    intervalEvery: (m: number) =>
      m < 60 ? `Every ${m} min` : `Every ${m / 60} h`,
    schedule: {
      manual: "Manual",
      interval: "Interval",
      daily: "Daily",
      weekly: "Weekly",
      advanced: "Advanced",
      every: "Every",
      minutes: "minutes",
      hours: "hours",
      at: "at",
      cronPlaceholder: "Enter a cron expression",
      whatIsCron: "What is cron?",
      cronDocsUrl: "https://crontab.guru",
      daysShort: ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"],
      dailyAt: (t: string) => `Daily ${t}`,
    },
    createSource: "Create",
    cancel: "Cancel",
    deleteSource: "Delete source",
    deleteSourceHint: "Documents are kept and move to Uploads.",
    newSourceTitle: "New source",
    iconLabel: "Icon",
    pageOf: (from: number, to: number, total: number) =>
      `${from}–${to} of ${total}`,
    syncHistory: "History",
    runNew: (n: number) => `+${n} new`,
    runUpdated: (n: number) => `~${n} updated`,
    runNothing: "no changes",
    noRuns: "No syncs recorded yet.",
    sourceSettings: "Source settings",
    editSourceTitle: "Source settings",
    saveChanges: "Save changes",
    authHeaderEditField: "Authorization header",
    authKeepHint: "Leave blank to keep the current value.",
    editKeepNote:
      "Changing what this source fetches never deletes existing documents. " +
      "Items the new configuration no longer returns are marked “Not in source” " +
      "after the next sync. Switching to a different service entirely? Create a new source instead.",
    decay: {
      toggle: "Let documents decay",
      hint:
        "A document nobody retrieves loses score over time. Once the source holds more " +
        "than the limit, the lowest-scoring old documents are deleted once a day. " +
        "They can be restored from the trash.",
      maxItems: "Keep at most (documents)",
      halfLife: "Score halves every (days)",
      staleAfter: "Rank lower after (days unread)",
      minAge: "Never delete newer than (days)",
    },
    notInSource: "Not in source",
    cleanupMissing: (n: number) => `Clean up ${n} missing`,
    cleanupTitle: "Delete missing documents",
    cleanupHint: (n: number, name: string) =>
      `${n} document${n === 1 ? "" : "s"} in “${name}” ${n === 1 ? "is" : "are"} no longer ` +
      "present in the source. Deleting removes them from the base and retires the facts that " +
      "had no other source. Their content is kept, and a deleted document can be restored.",
    cleanupConfirm: "Delete them",
    deleteSourceTitle: "Delete this source",
    deleteSourceBody: (name: string) =>
      `Type “${name}” to confirm. Documents are kept and move to Uploads; ` +
      "scheduled syncing stops.",
    dangerZone: "Danger zone",
  },
  search: {
    placeholder: "Search the knowledge base… (keyword + semantic)",
    button: "Search",
    searching: "Searching…",
    noResults: "No results found",
    previousPage: "Previous page",
    nextPage: "Next page",
    retry: "Retry search",
    chunkOf: (filename: string, seq: number) => `${filename} · section ${seq}`,
  },
  ask: {
    streamInterrupted: "The answer stream was interrupted. Reopen the conversation to check its status.",
    /* When reopening a conversation, the last question has no answer and no answer being written (most likely it failed halfway) */
    noActiveAnswer: "The last question has no answer. Retry it, or send a new message.",
    historyLoadFailed: "Could not load this conversation.",
    retryHistory: "Retry",
    // Re-answer the last question that has no answer (#936)
    retryQuestion: "Retry",
    loadingHistory: "Loading conversation…",
    loadEarlierConversations: "Load earlier conversations",
    conversationsLoadFailed: "Could not load conversations.",
    retryConversations: "Retry",
    /* Serif greeting on the new-conversation first screen: the inscription serif, with the brand name in the sentence (no full stop on the title) */
    greeting: "Ask Utopia what it remembers",
    emptyTitle: "Chat",
    emptyBody:
      "Converse with your knowledge base — cited answers, temporal questions, and it can remember.\nUpload documents in Library and configure a model in Administration → Models first.",
    placeholder: "Ask anything…",
    composerHint: "Enter to send · Shift+Enter for a new line",
    scopeLabel: "Knowledge base",
    send: "Send",
    stop: "Stop",
    stopping: "Stopping…",
    stopped: "Stopped",
    thinking: "Thinking…",
    /* The wording for each step in the trace (#942). The server only supplies fields: status, count, time; how it is phrased is set here.
       Queries, entity names, source names, the purpose of an analytics query, and the sentence recorded are data, shown verbatim;
       old messages without these fields still show the English saved at the time */
    step: {
      failed: "failed",
      documentNotFound: "document not found",
      entityNotFound: "entity not found",
      sourceNotFound: "no such data source",
      unknownTool: "unknown tool",
      /* Parameters failed to parse at all: most likely the model's output hit the limit and was cut off midway */
      unparsed: "incomplete arguments",
      missing: (param: string) => `missing ${param}`,
      invalid: (param: string) => `invalid ${param}`,
      sources: (n: number) => `${n} source${n === 1 ? "" : "s"}`,
      sections: (n: number) => `${n} section${n === 1 ? "" : "s"}`,
      matches: (n: number, total?: number) =>
        total === undefined
          ? `${n} match${n === 1 ? "" : "es"}`
          : `${n} of ${total} matches`,
      /* `at` is world time (when it held), `recorded` is one of the two sentences below (recorded at the time) */
      facts: (n: number, at: string | null, recorded: string | null) =>
        `${n} fact${n === 1 ? "" : "s"}${at ? ` at ${at}` : ""}${recorded ? `, ${recorded}` : ""}`,
      recordedBy: (t: string) => `as recorded by ${t}`,
      recordedBefore: (t: string) => `as recorded before ${t}`,
      linked: (n: number, total: number) =>
        n === total ? `${n} linked` : `${n} of ${total} linked`,
      dated: (n: number, total: number) =>
        n === total
          ? `${n} dated fact${n === 1 ? "" : "s"}`
          : `${n} of ${total} dated facts`,
      /* `more`: the list stopped at the cap, there may be more after it */
      paths: (n: number, more: boolean, hops: number) =>
        n === 0
          ? "no path"
          : `${n}${more ? "+" : ""} path${n === 1 && !more ? "" : "s"}, shortest ${hops} hop${hops === 1 ? "" : "s"}`,
      changes: (n: number, more: boolean) =>
        n === 0 ? "no changes" : `${n}${more ? "+" : ""} change${n === 1 && !more ? "" : "s"}`,
      /* A window with no `until` runs to now: '2026-09-01 → now' */
      now: "now",
      rows: (n: number, more: boolean) =>
        `${n}${more ? "+" : ""} row${n === 1 && !more ? "" : "s"}`,
      rules: (n: number) => (n === 0 ? "none" : `${n} rule${n === 1 ? "" : "s"}`),
      /* rule_matches: how many places this rule flags right now */
      marked: (n: number) => `${n} marked`,
      /* The analytics step can be expanded to show the SQL it ran (#936) */
      sql: "SQL",
    },
    newChat: "New chat",
    recent: "Recent",
    untitled: "Untitled",
    noConversations: "No conversations yet.",
    deleteConversation: "Delete conversation",
    searchConversations: "Search chats",
    moreActions: "More",
    rename: "Rename",
    copyTitle: "Copy title",
    deleteTitle: "Delete conversation?",
    deleteHint: (name: string) =>
      `“${name}” and its messages will be permanently removed.`,
    deleteBtn: "Delete",
    cancel: "Cancel",
    // This answer has no source at all behind it (#547). It is a factual statement, so every one attaches; no guessing which should attach
    noSources: "No sources cited",
    // The exit at the top right of the preview popup: for someone who wants the whole document after reading this section
    openOriginal: "Open original",
    // Copy (#936): one for the whole answer, one per code block
    copyAnswer: "Copy answer",
    copyCode: "Copy code",
    copied: "Copied",
  },
  graph: {
    // Entity whose type has not been judged yet (0009). Not a class, it means 'this cell is still empty'
    untyped: "Untyped",
    legendMore: (n: number) => `All ${n} classes`,
    nodeBudget: "How many entities to draw",
    nodeBudgetMore: "Draw more",
    nodeBudgetLess: "Draw fewer",
    legendSearch: "Filter classes",
    legendNone: "No class matches",
    legendOnly: "Only",
    legendShowAll: (n: number) => `Show all (${n} hidden)`,
    legendHideAll: "Hide all",
    legendAllHint:
      "Every class on screen, most common first. Click to show or hide.",
    searchMore: (n: number) => `${n} more — load 20`,
    zoomIn: "Zoom in",
    zoomOut: "Zoom out",
    fitView: "Fit view",
    layoutForce: "Force layout",
    layoutCircular: "Circular layout",
    layoutPack: "Cluster by type",
    searchEntity: "Search entities…",
    searchInSubgraph: "Search in subgraph…",
    backToOverview: "← Full graph",
    // The order is deliberate: until the model is configured, uploaded documents just queue and no entity can be extracted.
    // Configure the model first, then upload documents
    // The admin page is called Administration in the avatar menu; the hint must use the same name (#267).
    // Those who can configure models and those who cannot see different sentences: the latter can only ask an admin
    facts: "facts",
    noFacts: "No facts for this entity yet",
    confidence: "confidence",
    evidence: "evidence",
    noEvidence: "No evidence recorded",
    noQuote: "(no quote)",
    openInDoc: "Open the passage in the document",
    /* The predicate the extractor read from the source was normalized to an identifier. Out-of-vocabulary wording is downgraded to
       related to, and the original meaning survives only here.
       **The wording must not claim this is a quote**: relation keys can only be [a-z0-9_], so in Chinese corpora
       a Chinese verb meaning 'bought' comes out as purchases, and saying 'the source says purchases' would be false.
       The verbatim sentence is in the evidence quote next to it, nothing lost. */
    proposedPredicate: (p: string) => `read from the text as “${p}”`,
    saidAs: (words: string) => `the documents say it “${words}”`,
    inferredPredicate:
      "not a relation in the ontology, this is the source's wording",
    unknownPredicate: "no relation stated",
    sectionRef: (filename: string, seq: number) =>
      `${filename} · section ${seq} →`,
    fromVersion: (v: number) => `v${v}`,
    /** The document holding the evidence was deleted (#268): the fact still stands because it has another source */
    sourceDeleted: "source deleted",
    sourceDeletedHint:
      "The document this quote came from was deleted. The fact stays because it has another source.",
    staleEvidenceHint:
      "This evidence comes from an earlier version of the document. " +
      "The document has since been updated; the fact itself is unaffected.",
    staleFactChip: "unconfirmed",
    staleFactHint:
      "All evidence for this fact comes from earlier versions of its source documents — " +
      "the current content no longer states it. It may still be true; review it under Review.",
    /* Entity correction: the extractor's first judgment, which a person can overturn */
    edit: "Edit",
    editName: "Name",
    editType: "Type",
    editSave: "Save",
    editCancel: "Cancel",
    editSaved: "Entity updated",
    editEmptyName: "Name cannot be empty",
    /* Same name is not an error: two people named Zhang Wei can coexist. Only a hint, no blocking */
    viewRelations: "Relations",
    viewTimeline: "Timeline",
    /* Third view: the record timeline. Not 'when things happened' but 'when we came to believe it' */
    viewHistory: "History",
    viewDerived: "Derived",
    derivedEdges: (n: number) => `${n} derived`,
    derivedHint:
      "Nothing here was asserted — the engine worked it out, from an axiom your ontology declares or from a rule someone wrote. Each one shows the premises it came from.",
    derivedNoProof: "The premises are gone.",
    /* Dispute (0017 §3) */
    contestedChip: "disputed",
    contestedHint: (kind: string, derived: string | null) =>
      kind === "derived_contradiction"
        ? `A derivation contradicts this assertion${derived ? `: ${derived}` : ""}. Open it under Review.`
        : kind === "temporal_conflict"
          ? "A newer assertion conflicts with this one in time. Open it under Review."
          : "This assertion breaks an axiom the ontology declares. Open it under Review.",
    blockedTitle: "Did not land",
    blockedHint:
      "The engine could draw these edges; an assertion stood in the way. They show on the graph as ghost edges.",
    blockedBy: (t: string) => `blocked by ${t}`,
    blockedReview: "Review",
    /** Proof chain (0002 R2): each step is a premise of an assertion, expandable to the original sentence */
    proofStep: (n: number) => `Step ${n}`,
    proofRetracted: "since retracted",
    proofLoading: "Tracing the proof…",
    derivedPanel: "Inference",
    derivedRunAsk: "Re-run inference for the whole base?",
    derivedRunGo: "Run",
    derivedRunCancel: "Cancel",
    derivedCountLabel: "Edges derived",
    derivedStateLabel: "Schedule",
    derivedLastLabel: "Last run",
    derivedOn: (mins: number) => `every ${mins} min`,
    derivedOff: "off",
    derivedNever: "never",
    derivedAgo: (mins: number) =>
      mins < 1
        ? "just now"
        : mins < 60
          ? `${mins} min ago`
          : `${Math.round(mins / 60)} h ago`,
    derivedRun: "Run now",
    derivedRunning: "Running…",
    derivedNoChange: "Nothing changed.",
    derivedChanged: (added: number, gone: number) =>
      `${added} added · ${gone} retracted`,
    derivedCapped: (n: number) => `${n} predicate(s) not closed fully`,
    close: "Close",
    // The rule that a derived edge came from. **All four must exist**: a lookup miss falls back to the raw kind string,
    // which means nothing to the reader
    ruleNames: {
      transitive: "transitive",
      symmetric: "symmetric",
      inverse: "inverse",
      sub_property: "sub-property",
    } as Record<string, string | undefined>,
    historyHint: "How this entity's record changed — and who changed it.",
    historyEmpty: "Nothing recorded for this entity yet.",
    historyKind: {
      asserted: "Recorded",
      corrected: "Interval corrected",
      rejected: "Withdrawn",
      /* Merged into another assertion: no content was lost, it is not a withdrawal */
      merged: "Merged into an existing fact",
      /* Entity merge. **Say the two directions separately**: 'absorbed whom' and 'absorbed by whom' are two different things on the graph,
         and rollback is done per direction too */
      merged_in: "Another entity was merged into this one",
      merged_away: "Merged into another entity",
      merge_reverted: "Merge undone",
      /* The class on the node changed; no fact moved */
      retyped: "Type changed",
      retype_reverted: "Type change undone",
    } as Record<string, string>,
    /* The counterpart entity is gone already (the library was cleaned up): the merge event must still be listed */
    historyGoneEntity: "an entity that is gone",
    historyEngine: "engine",
    /* A change to the validity interval: after correction the interval closes at a point in time */
    historyClosedAt: (t: string) => `closed at ${t}`,
    historyFrom: (t: string) => `from ${t}`,
    historyOngoing: "open-ended",
    historicalNote: (n: number) =>
      `${n} past fact${n === 1 ? "" : "s"} not shown — see Timeline →`,
    undated: "Undated",
    /* 0064: the sentence has no time, but the document states its own date. It means 'as of', not 'since' */
    asOf: (date: string, by: string | null) =>
      by ? `as of ${date} · ${by}` : `as of ${date}`,
    /* Entity panel Relations: the two section titles, the fold at the group end, the evidence toggle on each row */
    fromEntity: (name: string) => `From ${name}`,
    toEntity: (name: string) => `To ${name}`,
    openEntity: (name: string) => `Open ${name}`,
    past: (n: number) => (n === 1 ? "1 past" : `${n} past`),
    sources: (n: number) => (n === 1 ? "1 source" : `${n} sources`),
    /* Names section (0041): real name, short name, former names */
    names: "Names",
    shownName: "shown name",
    nameUntil: (d: string) => `until ${d}`,
    removeName: "Remove",
    // Ask before removing: a name is a fact, and there is nowhere in the UI that adds it back
    removeNameAsk: "Remove this name?",
    removeNameCancel: "Keep",
    removeNameGo: "Remove",
    nameRemoved: "Name removed",
    timelineEmpty: "No dated facts yet.",
    lastConfirmed: (d: string) => `confirmed ${d}`,
    /* Three sources share one marker (the engine's reconciliation takeover, Review verdicts, and manual edits), so this sentence
       no longer claims which one it was: after a manual edit, the old 'closed by reconciliation' would misstate the source.
       To see who changed it, History has the actor and time */
    correctedHint:
      "This interval comes from a correction rather than a sentence in a document: " +
      "automatic succession, a review decision, or someone editing it. " +
      "The superseded assertion stays in the ledger — see History for who and when.",
    /* ---- Manual correction of validity interval (302) ---- */
    editTime: "Correct the interval",
    editTitle: "Edit entity",
    timeStart: "Start",
    timeEnd: "End",
    /* The three states at the end map one-to-one to the three forms in the ledger (see the comment in migration 0003) */
    timeEndOpen: "Still going",
    timeEndUnknown: "Ended, date unknown",
    timeEndDate: "Ended on",
    /* The number of digits written is the precision: 2023 means 'that year', 2023-06 means 'that month' */
    timeFormat: "2023 · 2023-06 · 2023-06-15 · 2023-06-15T14:32Z",
    timeBadDate:
      "Use 2023, 2023-06, 2023-06-15, or a clock time with its zone such as 2023-06-15T14:32Z — a time without a zone is not a moment.",
    timeNote: "Why (optional)",
    timeNotePlaceholder: "The document says the first half of 2023",
    timeSave: "Save",
    timeCancel: "Cancel",
    timeSaved: "Interval corrected",
    timeSavedClosed: (n: number) =>
      `Interval corrected — ${n} open fact${n === 1 ? "" : "s"} closed to match`,
    timeSavedConflicts: (n: number) =>
      `Interval corrected — ${n} conflict${n === 1 ? "" : "s"} need a ruling in Review`,
    ongoing: "now",
    /* Must be distinguishable from ongoing: confusing the two is exactly what migration 0046 fixes,
       the source says 'former CEO' but the UI shows now */
    endedUnknown: "ended, date unknown",
    stats: (n: number, e: number, active: number | null) =>
      `${n} entities · ${e} facts${active === null ? "" : ` · ${active} active`}`,
    /** The canvas draws only the highest-degree batch. **State clearly how many were drawn and how many in total**; previously this said
     *  the cap, so a library with tens of thousands of entities always showed 150 in the top right */
    statsCapped: (
      shown: number,
      total: number,
      shownE: number,
      totalE: number,
      active: number | null,
    ) =>
      `showing ${shown} of ${total} entities · ${shownE} of ${totalE} facts${active === null ? "" : ` · ${active} active`}`,
    cappedHint: (shown: number, total: number) =>
      `The canvas draws the ${shown} best-connected entities of ${total}. Search to reach the rest.`,
    stabilizing: "Stabilizing layout",
    scrubUnitHint: "Step size for playback and for each bar",
    scrubUnitYear: "Yr",
    scrubUnitMonth: "Mo",
    scrubUnitDay: "Dy",
    scrubBarMerged: (n: number) => `each bar covers ${n} steps`,
    allTime: "All time",
    nowBtn: "Now",
    play: "Play timeline",
    pause: "Pause",
  },
  origin: {
    ocr: (page: number | null) => (page === null ? "OCR" : `OCR · p. ${page}`),
    transcribed: (span: string | null, speakers: string[]) =>
      ["Transcribed", span, speakers.length > 0 ? speakers.join(", ") : null]
        .filter(Boolean)
        .join(" · "),
    described: "Described by a model",
    ocrHint: "Read from a scan or image. A character or digit may be misread.",
    transcribedHint: "Transcribed from a recording. A name may be misheard.",
    describedHint: "A model's description of an image. Nobody wrote or said these words.",
    readBy: (model: string) => `Read by ${model}.`,
  },
  doc: {
    backToLibrary: "← Back to Library",
    sections: "sections",
    section: "Section",
    citedHere: "← cited here",
    loading: "Loading…",
    extracted: "Extracted",
    ongoing: "now",
    /** `#610`: `doc_time` is the date read from the file; it is a separate thing from the upload time */
    docDate: "Document date",
    docDateFromContent: "Detected from the filename or first line",
    uploadedAt: "Uploaded",
  },
  settings: {
    title: "Administration",
    tabModels: "Models",
    tabMembers: "Users",
    tabKbs: "Knowledge bases",
    tabDeployment: "Deployment",
    tabSso: "Single sign-on",
    cardAccounts: "Accounts",
    newUser: "Create user",
    initialPassword: "Initial password (min. 8 characters)",
    createUserBtn: "Create",
    searchUsers: "Filter by name or email…",
    deployment: {
      openReg: "Allow self-registration",
      openRegHint:
        "When off, the sign-up form is closed and only admins can create accounts here.",
      workers: "Background workers",
      workersHint:
        "How many model jobs (extraction, resolution) can run at once (1–256), plus two slots " +
        "reserved for intake jobs such as syncs, which make no model call and so never wait " +
        "behind model work. It is an outer ceiling that stops work piling up without bound; the " +
        "real throttle is the per-model limit below, so keep this comfortably above the sum of " +
        "those. Takes effect immediately.",
      workersApply: "Apply",
      /* The real throttle: the constraint comes from the provider's rate limit, which is computed per model */
      modelConcurrency: "Model concurrency",
      modelConcurrencyHint:
        "How many calls a model will take at once. The limit that matters belongs to the " +
        "provider and is per model — a local Ollama may manage two, a hosted API fifty. " +
        "Background work (extraction, resolution, indexing) waits for a slot; chat and search " +
        "never do. Takes effect immediately.",
      /* Deployment-level default: used when creating a new library. The name deliberately is not 'system language' */
      ontologyLang: "Default ontology language",
      ontologyLangHint:
        "The language new knowledge bases start their ontology in — class descriptions go " +
        "into the extraction prompt, so this follows the documents you expect, not the " +
        "interface. Each knowledge base can change its own afterwards. " +
        "Interface language is a per-reader choice in the account menu.",
      modelDefault: "Default",
      modelReset: "Reset",
      modelResetHint:
        "Drop this model's own limit and fall back to the default.",
    },
    datasources: {
      tab: "Data sources",
      title: "Data sources",
      hint:
        "Read-only database connections for asking questions about your data in Chat. " +
        "Register connections here; each knowledge base mounts the ones it may query.",
      name: "Name",
      connString: "Connection string — the scheme picks the engine",
      // One line per form; the token goes in the password slot, and the Databricks path is the httpPath in the console
      connSchemes:
        "postgres://user:pass@host:5432/db\n" +
        "mysql://user:pass@host:3306/db   (MariaDB, TiDB, OceanBase, Doris, StarRocks)\n" +
        "trino://user[:pass]@host:8080/catalog[/schema]   (Iceberg, Delta Lake, Hive)\n" +
        "databricks://:TOKEN@host/sql/1.0/warehouses/ID?catalog=main\n" +
        "snowflake://:TOKEN@account.snowflakecomputing.com/DB/SCHEMA?warehouse=WH\n" +
        "clickhouse://user:pass@host:8123/db   (?ssl=true: https, 8443)",
      add: "Add data source",
      newTitle: "New data source",
      engine: "Engine",
      engineRaw: "Connection string",
      optional: "(optional)",
      testConn: "Test connection",
      testing: "Testing…",
      colConn: "Connection",
      colStatus: "Status",
      grantsNoneShort: "Not granted",
      grantsCount: (n: number) => (n === 1 ? "1 workspace" : `${n} workspaces`),
      fHost: "Host",
      fPort: "Port",
      fDatabase: "Database",
      fUser: "User",
      fPassword: "Password",
      fCatalog: "Catalog",
      fSchema: "Schema",
      fToken: "Token",
      fWarehouse: "Warehouse",
      fWarehouseId: "Warehouse ID",
      fAccount: "Account host",
      test: "Test",
      testOk: "Connected",
      testFail: "Failed",
      neverTested: "Untested",
      remove: "Remove",
      empty: "No data sources registered yet. Register one below.",
      grants: "Available to",
      grantsHint:
        "Which workspaces may use this source. Once granted, KB admins in those workspaces choose whether to mount it — " +
        "this controls what they can reach, not what they have mounted.",
      grantsNone:
        "Not granted to any workspace — no knowledge base can mount it.",
      grantAdd: "Grant a workspace…",
      grantRevoke: "Revoke",
      grantRevoked: (n: number) =>
        n === 0
          ? "Revoked."
          : `Revoked, and unmounted it from ${n} knowledge base(s).`,
    },
    sso: {
      title: "Single sign-on",
      hint:
        "Sign-in through an identity provider (OIDC), configured with environment variables (see " +
        ".env.example). People link their own identity from their account page; an administrator " +
        "can see and remove links but can't create one for someone else.",
      disabled:
        "Not configured on this deployment. Set UTOPIA_OIDC_ISSUER, UTOPIA_OIDC_CLIENT_ID and " +
        "UTOPIA_OIDC_REDIRECT_URI (and, if the provider needs one, UTOPIA_OIDC_CLIENT_SECRET) " +
        "and restart.",
      issuer: "Issuer",
      clientId: "Client ID",
      redirectUri: "Redirect URI",
      colUser: "User",
      colSubject: "Subject",
      empty: "Nobody has linked an identity yet.",
      unlink: "Unlink",
      unlinkTitle: (email: string) => `Unlink ${email}?`,
      unlinkHint:
        "They won't be able to sign in with SSO until they link again. Sessions already open " +
        "stay signed in; deactivate the account to cut access immediately.",
    },
    kbs: {
      hint:
        "Every knowledge base in this deployment. Open ones are readable by all members; " +
        "restricted ones are invite-only. Creating a knowledge base is an admin action — " +
        "members switch between them from the top bar.",
      defaultChip: "Default",
      newKb: "New knowledge base",
      packsLabel: "Bundled ontologies",
      packsHint: "Optional, and more can be imported later.",
      packsPick: "Search packs…",
      packsNone: "None — the ontology grows out of the documents",
      packsCount: (c: number, p: number) => `${c} classes · ${p} properties`,
      scenariosLabel: "Scenario packs",
      scenariosHint:
        "Classes, properties and the questions to answer for one kind of corpus; " +
        "installed as this base's own ontology. Optional.",
      scenariosPick: "Search scenarios…",
      scenariosNone: "None — build the ontology by hand later",
      scenariosCount: (c: number, p: number, q: number) =>
        `${c} classes · ${p} properties · ${q} questions`,
      name: "Name",
      description: "Description",
      visibility: "Visibility",
      visOpen: "Open — everyone in this deployment",
      visRestricted: "Invited only",
      create: "Create",
      openSettings: "Settings",
      empty: "No knowledge bases yet. Create the first one above.",
      docs: (n: number) => `${n} docs`,
    },
    modelsIntro:
      "OpenAI-compatible protocol — DeepSeek, Qwen, GLM, Ollama, vLLM all work. Fully on-prem friendly.",
    chatModel: "Chat model",
    reasoningEffort: "Reasoning effort",
    reasoningDefault: "endpoint default",
    reasoningHint:
      "Reasoning models think before they answer, and most of what they output is that thinking. Every call uses this one setting. Which value turns thinking off depends on the endpoint: minimal on some, none on others (DeepSeek).",
    embedModel: "Embedding model (optional, enables semantic search)",
    baseUrl: "Base URL",
    model: "Model",
    apiKey: "API key",
    keyConfigured: "(configured — leave blank to keep)",
    save: "Save",
    saving: "Saving…",
    saved: "Saved",
    savedRefreshFailed: "Saved, but the latest settings could not be loaded.",
    reloadSettings: "Reload settings",
    test: "Test connection",
    testing: "Testing…",
    chatLabel: "Chat",
    embedLabel: "Embedding",
    ok: (reply: string) => `Reachable and authenticated (${reply})`,
    /** One line per request shape the jobs send (#1354) */
    chatShapes: {
      plain: "Plain request",
      streaming: "Streamed at temperature 0 with the completion ceiling (extraction)",
      tools: "With a tool (chat, second look)",
    },
    chatShapeOk: "ok",
    chatShapeRefused: (error: string) => `refused: ${error}`,
    chatShapeTimedOut: (error: string) => `timed out (${error})`,
    okDim: (dim: number) => `Reachable and authenticated (dim ${dim})`,
    unsaved: "Unsaved changes. Save this card to test them.",
    readersTitle: "Reading scans and recordings",
    readersIntro:
      "Scanned PDFs, images and recordings have no text to parse, so each needs its own reader. They are set apart from chat so that sensitive files can stay on your own servers. A file that arrives before its reader waits, and the message center says so; saving the reader reads it.",
    ocrService: "Document reading (OCR)",
    ocrHint:
      "A MinerU service (mineru-api). It reads each page's layout first, so every passage keeps its page and position.",
    ocrHintArk:
      "A Volcengine Ark vision model reads each page and returns only the written text. Passages keep their page; there are no positions. Every page is a paid model call.",
    readerProvider: "Provider",
    arkProvider: "Volcengine Ark",
    serviceUrl: "Service URL",
    backend: "Backend (optional)",
    transcribeModel: "Transcription",
    transcribeHint:
      "An OpenAI-compatible endpoint that labels speakers (diarized_json), such as gpt-4o-transcribe-diarize. A transcript that cannot say who spoke is not used.",
    okVersion: (version: string) => `Reachable (${version})`,
    okReachable: "Reachable and authenticated",
    savedRequeued: (n: number) =>
      `Saved. ${n} waiting ${n === 1 ? "file is" : "files are"} being read.`,
  },
  ontology: {
    casesShort: "Regression cases",
    casesTitle: "Property regression cases",
    casesHint: "The last recorded comparisons of expected properties with alignment decisions. Refresh reads these results; property edits trigger the regular alignment process.",
    casesEmpty: "No cases yet. Adopt a property proposal or add an open statement from a document.",
    caseStatement: "Statement",
    caseExpected: "Expected",
    caseActual: "Actual",
    caseResult: "Result",
    caseChecked: "Last checked",
    caseOrigin: "Added by",
    casePassed: "Matches",
    caseMismatch: "Mismatch",
    caseNotChecked: "Not checked",
    caseUnbound: "No property bound",
    caseUndecided: "Undecided",
    caseUnavailableProperty: "Bound property no longer available",
    caseDecidedAt: "Decision made",
    caseNoProperties: "Create a suitable property in Ontology before adding this case.",
    caseHumanBound: "Human decision retained",
    caseHumanHint: "A manual binding is preserved and counts as passing. This is not an independent model check.",
    caseAdoption: "Proposal adoption",
    casePerson: "Manual case",
    caseUnknownCreator: "Former member",
    caseEvidence: "Show source evidence",
    caseHideEvidence: "Hide source evidence",
    caseMake: "Add regression case",
    caseAddTitle: "Add a property regression case",
    caseExpectBound: "Bind to a property",
    caseNoneHint: "Only an explicit no-binding decision passes. An undecided or bound result fails; no decision remains unchecked.",
    caseHumanConflict: "Failed: manual binding conflict",
    caseHumanConflictHint: "This case expects no binding, but a person bound the statement. The case fails; the manual binding is retained.",
    caseAddHint: "Save whether this open statement should bind to a property or remain unbound. Its alignment decision is compared with that expectation.",
    caseExpectedProperty: "Expected property",
    caseDirection: "Direction",
    caseForward: "Subject → Object",
    caseReverse: "Object → Subject",
    caseAdded: "Regression case added",
    caseAdd: "Add case",
    title: "Ontology",
    hint: "Classes & properties",
    tabClasses: "Classes",
    colName: "Name",
    multiParentHint: "This class has more than one parent; the indentation follows one of them, and the Parent column lists them all",
    colSignature: "Subject → Object",
    colInstances: "Instances",
    colFacts: "Facts",
    colOnClass: "On class",
    colDatatype: "Type",
    colUnit: "Unit",
    colSingleValued: "Single-valued",
    yes: "Yes",
    rowsShown: (n: number) => `${n} rows`,
    viewTable: "Table",
    switchToTable: "Switch to table view",
    switchToGraph: "Switch to graph view",
    viewDiagram: "Graph",
    axiomTransitive: "Transitive",
    axiomSymmetric: "Symmetric",
    axiomAsymmetric: "Asymmetric",
    axiomIrreflexive: "Irreflexive",
    tabProperties: "Relations",
    newClass: "New class",
    newSubClass: "New sub-class",
    newProperty: "New relation",
    filter: "Filter…",
    missesShort: "Unmatched",
    uniquenessShort: "Overlaps",
    refineShort: "Refine types",
    /* ---- Business rules (0021 / #277) ---- */
    rulesShort: "Business rules",
    ruleExpressionReadOnly: "This definition is read-only in this form. You can edit its name and description without changing its expressions or conditions.",
    ruleUnknownExpression: "Unsupported expression (read-only)",
    ruleDependencies: "Potential dependencies",
    ruleDependenciesHint: "Candidates from rule definitions, not proof of execution. Readings, conditions and time determine what actually runs; disabled definitions are included.",
    ruleDependenciesIncomplete: "Some definitions or classes could not be read completely. This list may be incomplete.",
    rulePotentialProducers: "May receive input from",
    rulePotentialConsumers: "May provide input to",
    ruleDependenciesEmpty: "No candidates found in the readable definitions.",
    rulesTitle: "Business rules",
    /* Say three things: who wrote it, what identity the conclusion carries, and when it is recomputed. The third is easiest to misread as
       'it takes effect on save', when it actually waits for the next materialization round */
    rulesHint: "Rules that decide a class or compute a value from an entity's own attributes. A conclusion is derived and lapses when its premises do.",
    rulesEmpty: "No rules yet.",
    rulesNoMatch: "No rule matches that.",
    /** Search covers the whole rule, not just the name: the predicates and values in the criteria are included too */
    ruleSearch: "Search rules, attributes, values",
    ruleNew: "New rule",
    /* When you cannot proceed, say why. **A rule judges the value of an attribute**, and without an attribute there is nothing to write,
       a greyed-out button explains nothing, and the reader would just think it is broken */
    ruleNeedsAttribute:
      "A rule tests an attribute's value, and this ontology has none yet. Open a class and add one under Attributes.",
    ruleNeedsClass: "Add a class first — a rule concludes one.",
    ruleColRule: "Rule",
    ruleColDerived: "Derived",
    ruleColStatus: "Status",
    /* Conditions are joined with AND. **Write 'and' rather than a dot**: a symbol does not convey 'all must hold',
       and that is exactly where rules are most easily misread */
    ruleAnd: "and",
    /** Between groups. **Reads as 'or' in words, not a symbol**: another way for the same criterion to be satisfied */
    ruleOr: "or",
    ruleWhere: "where",
    ruleDropCondition: "Remove this condition",
    ruleOperandPlaceholder: (kind: string): string =>
      kind === "set"
        ? "gas anomaly, post-effect gas anomaly"
        : kind === "range"
          ? "8 - 12"
          : "12.0",
    ruleName: "Name",
    ruleNamePlaceholder: "Gas-bearing well",
    ruleDescription: "What it means (optional)",
    ruleSubject: "Applies to",
    ruleSubjectHint: "and its subclasses",
    ruleConcludes: "Concludes",
    ruleConcludesTyping: "the class",
    ruleConcludesAttribute: "the attribute",
    ruleConcludesRelation: "the relation",
    ruleConcludesRelationText: (join: string, conclude: string): string =>
      `${conclude} from X through ${join}`,
    ruleSideX: "X",
    ruleSideY: "Y",
    /* It used to read 'when all of the following hold'. **A rule can now have a second case**, so that sentence
       is no longer true: the label falls back to 'when', and whether all hold is explained by the note below */
    ruleConditions: "When",
    ruleConditionsHint:
      "The conditions in a block must all hold. Add another way and any one block is enough.",
    ruleAddCondition: "Add a condition",
    /** Adds a whole block: another way for the same criterion to be satisfied, not another rule */
    ruleAddGroup: "Another way",
    ruleOpGt: "is above",
    ruleOpGte: "is at least",
    ruleOpLt: "is below",
    ruleOpLte: "is at most",
    ruleOpBetween: "is between",
    ruleOpIn: "is one of",
    ruleOpNotIn: "is not one of",
    ruleOpPresent: "is recorded",
    ruleOperandNumber: "12.0",
    ruleOperandSet: "gas anomaly, post-effect gas anomaly",
    ruleOperandSetHint: "comma separated",
    ruleSave: "Save rule",
    ruleSaved: "Rule saved",
    ruleDeleted: "Rule deleted",
    ruleDelete: "Delete",
    ruleDeleteConfirm: (n: string) => `Delete “${n}”? What it concluded goes with it.`,
    ruleEnabled: "On",
    ruleDisabled: "Off",
    /* The number is 'how many conclusions currently hold on its strength', not the historical total */
    ruleDerivedCount: (n: number) =>
      n === 1 ? "1 entity" : `${n} entities`,
    ruleRun: "Run now",
    ruleRunning: "Running…",
    /* After a run, say three things, because the graph changes by itself: how many matched, how many newly landed, how many were withdrawn */
    ruleRunDone: (hits: number, inserted: number, invalidated: number) =>
      `${hits} matched · ${inserted} new · ${invalidated} retired`,
    ruleRunCapped: (n: number) =>
      `${n} entity/rule pairs had too many readings to expand; their conclusions are incomplete`,
    /** The chain hit its cap and stopped. **Say so**: an unconnected link looks the same as 'not satisfied' */
    ruleRunRoundsCapped: (n: number): string =>
      `Rules kept concluding after ${n} rounds; anything further down the chain was not reached`,
    ruleNeedsCondition: "A rule needs at least one condition.",
    /* ---- Polish: clickable counts, persistent capped hint, changing conclusions ---- */
    ruleEdit: "Edit",
    ruleEditing: "Editing",
    ruleMatchesTitle: "What it marks",
    ruleMatchesEmpty: "Nothing right now.",
    ruleVersion: (n: number) => `v${n}`,
    ruleHistoryTitle: "How this rule has read",
    ruleHistoryEmpty: "No history yet.",
    ruleVersionCurrent: "current",
    ruleVersionSince: (from: string, to: string | null) => (to ? `${from} to ${to}` : `since ${from}`),
    ruleVersionStanding: (n: number) => (n === 1 ? "1 conclusion stands on it" : `${n} conclusions stand on it`),
    /* Premises should read as 'on what grounds', so start with 'because' instead of listing bare items */
    ruleMatchBecause: (premises: string) => `because ${premises}`,
    /* The same entity can appear several times because of readings from different periods; without this paragraph it looks like a duplicate */
    ruleMatchSpan: (from: string, to: string | null) =>
      to ? `${from} – ${to}` : `since ${from}`,
    ruleMatchesMore: (shown: number, total: number) =>
      `showing ${shown} of ${total}`,
    /* Persistent on the card, not just in the toast at the moment of the run: fewer pushes, and 'not satisfied' looks the same in the result,
       so the reader has to be able to see it at all times */
    ruleCappedChip: "incomplete",
    ruleCappedHint:
      "Some entities carry too many readings of the same attribute to expand every combination, so this rule's conclusions for them are incomplete.",
    refineTitle: "Refine types",
    refineHint: "Narrow entity types that came out broader than they should be.",
    refinePreview: "Look first",
    refineLooking: "Looking…",
    refineRun: "Run and apply",
    refineRunning: "Running…",
    refineNothing: "Nothing to refine.",
    refineCandidates: (n: number) => `${n} entities would be considered`,
    refineNoCandidates: "Retrieval found no class for this one.",
    refineModelSays: (t: string) => `the model called it “${t}”`,
    refineRetyped: (n: number) => `${n} retyped automatically`,
    refineUndo: "Undo this batch",
    refineUndone: (n: number) => `${n} put back`,
    refineForReview: (n: number) => `${n} need your call`,
    refineCrossesAxis: "different axis",
    refineApprovePair: "Approve this class pair",
    refineLeftAlone: (n: number) => `${n} left alone`,
    refineTopCandidate: (c: string) => `closest class was ${c}`,
    instances: "Instances",
    instanceFacts: (n: number) => `${n} facts`,
    description: "Description",
    descriptionHint:
      "Guides the extractor: what belongs here, with a couple of examples. Fed straight into the extraction prompt.",
    attributes: "Attributes",
    attributesHint:
      "Literal-valued fields of this class (a person's salary, a contract's amount). Extracted with evidence and history, like any fact.",
    newAttribute: "New attribute",
    /* Edit dialog (the panel only displays; changes are made in the dialog): the title and the entry point in the panel */
    edit: "Edit",
    editClass: "Edit class",
    editProperty: "Edit relation",
    editAttribute: "Edit attribute",
    attributeHost: "Belongs to class",
    connectTitle: "Connect a relationship",
    connectOpen: "Connect an existing relationship…",
    noDescription: "No description yet.",
    axiomsNone: "None declared.",
    attrDatatype: "Value type",
    attrUnit: "Unit",
    attrUnitHint: "optional — e.g. CNY, %",
    attrSingle: "Single-valued — a new value closes the previous one",
    datatypeNames: {
      text: "Text",
      number: "Number",
      date: "Date",
      bool: "Boolean",
    } as Record<string, string>,
    cancel: "Cancel",
    key: "Key",
    label: "Label",
    color: "Color",
    shapeColor: "Shape & color",
    parent: "Parent class",
    noParent: "(top level)",
    subclasses: "Subclasses",
    qualifiers: "Edge attributes",
    noQualifiers: "None",
    qualifiersHint: "Attributes an edge of this relation may carry, e.g. amount on invested_in",
    noSubclasses: "None",
    disjoint: "Cannot also be",
    disjointHint:
      "Classes nothing can belong to at the same time. A Person is not an Organisation. The consistency check uses this to find classes that can never have an instance.",
    noDisjoint: "None declared yet",
    disjointWithParent:
      "This class inherits from a class it says it cannot be — nothing could ever satisfy it.",
    /* With multiple parents the left column can draw only one place; say which branch it is drawn under */
    primaryParentHint: "Shown in the tree under the first one.",
    /* Type signature. The wording must make clear it is a guide, not a gate: the model can still override it when the ontology is wrong */
    signature: "Type signature",
    signatureHint:
      "Which classes this relation connects. It goes into the extraction prompt as a hint, " +
      "not a gate: it steers the model as it writes, and the text still wins when the " +
      "ontology is wrong.",
    domainLabel: "Subject",
    rangeLabel: "Object",
    anyType: "Any type",
    searchTypes: "Search classes…",
    temporal: "Temporal semantics",
    temporalState: "State (has interval)",
    temporalEvent: "Event (point in time)",
    temporalEternal: "Eternal (timeless)",
    functional: "Functional (single value at a time)",
    inverseFunctional: "Inverse functional (one subject per object at a time)",
    axioms: "Axioms",
    axiomsHint:
      "What this relation guarantees. These are not descriptions — they change what the system does: the temporal engine closes old values, and the reasoning engine adds edges to the graph.",
    functionalHint:
      "One subject, one value at a time. A new value closes the old one.",
    inverseFunctionalHint: "One object, one subject. A project has one lead.",
    transitive: "Transitive",
    transitiveHint: "A→B and B→C means A→C. The engine will add those edges.",
    symmetric: "Symmetric",
    symmetricHint: "A→B means B→A. The engine will add the other direction.",
    asymmetric: "Asymmetric",
    asymmetricHint:
      "A→B rules out B→A. Both directions get reported as a contradiction.",
    irreflexive: "Irreflexive",
    irreflexiveHint: "Nothing can point at itself through this relation.",
    axiomConflict:
      "Symmetric and asymmetric together hold only for a relation with no facts at all — one of the two is wrong.",
    noLink: "None",
    inverseOf: "Inverse",
    inverseOfHint:
      "The relation that says the same thing the other way round. Declare it on one side only — the other direction follows.",
    subPropertyOf: "Super-property",
    subPropertyOfHint:
      "The broader relation this one is a special case of. Stating the specific one also states the broader one.",
    linkMeansInverse: (p: string, q: string) =>
      `A ${p} B also means B ${q} A.`,
    linkMeansSuper: (p: string, q: string) => `A ${p} B also means A ${q} B.`,
    usage: (n: number) => `${n} in use`,
    builtin: "built-in",
    save: "Save",
    delete: "Delete",
    deleteBlocked: "In use — cannot delete",
    /* ---- OWL / RDFS import ---- */
    importShort: "Import",
    importTitle: "Import an ontology",
    importHint: "Import an OWL or RDFS file (.owl, .rdf, .ttl). Classes and properties are matched by IRI, so importing a newer version of the same vocabulary updates what is already here.",
    importPick: "Choose file",
    importChange: "Choose another",
    importReading: "Reading…",
    importApplying: "Importing…",
    importApply: "Import",
    importCancel: "Cancel",
    importParsed: (fmt: string, triples: number) =>
      `${fmt === "rdfxml" ? "RDF/XML" : "Turtle"} · ${triples.toLocaleString()} triples`,
    importNothing:
      "Nothing to import — no classes or properties found in this file.",
    /* Plan's three columns: new / update / key taken */
    importWillCreate: (n: number) => `${n} new`,
    importWillUpdate: (n: number) => `${n} updated`,
    importKeyTaken: (n: number) => `${n} skipped`,
    importClasses: "Classes",
    importRelations: "Relations",
    importAttributes: "Attributes",
    /* Attributes cannot land in the library yet: they need a domain, and the domain waits until the class is created and the IRI resolved */
    importAttributesLater:
      "Parsed, but not created yet — attributes need a class to hang from, which lands in the next step.",
    /* The first thing the preview must warn about: functional makes the temporal engine automatically close old facts.
       part_of, in one such import, a wrong uniqueness declaration created 59 false conflicts */
    warnFunctional: (n: number) =>
      `${n} ${n === 1 ? "relation declares" : "relations declare"} itself functional`,
    warnFunctionalBody:
      "A functional relation may hold one value at a time, so a new fact automatically closes the previous one. When the vocabulary claims uniqueness your data does not keep, that shows up as a queue of conflicts. Review these after importing.",
    /* Second: description goes verbatim into the extraction prompt; classes without it extract noticeably worse */
    warnNoDescription: (n: number) =>
      `${n} ${n === 1 ? "class arrives" : "classes arrive"} with no description`,
    warnNoDescriptionBody:
      "A class description goes verbatim into the extraction prompt — it is the only thing telling the model what belongs there. Write one for these, or they will quietly under-extract.",
    /* key /* Key collision: the report does not resolve it. Auto-adding a suffix would make the next re-import unable to recognize what it created last time */
    warnKeyTaken: (n: number) =>
      `${n} ${n === 1 ? "key is" : "keys are"} already taken`,
    warnKeyTakenBody:
      "Something else already holds this key under a different identity. These are left alone — rename the existing one first if you want the imported version instead.",
    /* A placeholder with no IRI = created by hand or built in for this library; that sentence is more useful than an empty IRI */
    importTakenBy: (iri: string | null) =>
      iri
        ? `taken by ${iri}`
        : "taken by an entry defined in this knowledge base",
    /* Axioms that appeared but are not projected today, listed by name and count: 'not projected yet' is not 'skipped' */
    importUnprojected: "Not projected yet",
    importUnprojectedBody:
      "Axioms this file uses that Utopia does not consume yet. Nothing is lost: the source file is stored as uploaded, so a later version can project them.",
    importDone: (created: number, updated: number) =>
      `Imported — ${created} classes created, ${updated} updated.`,
    importHistory: "Previous imports",
    importColFile: "File",
    importColFormat: "Format",
    importColSize: "Size",
    importColTriples: "Triples",
    importColWhen: "Imported",
    importDetail: "Detail",
    importCreatedN: (n: number) => `${n} new`,
    importUpdatedN: (n: number) => `${n} updated`,
    importSkippedN: (n: number) => `${n} skipped`,
    importTakenN: (n: number) => `${n} key taken`,
    /* Row names of the detailed ledger. **All verbs are past tense**: this screen says what that import did */
    statClassesCreated: "Classes created",
    statClassesUpdated: "Classes updated",
    statClassesTaken: "Classes whose key was taken",
    statClassesNoDesc: "Classes without a description",
    statTriples: "Triples read",
    statRelationsSeen: "Relations in the file",
    statRelationsCreated: "Relations created",
    statRelationsUpdated: "Relations updated",
    statFunctional: "Relations declared functional",
    statInverseLinked: "Inverse properties linked",
    statSubPropertyLinked: "Sub-properties linked",
    statAttributesSeen: "Attributes in the file",
    statAttributesCreated: "Attributes created",
    statAttributesSkipped: "Attributes skipped",
    /* Reasons for skipping. **Each one must say where to act next**, otherwise reporting it is pointless */
    skipReason: {
      key_taken: "key already in use",
      unusable_range: "range cannot be a value type",
      no_domain: "no domain — it never says which class it belongs to",
      domain_skipped: "its domain class was skipped too",
      unknown_domain: "domain class is not in this base",
    } as Record<string, string | undefined>,
    importNoHistory: "No imports yet.",
    importBy: (who: string, when: string) => `${who} · ${when}`,
    importSize: (bytes: number) =>
      bytes < 1024 ? `${bytes} B` : `${(bytes / 1024).toFixed(0)} KB`,
    misses: "Not in the ontology yet",
    missesHint: "What the documents say that the ontology has no place for, and what the agent proposes for it.",
    /* ---- A predicate with two or more open values on one end (#341) ----
       Restrained copy: state in one sentence, consequence in one sentence, the action is on the button. The reader at this level needs to judge
       'can this relation have only one value at a time?', not read an essay on bitemporality */
    uniqueness: "Overlapping values",
    uniquenessHint: "Say which relations may hold only one value at a time, and a new value will end the old one on its own.",
    uniquenessEmpty: "No overlaps. Every holder has at most one open value.",
    /* Subject side / object side: plain words, not functional / inverse functional */
    uniquenessSubject: (n: number) =>
      `${n} subject${n === 1 ? "" : "s"} with two or more open values`,
    uniquenessObject: (n: number) =>
      `${n} value${n === 1 ? "" : "s"} held open by two or more subjects`,
    /* Pressing the button changes the ledger, so first say how much will change */
    uniquenessEffect: (close: number, review: number) =>
      review > 0
        ? `Closes ${close}, sends ${review} to review`
        : `Closes ${close}`,
    uniquenessDeclare: "Declare and close",
    /* Declared, just not yet reconciled against the ledger (imported, or rows that existed before the declaration) */
    uniquenessReconcile: "Close them",
    uniquenessDeclared: "Declared",
    uniquenessBusy: "Closing…",
    uniquenessDone: (close: number, review: number) =>
      review > 0
        ? `Closed ${close} · ${review} in Review`
        : `Closed ${close}`,
    uniquenessSince: (d: string) => `since ${d}`,
    /* The number is the one that keeps climbing **after ignoring**. The whole point of this row is:
       the original 'appeared only once' judgment may no longer hold */
    noMisses: "Nothing the documents say is waiting for a place in the ontology.",
    uncovered: (kindWords: number, phrases: number) => {
      const what = [
        kindWords > 0 ? `${kindWords} ${kindWords === 1 ? "kind of thing" : "kinds of thing"}` : "",
        phrases > 0 ? `${phrases} relation ${phrases === 1 ? "phrase" : "phrases"}` : "",
      ]
        .filter(Boolean)
        .join(" and ");
      return `The documents use ${what} the ontology has no place for. Ask the agent to propose classes and properties for them; nothing is added until you accept it.`;
    },
    approve: "Add",
    /* Button for the mapping tier. Deliberately not 'Add': it adds nothing, the ontology already has it.
       If both buttons said Add, the fact that 'it already exists' would vanish from the interface */
    mapOver: "Use existing",
    /* Impact: how many predicate-less facts adopting a proposal will confirm.
       Without this sentence, 'Add' just creates an empty relation out of nowhere */
    /* Notification for automatic ontology expansion: default-on requires its action to be visible and reversible.
       Recording it only in the audit ledger does not count as visible: that is for verification, not notification */
    proposals: "AI proposals",
    keyHint: "lowercase_snake_case",
    /* ---- 0061: the ontology agent and its questions ---- */
    askAgent: "Ask the agent",
    askAgentHint:
      "Proposes classes and properties from phrases and kind words the ontology does not cover yet, judged against the competency questions.",
    agentQueued: "The agent is looking — proposals appear here when it is done.",
    agentWaiting: "Agent working…",
    agentChip: "agent",
    servesQuestions: (n: number) =>
      n === 1 ? "serves 1 question" : `serves ${n} questions`,
    bindsShapes: (n: number) =>
      n === 1 ? "binds 1 phrase shape" : `binds ${n} phrase shapes`,
    bindsKindWords: (n: number) =>
      n === 1 ? "1 kind word" : `${n} kind words`,
    closestExisting: (key: string, label: string) => `closest: ${key} ${label}`,
    rejectProposal: "Reject",
    rejectReasonPrompt:
      "Why not? (optional — the agent reads it before proposing again)",
    rejectedProposal: "Rejected",
    /* Items a person decided separately during reuse are kept as they were, not overwritten (0061 decision 3): only says how many were kept. Does not claim they were decided after the agent read the material:
       old proposals that did not record when they were read, and decisions made before the proposal, are kept too */
    adoptKept: (n: number) =>
      n === 1
        ? "Saved. One of these was decided differently by a person and stays as they decided."
        : `Saved. ${n} of these were decided differently by a person and stay as they decided.`,
    questions: "Competency questions",
    questionsHint:
      "What this knowledge base should be able to answer. The agent proposes classes and properties that serve these; they are the ontology's acceptance test.",
    questionPlaceholder: "e.g. Which companies did each founder start, and when?",
    addQuestion: "Add",
    noQuestions:
      "No questions yet. Without them the agent judges proposals only by the phrases documents use.",
    retireQuestion: "Retire",
    deleteQuestion: "Delete",
    questionAdded: "Question added",
    questionRetired: "Question retired",
    askQuestions: "Ask the agent for questions",
    askQuestionsHint:
      "Proposes questions from what the documents say most. They wait here until you accept them.",
    questionsQueued: "The agent is writing questions — they appear here when it is done.",
    acceptQuestion: "Accept",
    questionAnswered: "answered",
    questionUnanswered: "not answered",
    /* Where the answer came from (the via recorded by competency.mjs) */
    questionVia: { graph: "from the graph", text: "from text", both: "graph and text" } as Record<string, string>,
    questionsScore: (answered: number, checked: number) =>
      `${answered} of ${checked} answered`,
    questionsScoreHint:
      "Accepted questions the competency bench asked over the graph, and how many it answered.",
    viaGraph: (withGraph: number, graphOnly: number) =>
      `${withGraph} with graph facts · ${graphOnly} graph only`,
    viaGraphHint:
      "Of the answered questions, how many were answered after fetching facts from the graph, and how many without reading document text at all. An answer read from text alone did not need the ontology.",
    proposalsChanged: (changed: number, decided: number) =>
      `${changed} of ${decided} proposals changed`,
    proposalsChangedHint:
      "Of the agent's proposals people decided on, how many were rejected or edited before adoption.",
    /* ---- Schema diagram ---- */
    schemaDiagram: "Schema diagram",
    /* This sentence used to present 'first add a class or import an OWL file' as the precondition to start, although the ontology grows from the corpus by itself
       (0003, on by default): that line is exactly the discouragement #313 describes. Now it only states the status,
       and leaves the actions to the New class and Import already in the left column */
    schemaEmpty:
      "No classes yet. Documents are read without them; classes and properties are what turn what was read into typed facts. Have the agent propose them from your documents, import an ontology, or add a class yourself.",
    schemaEmptyAction: "See what the documents need",
    schemaFitView: "Fit view",
    schemaZoomIn: "Zoom in",
    schemaZoomOut: "Zoom out",
    schemaLegendInheritance: "Inheritance",
    schemaLegendRelation: "Relations",
    schemaLegendDisjoint: "Disjoint",
    schemaLegendRule: "Business rule",
    schemaUnscoped: (n: number) => `Unscoped properties (${n})`,
    schemaUnscopedHint:
      "Not limited to specific classes, so no line on the canvas would be honest. Select one to inspect or edit it.",
    // Framing note: the large ontology draws only the classes the library uses; here we say how many were not drawn, why, and where to find them
    schemaMoreClasses: (n: number) => (n === 1 ? "+1 class" : `+${n} classes`),
    schemaScopeInUseHint:
      "Drawn: the classes with instances, and their ancestors. Pick any other class in the rail to add it.",
    schemaScopeTopHint:
      "No class has instances yet, so the top two levels are drawn. Pick any other class in the rail to add it.",
    schemaClosePanel: "Close",
    // The four sections in the panel: definition (form) / relations (edges) / attributes (literal value fields) / instances.
    // Relations and attributes are different things: the left column's Properties lists only relations, attributes hang under their class
    schemaTabDefinition: "Definition",
    schemaTabRelations: "Relations",
    schemaTabAttributes: "Attributes",
    schemaTabInstances: "Instances",
    schemaAddRelationship: "New relationship…",
    schemaCheckDefects: (n: number) =>
      n === 1
        ? "1 new ontology issue from this change"
        : `${n} new ontology issues from this change`,
    schemaCheckReview: "Review",
    /** The word written on the edge where relations are merged into one line on the canvas */
    schemaBundle: (n: number) => `${n} relations`,
    schemaOutgoing: "From this class",
    schemaIncoming: "To this class",
    schemaNoRelationships:
      "This class takes part in no relationships. Its inheritance is shown under Definition.",
    schemaNoInstances: "No instances yet.",
    schemaConnectHint: "Connect using an existing relationship",
    schemaConnectPlaceholder: "Search relationships…",
    schemaConnectAs: "As",
    schemaConnect: "Connect",
    schemaConnected: (label: string) => `Connected via ${label}.`,
  },
  mapping: {
    title: "Data mapping",
    hint: "Which tables and columns each business concept points at, and how it is computed. Ask answers only from confirmed definitions.",
    tabDefinitions: "Definitions",
    tabSources: "Data sources",
    filterAll: "All",
    filterProposed: "Pending",
    filterConfirmed: "Confirmed",
    filterRejected: "Rejected",
    searchPlaceholder: "Search concept, source or table…",
    total: (n: number) => `${n} total`,
    range: (from: number, to: number, total: number) =>
      `${from}–${to} of ${total}`,
    prev: "Previous",
    next: "Next",
    empty:
      "No definitions yet. Mount a data source, then run Explore to have an agent propose a first batch.",
    emptyFiltered: "No definitions match.",
    rejectedHint:
      "Rejected ones are listed too — otherwise “why was this concept never mapped?” has no answer.",
    colConcept: "Concept",
    colSource: "Source",
    colDefinition: "How it is computed",
    colStatus: "Status",
    derivedBadge: "Derived",
    noDefinition: "(empty)",
    approve: "Confirm",
    reject: "Reject",
    selectPage: "Select this page",
    selectMapping: (name: string, source: string) =>
      `Select ${name} from ${source}`,
    selected: (n: number) => (n === 1 ? "1 selected" : `${n} selected`),
    edit: "Edit",
    editTitle: "Revise definition",
    fieldTable: "Table",
    fieldExpr: "Expression",
    fieldSql: "SQL",
    fieldUnit: "Unit",
    fieldSummary: "Summary",
    fieldDerived: "Derived metric (computed, not a column)",
    save: "Save",
    cancel: "Cancel",
    needOne: "Fill in at least one of table, expression or SQL.",
    history: "Revision history",
    historyHint:
      "A full snapshot of the version before each change. Kept so “how was this number computed last quarter?” has an answer.",
    historyEmpty: "Never revised.",
    historyBy: (who: string) => `Revised by ${who}`,
    historyUnknown: "a removed user",
    sourcesHint:
      "Read-only databases mounted here. Mounting ingests the schema so Ask knows which tables exist before writing SQL.",
    mount: "Mount",
    unmount: "Unmount",
    syncSchema: "Refresh schema",
    schemaSynced: (n: number) => `Schema ingested (${n} tables)`,
    // Mounted, schema did not. **Do not call this a failed mount** — the source is mounted
    schemaFailed:
      "The data source is mounted, but its schema could not be ingested — Ask cannot see which tables exist. " +
      "This is in the alert centre; check the connection, then use Refresh schema.",
    explore: "Explore mappings",
    exploreHint:
      "An agent reads these schemas and proposes metric and dimension definitions. Proposals land in Pending; Ask uses them only once confirmed.",
    exploreQueued:
      "Exploration queued — proposals will appear under Pending. If nothing can be proposed, the alert bell will say so.",
    // **The ledger of the latest exploration round**: looking at the list alone cannot answer 'how much was missed' (#503):
    // twelve proposals against a wide table of eighty columns look the same as exactly covering a small library. Pasting this lets people
    // see the coverage scope among 'equal-sized proposals'.
    lastRun: (r: {
      tables: number;
      columns: number;
      returned: number;
      accepted: number;
      truncated: boolean;
    }) => {
      const parts = [
        `${r.tables} tables`,
        `${r.columns} columns`,
        `returned ${r.returned}`,
        `accepted ${r.accepted}`,
      ];
      if (r.truncated) parts.push("(schema truncated)");
      return `Last exploration: ${parts.join(" / ")}`;
    },
    lastRunMissing: "No exploration has run yet.",
    sourcesEmpty: "No data sources mounted.",
    sourcesNoneAvailable:
      "No data sources registered yet — ask a deployment admin to register one.",
    newConn: "Register a new connection",
  },
  review: {
    title: "Review",
    hint: "Duplicates & low-confidence facts",
    tabHistory: "History",
    empty: "Nothing to review — the graph is clean.",
    historyEmpty: "No merges yet.",
    // Left column category navigation
    railOverview: "Overview",
    railPending: "Awaiting your nod",
    railDuplicates: "Duplicates",
    railConflicts: "Conflicts",
    railUnconfirmed: "Unconfirmed",
    railLowConfidence: "Low confidence",
    railMappings: "Data mapping",
    railViolations: "Axioms",
    railDefects: "Ontology",
    railAlignment: "Alignment",
    railErrata: "Errata",
    railDecisions: "Decisions",
    railMerges: "Merges",
    railAgent: "Agent",
    categoryEmpty: "This queue is clear.",
    // The agent queue (0025)
    agentTitle: "Agent",
    agentHint:
      "What the agent proposed or decided for this base, from the decisions people made here before. Answering here is your decision, and it becomes precedent for the next look.",
    agentEmpty: "The agent has not looked at anything yet.",
    agentActions: { merge: "Merge", keep: "Keep apart", unsure: "Unsure" } as Record<string, string>,
    agentStatus: {
      proposed: "Proposed",
      applied: "Applied",
      accepted: "Accepted",
      overridden: "Overridden",
      reverted: "Reverted",
      superseded: "Superseded",
    } as Record<string, string>,
    agentSuggests: (action: string, pct: number) => `Agent: ${action.toLowerCase()} · ${pct}%`,
    agentPrecedents: (n: number) => (n === 1 ? "1 precedent" : `${n} precedents`),
    agentPrecedentMerged: "merged by a person",
    agentPrecedentKept: "kept apart by a person",
    agentPrecedentReverted: "merge reverted by a person",
    agentPrecedentHabit: (merged: number, kept: number, reverted: number) =>
      `This type pair in this base: ${merged} merged, ${kept} kept apart, ${reverted} reverted`,
    agentAnsweredBy: (name: string, date: string) => `${name} · ${date}`,
    agentAsks: "Asks:",
    agentLookups: (n: number) => (n === 1 ? "1 lookup" : `${n} lookups`),
    agentLookedAt: "Looked at",
    overviewAgent: "Agent",
    overviewAgentOff: "Governance is off for this base.",
    overviewAgentSettings: "Turn it on in settings",
    overviewAgentOpen: "Waiting for your answer",
    overviewAgentApplied: "Decided on its own",
    overviewAgentAccepted: "Proposals you accepted",
    overviewAgentOverridden: (n: number) =>
      n === 1 ? "1 overridden, last 30 days" : `${n} overridden, last 30 days`,
    overviewAgentReverted: "Reverted by you",
    overviewAgentRunning: (n: number) =>
      n === 0 ? "The agent is working on its last cluster" : `The agent is working · ${n} pairs still to look at`,
    overviewAgentQueue: (n: number) => (n === 1 ? "1 pair waiting for the agent" : `${n} pairs waiting for the agent`),
    agentDeciding: "The agent is deciding this pair",
    // Overview (#377)
    overviewTitle: "Overview",
    overviewHint:
      "What the base needs from you: how much is waiting and for how long, what has been decided, and how much of the base is still provisional.",
    overviewWaiting: "Waiting",
    overviewAllClear: "Nothing is waiting — the base is clean.",
    overviewOldest: (days: number) =>
      days === 0 ? "oldest since today" : days === 1 ? "oldest since yesterday" : `oldest waiting ${days} days`,
    overviewOpen: "Review these",
    overviewDecided: "Decided",
    overviewLast7: "Last 7 days",
    overviewLast30: "Last 30 days",
    overviewAutomatic: (n: number) =>
      n === 1 ? "1 by the adjudicator" : `${n} by the adjudicator`,
    overviewDaily: "Decisions per day, last 14 days",
    overviewByAction: "By kind",
    overviewByActor: "Who decided",
    overviewNoDecisions: "No decisions in the last 30 days.",
    overviewHealth: "Base health",
    overviewFacts: (n: number) =>
      n === 1 ? "1 fact currently held. Of it:" : `${n} facts currently held. Of them:`,
    overviewContested: "Contested",
    // Decision ledger
    decisionsTitle: "Decisions",
    decisionsHint:
      "Every review decision, by whom and when — snapshots taken at decision time, kept even after the underlying fact is gone.",
    decisionsEmpty: "No decisions recorded yet.",
    aiActor: "AI adjudicator",
    decisionActions: {
      "review.merge": "Merged",
      "review.keep": "Kept apart",
      "fact.confirm": "Confirmed",
      "fact.reject": "Rejected",
      "fact.nod_confirmed": "Confirmed a remembered fact",
      "fact.nod_rejected": "Rejected a remembered fact",
      "fact.close": "Closed",
      "conflict.close_old": "Closed old",
      "conflict.retime_new": "Retimed new start",
      "conflict.keep_both": "Kept both",
      "conflict.reject_new": "Rejected new",
      "merge.revert": "Reverted merge",
      "merge.manual": "Merged manually",
      "fact.time_corrected": "Time corrected",
    } as Record<string, string>,
    /** Reasons for escalating to a human decision. The server stores a code (optionally | detail); the wording lives here */
    escalated: {
      escalate_no_model: "No chat model — the adjudicator could not run",
      escalate_no_verdict: "The adjudicator returned no verdict",
      escalate_entity_changed: "The entity changed while being adjudicated",
      escalate_unsure: "The adjudicator was not confident enough",
      /* A sample for a person to review (0026): the machine is confident but does not act; detail is its original answer */
      escalate_sample: "Sampled for a person; the adjudicator was confident",
      /* Execution gate (0027): a merge immediately sends something outside the graph, so it stays with a person however confident the machine is */
      escalate_impact: "Held for a person; the merge would not stay in the graph",
      escalate_identity:
        "Held for a person; a source treats one side's names as identifiers",
      proposed: "The agent looked and left a proposal",
      governed: "Decided by the agent from precedent",
      namesake: "Two entities with this name in one document",
      /* Ties when profiles cannot be told apart: the score is real, so the percentage still shows (unlike the namesake sentinel value) */
      namesake_tie: "Same name, and the profiles cannot tell them apart",
      shared_name: "Another entity already has this name",
      /* Name vector recall (0041 step 2): short names, the same name in another script; only proposes, the adjudicator decides */
      name_vector: "A similar name, found by vector recall",
      /* Names containing one another: equality recall cannot see them, and a short name silently becomes a second entity */
      contains: "One name contains the other",
      ambiguous_name: "Same name, context did not settle it",
      type_drift: "Same name arrived under a different type",
      auto_merged: "Merged by the AI adjudicator",
      kept_apart: "The AI adjudicator judged these different",
    } as Record<string, string>,
    /** Reasons left by the gate, worded by kind; value is the predicate label or a number */
    impact: {
      contradiction: (p: string) => `it would put two “${p}” facts on one entity`,
      derived: (n: string) => `${n} derived facts rest on one side`,
      answered: (n: string) => `one side was named in ${n} answers`,
    } as Record<string, (v: string) => string>,
    duplicates: "Possible duplicates",
    duplicatesHint:
      "Same name, different context. The AI adjudicates clear cases in the background; the rest wait for you. Merging is always reversible.",
    stageAdjudicating: "AI adjudicating",
    stageHuman: "Needs your decision",
    similarity: (pct: number) => `${pct}% context similarity`,
    factsCount: (n: number) => `${n} facts`,
    noFacts: "No recorded facts",
    merge: "Merge",
    keep: "Keep separate",
    /** Reason box (0026). It is a question, not a table: it can be left unanswered */
    rationalePlaceholder: "What told you? Optional",
    // Type filter and batch decisions for duplicates (#428)
    typesAny: "All",
    typesSame: "Same type",
    typesConflict: "Types differ",
    typesEmpty: "Nothing in this group.",
    typesDiffer: (a: string, b: string) => `${a} / ${b}`,
    typesDifferHint:
      "The two sides are different kinds of thing. Merging them would fold one meaning into another.",
    pickPair: "Select this pair",
    selectPage: "Select this page",
    selected: (n: number) => (n === 1 ? "1 selected" : `${n} selected`),
    mergeSelected: "Merge selected",
    keepSelected: "Keep selected apart",
    batchDone: (ok: number, failed: number) =>
      failed === 0
        ? ok === 1
          ? "1 pair decided"
          : `${ok} pairs decided`
        : `${ok} decided, ${failed} could not be — they stay in the queue`,
    lowConfidence: "Low-confidence facts",
    // Alignment queue (#725)
    alignment: "The aligner could not settle these",
    alignmentHint:
      "A phrase between two kinds of thing, or a kind word, where the aligner's two votes disagreed. Pick the property or class the documents' words mean, or say none fits: the statements then stay in the open graph. Your decision stands; the aligner never overrides a person.",
    alignmentValue: "a value",
    alignmentNone: "none fits",
    alignmentForward: "as written",
    alignmentReverse: "reversed",
    alignmentBind: "Bind",
    alignmentLeaveOpen: "Leave open",
    alignmentStatements: (n: number) => (n === 1 ? "1 statement" : `${n} statements`),
    alignmentEntities: (n: number) => (n === 1 ? "1 thing" : `${n} things`),
    alignmentVotes: (first: string, second: string) => `Votes: ${first} · ${second}`,
    // Under the status property, what a single date stands for (#966)
    alignmentMarks: "A single date",
    alignmentMarksStart: "starts it",
    alignmentMarksEnd: "ends it",
    alignmentMarksNone: "neither",
    alignmentMarksStartHint: "joined, was appointed, took over: it holds from that date",
    alignmentMarksEndHint: "left, resigned from, stepped down as: it ends on that date",
    alignmentMarksNoneHint:
      "only seen to hold then: statements with a single date stay in the open graph",
    alignmentMarksRequired: "Say what a single date marks first.",
    alignmentAwaitsMarks: (property: string) =>
      `Bound to ${property}, but what a single date marks is not settled. Until you say, statements with a single date stay in the open graph.`,
    alignmentRuleImplies: (property: string) => `also implies ${property}`,
    alignmentRuleObjectIsStatement: "object: the statement's own object",
    alignmentRuleReading: (reading: string) => `object: read from the words as ${reading.replace(/_/g, " ")}`,
    alignmentRuleKindWord: (word: string) => `things called "${word}"`,
    alignmentApprove: "Approve rule",
    alignmentReject: "Reject",
    alignmentRuleAccepted: "Saved. Implied facts are being computed in the background.",
    // Errata queue (0044 decision 7)
    errata: "The errata agent held these for you",
    errataHint:
      "After extraction an agent rereads each document's typed facts, structural flags first, and retracts, revises or adds with the document's own words as evidence. An action that would reach outside the graph (a derived fact rests on it, someone asked about it, or it would give a one-value property two values) waits here for a person.",
    errataRetract: "wants to retract",
    errataRevise: "wants to revise to",
    errataAdd: "wants to add",
    errataFlag: (flag: string) =>
      ({
        domain: "flagged: subject outside the property's kinds",
        range: "flagged: object outside the property's kinds",
        name_absent: "flagged: a name not in the document",
        no_date: "flagged: date property without a date",
      })[flag] ?? flag,
    errataHeld: (detail: string) => {
      const [kind, ...rest] = detail.split(" ");
      const value = rest.join(" ");
      if (kind === "derived") return `Held: ${value} derived fact(s) rest on it`;
      if (kind === "answered") return `Held: it was named in ${value} answer(s)`;
      if (kind === "contradiction") return `Held: "${value}" allows one value and would get two`;
      if (kind === "unflagged") return "Held: the structure did not doubt this fact; a person confirms the change";
      return `Held: ${detail}`;
    },
    errataQuote: "Document says:",
    errataApprove: "Apply",
    errataReject: "Reject",
    errataDecided: "Saved.",
    alignmentTooMany: (n: number) => `${n} properties could apply; too many to ask the model. Pick one or leave it open.`,
    alignmentConflict: "This decision conflicts with the current state. Refresh and review it before trying again.",
    alignmentKindWordBusy: "This kind word is being updated by another operation. Please try again shortly.",
    alignmentAccepted: "Decision saved. The typed graph is being recomputed and will refresh here when it is done.",
    defects: "Ontology contradicts itself",
    defectsHint:
      "Problems in the definitions themselves — no facts involved. These come first: while a definition contradicts itself, every fact-level finding that rests on it is suspect.",
    defectSymAsym: "Declared both symmetric and asymmetric",
    defectTransFunc: "Transitive and functional at once",
    defectCycle: "subClassOf runs in a circle",
    defectDisjointAncestor: "Disjoint with its own ancestor",
    defectInheritsDisjoint: "Inherits from two disjoint classes",
    defectInverseSelf: "Its own inverse — say symmetric instead",
    defectInverseNotMutual: "The inverse does not point back",
    defectSubPropertyCycle: "subPropertyOf runs in a circle",
    defectRulesDisagree: "Two rules produce contradicting derivations",
    rulesDisagreeCount: (n: number) =>
      `${n} pair(s) of derivations held back until this is settled`,
    rulesDisagreeRule: (
      a: string,
      va: string,
      b: string,
      vb: string,
      axiom: string,
    ) => `${a} on ${va} with ${b} on ${vb}, against ${axiom}`,
    defectNeverInstantiable: "no instance can ever satisfy it",
    defectFixed: "I fixed the ontology",
    defectAccepted: "Leave it",
    runInference: "Run inference",
    inferring: "Inferring…",
    inferenceNoRules:
      "No transitive or symmetric property is declared, so there is no rule to run.",
    inferenceAdded: (n: number) => `${n} facts derived`,
    inferenceRetracted: (n: number) => `${n} retracted`,
    inferenceNothing: "Nothing new to derive",
    inferenceCapped: (n: number) =>
      `${n} predicate(s) hit the per-predicate limit and were not closed fully`,
    violations: "Axiom violations",
    violationsHint:
      "Facts that contradict axioms your ontology declares. Nothing here is a guess — a predicate that declares no axioms is never checked.",
    violationSelfLoop: "Points at itself",
    violationAsymmetry: "Both directions hold at once",
    violationCycle: "Cycle through the transitive chain",
    /** The three mutually exclusive kinds are reported only when they hold at the same time (#634), so the wording says 'at the same time' */
    violationFunctional: "More than one value at once",
    /** Object side (#634): the same object is pointed to by more than one subject at the same time */
    violationInverseFunctional: "More than one holder at once",
    /** Signature violation (#190 / #196): a fact's subject or object falls outside the class the predicate declares,
     *  the extraction corrects it, but the accept and merge paths used to bypass the check */
    violationSignature: "Subject or object outside the declared types",
    /** 0017: a derivation colliding with an assertion. The card is one review, and the clue points to an upstream error */
    violationDerived: "A derivation contradicts an assertion",
    derivedLine: (s: string, p: string, o: string) =>
      `Derived: ${s} · ${p} · ${o}`,
    derivedBy: (rule: string, via: string) => `by ${rule} on ${via}`,
    assertedLine: (t: string) => `Asserted: ${t}`,
    hintStale:
      "The assertion has no end date and the derivation starts later. It may simply have ended.",
    hintDuplicate:
      "Two entities share this name. They may be the same one.",
    hintUnsure:
      "The assertion was extracted with low confidence. Read its sentence.",
    hintReadBoth: "Read both sentences and decide which one is wrong.",
    closeAssertion: "Give the assertion an end date",
    retractAssertion: "Retract the assertion",
    seeDuplicates: "See duplicates",
    openOntology: "Open the ontology",
    letBothStand: "Let both stand",
    violationVia: (p: string) => `via ${p}`,
    violationPath: (n: number) => `${n} facts in the cycle`,
    retractFact: "Data is wrong",
    /** Violations on double facts and cycles: retract the specific one (#202) */
    retractThis: "Retract",
    retractThisHint: "Withdraw this fact from the graph; the rest stay.",
    relaxAxiom: "Axiom is wrong",
    /** Mutual-exclusion groups can have more than two members */
    acceptBoth: (n: number): string => (n > 2 ? "All are right" : "Both are right"),
    runCheck: "Run check",
    checkNeverRun:
      "Not checked yet. Contradictions are found by asking your ontology, so a run here only reports what its axioms actually say.",
    checking: "Checking…",
    checkNoAxioms:
      "No axioms declared, so nothing could be checked. Import an ontology that declares them.",
    checkFound: (n: number) => `${n} new`,
    /** Computed, but all are already in the queue or already decided: saying '3 conflicts' while the list has only
     *  one entry would make people think the interface dropped something */
    checkNothingNew: "Nothing new",
    checkClean: (n: number) => `${n} facts checked, no contradictions`,
    mappings: "Data mapping",
    mappingsHint:
      "Proposed mappings from a business concept to how it is computed. Confirm one and Ask uses it instead of guessing from the schema.",
    mappingDerived: "derived",
    // Facts extracted from memory, awaiting a person's nod (0015)
    pending: "Awaiting your confirmation",
    pendingHint:
      "Facts extracted from what you asked the assistant to remember. Nothing here is in the graph yet: " +
      "the sentence is kept, the facts wait for your nod. Confirm to add with the sentence as evidence; " +
      "reject and it will not be proposed again.",
    pendingNoPredicate: "The ontology has no relation for this; the word is the model's own.",
    pendingOwnWords: "In the document's own words; binding to the ontology comes with alignment.",
    pendingNoPredicateChip: "no relation in ontology",
    pendingSaidBy: (name: string) => `said by ${name}`,
    /* The same person can have several agents attached; writing only the person's name cannot tell which one recorded it */
    pendingSaidVia: (name: string, agent: string) => `said by ${name} · via ${agent}`,
    nodCardTitle: (n: number) =>
      n === 1
        ? "One fact extracted from this. Confirm to add it to the graph, or reject."
        : `${n} facts extracted from this. Confirm to add them to the graph, or reject.`,
    lowConfidenceHint:
      "Extracted with confidence below 75%. Confirm to trust, reject to remove from the graph (the ledger keeps the record).",
    confirm: "Confirm",
    reject: "Reject",
    confidence: (pct: number) => `${pct}%`,
    mergeHistory: "Merge history",
    mergedBy: (name: string) => `by ${name}`,
    mergedByAi: "by AI adjudicator",
    revert: "Revert",
    reverted: "Reverted",
    ongoing: "now",
    conflicts: "Temporal conflicts",
    conflictsHint:
      "Two facts claim the same single-valued relation. Clear successions close automatically; " +
      "these need a human call. Closing is reversible through the ledger.",
    conflictReason: {
      no_time: "new fact has no date",
      simultaneous: "same start date",
      described_evidence: "the newer fact was read off a picture",
      low_confidence: "the newer fact was not sure enough",
    } as Record<string, string>,
    conflictVs: "vs",
    conflictSince: (d: string) => `since ${d}`,
    closeOld: "Close old",
    closeOldAt: (d: string) => `Close old at ${d}`,
    retimeNew: "Retime new start",
    conflictDate: "Date to close the old fact or retime the new start",
    keepBoth: "Keep both",
    rejectNew: "Reject new",
    closeAtPlaceholder: "2024-06-15 · 2024-06 · 2024 · 2024-06-15T14:32Z",
    unconfirmed: "No longer stated",
    unconfirmedHint:
      "Every source that stated these facts has since been updated without them. " +
      "Nothing is deleted automatically — absence isn't negation. Reject extraction " +
      "errors, or close a fact that genuinely ended (pick the date it ended).",
    closeFact: "Close",
    closeFactAt: (d: string) => `Close at ${d}`,
  },
  /** Generic component copy (SearchSelect etc.) */
  ui: {
    close: "Close",
    noMatches: "No matches",
    keepTyping: (n: number) => `${n} more — keep typing to narrow down`,
  },
  kbset: {
    title: "Knowledge base settings",
    general: "General",
    members: "Members",
    materialize: "Materialize inferences",
    materializeNote:
      "Write facts the ontology entails into the ledger — transitive chains and symmetric pairs. " +
      "They are marked as derived and kept in their own section, so if the declaration behind " +
      "them turns out to be wrong, taking them back takes one click.",
    autoResolveTypes: "Resolve entity types after extraction",
    autoResolveTypesNote:
      "After each document is extracted, run a round of type resolution on entities the engine has not looked at yet. Only refinements within the current class are applied on their own — a re-classification across the tree still waits for you on the Ontology page. Every batch is listed there and can be undone.",
    governance: "Let the agent work the duplicates queue",
    governanceNote:
      "First in, first out. Before deciding a pair the agent reads what people in this base decided on the same names and the same kinds of pairs. It merges only where that history supports it, keeps apart on confidence, and leaves a proposal for everything else. Every decision is listed under Agent on the Review page and can be reverted. Turning this off stops the queue.",
    inferEvery: "Re-derive every",
    minutes: "minutes",
    lastInference: (when: string) => `last run ${when}`,
    failedJobs: (n: number) => (n === 1 ? "1 failed job" : `${n} failed jobs`),
    requeue: "Run again",
    requeued: (n: number) =>
      n === 1 ? "1 job back in the queue" : `${n} jobs back in the queue`,
    exportKb: "Export knowledge base (RDF)",
    exportKbNote: "Download every fact and its provenance as a Turtle file.",
    exportKbAction: "Export",
    /* Corpus language. The wording must make clear this is not the interface language, otherwise someone will treat it as the interface switch */
    cardIdentity: "Name and description",
    cardIdentityNote:
      "Up to 64 characters. This is the name in the switcher and at the top of every page of this base.",
    cardVisibilityNote:
      "Open means everyone in the deployment can read this base. Restricted means only the people listed under Members.",
    cardVisibilityFoot: "Takes effect immediately. Roles granted under Members are kept either way.",
    cardAutomation: "What runs on its own",
    cardAutomationNote:
      "Applies from the next extraction on. Everything these do is listed and can be undone.",
    cardJobs: "Background jobs",
    ontologyLang: "Language of this ontology",
    ontologyLangNote:
      "Which language class and relation descriptions are written in. Those go straight " +
      "into the extraction prompt, so the reader is the model while it reads your documents — " +
      "match your documents, not your interface. Changing this does not rewrite what is " +
      "already here; it decides the language of descriptions written from now on.",
    defaultOpenLabel: "Open to everyone",
    defaultOpenNote:
      "This is the deployment's default knowledge base, so visibility is locked: every member " +
      "gets at least viewer access here, which guarantees nobody signs in to an empty screen. " +
      "It can't be deleted for the same reason. To give someone more than viewing, grant a " +
      "role under Members; for a private space, create a separate knowledge base and set it " +
      "to Restricted.",
    data: "Data",
    dataHint:
      "Mounted read-only databases this knowledge base may query from Chat. " +
      "Mounting ingests the database schema so the assistant knows the tables.",
    dataMount: "Mount",
    dataUnmount: "Unmount",
    dataSyncSchema: "Refresh schema",
    dataSchemaSynced: (n: number) => `Schema ingested (${n} tables)`,
    dataExplore: "Explore mappings",
    dataExploreHint:
      "An agent reads the schemas and proposes metric/dimension definitions — review them in Review before Chat uses them.",
    dataExploreQueued:
      "Exploration queued — proposals will appear in Review shortly.",
    dataNone: "No data sources mounted.",
    dataNoneAvailable:
      "No data sources registered yet — ask a deployment admin to register one.",
    dataNewConn: "Register a new connection",
    activity: "Activity",
    activityHint:
      "Who changed what in this knowledge base. Pure audit — records are append-only.",
    auditAllActions: "All actions",
    auditSince: "From this date",
    auditUntil: "Up to this date",
    auditClear: "Clear filters",
    auditTotal: (n: number) => `${n} events`,
    activityLoadError: "Could not load the activity log.",
    activityRetry: "Retry",
    activityEmpty: "Nothing recorded yet.",
    deletedUser: "a removed user",
    // The two engine actions with an empty actor_id: automatic verdicts in the review queue, and other background work
    adjudicator: "AI adjudicator",
    engine: "the engine",
    auditActions: {
      "entity_type.created": "created entity type",
      "entity_type.updated": "updated entity type",
      "entity_type.deleted": "deleted an entity type",
      "relation_type.created": "created relation type",
      "relation_type.updated": "updated relation type",
      "relation_type.deleted": "deleted a relation type",
      "kb.updated": "updated knowledge base settings",
      "kb.member_set": "set a member role",
      "kb.member_removed": "removed a member",
      "source.created": "created source",
      "source.updated": "updated source",
      "source.deleted": "deleted a source",
      "document.deleted": "deleted document",
      "ontology.imported": "imported an ontology",
    } as Record<string, string>,
    membersHintOpen:
      "Everyone in this deployment can already read this knowledge base, so there is no " +
      "viewer role to grant — list someone here only to give them write access. " +
      "Deployment admins always have it.",
    membersHintRestricted:
      "Only the people listed here can see this knowledge base, and their role decides " +
      "what they can change. Deployment admins always have access.",
    addMember: "Add…",
    addMemberTitle: "Add member",
    roles: { viewer: "Viewer", editor: "Editor", admin: "Admin" },
    remove: "Remove",
    noMembers: "No per-KB roles set.",
    noWriters: "Nobody has been given write access yet.",
    save: "Save",
    saved: "Saved",
    danger: "Danger zone",
    deleteKb: "Delete this knowledge base",
    deleteHint: (name: string) =>
      `Type “${name}” to confirm. Documents, graph and sources are permanently removed.`,
    deleteBtn: "Delete permanently",
    deleteRowTitle: "Delete this knowledge base",
    deleteRowHint: "Documents, graph and sources are removed permanently.",
    deleteRowBtn: "Delete",
  },
  members: {
    title: "Deployment users",
    systemAdmin: "System admin",
    systemAdminHint: "Opens Administration: deployment settings, accounts, data sources. You can't remove your own.",
    remove: "Remove",
    deactivate: "Deactivate",
    cancel: "Cancel",
    deactivateHint:
      "Cuts off access everywhere — sign-in and any token already issued. What they did stays attributed to them.",
    deactivatedTitle: "Deactivated accounts",
    deactivatedHint:
      "They cannot sign in and do not appear in any member list. What they did is still attributed to them — that is why the account is kept rather than deleted.",
    editMember: "Edit member",
    close: "Close",
    save: "Save",
    reactivate: "Restore",
    roleLabel: "Role",
    filterAll: "All users",
    filterAllRoles: "All roles",
    statusLabel: "Status",
    filterActive: "Active",
    filterDeactivated: "Deactivated",
    deactivateConfirm: (name: string) =>
      `Deactivate ${name}? They lose access everywhere. Their past decisions stay on record.`,
    addExisting: "Add existing user",
    userLabel: "User",
    pickUser: "Select a user to add…",
    add: "Add",
    roles: {
      owner: "Owner",
      admin: "Admin",
      editor: "Editor",
      viewer: "Viewer",
    },
  },
};

/** Structural contract for language packs. Other packs are written as `const zh: Strings = {…}`, and a missing entry fails to compile */
export type Strings = typeof en;
