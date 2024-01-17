use crate::library::std::ai::memory::{VectorDatabase, VectorDbError};
use async_trait::async_trait;
use hnsw_rs_thousand_birds::dist::DistDot;
use hnsw_rs_thousand_birds::hnsw::{Hnsw, Neighbour};
use http_body_util::BodyExt;
use serde_json::Value;
use std::collections::HashMap;

// TODO: manage multiple independent named collections
pub struct InMemoryVectorDbCollection {
    db: HashMap<usize, Value>,
    id_counter: usize,
    hnsw: Hnsw<f32, DistDot>,
}

pub struct InMemoryVectorDb {
    collections: HashMap<String, InMemoryVectorDbCollection>,
}

impl InMemoryVectorDb {
    pub fn new() -> Self {
        Self {
            collections: HashMap::new(),
        }
    }

    pub fn new_collection(&mut self, collection_name: String) {
        let mut hnsw = Hnsw::<f32, DistDot>::new(
            // max_nb_connection (in hnsw initialization) The maximum number of links from one
            // point to others. Values ranging from 16 to 64 are standard initialising values,
            // the higher the more time consuming.
            16,
            100_000,
            // max_layer (in hnsw initialization)
            // The maximum number of layers in graph. Must be less or equal than 16.
            16,
            // ef_construction (in hnsw initialization)
            // This parameter controls the width of the search for neighbours during insertion.
            // Values from 200 to 800 are standard initialising values, the higher the more time consuming.
            200,
            // Distance function
            DistDot {},
        );
        hnsw.set_extend_candidates(true);
        self.collections.insert(
            collection_name,
            InMemoryVectorDbCollection {
                db: HashMap::new(),
                id_counter: 0,
                hnsw,
            },
        );
    }

    pub fn insert(&mut self, collection_name: String, data: &Vec<(&Vec<f32>, Value)>) {
        // usize is the id
        let mut collection = self.collections.get_mut(&collection_name).unwrap();
        let mut insert_set = vec![];
        for item in data {
            collection.id_counter += 1;
            collection.db.insert(collection.id_counter, item.1.clone());
            insert_set.push((item.0, collection.id_counter));
        }
        collection.hnsw.parallel_insert(&insert_set);
    }

    pub fn search(
        &mut self,
        collection_name: String,
        data: Vec<f32>,
        num_neighbors: usize,
    ) -> Vec<(Neighbour, Value)> {
        let mut collection = self.collections.get_mut(&collection_name).unwrap();
        collection.hnsw.set_searching_mode(true);
        let mut results = vec![];
        let neighbors = collection
            .hnsw
            .parallel_search(&vec![data], num_neighbors, 16);
        for neighbor in neighbors.first().unwrap() {
            results.push((
                neighbor.clone(),
                collection.db.get(&neighbor.d_id).unwrap().clone(),
            ));
        }
        results
    }
}

struct MemoryInMemory {
    client: InMemoryVectorDb,
}

#[async_trait]
impl VectorDatabase<InMemoryVectorDb> for MemoryInMemory {
    fn attach_client(client: InMemoryVectorDb) -> Result<Self, VectorDbError> {
        Ok(MemoryInMemory { client })
    }

    async fn create_collection(
        &mut self,
        collection_name: String,
        embedding_length: u64,
    ) -> Result<(), VectorDbError> {
        self.client.new_collection(collection_name);
        Ok(())
    }

    async fn insert_vector(
        &mut self,
        collection_name: String,
        id: u64,
        vector: Vec<f32>,
        payload: Option<serde_json::Value>,
    ) -> Result<(), VectorDbError> {
        self.client
            .insert(collection_name, &vec![(&vector, payload.unwrap())]);
        Ok(())
    }

    async fn query_by_vector(
        &mut self,
        collection_name: String,
        vector: Vec<f32>,
        top_k: usize,
    ) -> Result<Vec<u64>, VectorDbError> {
        let results = self.client.search(collection_name, vector, top_k);
        Ok(results.iter().map(|r| r.0.d_id as u64).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_memory_db() {
        let mut db = InMemoryVectorDb::new();
        db.new_collection("default".to_string());
        let embedding = vec![0.1, 0.2, 0.3];
        let contents = json!({"name": "test"});
        let row = vec![(&embedding, contents)];
        db.insert("default".to_string(), &row);
        let search = vec![0.1, 0.2, 0.3];
        let result = db.search("default".to_string(), search, 1);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].1, json!({"name": "test"}));
    }
}
