//! utopia-search: embedded Tantivy full-text index (jieba Chinese tokenization) + RRF fusion.
//! One index for many KBs: kb_id is a filter field; chunk bodies live in Postgres, and the index only stores the id mapping.

mod docs;
pub use docs::{DocsIndex, DocsSection};

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use anyhow::Context;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, TermQuery, TermSetQuery};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, STORED, STRING,
};
use tantivy::tokenizer::TextAnalyzer;
use tantivy::{Index, IndexReader, IndexWriter, TantivyDocument, Term};

const JIEBA: &str = "jieba";
const WRITER_HEAP: usize = 64 * 1024 * 1024;

pub struct SearchIndex {
    writer: Mutex<IndexWriter>,
    reader: IndexReader,
    analyzer: TextAnalyzer,
    f_chunk_id: Field,
    f_kb_id: Field,
    f_document_id: Field,
    f_text: Field,
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub chunk_id: String,
    pub score: f32,
}

impl SearchIndex {
    pub fn open(dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;

        let mut schema_builder = Schema::builder();
        let text_indexing = TextFieldIndexing::default()
            .set_tokenizer(JIEBA)
            .set_index_option(IndexRecordOption::WithFreqsAndPositions);
        let f_chunk_id = schema_builder.add_text_field("chunk_id", STRING | STORED);
        let f_kb_id = schema_builder.add_text_field("kb_id", STRING);
        let f_document_id = schema_builder.add_text_field("document_id", STRING);
        let f_text = schema_builder.add_text_field(
            "text",
            TextOptions::default().set_indexing_options(text_indexing),
        );
        let schema = schema_builder.build();

        let mmap =
            tantivy::directory::MmapDirectory::open(dir).context("Failed to open index dir")?;
        let index =
            Index::open_or_create(mmap, schema).context("Failed to open/create Tantivy index")?;
        index
            .tokenizers()
            .register(JIEBA, tantivy_jieba::JiebaTokenizer::new());

        let writer = index.writer(WRITER_HEAP)?;
        // Every write reloads itself under the writer lock; keeping automatic reload on top of that would let an async reload land late
        // and, carrying an older commit, hide a document that was just written. This process holds the only writer,
        // so nobody else commits, and a manual reload misses nothing
        let reader = index
            .reader_builder()
            .reload_policy(tantivy::ReloadPolicy::Manual)
            .try_into()?;
        let analyzer = index
            .tokenizers()
            .get(JIEBA)
            .context("jieba tokenizer is not registered")?;

        Ok(Self {
            writer: Mutex::new(writer),
            reader,
            analyzer,
            f_chunk_id,
            f_kb_id,
            f_document_id,
            f_text,
        })
    }

    /// Rebuild the index entries of a document (delete then add, idempotent), then commit.
    pub fn reindex_document(
        &self,
        kb_id: &str,
        document_id: &str,
        chunks: &[(String, String)],
    ) -> anyhow::Result<()> {
        let mut writer = self.writer.lock().expect("writer lock is poisoned");
        writer.delete_term(Term::from_field_text(self.f_document_id, document_id));
        for (chunk_id, text) in chunks {
            let mut doc = TantivyDocument::default();
            doc.add_text(self.f_chunk_id, chunk_id);
            doc.add_text(self.f_kb_id, kb_id);
            doc.add_text(self.f_document_id, document_id);
            doc.add_text(self.f_text, text);
            writer.add_document(doc)?;
        }
        writer.commit()?;
        // Readable right after writing: do not wait for the reader's async reload window
        self.reader.reload()?;
        Ok(())
    }

    /// Discard an incomplete rebuild while retaining the index's writer lock.
    /// Removing its directory first could erase another process's live index.
    pub fn clear(&self) -> anyhow::Result<()> {
        let mut writer = self.writer.lock().expect("writer lock is poisoned");
        writer.delete_all_documents()?;
        writer.commit()?;
        self.reader.reload()?;
        Ok(())
    }

    pub fn delete_document(&self, document_id: &str) -> anyhow::Result<()> {
        let mut writer = self.writer.lock().expect("writer lock is poisoned");
        writer.delete_term(Term::from_field_text(self.f_document_id, document_id));
        writer.commit()?;
        self.reader.reload()?;
        Ok(())
    }

    /// Number of chunks in the index. **Reconcile it against the database count at startup**: the index directory is a file separate
    /// from the database (a new machine, an unmounted volume, or corruption can all leave it empty), and once it is empty,
    /// search silently returns zero results, with no visible sign in the UI.
    pub fn len(&self) -> usize {
        self.reader.searcher().num_docs() as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// BM25 search (scoped to a kb).
    /// The query is tokenized with the exact same jieba analyzer as the index, then combined with OR --
    /// QueryParser cannot be used: a whole CJK sentence would be treated as a phrase query (words must appear consecutively), and recall drops to zero.
    ///
    /// `documents`: search only within these documents. Like kb, it is a `Must`, and `limit` counts only the allowed chunks
    pub fn search(
        &self,
        kb_id: &str,
        query: &str,
        limit: usize,
        documents: Option<&[String]>,
    ) -> anyhow::Result<Vec<Hit>> {
        let mut analyzer = self.analyzer.clone();
        let mut stream = analyzer.token_stream(query);
        let mut term_queries: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        while stream.advance() {
            let token = stream.token();
            let word = token.text.trim();
            if word.is_empty() || word.chars().all(|c| !c.is_alphanumeric()) {
                continue;
            }
            term_queries.push((
                Occur::Should,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.f_text, word),
                    IndexRecordOption::WithFreqs,
                )),
            ));
        }
        if term_queries.is_empty() {
            return Ok(Vec::new());
        }

        let kb_filter: Box<dyn Query> = Box::new(TermQuery::new(
            Term::from_field_text(self.f_kb_id, kb_id),
            IndexRecordOption::Basic,
        ));
        let text_query: Box<dyn Query> = Box::new(BooleanQuery::new(term_queries));
        let mut clauses = vec![(Occur::Must, kb_filter), (Occur::Must, text_query)];
        if let Some(documents) = documents {
            let terms = documents
                .iter()
                .map(|id| Term::from_field_text(self.f_document_id, id));
            clauses.push((Occur::Must, Box::new(TermSetQuery::new(terms))));
        }
        let combined = BooleanQuery::new(clauses);

        let searcher = self.reader.searcher();
        let collector = TopDocs::with_limit(limit).order_by_score();
        let top = searcher.search(&combined, &collector)?;
        let mut hits = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let doc: TantivyDocument = searcher.doc(addr)?;
            if let Some(chunk_id) = doc.get_first(self.f_chunk_id).and_then(|v| v.as_str()) {
                hits.push(Hit {
                    chunk_id: chunk_id.to_string(),
                    score,
                });
            }
        }
        Ok(hits)
    }
}

/// One fused entry: the RRF total score, and its rank in each channel (0-based; None if it did not make that channel).
/// Scores are only comparable within the same query; in other words, each query has its own ruler
#[derive(Debug, Clone, PartialEq)]
pub struct Fused {
    pub id: String,
    pub score: f64,
    pub ranks: Vec<Option<usize>>,
}

/// Reciprocal Rank Fusion: fuses rankings from multiple retrieval channels (k=60 is an empirical constant).
pub fn rrf_fuse(lists: &[Vec<String>], limit: usize) -> Vec<String> {
    rrf_fuse_scored(lists, limit)
        .into_iter()
        .map(|fused| fused.id)
        .collect()
}

/// Same as [`rrf_fuse`], but also returns the scores and each channel's rank
pub fn rrf_fuse_scored(lists: &[Vec<String>], limit: usize) -> Vec<Fused> {
    const K: f64 = 60.0;
    let mut by_id: HashMap<String, Fused> = HashMap::new();
    for (channel, list) in lists.iter().enumerate() {
        for (rank, id) in list.iter().enumerate() {
            let fused = by_id.entry(id.clone()).or_insert_with(|| Fused {
                id: id.clone(),
                score: 0.0,
                ranks: vec![None; lists.len()],
            });
            fused.score += 1.0 / (K + rank as f64 + 1.0);
            fused.ranks[channel] = Some(rank);
        }
    }
    let mut ranked: Vec<Fused> = by_id.into_values().collect();
    // Ties are ordered by id; otherwise HashMap's random iteration order would change which evidence and cutoff survive the limit.
    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    ranked.truncate(limit);
    ranked
}

#[cfg(test)]
mod rrf_tests {
    use super::{rrf_fuse, rrf_fuse_scored, Fused};

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// A single channel's results are that channel's order; fusion must not reorder them
    #[test]
    fn one_list_comes_back_in_its_own_order() {
        let lists = vec![ids(&["a", "b", "c"])];
        assert_eq!(rrf_fuse(&lists, 10), ids(&["a", "b", "c"]));
    }

    /// Items named by both channels rank first; items in only one channel are compared by summing their reciprocal ranks:
    /// a = 1/61 + 1/62, c = 1/63 + 1/61, b = 1/62
    #[test]
    fn two_lists_follow_the_reciprocal_rank_arithmetic() {
        let lists = vec![ids(&["a", "b", "c"]), ids(&["c", "a"])];
        assert_eq!(rrf_fuse(&lists, 10), ids(&["a", "c", "b"]));
    }

    /// The score is the sum of reciprocal ranks, with each rank recorded at its position in its own channel (#1271)
    #[test]
    fn scored_fusion_reports_the_sum_and_each_channels_rank() {
        let lists = vec![ids(&["a", "b", "c"]), ids(&["c", "a"])];
        let fused = rrf_fuse_scored(&lists, 10);
        assert_eq!(
            fused,
            vec![
                Fused {
                    id: "a".into(),
                    score: 1.0 / 61.0 + 1.0 / 62.0,
                    ranks: vec![Some(0), Some(1)],
                },
                Fused {
                    id: "c".into(),
                    score: 1.0 / 63.0 + 1.0 / 61.0,
                    ranks: vec![Some(2), Some(0)],
                },
                Fused {
                    id: "b".into(),
                    score: 1.0 / 62.0,
                    ranks: vec![Some(1), None],
                },
            ]
        );
    }

    /// An empty channel is not a failed channel: it adds no score and affects no one else
    #[test]
    fn an_empty_list_contributes_nothing() {
        let lists = vec![ids(&["a", "b"]), Vec::new()];
        assert_eq!(rrf_fuse(&lists, 10), ids(&["a", "b"]));
    }

    /// The limit is applied after fusion, not per channel
    #[test]
    fn the_limit_cuts_after_fusion() {
        let lists = vec![ids(&["a", "b"]), ids(&["b", "c"])];
        assert_eq!(rrf_fuse(&lists, 1), ids(&["b"]));
    }

    #[test]
    fn equal_scores_keep_the_same_candidates_at_every_limit() {
        let mut lists = vec![ids(&["z", "b"]), ids(&["y", "a"])];
        let expected = ids(&["y", "z", "a", "b"]);
        // Each call builds a newly seeded HashMap. Both channel heads must
        // precede their runners-up, and a cutoff through either tie is stable.
        for _ in 0..32 {
            for limit in 0..=expected.len() + 1 {
                assert_eq!(
                    rrf_fuse(&lists, limit),
                    expected[..limit.min(expected.len())]
                );
            }
            lists.reverse();
        }
    }

    #[test]
    fn equal_scores_from_opposite_ranks_are_ordered_by_id() {
        let lists = vec![ids(&["c", "a", "b"]), ids(&["b", "a", "c"])];
        // b and c each score 1/61 + 1/63, above a's 2/62. Equal totals
        // must not acquire a preference for whichever channel came first.
        for _ in 0..32 {
            assert_eq!(rrf_fuse(&lists, 3), ids(&["b", "c", "a"]));
            assert_eq!(rrf_fuse(&lists, 1), ids(&["b"]));
        }
    }
}
