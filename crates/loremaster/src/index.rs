//! In-memory vector index. Exact search is fast enough at 50k memories; an ANN index would be the
//! next step if the limit grew by orders of magnitude.

use std::collections::HashMap;

use loremaster_core::{RecordType, Scope};
use thiserror::Error;

use crate::store::StoredMemory;

/// Embedding dimensions of the pinned model.
pub const DIMENSIONS: usize = 384;

/// A vector of the wrong length.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("vector has {got} dimensions, expected {expected}")]
pub struct DimMismatch {
    pub expected: usize,
    pub got: usize,
}

struct Row {
    memory_id: String,
    record_type: Option<RecordType>,
    scope: Option<Scope>,
    project_id: Option<String>,
    created_at: Option<i64>,
    live: bool,
}

/// One eligible memory with its raw similarity to the query.
#[derive(Debug, PartialEq)]
pub struct Hit<'a> {
    pub memory_id: &'a str,
    pub record_type: Option<RecordType>,
    pub created_at: Option<i64>,
    pub score: f32,
}

/// Vectors of a user's memories, for one embedding model.
pub struct Index {
    model_id: String,
    vectors: Vec<f32>,
    rows: Vec<Row>,
    positions: HashMap<String, usize>,
}

impl Index {
    /// Creates an empty index for `model_id`. Vectors from other models are not comparable, so
    /// they stay out until re-embedded.
    pub fn new(model_id: &str) -> Self {
        Self {
            model_id: model_id.to_owned(),
            vectors: Vec::new(),
            rows: Vec::new(),
            positions: HashMap::new(),
        }
    }

    /// Adds or replaces a memory.
    pub fn insert(&mut self, memory: &StoredMemory) -> Result<(), DimMismatch> {
        let Some(vector) = memory.vector.as_deref() else {
            return Ok(());
        };
        if memory.model_id.as_deref() != Some(self.model_id.as_str()) {
            return Ok(());
        }
        if vector.len() != DIMENSIONS {
            return Err(DimMismatch {
                expected: DIMENSIONS,
                got: vector.len(),
            });
        }
        let record = &memory.record;
        let row = Row {
            memory_id: record.memory_id.clone(),
            record_type: record.record_type,
            scope: record.scope,
            project_id: record.project_id.clone(),
            created_at: record.created_at,
            live: !record.deleted && record.superseded_by.is_none(),
        };
        match self.positions.get(&record.memory_id) {
            Some(&position) => {
                self.vectors[position * DIMENSIONS..][..DIMENSIONS].copy_from_slice(vector);
                self.rows[position] = row;
            }
            None => {
                self.positions
                    .insert(record.memory_id.clone(), self.rows.len());
                self.vectors.extend_from_slice(vector);
                self.rows.push(row);
            }
        }
        Ok(())
    }

    /// Takes a memory out of search results.
    pub fn mark_not_live(&mut self, memory_id: &str) {
        if let Some(&position) = self.positions.get(memory_id) {
            self.rows[position].live = false;
        }
    }

    /// Scores every live memory eligible in `project_id`: user scope, or that project.
    pub fn search(&self, query: &[f32], project_id: &str) -> Vec<Hit<'_>> {
        self.rows
            .iter()
            .zip(self.vectors.as_chunks::<DIMENSIONS>().0)
            .filter(|(row, _)| {
                row.live
                    && match row.scope {
                        Some(Scope::User) => true,
                        Some(Scope::Project) => row.project_id.as_deref() == Some(project_id),
                        None => false,
                    }
            })
            .map(|(row, vector)| Hit {
                memory_id: &row.memory_id,
                record_type: row.record_type,
                created_at: row.created_at,
                score: vector.iter().zip(query).map(|(a, b)| a * b).sum(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loremaster_core::{Kind, Record};

    const MODEL: &str = "model-a";

    fn unit(hot: usize) -> Vec<f32> {
        let mut vector = vec![0.0; DIMENSIONS];
        vector[hot] = 1.0;
        vector
    }

    fn memory(id: &str, project: Option<&str>) -> StoredMemory {
        StoredMemory {
            record: Record {
                user_id: None,
                memory_id: id.into(),
                seq: None,
                hlc: "1761750000123-00002-w".into(),
                writer: "w".into(),
                record_type: Some(RecordType::Fact),
                kind: Some(Kind::Decision),
                scope: Some(project.map_or(Scope::User, |_| Scope::Project)),
                project_id: project.map(str::to_owned),
                text: Some("t".into()),
                created_at: Some(1),
                superseded_by: None,
                deleted: false,
                evict_key: None,
            },
            vector: Some(unit(0)),
            model_id: Some(MODEL.into()),
        }
    }

    fn sample_index() -> Index {
        let mut superseded = memory("D", None);
        superseded.record.superseded_by = Some("A".into());
        let mut deleted = memory("E", None);
        deleted.record.deleted = true;
        let mut index = Index::new(MODEL);
        for row in [
            memory("A", None),
            memory("B", Some("github.com/a/x")),
            memory("C", Some("github.com/a/y")),
            superseded,
            deleted,
        ] {
            index.insert(&row).unwrap();
        }
        index
    }

    fn ids(index: &Index, project: &str) -> Vec<String> {
        let mut ids: Vec<String> = index
            .search(&unit(0), project)
            .into_iter()
            .map(|hit| hit.memory_id.to_owned())
            .collect();
        ids.sort();
        ids
    }

    #[test]
    fn search_is_limited_to_user_scope_and_the_current_project() {
        let index = sample_index();
        assert_eq!(ids(&index, "github.com/a/x"), ["A", "B"]);
        assert_eq!(ids(&index, "github.com/a/y"), ["A", "C"]);
    }

    #[test]
    fn marking_not_live_removes_a_memory() {
        let mut index = sample_index();
        index.mark_not_live("A");
        index.mark_not_live("unknown");
        assert_eq!(ids(&index, "github.com/a/x"), ["B"]);
    }

    #[test]
    fn score_is_the_dot_product() {
        let mut index = Index::new(MODEL);
        index.insert(&memory("A", None)).unwrap();
        let hits = index.search(&unit(0), "p");
        assert_eq!(hits[0].score, 1.0);
        assert_eq!(index.search(&unit(1), "p")[0].score, 0.0);
    }

    #[test]
    fn inserting_an_id_again_replaces_it() {
        let mut index = Index::new(MODEL);
        index.insert(&memory("A", None)).unwrap();
        let mut newer = memory("A", None);
        newer.vector = Some(unit(1));
        index.insert(&newer).unwrap();
        let hits = index.search(&unit(1), "p");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].score, 1.0);
    }

    #[test]
    fn wrong_dimensions_are_rejected() {
        let mut row = memory("A", None);
        row.vector = Some(vec![0.0; 383]);
        assert_eq!(
            Index::new(MODEL).insert(&row),
            Err(DimMismatch {
                expected: 384,
                got: 383
            })
        );
    }

    #[test]
    fn empty_index_finds_nothing() {
        assert!(Index::new(MODEL).search(&unit(0), "p").is_empty());
    }

    #[test]
    fn other_models_and_missing_vectors_stay_out() {
        let mut index = Index::new(MODEL);
        let mut other_model = memory("A", None);
        other_model.model_id = Some("model-b".into());
        index.insert(&other_model).unwrap();
        let mut no_vector = memory("B", None);
        no_vector.vector = None;
        index.insert(&no_vector).unwrap();
        assert!(index.search(&unit(0), "p").is_empty());
    }

    #[test]
    fn row_without_a_scope_is_not_eligible() {
        let mut row = memory("A", None);
        row.record.scope = None;
        let mut index = Index::new(MODEL);
        index.insert(&row).unwrap();
        assert!(index.search(&unit(0), "p").is_empty());
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn search_over_50k_memories_is_fast() {
        let mut index = Index::new(MODEL);
        let mut base = memory("base", None);
        for i in 0..50_000 {
            base.record.memory_id = format!("m{i}");
            let mut vector = unit(i % DIMENSIONS);
            vector[(i * 7) % DIMENSIONS] += 0.5;
            let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
            vector.iter_mut().for_each(|v| *v /= norm);
            base.vector = Some(vector);
            index.insert(&base).unwrap();
        }
        let started = std::time::Instant::now();
        let hits = index.search(&unit(0), "p");
        let elapsed = started.elapsed();
        assert_eq!(hits.len(), 50_000);
        assert!(elapsed.as_millis() < 20, "{elapsed:?}");
    }
}
