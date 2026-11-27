//! The store's integration tests compile into **one** test binary, rather than one per file.
//!
//! Previously the 120 files under `tests/` were 120 binaries: each had to be compiled separately, with the whole dependency graph
//! linked once more (a large share of the test job's compile time on CI), and they ran one after another in series,
//! with most binaries holding only one or two tests, so the machine spent most of its time waiting for the next process to start. After merging into one
//! it is compiled once and linked once, and tests still run in parallel among themselves: locally, 274 tests went from 128 seconds to 13 seconds.
//!
//! The cost is that the tests here **really do run at the same time**, whereas before, tests in different files never ran concurrently. Most
//! tests build and tear down one KB of their own, so they are naturally unrelated to each other. Tests that touch state **outside a KB** stay at the top level of `tests/`,
//! and each remains a separate binary (cargo runs binaries one at a time, so nobody else is running while they do):
//!
//! - `a_queued_job_wakes_an_idle_worker`: guards the `utopia_jobs` notification channel, which any other test's enqueue would wake.
//! - `a_vector_index_is_built_by_a_job`, `a_vector_build_uses_one_connection`: run `CREATE INDEX CONCURRENTLY` on `chunks`.
//!   Two CONCURRENTLY builds at the same time will inevitably deadlock each other (the Postgres rule; the comment on
//!   `vector_index::build` explains why it is serial by design), and the former also counts the build jobs of the whole DB.
//! - `human_phrase_materialization_delivery`: starts a real `run_worker`, which claims **any** queued job in the DB,
//!   and on meeting one of a kind that is not its own, errors and marks it failed.
//! - `the_engine_redraws_only_what_it_drew`: runs migration 0057's backfill statement verbatim; that `UPDATE facts` does not filter by kb and locks rows across the whole DB,
//!   deadlocking against the test that is recomputing a timeline at the same time —
//!   30% of runs in the experiment with the merged binary fell over here.
//! - `a_rewritten_row_gets_its_statements_back`: runs migration 0096 verbatim; it likewise does not filter by kb —
//!   the corrected and duplicate rows are found across the whole DB, and it can change rows that another test is materialising or rewriting.
//! - `a_row_a_rule_implied_follows_its_statements`: runs migration 0101 verbatim; likewise does not filter by kb —
//!   it recomputes the witnesses of typed rows across the whole DB by source, and can change rows that another test is materialising.
//! - `a_document_waits_for_one_extraction`: runs migration 9011 verbatim; it deletes the duplicate extractions queued across the whole DB,
//!   without filtering by kb, and would delete extractions another test has queued.
//! - `a_due_base_is_queued_once`: `queue_due_inferences` scans every KB that is due across the whole DB. KBs created since 0050 have inference switched on
//!   and have never run one, so every KB another test just created counts as due and gets an inference job queued by it — a test counting its own KB's jobs
//!   (for example `phrase_bindings_delivery_tests` in the lib unit tests) would then get one too many.
//! - `intake_runs_while_model_work_holds_every_slot`: likewise starts a real `run_worker`, and likewise claims
//!   any queued job in the DB, erroring on ones that are not its own.
//! - `a_name_embedding_runs_while_model_work_holds_every_slot`: as above, with `embed_names` instead.
//!
//! New tests belong in this directory by default; only those touching the kind of global state above go at the top level, and a line here should explain why.
mod a_batch_decides_like_a_person;
mod a_batch_gathers_its_neighbours_in_order;
mod a_bound_statement_becomes_a_typed_fact;
mod a_chunk_says_where_its_words_came_from;
mod a_clash_needs_both_at_once;
mod a_contradiction_points_upstream;
mod a_corrected_time_keeps_its_statements;
mod a_cycle_holds_at_one_moment;
mod a_cycle_is_keyed_by_all_its_facts;
mod a_cycle_search_that_stops_says_so;
mod a_decision_records_why;
mod a_declaration_arrives_late;
mod a_declared_disjointness_keeps_names_apart;
mod a_deferred_job_does_not_spend_its_budget;
mod a_definition_can_be_written_by_hand;
mod a_deleted_document_no_longer_attests;
mod a_deletion_is_an_event;
mod a_deletion_reattests_after_its_source;
mod a_derivation_follows_the_second_clock;
mod a_described_thing_is_an_entity_without_a_name;
mod a_dirty_ledger_stops_the_migration;
mod a_disambiguator_follows_the_ontology;
mod a_document_opening_is_its_first_live_chunk;
mod a_fact_awaits_a_nod;
mod a_fact_says_where_its_dates_came_from;
mod a_fact_waits_for_its_new_version_to_speak;
mod a_failed_job_finds_its_way_back;
mod a_forward_reference_is_judged_at_commit;
mod a_governor_reads_the_ledger;
mod a_judged_entity_waits_its_turn;
mod a_kind_word_binds_to_a_class;
mod a_late_value_takes_its_place_in_history;
mod a_mapping_is_not_a_fact;
mod a_merge_keeps_distinct_statements;
mod a_merge_rewinds_with_the_second_clock;
mod a_merged_target_stays_out_of_the_export;
mod a_moment_marks_its_state;
mod a_name_created_twice_at_once_is_one_entity;
mod a_name_is_a_fact;
mod a_namesake_tie_goes_to_review_not_a_coin_flip;
mod a_number_is_one_number_however_written;
mod a_page_never_skips_a_row;
mod a_path_joins_two_entities;
mod a_pending_statement_keeps_the_documents_words;
mod a_plan_step_follows_its_premise;
mod a_proof_reaches_the_sentence;
mod a_purge_is_final;
mod a_purge_judges_its_blobs_once;
mod a_pushed_item_closes_what_its_new_version_stopped_stating;
mod a_qualifier_is_not_the_edges_identity;
mod a_question_picks_its_definitions;
mod a_reader_keeps_its_key_for_its_provider;
mod a_relation_points_only_inside_its_own_kb;
mod a_remembered_episode_strips_nul;
mod a_retired_account;
mod a_retraction_leaves_the_graph;
mod a_retrieval_is_recorded_only_for_a_decaying_source;
mod a_review_has_a_summary;
mod a_review_page_is_assembled_in_batches;
mod a_rewritten_statement_keeps_its_section_dates;
mod a_rewritten_statement_stays_a_statement;
mod a_role_word_moves_onto_its_evidence_chunks;
mod a_rule_computes_what_it_concludes;
mod a_rule_concludes_a_type;
mod a_rule_reads_what_a_rule_concluded;
mod a_schema_document_is_searched_not_extracted;
mod a_search_puts_the_exact_name_first;
mod a_search_reads_the_base_as_it_was;
mod a_secret_is_sealed_at_rest;
mod a_signature_holds_on_every_path;
mod a_signature_vector_lasts_while_its_text_does;
mod a_similar_name_is_proposed_not_merged;
mod a_single_name_candidate_is_not_counted;
mod a_source_closes_its_unconfirmed_facts;
mod a_source_decay_is_set_and_cleared_by_update;
mod a_source_decays_only_when_asked;
mod a_source_kind_is_listed_once;
mod a_source_over_its_cap_gives_up_its_lowest_scores;
mod a_source_reaches_only_where_it_was_granted;
mod a_stale_document_is_found_among_a_decaying_sources;
mod a_statement_is_attested_by_what_its_document_says;
mod a_statement_is_materialized_only_when_its_binding_fits;
mod a_stopped_chat_is_replayed_as_incomplete;
mod a_substring_search_matches_names_and_aliases_only;
mod a_time_mention_is_resolved_against_its_document;
mod a_time_mention_is_words_not_a_date;
mod a_timeline_holds_whatever_the_order;
mod a_timeline_is_recomputed_from_the_rows_it_has;
mod a_token_is_the_person_but_not_all_of_them;
mod a_trimmed_description_is_not_stale;
mod a_typed_fact_is_as_confirmed_as_its_statement;
mod a_typed_row_is_attested_by_its_live_statements;
mod a_viewer_never_sees_a_credential;
mod a_wide_embedding_is_ranked_at_full_precision;
mod a_wrong_time_can_be_corrected;
mod adopting_an_iri_adopts_the_shape;
mod an_admin_grants_system_admin;
mod an_agent_can_record;
mod an_alias_counts_only_as_a_known_as_name;
mod an_attribute_value_is_read_in_its_unit;
mod an_automatic_merge_is_gated_by_what_it_can_undo;
mod an_axiom_that_moved_closes_its_findings;
mod an_earlier_mention_keeps_the_stated_end;
mod an_empty_ontology_does_not_cover_the_corpus;
mod an_end_date_closes_the_open_span;
mod an_event_holds_at_the_moment_it_names;
mod an_evicted_document_is_remembered_until_it_is_restored;
mod an_exact_name_search_returns_only_that_entity;
mod an_exploration_says_what_it_covered;
mod an_export_carries_the_whole_ledger;
mod an_extraction_is_queued_once_per_document;
mod an_identity_name_attaches_by_its_exact_name;
mod an_identity_name_skips_what_it_must_not_hold;
mod an_instance_list_names_what_a_type_lacks;
mod an_old_version_keeps_no_vector;
mod an_open_statement_keeps_the_documents_words;
mod an_sso_login_finds_or_opens_its_account;
mod an_undeclared_name_is_looked_up;
mod an_unknown_date_is_not_an_open_one;
mod an_untyped_name_meets_its_namesake;
mod an_upload_has_no_date;
mod axioms_judge_the_ledger;
mod axioms_reach_the_database;
mod bench_100k;
mod blocked_for_entity_respects_as_of;
mod closing_an_unconfirmed_fact_clears_the_queue;
mod concurrent_chunk_replacement;
mod cross_kb_provenance_fails_closed;
mod derived_facts_are_second_class;
mod ended_when_unknown;
mod evidence_is_read_by_document_through_an_index;
mod evidence_stays_inside_its_base;
mod exploration_describes_the_data;
mod export_surfaces;
mod exported_references_never_cross_a_kb;
mod extraction_progress_counts_a_document_once;
mod facts_corroborate_an_identity;
mod graph_changes;
mod history_shows_the_merge_itself;
mod human_type_decisions;
mod identity_name_seed;
mod materialization_is_serial;
mod merge_direction_counts_live_facts_in_each_base;
mod migration_0070_runs_under_any_search_path;
mod negative_binding_definition_edit;
mod no_predicate_still_shows;
mod ontology_regressions;
mod phrase_signature_evidence;
mod rejecting_a_statement_retires_its_typed_row;
mod review_stages;
mod rss_full_content;
mod rss_ledger_contract;
mod same_name_peers_respects_as_of;
mod search_entities_degree_respects_as_of;
mod the_backstop_can_be_raised;
mod the_floor_under_retrieval;
mod the_nearest_chunk_is_found_however_it_is_reached;
mod the_second_clock_can_be_rewound;
mod the_world_axis_reaches_the_second;
mod tied_signatures_come_in_a_fixed_order;
